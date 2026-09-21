//! Encoding and verification.
//!
//! Nothing here implements a codec. Each recipe shells out to the tool that already
//! does the job well — `cjxl`/`djxl`, `flac`, `ffmpeg` — and this module's work is
//! to invoke them correctly, confine them to their allotted cores, and prove
//! afterwards that nothing was lost.
//!
//! The order of operations matters for disk: a source digest is taken first, the
//! encode runs, then the local source can be dropped, because verification compares
//! against the recorded digest rather than the file.

pub mod audio;
pub mod jxl;

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

use crate::hash;
use crate::policy::Recipe;

/// What a completed conversion is guaranteed to preserve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fidelity {
    /// The original file can be rebuilt byte for byte.
    ByteExact,
    /// Pixels or audio samples are identical; container and metadata bytes are not.
    ContentExact,
}

impl Fidelity {
    pub fn as_str(self) -> &'static str {
        match self {
            Fidelity::ByteExact => "byte-exact",
            Fidelity::ContentExact => "content-exact",
        }
    }
}

/// Digests of the source, taken before encoding so the source can then be deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    /// Of the original bytes. Proves a byte-exact round trip.
    pub sha256: String,
    /// Of the decoded content. The fallback when byte-exactness is not on offer.
    pub content: Option<ContentHash>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentHash {
    /// Per-frame hashes of decoded pixels.
    Pixels(String),
    /// MD5 of decoded PCM samples.
    Pcm(String),
}

/// Takes whatever digests the recipe's verification will need.
pub async fn fingerprint(recipe: Recipe, input: &Path) -> Result<Fingerprint> {
    let sha256 = hash::sha256_file(input).await?;
    let content = match recipe {
        // Byte-exactness is provable on its own, and the JPEG round trip is checked
        // against the file bytes rather than pixels for the reasons in `jxl`.
        Recipe::JxlFromJpeg => None,
        Recipe::JxlFromRaster => Some(ContentHash::Pixels(hash::frame_hash(input).await?)),
        // Audio keeps a sample digest as a fallback for sources whose container
        // metadata FLAC cannot carry across.
        Recipe::Flac | Recipe::FlacRecompress => {
            Some(ContentHash::Pcm(hash::pcm_md5(input).await?))
        }
        Recipe::TsRemux | Recipe::Ffv1 | Recipe::Av1 => None,
    };
    Ok(Fingerprint { sha256, content })
}

/// Encodes `input` to `output` according to `recipe`.
pub async fn encode(recipe: Recipe, input: &Path, output: &Path, cores: &[usize]) -> Result<()> {
    match recipe {
        Recipe::JxlFromJpeg => jxl::encode_from_jpeg(input, output, cores).await,
        Recipe::JxlFromRaster => jxl::encode_from_raster(input, output, cores).await,
        Recipe::Flac | Recipe::FlacRecompress => audio::encode_flac(input, output, cores).await,
        Recipe::TsRemux | Recipe::Ffv1 | Recipe::Av1 => {
            bail!("the video tier is not wired up yet")
        }
    }
}

/// Proves the encode preserved what the recipe promises.
///
/// `work_dir` holds the reconstruction produced for the comparison; it is removed
/// before returning, and is never uploaded.
pub async fn verify(
    recipe: Recipe,
    output: &Path,
    fingerprint: &Fingerprint,
    work_dir: &Path,
    cores: &[usize],
) -> Result<Fidelity> {
    match recipe {
        Recipe::JxlFromJpeg => jxl::verify_jpeg(output, &fingerprint.sha256, work_dir).await,
        Recipe::JxlFromRaster => {
            let Some(ContentHash::Pixels(expected)) = &fingerprint.content else {
                bail!("a raster conversion needs a pixel fingerprint");
            };
            jxl::verify_raster(output, expected, work_dir).await
        }
        Recipe::Flac | Recipe::FlacRecompress => {
            let Some(ContentHash::Pcm(expected)) = &fingerprint.content else {
                bail!("an audio conversion needs a PCM fingerprint");
            };
            audio::verify_flac(output, &fingerprint.sha256, expected, work_dir, cores).await
        }
        Recipe::TsRemux | Recipe::Ffv1 | Recipe::Av1 => {
            bail!("the video tier is not wired up yet")
        }
    }
}

/// Builds a command, confined to `cores` when the platform can do that.
///
/// SVT-AV1 offers no way to cap its own core count (`--lp` is a 0..6 parallelism
/// level, not a processor count), so the limit is imposed from outside with
/// `taskset`. Doing it uniformly for every encoder keeps one mechanism rather than
/// a different knob per tool. On platforms without `taskset` the budget is
/// advisory; preflight says so.
pub fn command(program: &str, cores: &[usize]) -> Command {
    if cores.is_empty() || !taskset_available() {
        let mut cmd = Command::new(program);
        cmd.stdin(Stdio::null());
        return cmd;
    }
    let list = cores
        .iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let mut cmd = Command::new("taskset");
    cmd.arg("-c").arg(list).arg(program).stdin(Stdio::null());
    cmd
}

fn taskset_available() -> bool {
    cfg!(target_os = "linux")
}

/// Runs a command, failing with its stderr attached.
pub(crate) async fn run(mut cmd: Command, what: &str) -> Result<()> {
    let output = cmd
        .output()
        .await
        .with_context(|| format!("failed to execute {what}"))?;
    if !output.status.success() {
        bail!(
            "{what} failed (exit {}): {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Whether a command succeeded, without treating failure as an error.
pub(crate) async fn try_run(mut cmd: Command) -> Result<bool> {
    let output = cmd.output().await.context("failed to execute command")?;
    Ok(output.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fidelity_names_are_stable() {
        assert_eq!(Fidelity::ByteExact.as_str(), "byte-exact");
        assert_eq!(Fidelity::ContentExact.as_str(), "content-exact");
    }

    #[test]
    fn command_without_cores_invokes_the_program_directly() {
        let cmd = command("cjxl", &[]);
        assert_eq!(cmd.as_std().get_program(), "cjxl");
    }

    #[test]
    fn command_with_cores_is_pinned_where_supported() {
        let cmd = command("cjxl", &[0, 1, 2]);
        let program = cmd.as_std().get_program().to_string_lossy().into_owned();
        if cfg!(target_os = "linux") {
            assert_eq!(program, "taskset");
            let args: Vec<_> = cmd
                .as_std()
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            assert_eq!(args, vec!["-c", "0,1,2", "cjxl"]);
        } else {
            // Elsewhere the budget cannot be enforced, so the program runs unpinned.
            assert_eq!(program, "cjxl");
        }
    }
}
