//! Digests used to prove a conversion did not lose anything.
//!
//! Every digest is taken *before* encoding, so the local copy of the source can be
//! deleted as soon as the encoder is done and the staging budget stays small. The
//! comparison afterwards is against the recorded digest, never against the file.

use std::path::Path;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tokio::process::Command;

/// SHA-256 of a file's raw bytes.
///
/// Runs on a blocking thread: hashing multi-gigabyte files would otherwise starve
/// the async runtime.
pub async fn sha256_file(path: &Path) -> Result<String> {
    /// Large enough that syscall overhead disappears against multi-gigabyte files.
    const CHUNK: usize = 1024 * 1024;

    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::io::Read;

        let mut file = std::fs::File::open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; CHUNK];
        loop {
            // Read in chunks rather than via io::copy: sha2 0.11 no longer
            // implements io::Write for its hashers.
            let n = file
                .read(&mut buf)
                .with_context(|| format!("failed to read {}", path.display()))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hex::encode(hasher.finalize()))
    })
    .await
    .context("hashing task panicked")?
}

/// BLAKE3 of a file's raw bytes.
///
/// This is the hash Filen exposes, so an uploaded object can be checked against
/// its local original without downloading it back.
pub async fn blake3_file(path: &Path) -> Result<String> {
    const CHUNK: usize = 1024 * 1024;

    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::io::Read;

        let mut file = std::fs::File::open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let mut hasher = blake3::Hasher::new();
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = file
                .read(&mut buf)
                .with_context(|| format!("failed to read {}", path.display()))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hasher.finalize().to_hex().to_string())
    })
    .await
    .context("hashing task panicked")?
}

