//! FLAC encoding, and the checks that back its guarantees.

use std::path::Path;

use anyhow::{Result, bail};

use super::{Fidelity, command, run, try_run};
use crate::hash;

/// Encodes to FLAC at maximum compression.
///
/// `--keep-foreign-metadata` stores the source container's non-audio chunks inside
/// the FLAC, which upgrades the guarantee from "same samples" to "same bytes": the
/// original file can be rebuilt exactly. It costs almost nothing — on a test WAV,
/// 110 bytes on a 34 KB file.
///
/// Not every source carries chunks FLAC can round-trip, so a rejection falls back
/// to a plain encode. Verification then settles for sample identity, and says so.
pub async fn encode_flac(input: &Path, output: &Path, cores: &[usize]) -> Result<()> {
    let mut cmd = command("flac", cores);
    cmd.args(["-8", "-V", "--keep-foreign-metadata", "-f", "-o"])
        .arg(output)
        .arg(input);
    if try_run(cmd).await? {
        return Ok(());
    }

    // `-V` decodes as it encodes and fails if the samples do not match, so a
    // successful run has already proved the audio survived.
    let mut plain = command("flac", cores);
    plain
        .args(["-8", "-V", "-f", "-o"])
        .arg(output)
        .arg(input);
    run(plain, "flac").await
}

/// Confirms the FLAC reproduces the source, preferring byte-exactness.
///
/// First it tries to rebuild the original container from the stored foreign
/// metadata and compare file bytes. If the FLAC has no such metadata, it falls
/// back to comparing decoded PCM, which is what "lossless audio" ordinarily means.
pub async fn verify_flac(
    output: &Path,
    expected_sha256: &str,
    expected_pcm: &str,
    work_dir: &Path,
    cores: &[usize],
) -> Result<Fidelity> {
    // The stream must be internally sound regardless of which comparison follows.
    let mut test = command("flac", cores);
    test.arg("-t").arg(output);
    if !try_run(test).await? {
        bail!("{} did not pass flac's own integrity test", output.display());
    }

    let rebuilt = work_dir.join("roundtrip.src");
    let mut decode = command("flac", cores);
    decode
        .args(["-d", "--keep-foreign-metadata", "-f", "-o"])
        .arg(&rebuilt)
        .arg(output);

    if try_run(decode).await? {
        let actual = hash::sha256_file(&rebuilt).await;
        let _ = tokio::fs::remove_file(&rebuilt).await;
        if actual? == expected_sha256 {
            return Ok(Fidelity::ByteExact);
        }
        // Foreign metadata was present but did not rebuild the original. Fall
        // through: the samples may still be intact, and the caller is told which
        // guarantee actually held.
    } else {
        let _ = tokio::fs::remove_file(&rebuilt).await;
    }

    let actual = hash::pcm_md5(output).await?;
    if actual != expected_pcm {
        bail!("FLAC output does not decode to the source samples");
    }
    Ok(Fidelity::ContentExact)
}

#[cfg(test)]
mod tests {
    // The behaviour here is entirely about how `flac` responds, so it is covered by
    // the round-trip integration tests against real files rather than by mocking.
}
