//! JPEG XL encoding, and the round-trip checks that back its guarantees.

use std::path::Path;

use anyhow::{Context, Result, bail};

use super::{Fidelity, command, run, try_run};
use crate::classify::{ImageFormat, sniff_image};
use crate::hash;

/// Maximum effort, and not configurable, because there is nothing here to tune.
///
/// A lossless JPEG transcode moves the existing DCT coefficients rather than
/// searching for an encoding, so effort has almost nothing to act on: 10 lands
/// within +/-20 bytes of 9 across real files, for 0-10% more time. A flag would
/// only offer a choice that does not change the outcome.
const JPEG_EFFORT: &str = "10";

/// One below the ceiling, and deliberately so, but only as a starting point:
/// unlike the JPEG path this is a real lever, and where it lands is a property of
/// the drive rather than of the format.
///
/// Measured at a fixed 1280x720, effort 10 costs between 1.6 and 15.7 seconds per
/// megapixel depending only on what the image holds, and returns between 0.2% and
/// 80% — with no relationship between the two. A detailed fractal spends 14s to
/// save 4%; a flat colour field spends 4s to save 80%. Nothing observable before
/// encoding predicts which one a file will be, so the choice belongs to whoever
/// knows the drive. Hence a conservative default and a `--raster-effort` flag,
/// rather than a number picked here.
pub const DEFAULT_RASTER_EFFORT: u8 = 9;

/// Losslessly transcodes an existing JPEG.
///
/// `-j 1` asks for the JPEG-specific path explicitly. It is already the default for
/// JPEG input, but the whole guarantee rests on it, so it is not left implicit.
pub async fn encode_from_jpeg(input: &Path, output: &Path, cores: &[usize]) -> Result<()> {
    let mut cmd = command("cjxl", cores);
    cmd.args(["-j", "1", "-d", "0", "-e", JPEG_EFFORT])
        .arg(threads_flag(cores))
        .arg(input)
        .arg(output);
    run(cmd, "cjxl (jpeg transcode)").await
}

/// Losslessly encodes PNG, GIF, BMP, TIFF, or lossless WebP.
///
/// What the file *is* decides the route, not what it is called. cjxl reads PNG,
/// GIF and JPEG and nothing else, and says only "Getting pixel data failed" to
/// everything it does not recognise, which is what a WebP saved as `.png`, a BMP
/// or a TIFF all produce. So:
///
/// - WebP is expanded to PNG with `dwebp`, libwebp's own decoder.
/// - PNG and GIF go to cjxl directly, and if cjxl still refuses one (libjxl is
///   stricter than most decoders about damaged chunks) it is retried through
///   ffmpeg, which already read the file to fingerprint it.
/// - Anything else goes through ffmpeg.
///
/// The ffmpeg intermediate is a lossless PNG of the first frame, so pixels,
/// bit depth, alpha and colour profile survive; EXIF and text chunks do not.
/// Verification compares against the source's own pixels, so an intermediate that
/// lost something would fail there rather than be accepted.
///
/// `effort` trades encoding time for size; see [`DEFAULT_RASTER_EFFORT`] for why
/// the caller picks it.
pub async fn encode_from_raster(
    input: &Path,
    output: &Path,
    work_dir: &Path,
    cores: &[usize],
    effort: u8,
) -> Result<()> {
    let format = sniff_file(input).await;
    let intermediate = work_dir.join("raster-decoded.png");

    let result = match format {
        Some(ImageFormat::Webp) => {
            let decoded = decode_webp(input, &intermediate, cores).await;
            match decoded {
                Ok(()) => cjxl_raster(&intermediate, output, cores, effort).await,
                Err(e) => Err(e),
            }
        }
        Some(ImageFormat::Png | ImageFormat::Gif) => {
            match cjxl_raster(input, output, cores, effort).await {
                Ok(()) => Ok(()),
                Err(direct) => {
                    tracing::debug!("{}: retrying through ffmpeg: {direct:#}", input.display());
                    let _ = tokio::fs::remove_file(output).await;
                    via_ffmpeg(input, output, &intermediate, cores, effort)
                        .await
                        .map_err(|fallback| {
                            anyhow::anyhow!("{direct:#}; the ffmpeg fallback also failed: {fallback:#}")
                        })
                }
            }
        }
        _ => via_ffmpeg(input, output, &intermediate, cores, effort).await,
    };

    let _ = tokio::fs::remove_file(&intermediate).await;
    result.with_context(|| content_note(input, format))
}

async fn cjxl_raster(source: &Path, output: &Path, cores: &[usize], effort: u8) -> Result<()> {
    let mut cmd = command("cjxl", cores);
    cmd.args(["-d", "0", "-e", &effort.to_string()])
        .arg(threads_flag(cores))
        .arg(source)
        .arg(output);
    run(cmd, "cjxl (raster)").await
}

async fn via_ffmpeg(
    input: &Path,
    output: &Path,
    intermediate: &Path,
    cores: &[usize],
    effort: u8,
) -> Result<()> {
    decode_with_ffmpeg(input, intermediate, cores).await?;
    cjxl_raster(intermediate, output, cores, effort).await
}

/// How much of a file is needed to recognise its format.
const SNIFF_BYTES: usize = 64;

async fn sniff_file(path: &Path) -> Option<ImageFormat> {
    use tokio::io::AsyncReadExt;

    let mut file = tokio::fs::File::open(path).await.ok()?;
    let mut buf = [0u8; SNIFF_BYTES];
    let n = file.read(&mut buf).await.ok()?;
    sniff_image(&buf[..n])
}

