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

async fn run_ffmpeg(args: &[&str], path: &Path) -> Result<String> {
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(args)
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
    let hashes = extract_hash_lines(&raw);
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

/// Keeps only the per-frame rows of a `framehash` dump.
///
/// The header carries fields that legitimately differ between two files holding
/// identical pixels: re-encoding a PNG through JPEG XL flips `#sar` from `1/1` to
/// `0/1`, for instance. Comparing the whole dump would report a loss that did not
/// happen.
fn extract_hash_lines(raw: &str) -> String {
    raw.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
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
    fn extracts_only_frame_rows() {
        let lines = extract_hash_lines(ORIGINAL);
        assert_eq!(lines.lines().count(), 1);
        assert!(lines.starts_with("0,"));
        assert!(lines.contains("b78b3172"));
    }

    #[test]
    fn a_real_pixel_change_is_still_detected() {
        let altered = ROUND_TRIPPED.replace("b78b3172", "ffffffff");
        assert_ne!(extract_hash_lines(ORIGINAL), extract_hash_lines(&altered));
    }

    #[test]
    fn multi_frame_output_keeps_every_row() {
        let raw = "#format: frame checksums\n0, 0, 0, 1, 100, aaa\n0, 1, 1, 1, 100, bbb\n";
        assert_eq!(extract_hash_lines(raw).lines().count(), 2);
    }

    #[test]
    fn blank_lines_are_ignored() {
        assert_eq!(extract_hash_lines("#h\n\n0, 0, 0, 1, 1, aa\n\n"), "0, 0, 0, 1, 1, aa");
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
}
