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
pub mod video_av1;
pub mod video_lossless;

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
    /// Per-stream digests of encoded data, for verifying a stream copy.
    Streams(video_lossless::StreamDigest),
}

/// Takes whatever digests the recipe's verification will need.
/// What both sides of a WebP comparison are put into. WebP stores 8-bit ARGB,
/// so nothing is lost on the way in or out.
const WEBP_PIX_FMT: &str = "rgba";

pub async fn fingerprint(recipe: Recipe, input: &Path) -> Result<Fingerprint> {
    let sha256 = hash::sha256_file(input).await?;
    let content = match recipe {
        // Byte-exactness is provable on its own, and the JPEG round trip is checked
        // against the file bytes rather than pixels for the reasons in `jxl`.
        Recipe::JxlFromJpeg => None,
        Recipe::JxlFromRaster => Some(ContentHash::Pixels(hash::frame_hash(input).await?)),
        // WebP is an 8-bit ARGB format, so normalising costs nothing and makes
        // the comparison against the decoded JXL possible at all.
        Recipe::JxlFromWebp => Some(ContentHash::Pixels(
            hash::frame_hash_as(input, WEBP_PIX_FMT).await?,
        )),
        // Audio keeps a sample digest as a fallback for sources whose container
        // metadata FLAC cannot carry across.
        Recipe::Flac | Recipe::FlacRecompress => {
            Some(ContentHash::Pcm(hash::pcm_md5(input).await?))
        }
        // A remux copies encoded data, so the digests are of the streams
        // themselves rather than of anything decoded.
        Recipe::TsRemux => Some(ContentHash::Streams(
            video_lossless::StreamDigest::of(input).await?,
        )),
        Recipe::Ffv1 => Some(ContentHash::Pixels(hash::frame_hash(input).await?)),
        // AV1 is scored against the source directly, so there is nothing to
        // record in advance.
        Recipe::Av1 => None,
    };
    Ok(Fingerprint { sha256, content })
}

/// Options that only the video recipes need.
#[derive(Debug, Clone, Copy, Default)]
pub struct VideoOptions {
    pub preset: Option<u8>,
    pub temporal_filtering_off: bool,
    pub hwaccel: bool,
    /// Lets ffmpeg drop damaged transport-stream packets. Makes the remux lossy,
    /// so it is opt-in and recorded as such.
    pub allow_discard_corrupt: bool,
}

impl VideoOptions {
    /// The preset this run will actually encode at, with the default resolved.
    ///
    /// A cached CRF only holds for the preset it was found at, so the caller
    /// needs the resolved figure rather than the `Option`.
    pub fn preset_used(self) -> u8 {
        self.preset.unwrap_or(video_av1::DEFAULT_PRESET)
    }

    fn settings(self) -> video_av1::Settings {
        video_av1::Settings {
            preset: self.preset_used(),
            temporal_filtering: !self.temporal_filtering_off,
            hwaccel: self.hwaccel,
        }
    }
}

/// Encodes `input` to `output` according to `recipe`.
///
/// AV1 is absent here: it cannot be separated from its measurement, because the
/// CRF is chosen by scoring the result. [`convert_av1`] does both.
pub async fn encode(
    recipe: Recipe,
    input: &Path,
    output: &Path,
    work_dir: &Path,
    cores: &[usize],
    video: VideoOptions,
) -> Result<()> {
    match recipe {
        Recipe::JxlFromJpeg => jxl::encode_from_jpeg(input, output, cores).await,
        Recipe::JxlFromRaster | Recipe::JxlFromWebp => {
            jxl::encode_from_raster(input, output, work_dir, cores).await
        }
        Recipe::Flac | Recipe::FlacRecompress => audio::encode_flac(input, output, cores).await,
        Recipe::TsRemux => {
            video_lossless::remux_to_mp4(input, output, cores, video.allow_discard_corrupt).await
        }
        Recipe::Ffv1 => video_lossless::encode_ffv1(input, output, cores).await,
        Recipe::Av1 => bail!("AV1 encoding goes through convert_av1, which also scores it"),
    }
}

/// Where one encode happens: the files it reads and writes, the scratch space
/// it may use, and the cores it was granted.
#[derive(Debug, Clone, Copy)]
pub struct Workbench<'a> {
    pub input: &'a Path,
    pub output: &'a Path,
    pub work_dir: &'a Path,
    pub cores: &'a [usize],
}

