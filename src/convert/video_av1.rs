//! AV1 re-encoding under a measured quality gate.
//!
//! This is the one tier that is not reversible, so it is the one that has to
//! prove itself. Every encode is scored against its source and discarded if it
//! does not clear the gate; nothing is accepted on the strength of a CRF number
//! alone.
//!
//! Encoding is done in software. The card in this machine has an AV1 encoder, but
//! hardware AV1 trails SVT-AV1 by 15-18 BD-rate points, meaning roughly 30-45%
//! more bytes at matched quality — the opposite of the point. The GPU earns its
//! keep on the decode side instead, where VMAF has to decode two streams at once.

use std::path::Path;

use anyhow::{Context, Result, bail};

use super::{Fidelity, command};
use crate::classify::MediaProbe;
use crate::vmaf::{self, Scores};

/// Default SVT-AV1 preset. The encoder's own default is 8; this trades time for
/// bytes, which is the right trade for a one-off archival pass.
pub const DEFAULT_PRESET: u8 = 3;

/// Starting CRF when no search is available. Deliberately conservative.
const FALLBACK_CRF: u8 = 24;
/// How much to tighten CRF after a failed gate.
const CRF_STEP: u8 = 2;
/// Attempts before giving up on a file.
const MAX_ATTEMPTS: u8 = 3;

pub struct Settings {
    pub preset: u8,
    /// SVT-AV1's temporal filtering. On by default; turning it off reduces
    /// oversmoothing at a 4-8% BD-rate cost.
    pub temporal_filtering: bool,
    /// Whether a CUDA decoder is available.
    pub hwaccel: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            preset: DEFAULT_PRESET,
            temporal_filtering: true,
            hwaccel: false,
        }
    }
}

/// Builds the `-svtav1-params` value.
///
/// `tune` is always set explicitly. SVT-AV1 4.2.0 defaults it to 1 (PSNR), which
/// inflates VMAF and would quietly weaken the very gate this tier depends on.
/// `tune=5` optimises VMAF directly and is never used for the same reason: a
/// metric you optimise against stops measuring anything.
///
/// Film grain synthesis is off. It shrinks files substantially, but it
/// regenerates grain rather than preserving it, so frames differ from the source
/// by construction — outside what "visually lossless" can honestly cover.
pub fn svtav1_params(settings: &Settings) -> String {
    format!(
        "tune=0:film-grain=0:enable-tf={}",
        u8::from(settings.temporal_filtering)
    )
}

/// Encodes one attempt at the given CRF.
pub async fn encode(
    input: &Path,
    output: &Path,
    crf: u8,
    settings: &Settings,
    cores: &[usize],
    on_tick: impl FnMut(super::Tick),
) -> Result<()> {
    let mut cmd = command("ffmpeg", cores);
    // `-progress pipe:1` reports on stdout, which this invocation does not
    // otherwise use. Errors stay on stderr where the failure path expects them.
    cmd.args(["-v", "error", "-progress", "pipe:1", "-nostats"]);
    if settings.hwaccel {
        cmd.args(["-hwaccel", "cuda"]);
    }
    cmd
        // ffmpeg 7.0 onwards applies rotation during decode. Left alone, the
        // rotation is baked into the pixels and also kept in the display matrix,
        // so the result is rotated twice.
        .args(["-noautorotate", "-i"])
        .arg(input)
        .args([
            "-map", "0",
            "-map_metadata", "0",
            "-c:v", "libsvtav1",
            "-preset", &settings.preset.to_string(),
            "-crf", &crf.to_string(),
            // 10-bit even from 8-bit sources: standard AV1 practice, less banding,
            // better quality per bit. libsvtav1 accepts only yuv420p and this.
            "-pix_fmt", "yuv420p10le",
            "-svtav1-params", &svtav1_params(settings),
            // Never re-encode audio. A lossy track re-encoded a second time has
            // the first encoder's quantisation noise treated as signal, and the
            // filterbanks do not line up, so artefacts compound. It is also 1-3%
            // of the file, so there is nothing to win.
            "-c:a", "copy",
            "-c:s", "copy",
            "-c:d", "copy",
            // Carries QuickTime keyed atoms (GPS, device, timezone-bearing dates)
            // through to the output.
            "-movflags", "use_metadata_tags+faststart",
            "-y",
        ])
        .arg(output);
    super::run_with_progress(cmd, "ffmpeg (svt-av1)", on_tick).await
}

