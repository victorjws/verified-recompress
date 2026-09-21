//! Measuring whether a re-encode is visually lossless.
//!
//! The gate is deliberately two-sided. A mean alone hides a scene that fell
//! apart: a file can average 98 while one difficult passage sits at 80, and the
//! average is what most tooling reports. libvmaf's pooled output offers min, max,
//! mean and harmonic mean but no percentiles, so the low end is computed here from
//! the per-frame scores — which is also why frame subsampling is never enabled.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::convert::command;

/// Mean score a re-encode must reach.
pub const MIN_MEAN: f64 = 97.0;
/// Floor for the worst scenes, as a percentile of frames.
pub const MIN_LOW_PERCENTILE: f64 = 95.0;
/// Which percentile counts as "the worst scenes".
pub const LOW_PERCENTILE: f64 = 1.0;

/// Resolution above which the 4K-trained model is the right one.
///
/// Scoring 4K footage with the 1080p model produces numbers that do not mean what
/// they appear to.
const UHD_PIXEL_THRESHOLD: u64 = 1920 * 1080 * 2;

pub const MODEL_HD: &str = "vmaf_v0.6.1";
pub const MODEL_UHD: &str = "vmaf_4k_v0.6.1";

pub fn model_for(width: u32, height: u32) -> &'static str {
    if u64::from(width) * u64::from(height) > UHD_PIXEL_THRESHOLD {
        MODEL_UHD
    } else {
        MODEL_HD
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Scores {
    pub mean: f64,
    pub min: f64,
    /// Score at [`LOW_PERCENTILE`], computed from the per-frame values.
    pub low_percentile: f64,
    pub frames: usize,
}

impl Scores {
    /// Whether the encode is good enough to replace its source.
    pub fn passes(&self) -> bool {
        self.mean >= MIN_MEAN && self.low_percentile >= MIN_LOW_PERCENTILE
    }

    pub fn summary(&self) -> String {
        format!(
            "mean {:.2}, p{:.0} {:.2}, min {:.2}, {} frames",
            self.mean, LOW_PERCENTILE, self.low_percentile, self.min, self.frames
        )
    }
}

#[derive(Debug, Deserialize)]
struct RawLog {
    #[serde(default)]
    frames: Vec<RawFrame>,
}

#[derive(Debug, Deserialize)]
struct RawFrame {
    metrics: RawMetrics,
}

#[derive(Debug, Deserialize)]
struct RawMetrics {
    vmaf: Option<f64>,
}

pub fn parse_log(json: &str) -> Result<Scores> {
    let raw: RawLog = serde_json::from_str(json).context("failed to parse the libvmaf log")?;
    let values: Vec<f64> = raw
        .frames
        .iter()
        .filter_map(|f| f.metrics.vmaf)
        .filter(|v| v.is_finite())
        .collect();
    scores_from(values)
}

fn scores_from(mut values: Vec<f64>) -> Result<Scores> {
    if values.is_empty() {
        bail!("the libvmaf log contained no frame scores");
    }
    values.sort_by(|a, b| a.partial_cmp(b).expect("scores are finite"));

    let frames = values.len();
    let mean = values.iter().sum::<f64>() / frames as f64;
    Ok(Scores {
        mean,
        min: values[0],
        low_percentile: percentile(&values, LOW_PERCENTILE),
        frames,
    })
}

/// Nearest-rank percentile over an ascending slice.
///
/// Nearest-rank rather than interpolation because the point is to name an actual
/// frame's score, not a value no frame achieved. On short clips this collapses to
/// the minimum, which is the conservative answer.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    debug_assert!(!sorted.is_empty());
    let rank = (p / 100.0 * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

/// Scores `encoded` against `original`.
///
/// Both are decoded in full and compared frame by frame, so they must have the
/// same resolution and frame count; a mismatch makes the comparison meaningless
/// rather than merely inaccurate.
pub async fn measure(
    encoded: &Path,
    original: &Path,
    model: &str,
    work_dir: &Path,
    cores: &[usize],
    hwaccel: bool,
) -> Result<Scores> {
    let log = work_dir.join("vmaf.json");

    // The model name contains '=', which also separates filter options, so it has
    // to be quoted.
    let filter = format!(
        "[0:v][1:v]libvmaf=model='version={model}':log_fmt=json:log_path={}:n_threads={}",
        log.display(),
        cores.len().max(1),
    );

    let mut cmd = command("ffmpeg", cores);
    cmd.args(["-v", "error"]);
    // Decoding both streams at once is the expensive part; hand it to the GPU when
    // one is available so the CPU stays free for encoding.
    if hwaccel {
        cmd.args(["-hwaccel", "cuda"]);
    }
    // Order matters: the first input is the distorted one, the second the reference.
    cmd.arg("-i").arg(encoded);
    if hwaccel {
        cmd.args(["-hwaccel", "cuda"]);
    }
    cmd.arg("-i")
        .arg(original)
        .args(["-lavfi", &filter, "-f", "null", "-"]);

    let output = cmd.output().await.context("failed to execute ffmpeg")?;
    if !output.status.success() {
        bail!(
            "VMAF measurement failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let json = tokio::fs::read_to_string(&log)
        .await
        .context("libvmaf wrote no log")?;
    let scores = parse_log(&json);
    let _ = tokio::fs::remove_file(&log).await;
    scores
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a real ffmpeg 9.0.1 / libvmaf 3.2.0 run.
    const LOG: &str = r#"{
      "version": "3.2.0",
      "fps": 288.52,
      "frames": [
        {"frameNum": 0, "metrics": {"integer_adm2": 0.99, "vmaf": 94.760000}},
        {"frameNum": 1, "metrics": {"integer_adm2": 0.99, "vmaf": 94.160000}},
        {"frameNum": 2, "metrics": {"integer_adm2": 0.99, "vmaf": 96.680000}},
        {"frameNum": 3, "metrics": {"integer_adm2": 0.99, "vmaf": 97.670000}},
        {"frameNum": 4, "metrics": {"integer_adm2": 0.99, "vmaf": 98.389949}},
        {"frameNum": 5, "metrics": {"integer_adm2": 0.99, "vmaf": 94.540000}},
        {"frameNum": 6, "metrics": {"integer_adm2": 0.99, "vmaf": 95.580000}},
        {"frameNum": 7, "metrics": {"integer_adm2": 0.99, "vmaf": 95.270000}},
        {"frameNum": 8, "metrics": {"integer_adm2": 0.99, "vmaf": 97.440000}},
        {"frameNum": 9, "metrics": {"integer_adm2": 0.99, "vmaf": 93.835674}}
      ],
      "pooled_metrics": {
        "vmaf": {"min": 93.835674, "max": 98.389949, "mean": 95.83326, "harmonic_mean": 95.809368}
      }
    }"#;

    #[test]
    fn parses_a_real_log() {
        let s = parse_log(LOG).unwrap();
        assert_eq!(s.frames, 10);
        // Close to libvmaf's own pooled mean; the fixture rounds most frames to
        // two decimals, so an exact match is not expected.
        assert!((s.mean - 95.83326).abs() < 0.01, "{}", s.mean);
        assert!((s.min - 93.835674).abs() < 1e-6);
    }

    #[test]
    fn a_mediocre_encode_is_rejected() {
        assert!(!parse_log(LOG).unwrap().passes());
    }

    /// The case the two-sided gate exists for: a strong average concealing a
    /// stretch that fell apart. Four bad frames in two hundred is 2%, so they
    /// reach into the first percentile.
    #[test]
    fn a_good_mean_does_not_excuse_a_collapsed_scene() {
        let mut values = vec![99.5; 196];
        values.extend([40.0; 4]);
        let s = scores_from(values).unwrap();
        assert!(s.mean > MIN_MEAN, "mean is {}", s.mean);
        assert_eq!(s.low_percentile, 40.0);
        assert!(!s.passes(), "a collapsed scene must fail the gate");
    }

    /// A percentile gate does not react to a single isolated frame, and that is
    /// deliberate: per-frame VMAF dips at scene cuts, so a hard minimum would
    /// reject good encodes constantly. One frame in two hundred is 0.5% and sits
    /// below the first percentile.
    ///
    /// The worst frame is still reported, so it is visible rather than hidden.
    #[test]
    fn an_isolated_bad_frame_does_not_trip_the_percentile_gate() {
        let mut values = vec![99.5; 199];
        values.push(40.0);
        let s = scores_from(values).unwrap();
        assert_eq!(s.low_percentile, 99.5);
        assert!(s.passes());
        assert_eq!(s.min, 40.0);
        assert!(s.summary().contains("min 40.00"), "{}", s.summary());
    }

    #[test]
    fn a_genuinely_transparent_encode_passes() {
        let s = scores_from(vec![98.0; 500]).unwrap();
        assert!(s.passes());
    }

    /// Right at the thresholds, both of which are inclusive.
    #[test]
    fn the_boundary_is_inclusive() {
        let s = scores_from(vec![MIN_MEAN; 100]).unwrap();
        assert!(s.passes());

        let mut values = vec![99.0; 99];
        values.push(MIN_LOW_PERCENTILE - 0.01);
        assert!(!scores_from(values).unwrap().passes());
    }

    #[test]
    fn percentile_uses_nearest_rank() {
        let sorted: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&sorted, 1.0), 1.0);
        assert_eq!(percentile(&sorted, 50.0), 50.0);
        assert_eq!(percentile(&sorted, 100.0), 100.0);
    }

    /// On a short clip there is no meaningful 1st percentile, so it becomes the
    /// minimum, which is the conservative reading.
    #[test]
    fn a_short_clip_falls_back_to_the_minimum() {
        let s = scores_from(vec![90.0, 99.0, 99.0]).unwrap();
        assert_eq!(s.low_percentile, 90.0);
        assert_eq!(s.min, 90.0);
    }

    #[test]
    fn an_empty_log_is_an_error() {
        assert!(parse_log(r#"{"frames": []}"#).is_err());
        assert!(parse_log(r#"{"version":"3.2.0"}"#).is_err());
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(parse_log("not json").is_err());
    }

    /// Scoring 4K with the HD model yields numbers that do not mean what they look
    /// like, so the choice is made from the frame size.
    #[test]
    fn the_model_follows_the_resolution() {
        assert_eq!(model_for(1920, 1080), MODEL_HD);
        assert_eq!(model_for(1280, 720), MODEL_HD);
        assert_eq!(model_for(3840, 2160), MODEL_UHD);
        assert_eq!(model_for(4096, 2160), MODEL_UHD);
    }

    #[test]
    fn the_summary_names_both_gated_figures() {
        let text = parse_log(LOG).unwrap().summary();
        assert!(text.contains("mean"));
        assert!(text.contains("p1"));
    }
}