/// Encodes to AV1 and proves the result clears the quality gate.
///
/// `hint` is a CRF that suited this file last time, if one is on record.
/// `report` is `Send` because the pipeline runs files on spawned tasks.
pub async fn convert_av1(
    bench: Workbench<'_>,
    probe: &crate::classify::MediaProbe,
    video: VideoOptions,
    hint: Option<u8>,
    report: &mut (dyn FnMut(video_av1::Stage) + Send),
) -> Result<(Fidelity, video_av1::Attempt)> {
    video_av1::encode_to_gate(bench, probe, &video.settings(), hint, report).await
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
            jxl::verify_raster(output, expected, work_dir, None).await
        }
        Recipe::JxlFromWebp => {
            let Some(ContentHash::Pixels(expected)) = &fingerprint.content else {
                bail!("a raster conversion needs a pixel fingerprint");
            };
            jxl::verify_raster(output, expected, work_dir, Some(WEBP_PIX_FMT)).await
        }
        Recipe::Flac | Recipe::FlacRecompress => {
            let Some(ContentHash::Pcm(expected)) = &fingerprint.content else {
                bail!("an audio conversion needs a PCM fingerprint");
            };
            audio::verify_flac(output, &fingerprint.sha256, expected, work_dir, cores).await
        }
        Recipe::TsRemux => {
            let Some(ContentHash::Streams(expected)) = &fingerprint.content else {
                bail!("a remux needs stream digests");
            };
            video_lossless::verify_remux(output, expected).await
        }
        Recipe::Ffv1 => {
            let Some(ContentHash::Pixels(expected)) = &fingerprint.content else {
                bail!("an FFV1 conversion needs a pixel fingerprint");
            };
            video_lossless::verify_frames(output, expected).await
        }
        Recipe::Av1 => bail!("AV1 is verified as part of convert_av1"),
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
        // An encode dropped mid-flight — a cancelled run, an error unwinding —
        // would otherwise leave ffmpeg running against a staging directory
        // nobody owns any more, still burning the cores it was granted. This
        // cannot help against SIGKILL, where nothing of ours runs at all.
        cmd.stdin(Stdio::null()).kill_on_drop(true);
        return cmd;
    }
    let list = cores
        .iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let mut cmd = Command::new("taskset");
    cmd.arg("-c")
        .arg(list)
        .arg(program)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    cmd
}

fn taskset_available() -> bool {
    cfg!(target_os = "linux")
}

/// One ffmpeg progress report, as `-progress` emits them.
#[derive(Debug, Default, Clone, Copy)]
pub struct Tick {
    /// Position in the output, in microseconds.
    pub out_time_us: u64,
    /// Encoding rate relative to real time, as ffmpeg computes it.
    pub speed: f64,
}

impl Tick {
    /// How far along, given the source duration. `None` when the duration is
    /// unknown, which is better than drawing a bar that means nothing.
    pub fn fraction(&self, duration_secs: Option<f64>) -> Option<f64> {
        let total = duration_secs.filter(|d| *d > 0.0)?;
        Some((self.out_time_us as f64 / 1_000_000.0 / total).clamp(0.0, 1.0))
    }

    /// Seconds of wall clock still to go, from ffmpeg's own speed figure.
    pub fn eta_secs(&self, duration_secs: Option<f64>) -> Option<f64> {
        let total = duration_secs.filter(|d| *d > 0.0)?;
        if self.speed <= 0.0 {
            return None;
        }
        let done = self.out_time_us as f64 / 1_000_000.0;
        Some(((total - done) / self.speed).max(0.0))
    }
}

/// Parses one `key=value` line from `-progress`, updating `tick`.
///
/// Returns true once the block is complete, which is what makes a report worth
/// showing: the values within one block belong to the same instant.
pub(crate) fn absorb_progress(line: &str, tick: &mut Tick) -> bool {
    let Some((key, value)) = line.split_once('=') else {
        return false;
    };
    let value = value.trim();
    match key.trim() {
        "out_time_us" => tick.out_time_us = value.parse().unwrap_or(tick.out_time_us),
        // ffmpeg writes "14.1x", and "N/A" before it has measured anything.
        "speed" => {
            tick.speed = value
                .trim_end_matches('x')
                .parse()
                .unwrap_or(tick.speed)
        }
        "progress" => return true,
        _ => {}
    }
    false
}

/// Runs ffmpeg with `-progress`, reporting each block as it arrives.
///
/// Separate from [`run`] rather than replacing it: every verification path goes
/// through `run`, and they are short commands whose output is wanted whole. This
/// one exists for the encodes that take hours.
///
/// The progress stream has to be read to the end whatever happens. Leaving it
/// unread fills the pipe buffer and stops the encoder.
pub(crate) async fn run_with_progress(
    mut cmd: Command,
    what: &str,
    mut on_tick: impl FnMut(Tick),
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    tracing::debug!("run {}", crate::proc::describe(cmd.as_std()));
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to execute {what}"))?;

    let stdout = child
        .stdout
        .take()
        .context("ffmpeg produced no progress stream")?;
    let stderr = child.stderr.take().context("ffmpeg produced no stderr")?;

    // Drained concurrently: a failing encode can write more than a pipe holds,
    // and a blocked stderr stops the process just as surely as a blocked stdout.
    let collect = tokio::spawn(async move {
        let mut text = String::new();
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            text.push_str(&line);
            text.push('\n');
        }
        text
    });

    let mut tick = Tick::default();
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines.next_line().await? {
        if absorb_progress(&line, &mut tick) {
            on_tick(tick);
        }
    }

    let status = child
        .wait()
        .await
        .with_context(|| format!("failed to wait on {what}"))?;
    let errors = collect.await.unwrap_or_default();
    if !status.success() {
        bail!(
            "{what} failed (exit {}): {}",
            status.code().unwrap_or(-1),
            errors.trim()
        );
    }
    Ok(())
}

/// Runs a command, failing with its stderr attached.
pub(crate) async fn run(mut cmd: Command, what: &str) -> Result<()> {
    tracing::debug!("run {}", crate::proc::describe(cmd.as_std()));
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