/// Finds a CRF that meets the VMAF target, sampling scenes rather than encoding
/// the whole file repeatedly.
///
/// `ab-av1` exists precisely for this. Without it, targeting a quality figure
/// would mean a full encode per candidate, which is not viable on a real library.
pub async fn search_crf(input: &Path, settings: &Settings, cores: &[usize]) -> Result<u8> {
    let mut cmd = command("ab-av1", cores);
    cmd.args([
        "crf-search",
        "--encoder",
        "libsvtav1",
        "--preset",
        &settings.preset.to_string(),
        "--min-vmaf",
        &vmaf::MIN_MEAN.to_string(),
        "--svt",
        &svtav1_params(settings),
        "-i",
    ])
    .arg(input);

    let output = cmd.output().await.context("failed to execute ab-av1")?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() {
        bail!("ab-av1 could not find a suitable CRF: {}", text.trim());
    }
    parse_crf(&text).with_context(|| format!("could not read a CRF from ab-av1 output: {text}"))
}

/// Pulls the chosen CRF out of ab-av1's report.
///
/// ab-av1 bisects a continuous scale, so it reports halves and quarters —
/// "crf 34.25", "crf 37.5" — even though `--crf-increment` defaults to 1.0 for
/// SVT-AV1. ffmpeg's `-crf` is an integer option and accepts the fraction
/// anyway, rounding it half-to-even rather than rejecting it. So the sample
/// ab-av1 says it measured at 34.75 was really encoded at 35.
///
/// Rounding the same way is what makes the parsed value the one that was
/// actually proven, rather than a neighbour that merely looks close.
/// Verified against ffmpeg 9.0.1 and SVT-AV1 4.2.0: 34.25 and 34.5 both encode
/// identically to 34, while 34.75 and 35.5 match 35 and 36.
fn parse_crf(text: &str) -> Option<u8> {
    text.lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("crf ")?;
            let value: f64 = rest.split_whitespace().next()?.parse().ok()?;
            (value.is_finite() && value >= 0.0)
                .then(|| value.round_ties_even().min(63.0) as u8)
        })
        .next_back()
}

pub struct Attempt {
    pub crf: u8,
    pub scores: Scores,
}

/// Where an AV1 conversion has got to.
///
/// One file goes through a CRF search and then up to [`MAX_ATTEMPTS`] rounds of
/// encode-and-score, each of which can run for hours. Naming the stage is the
/// difference between "still going" and "still going, on the third attempt".
#[derive(Debug, Clone, Copy)]
pub enum Stage {
    CrfSearch,
    Encoding {
        attempt: u8,
        crf: u8,
        tick: Option<super::Tick>,
    },
    Scoring {
        attempt: u8,
    },
}

/// How many rounds a file may take, so a caller can say "2 of 3".
pub const ATTEMPTS: u8 = MAX_ATTEMPTS;

