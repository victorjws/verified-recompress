//! Video conversions that lose nothing at all.
//!
//! Two quite different things live here. An MPEG transport stream gets its
//! container overhead back by being rewritten as MP4 with the streams copied
//! untouched. An intra-only source — ProRes, DV, MJPEG and friends — is re-encoded
//! to FFV1, which is genuinely lossless: every decoded pixel is identical.
//!
//! Neither needs the `--allow-video` gate, because neither involves a judgement
//! about quality.

use std::path::Path;

use anyhow::{Result, bail};

use super::{Fidelity, command, run};
use crate::hash;

/// Rewrites a transport stream as MP4 without touching the encoded data.
///
/// A transport stream spends a fixed 4 bytes of header on every 188-byte packet,
/// plus a PES header per frame, stuffing, and periodically repeated tables. That
/// is 2-3% at high bitrates and over 6% at low ones, recoverable for free.
///
/// `aac_adtstoasc` rewrites the audio's ADTS headers into the form MP4 expects.
/// Only the header representation changes; the audio payload is copied verbatim.
/// Without it the resulting MP4 has silent or unplayable audio.
pub async fn remux_to_mp4(
    input: &Path,
    output: &Path,
    cores: &[usize],
    allow_discard_corrupt: bool,
) -> Result<()> {
    let mut cmd = command("ffmpeg", cores);
    cmd.args(["-v", "error"]);

    // `+genpts` reconstructs timestamps that DVR captures often lack.
    //
    // `+discardcorrupt` is deliberately not the default: it silently drops damaged
    // packets, which would make the "lossless" claim false. A file with damage
    // fails verification and is left alone unless the user opts in.
    if allow_discard_corrupt {
        cmd.args(["-fflags", "+genpts+discardcorrupt"]);
    } else {
        cmd.args(["-fflags", "+genpts"]);
    }

    cmd.arg("-i")
        .arg(input)
        .args([
            "-map", "0",
            "-c", "copy",
            "-bsf:a", "aac_adtstoasc",
            "-movflags", "+faststart",
            "-y",
        ])
        .arg(output);
    run(cmd, "ffmpeg (transport stream remux)").await
}

/// Re-encodes an intra-only source to FFV1.
///
/// Level 3 with slice CRCs: the slices make it parallel and the checksums make
/// corruption detectable rather than silent.
pub async fn encode_ffv1(input: &Path, output: &Path, cores: &[usize]) -> Result<()> {
    let mut cmd = command("ffmpeg", cores);
    cmd.args(["-v", "error", "-noautorotate", "-i"])
        .arg(input)
        .args([
            "-map", "0",
            "-map_metadata", "0",
            "-c:v", "ffv1",
            "-level", "3",
            "-coder", "1",
            "-context", "1",
            "-g", "1",
            "-slices", "16",
            "-slicecrc", "1",
            // The audio is already lossless or small; copying avoids a second
            // generation of loss on anything lossy.
            "-c:a", "copy",
            "-c:s", "copy",
            "-y",
        ])
        .arg(output);
    run(cmd, "ffmpeg (ffv1)").await
}

/// Confirms every decoded frame is identical to the source's.
///
/// `expected_frames` is the per-frame hash taken before encoding. Comparing
/// decoded frames rather than file bytes is the right test here: the container
/// changed by design, and the claim is about the pictures inside it.
pub async fn verify_frames(output: &Path, expected_frames: &str) -> Result<Fidelity> {
    let actual = hash::frame_hash(output).await?;
    if actual != expected_frames {
        bail!("{} does not decode to the source frames", output.display());
    }
    Ok(Fidelity::ContentExact)
}

/// Confirms a remux carried the content across intact.
///
/// The comparison is of decoded content, not of stored bytes, and it has to be:
/// the same H.264 stream is framed differently depending on the container. MP4
/// stores length-prefixed NAL units (AVCC) while a transport stream uses Annex B
/// start codes, and AAC is ADTS-framed in TS but raw-with-a-header in MP4. The
/// bytes therefore differ by design while the coded pictures and samples are
/// identical, which is exactly what a lossless remux means.
///
/// Frame counts are compared as well, because that is what catches packets
/// dropped as corrupt — a loss that decoded hashes of the surviving frames would
/// not reveal on their own.
pub async fn verify_remux(output: &Path, expected: &StreamDigest) -> Result<Fidelity> {
    let actual = StreamDigest::of(output).await?;
    if actual.frames != expected.frames {
        bail!(
            "the remux has {} video frames, the source had {} (packets were dropped)",
            actual.frames,
            expected.frames
        );
    }
    if actual.video != expected.video {
        bail!("the remux changed the decoded video");
    }
    if actual.audio != expected.audio {
        bail!("the remux changed the decoded audio");
    }
    Ok(Fidelity::ContentExact)
}

/// Digests of a container's decoded content.
///
/// Deliberately not of the stored bytes: those are container-specific even when
/// the coded data is the same. See [`verify_remux`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamDigest {
    /// Per-frame hashes of the decoded picture.
    pub video: Option<String>,
    /// MD5 of the decoded PCM.
    pub audio: Option<String>,
    pub frames: u64,
}

impl StreamDigest {
    pub async fn of(path: &Path) -> Result<Self> {
        Ok(Self {
            video: hash::frame_hash(path).await.ok(),
            audio: hash::pcm_md5(path).await.ok(),
            frames: hash::video_frame_count(path).await?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_compare_by_value() {
        let a = StreamDigest {
            video: Some("v".into()),
            audio: Some("a".into()),
            frames: 10,
        };
        assert_eq!(a.clone(), a);
        assert_ne!(
            a,
            StreamDigest {
                frames: 9,
                ..a.clone()
            }
        );
    }
}