async fn run_ffmpeg(args: &[&str], path: &Path) -> Result<String> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error", "-i"]).arg(path).args(args);
    tracing::debug!("run {}", crate::proc::describe(cmd.as_std()));
    let output = cmd
        .output()
        .await
        .context("failed to execute ffmpeg")?;

    if !output.status.success() {
        bail!(
            "ffmpeg failed on {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Per-frame SHA-256 of decoded pixels, for comparing images and video frame by
/// frame regardless of how they are stored.
pub async fn frame_hash(path: &Path) -> Result<String> {
    let raw = run_ffmpeg(&["-map", "0:v", "-f", "framehash", "-hash", "sha256", "-"], path).await?;
    frames_from(&raw, path)
}

/// Frame hashes with the pixels converted to `pix_fmt` first.
///
/// A source and its round trip can hold identical pixels and still hash
/// differently, because ffmpeg decodes each container in its own native format:
/// WebP comes back as ARGB, and the PNG a JXL decodes to is RGB24 whenever there
/// is no alpha. Four channels against three never matches, whatever the pixels
/// say. Converting both sides first compares the picture rather than its
/// memory layout.
///
/// Only for sources that fit the target format without loss. Forcing an 8-bit
/// format on a 16-bit PNG would quietly discard the precision the guarantee is
/// about, so this is not the default.
pub async fn frame_hash_as(path: &Path, pix_fmt: &str) -> Result<String> {
    let raw = run_ffmpeg(
        &[
            "-map", "0:v", "-pix_fmt", pix_fmt, "-f", "framehash", "-hash", "sha256", "-",
        ],
        path,
    )
    .await?;
    frames_from(&raw, path)
}

fn frames_from(raw: &str, path: &Path) -> Result<String> {
    let hashes = extract_hash_lines(raw);
    if hashes.is_empty() {
        bail!("no frames decoded from {}", path.display());
    }
    Ok(hashes)
}

/// MD5 of decoded PCM samples, independent of the container or codec used to store
/// them. FLAC records the same value in its STREAMINFO header.
pub async fn pcm_md5(path: &Path) -> Result<String> {
    let raw = run_ffmpeg(&["-map", "0:a", "-f", "hash", "-hash", "md5", "-"], path).await?;
    let value = raw
        .lines()
        .find_map(|l| l.strip_prefix("MD5="))
        .context("ffmpeg did not report an MD5")?;
    Ok(value.trim().to_string())
}

/// Number of video frames, used to catch packets silently dropped in a remux.
pub async fn video_frame_count(path: &Path) -> Result<u64> {
    let mut cmd = Command::new("ffprobe");
    cmd.args([
        "-v", "error",
        "-select_streams", "v:0",
        "-count_packets",
        "-show_entries", "stream=nb_read_packets",
        "-of", "csv=p=0",
    ])
    .arg(path);
    tracing::debug!("run {}", crate::proc::describe(cmd.as_std()));
    let output = cmd
        .output()
        .await
        .context("failed to execute ffprobe")?;

    if !output.status.success() {
        bail!(
            "ffprobe could not count frames in {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    // A transport stream reports the count twice, once per program, separated by
    // a blank line: `30\n\n30\n`. Take the first value rather than trying to
    // parse the lot.
    //
    // Failing loudly matters here. Defaulting to zero would make the frame-count
    // comparison pass trivially on both sides, which is the opposite of what it
    // is for.
    parse_frame_count(&String::from_utf8_lossy(&output.stdout))
        .with_context(|| format!("could not read a frame count for {}", path.display()))
}

fn parse_frame_count(raw: &str) -> Option<u64> {
    raw.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .and_then(|l| l.parse().ok())
}

/// Extracts just the per-frame picture hashes from a `framehash` dump.
///
/// Only the hash column is kept, in frame order. Everything else on the line is
/// container bookkeeping that legitimately differs between two files holding
/// identical pixels:
///
/// - the header carries `#sar`, which a JPEG XL round trip flips from `1/1` to `0/1`
/// - each row starts with dts, pts and duration, expressed in the container's own
///   timebase, so the same stream reads `0, 1, 2` in a transport stream and
///   `0, 512, 1024` once it is in MP4
///
/// Comparing whole lines therefore reports losses that did not happen. Frame
/// count is checked separately by [`video_frame_count`], and keeping the hashes
/// in order still catches reordering.
fn extract_hash_lines(raw: &str) -> String {
    raw.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| l.rsplit(',').next())
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Captured from ffmpeg 9.0.1 on a PNG and on the same image round-tripped
    /// through JPEG XL. The pixels are identical; the headers are not.
    const ORIGINAL: &str = "\
#format: frame checksums
#version: 2
#hash: SHA256
#software: Lavf63.1.101
#tb 0: 1/25
#media_type 0: video
#codec_id 0: rawvideo
#dimensions 0: 640x480
#sar 0: 1/1
#stream#, dts,        pts, duration,     size, hash
0,          0,          0,        1,   921600, b78b3172712b3102d41639b1bc0fd4a4d39ea325e7c2f780a8a41b84f63b637b
";

    const ROUND_TRIPPED: &str = "\
#format: frame checksums
#version: 2
#hash: SHA256
#software: Lavf63.1.101
#tb 0: 1/25
#media_type 0: video
#codec_id 0: rawvideo
#dimensions 0: 640x480
#sar 0: 0/1
#stream#, dts,        pts, duration,     size, hash
0,          0,          0,        1,   921600, b78b3172712b3102d41639b1bc0fd4a4d39ea325e7c2f780a8a41b84f63b637b
";

    /// The header differs (`#sar 1/1` vs `0/1`) while the pixels match, so a naive
    /// whole-output comparison would report a false loss.
    #[test]
    fn header_differences_do_not_count_as_pixel_differences() {
        assert_ne!(ORIGINAL, ROUND_TRIPPED, "the fixtures should differ overall");
        assert_eq!(extract_hash_lines(ORIGINAL), extract_hash_lines(ROUND_TRIPPED));
    }

    #[test]
    fn extracts_only_the_hash_column() {
        assert_eq!(
            extract_hash_lines(ORIGINAL),
            "b78b3172712b3102d41639b1bc0fd4a4d39ea325e7c2f780a8a41b84f63b637b"
        );
    }

    #[test]
    fn a_real_pixel_change_is_still_detected() {
        let altered = ROUND_TRIPPED.replace("b78b3172", "ffffffff");
        assert_ne!(extract_hash_lines(ORIGINAL), extract_hash_lines(&altered));
    }

    #[test]
    fn multi_frame_output_keeps_every_row() {
        let raw = "#format: frame checksums\n0, 0, 0, 1, 100, aaa\n0, 1, 1, 1, 100, bbb\n";
        assert_eq!(extract_hash_lines(raw), "aaa\nbbb");
    }

    /// Timestamps are expressed in the container's own timebase, so the same
    /// stream reads differently in a transport stream and in MP4 even though every
    /// picture is identical. Captured from a real 25fps remux.
    #[test]
    fn container_timebases_do_not_count_as_pixel_differences() {
        let ts = "\
#tb 0: 1/25
0,          0,          0,        1,   460800, dbb17381286adfadc02887f3d7d9dfd88
0,          1,          1,        1,   460800, dd1178d0cfe3b5009c58b763eb22c3837
";
        let mp4 = "\
#tb 0: 1/12800
0,          0,          0,      512,   460800, dbb17381286adfadc02887f3d7d9dfd88
0,        512,        512,      512,   460800, dd1178d0cfe3b5009c58b763eb22c3837
";
        assert_ne!(ts, mp4, "the fixtures should differ overall");
        assert_eq!(extract_hash_lines(ts), extract_hash_lines(mp4));
    }

    /// Reordered frames must still be caught: order is preserved in the output.
    #[test]
    fn reordered_frames_are_detected() {
        let forward = "0, 0, 0, 1, 1, aaa\n0, 1, 1, 1, 1, bbb";
        let reversed = "0, 0, 0, 1, 1, bbb\n0, 1, 1, 1, 1, aaa";
        assert_ne!(extract_hash_lines(forward), extract_hash_lines(reversed));
    }

    #[test]
    fn blank_lines_are_ignored() {
        assert_eq!(extract_hash_lines("#h\n\n0, 0, 0, 1, 1, aa\n\n"), "aa");
    }

    /// A transport stream reports its packet count once per program, so the raw
    /// output has a blank line and a repeat in it.
    #[test]
    fn frame_count_survives_the_transport_stream_layout() {
        assert_eq!(parse_frame_count("30\n"), Some(30));
        assert_eq!(parse_frame_count("30\n\n30\n"), Some(30));
        assert_eq!(parse_frame_count("  42  \n"), Some(42));
    }

    #[test]
    fn an_unreadable_frame_count_is_not_silently_zero() {
        assert_eq!(parse_frame_count(""), None);
        assert_eq!(parse_frame_count("N/A\n"), None);
    }

    #[tokio::test]
    async fn sha256_matches_a_known_value() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"hello").unwrap();
        file.flush().unwrap();
        assert_eq!(
            sha256_file(file.path()).await.unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[tokio::test]
    async fn sha256_of_a_missing_file_errors() {
        assert!(sha256_file(Path::new("/definitely/not/here")).await.is_err());
    }

    /// Must match what rclone reports for the same bytes, since upload
    /// confirmation compares the two.
    #[tokio::test]
    async fn blake3_matches_the_value_rclone_reports() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"hello").unwrap();
        file.flush().unwrap();
        assert_eq!(
            blake3_file(file.path()).await.unwrap(),
            "ea8f163db38682925e4491c5e58d4bb3506ef8c14eb78a86e908c5624a67200f"
        );
    }

    #[tokio::test]
    async fn blake3_spans_multiple_chunks() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let data = vec![7u8; 3 * 1024 * 1024 + 17];
        file.write_all(&data).unwrap();
        file.flush().unwrap();
        assert_eq!(
            blake3_file(file.path()).await.unwrap(),
            blake3::hash(&data).to_hex().to_string()
        );
    }
}