/// Encodes, measures, and tightens the CRF until the gate is met or attempts run out.
///
/// The search only samples scenes, so its answer is a starting point rather than
/// a verdict: the full encode is always measured in its own right.
pub async fn encode_to_gate(
    bench: super::Workbench<'_>,
    probe: &MediaProbe,
    settings: &Settings,
    hint: Option<u8>,
    report: &mut (dyn FnMut(Stage) + Send),
) -> Result<(Fidelity, Attempt)> {
    let super::Workbench {
        input,
        output,
        work_dir,
        cores,
    } = bench;
    let video = probe
        .video
        .first()
        .context("no video stream to encode")?;
    let model = vmaf::model_for(video.width, video.height);

    // A hint from a previous attempt at this file is worth more than a fresh
    // search: it cost the same sample encodes to find, and the file has not
    // changed. The full encode is still measured on its own, so a stale hint
    // costs an attempt rather than a wrong answer.
    let mut crf = match hint {
        Some(crf) => {
            tracing::debug!("reusing crf {crf} from a previous attempt");
            crf
        }
        None => {
            report(Stage::CrfSearch);
            match search_crf(input, settings, cores).await {
                Ok(crf) => crf,
                Err(e) => {
                    tracing::debug!("CRF search unavailable ({e:#}); starting from {FALLBACK_CRF}");
                    FALLBACK_CRF
                }
            }
        }
    };

    let mut last = None;
    for attempt in 1..=MAX_ATTEMPTS {
        report(Stage::Encoding {
            attempt,
            crf,
            tick: None,
        });
        encode(input, output, crf, settings, cores, |tick| {
            report(Stage::Encoding {
                attempt,
                crf,
                tick: Some(tick),
            })
        })
        .await?;

        // Scoring decodes both files in full, so it is its own wait and has to
        // say so rather than looking like a stalled encode.
        report(Stage::Scoring { attempt });
        let scores = vmaf::measure(output, input, model, work_dir, cores, settings.hwaccel).await?;

        if scores.passes() {
            return Ok((Fidelity::ContentExact, Attempt { crf, scores }));
        }

        tracing::debug!(
            "attempt {attempt}: crf {crf} scored {}, below the gate",
            scores.summary()
        );
        last = Some(scores);
        crf = crf.saturating_sub(CRF_STEP);
        if crf == 0 {
            break;
        }
    }

    let detail = last.map(|s| s.summary()).unwrap_or_default();
    bail!("no CRF reached the quality gate after {MAX_ATTEMPTS} attempts (best: {detail})")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from ab-av1 0.11.7. It reports fractional CRFs, which an integer
    /// parse silently rejected — so the search "failed" on every real run and
    /// every AV1 conversion quietly fell back to a fixed CRF, having paid for
    /// the search anyway.
    #[test]
    fn a_fractional_crf_is_read_the_way_ffmpeg_reads_it() {
        let report = "\
[INFO ab_av1::command::sample_encode] crf 37.5 VMAF 96.24 predicted video stream size 4.38 MiB (45%)
[INFO ab_av1::command::crf_search] crf 37.5 VMAF 96.24 (45%)
crf 34.25 VMAF 97.12 predicted video stream size 5.98 MiB (61%) taking 12 seconds";
        assert_eq!(parse_crf(report), Some(34));
    }

    /// ffmpeg rounds a fractional `-crf` half-to-even rather than rejecting it,
    /// so ab-av1's samples were encoded at the rounded value all along. Matching
    /// that is what makes the parsed CRF the one ab-av1 actually proved.
    /// Each of these was checked against a real encode.
    #[test]
    fn rounding_matches_what_the_encoder_did() {
        for (reported, encoded) in [
            (34.25, 34),
            (34.5, 34),
            (34.75, 35),
            (35.5, 36),
            (37.5, 38),
        ] {
            assert_eq!(
                parse_crf(&format!("crf {reported} VMAF 96.0")),
                Some(encoded),
                "{reported} is encoded as {encoded}"
            );
        }
    }

    #[test]
    fn a_whole_crf_still_parses() {
        assert_eq!(parse_crf("crf 27 VMAF 97.31 predicted video stream size"), Some(27));
    }

    /// The last line is the verdict; the ones before it are samples along the way.
    #[test]
    fn the_final_line_wins() {
        assert_eq!(parse_crf("crf 45 VMAF 90\ncrf 30 VMAF 96\ncrf 28 VMAF 97"), Some(28));
    }

    #[test]
    fn nonsense_is_not_a_crf() {
        assert_eq!(parse_crf("no crf here"), None);
        assert_eq!(parse_crf("crf notanumber VMAF 90"), None);
        assert_eq!(parse_crf("crf -3 VMAF 90"), None);
        assert_eq!(parse_crf(""), None);
        // Past what SVT-AV1 accepts, clamped rather than wrapped.
        assert_eq!(parse_crf("crf 300 VMAF 10"), Some(63));
    }

    /// The gate is scored with VMAF, so the encoder must not be tuned for it.
    #[test]
    fn the_encoder_is_never_tuned_for_the_metric_that_judges_it() {
        for temporal_filtering in [true, false] {
            let params = svtav1_params(&Settings {
                temporal_filtering,
                ..Default::default()
            });
            assert!(params.contains("tune=0"), "{params}");
            assert!(!params.contains("tune=5"), "{params}");
        }
    }

    /// SVT-AV1 4.2.0 defaults tune to PSNR, which inflates VMAF, so leaving it
    /// unset is not an option.
    #[test]
    fn tune_is_always_stated_explicitly() {
        assert!(svtav1_params(&Settings::default()).starts_with("tune=0"));
    }

    /// Film grain synthesis regenerates grain instead of preserving it, so it
    /// cannot be part of a visually-lossless claim.
    #[test]
    fn film_grain_synthesis_stays_off() {
        assert!(svtav1_params(&Settings::default()).contains("film-grain=0"));
    }

    #[test]
    fn temporal_filtering_is_reflected() {
        assert!(svtav1_params(&Settings::default()).contains("enable-tf=1"));
        assert!(
            svtav1_params(&Settings {
                temporal_filtering: false,
                ..Default::default()
            })
            .contains("enable-tf=0")
        );
    }

    /// SVT-AV1 4.2.0 defaults to preset 8; lower is smaller and slower, which is
    /// the right trade for a one-off archival pass.
    const _: () = assert!(DEFAULT_PRESET < 8);

    #[test]
    fn reads_the_crf_from_ab_av1_output() {
        let text = "\
- crf 32 VMAF 94.21 predicted video stream size 12.1 MiB (60%) taking 20 seconds
- crf 28 VMAF 96.55 predicted video stream size 14.8 MiB (72%) taking 22 seconds
crf 27 VMAF 97.31 predicted video stream size 15.4 MiB (75%) taking 23 seconds";
        assert_eq!(parse_crf(text), Some(27));
    }

    #[test]
    fn a_single_line_result_is_read() {
        assert_eq!(parse_crf("crf 24 VMAF 98.00 predicted"), Some(24));
    }

    #[test]
    fn output_without_a_crf_yields_nothing() {
        assert_eq!(parse_crf("Error: no good crf found"), None);
        assert_eq!(parse_crf(""), None);
    }

    #[test]
    fn a_non_numeric_crf_is_ignored() {
        assert_eq!(parse_crf("crf auto VMAF 90"), None);
    }
}