/// Names what the bytes were against what the file claimed, so a failure that
/// remains says which kind of file it was.
fn content_note(input: &Path, format: Option<ImageFormat>) -> String {
    let ext = input
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    format!(
        "(content: {}, named .{ext})",
        format.map_or("unrecognised", ImageFormat::name)
    )
}

/// Expands a WebP to PNG. `dwebp` is libwebp's own decoder, so it agrees with
/// whatever produced the file.
async fn decode_webp(input: &Path, output: &Path, cores: &[usize]) -> Result<()> {
    let mut cmd = command("dwebp", cores);
    cmd.arg("-quiet").arg(input).arg("-o").arg(output);
    run(cmd, "dwebp (webp decode)").await
}

/// Re-expresses a still image as a PNG with ffmpeg, for the formats cjxl cannot
/// read. Only the first video stream and frame are kept.
///
/// The colour tags ffmpeg infers are cleared. Some decoders invent them — a
/// TIFF with no colour information comes out tagged "primaries unspecified,
/// gamma 2.2" — and ffmpeg's PNG encoder writes them as `cICP` and `gAMA`
/// chunks, which cjxl then refuses with the same "Getting pixel data failed" this
/// path exists to get around. Samples are untouched; an ICC profile travels
/// separately and is kept.
async fn decode_with_ffmpeg(input: &Path, output: &Path, cores: &[usize]) -> Result<()> {
    const CLEAR_TAGS: &str =
        "setparams=color_primaries=unknown:color_trc=unknown:colorspace=unknown:range=unknown";

    let mut cmd = command("ffmpeg", cores);
    cmd.args(["-v", "error", "-y", "-i"])
        .arg(input)
        .args(["-map", "0:v:0", "-frames:v", "1", "-vf", CLEAR_TAGS, "-c:v", "png"])
        .arg(output);
    run(cmd, "ffmpeg (decode to png)").await
}

fn threads_flag(cores: &[usize]) -> String {
    // 0 lets libjxl pick; otherwise match the cores we were granted.
    format!("--num_threads={}", cores.len())
}

/// Confirms the JXL rebuilds the original JPEG byte for byte.
///
/// The check is deliberately on the reconstructed *file*, not on pixels.
///
/// Comparing pixels would be wrong twice over. It would report false failures,
/// because the JPEG standard specifies the inverse DCT only to a tolerance, so
/// libjpeg, libjpeg-turbo's SIMD paths and libjxl's own decoder legitimately
/// disagree by a unit or so on the same coefficients. Worse, it would report false
/// successes: matching pixels say nothing about whether the `jbrd` reconstruction
/// box survived, and a JXL that decodes to the right image but cannot rebuild the
/// original file would pass — after which the original gets deleted and is gone.
///
/// `-J` makes `djxl` fail outright when reconstruction is impossible instead of
/// quietly decoding to pixels and encoding a *new, lossy* JPEG.
pub async fn verify_jpeg(output: &Path, expected_sha256: &str, work_dir: &Path) -> Result<Fidelity> {
    let rebuilt = work_dir.join("roundtrip.jpg");

    let mut cmd = command("djxl", &[]);
    cmd.arg("-J").arg(output).arg(&rebuilt);
    if !try_run(cmd).await? {
        bail!(
            "{} cannot be rebuilt into its original JPEG; the source is one of the \
             shapes JPEG XL cannot carry losslessly (CMYK, oversized trailing data, \
             or unused quantization tables)",
            output.display()
        );
    }

    let actual = hash::sha256_file(&rebuilt).await;
    let _ = tokio::fs::remove_file(&rebuilt).await;
    let actual = actual?;

    if actual != expected_sha256 {
        bail!(
            "JPEG round trip did not reproduce the original bytes \
             (expected {expected_sha256}, got {actual})"
        );
    }
    Ok(Fidelity::ByteExact)
}

/// Confirms the JXL decodes to the same pixels as the source.
///
/// Unlike the JPEG path there is no reconstruction box here, so byte-exactness is
/// not on offer and pixel identity is exactly the promise being made. Pixel
/// comparison is also sound for these formats: PNG and friends use lossless
/// entropy coding with no inverse DCT, so decoding is bit-exactly specified.
///
/// The JXL is decoded with `djxl` rather than handed straight to ffmpeg, because
/// ffmpeg builds routinely ship the JPEG XL demuxer without a decoder. Going
/// through `djxl` also keeps the reference implementation on both sides.
pub async fn verify_raster(
    output: &Path,
    expected_pixels: &str,
    work_dir: &Path,
    pix_fmt: Option<&str>,
) -> Result<Fidelity> {
    let decoded = work_dir.join("roundtrip.png");

    let mut cmd = command("djxl", &[]);
    cmd.arg(output).arg(&decoded);
    if !try_run(cmd).await? {
        bail!("{} could not be decoded back to pixels", output.display());
    }

    let actual = match pix_fmt {
        Some(fmt) => hash::frame_hash_as(&decoded, fmt).await,
        None => hash::frame_hash(&decoded).await,
    };
    let _ = tokio::fs::remove_file(&decoded).await;

    if actual? != expected_pixels {
        bail!("JPEG XL output does not decode to the source pixels");
    }
    Ok(Fidelity::ContentExact)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_flag_tracks_the_granted_cores() {
        assert_eq!(threads_flag(&[0, 1, 2, 3]), "--num_threads=4");
        assert_eq!(threads_flag(&[]), "--num_threads=0");
    }
}
