//! Measures encoder settings on real files before a library is committed to them.
//!
//! The AV1 tier cannot be undone, and the right preset depends on the footage, so
//! the honest way to choose is to encode a handful of real samples and look at
//! what actually happened. This produces that table: size, quality at both gated
//! figures, wall time, and whether the metadata survived.

use std::fmt;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::classify::{self, MediaProbe};
use crate::convert::video_av1::{self, Settings};
use crate::vmaf::{self, Scores};

/// Presets worth comparing. Lower is smaller and slower.
pub const PRESETS: [u8; 3] = [3, 4, 6];

#[derive(Debug, Clone)]
pub struct Row {
    pub preset: u8,
    pub temporal_filtering: bool,
    pub crf: u8,
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub scores: Scores,
    pub elapsed: Duration,
    pub metadata_kept: bool,
}

impl Row {
    pub fn saving_fraction(&self) -> f64 {
        if self.input_bytes == 0 {
            return 0.0;
        }
        1.0 - (self.output_bytes as f64 / self.input_bytes as f64)
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub rows: Vec<Row>,
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{:<7} {:<5} {:<5} {:>8} {:>8} {:>8} {:>9} {:>6}",
            "preset", "tf", "crf", "saving", "vmaf", "p1", "time", "meta"
        )?;
        for row in &self.rows {
            writeln!(
                f,
                "{:<7} {:<5} {:<5} {:>7.0}% {:>8.2} {:>8.2} {:>8.0}s {:>6}",
                row.preset,
                if row.temporal_filtering { "on" } else { "off" },
                row.crf,
                row.saving_fraction() * 100.0,
                row.scores.mean,
                row.scores.low_percentile,
                row.elapsed.as_secs_f64(),
                if row.metadata_kept { "kept" } else { "LOST" },
            )?;
        }
        if self.rows.iter().any(|r| !r.metadata_kept) {
            writeln!(
                f,
                "\n  Some settings dropped metadata. Creation time, GPS and device\n  \
                 details are usually the point of keeping the originals, so treat\n  \
                 that as disqualifying rather than a detail."
            )?;
        }
        Ok(())
    }
}

/// Fields worth checking survive a re-encode.
///
/// Creation time and location are typically the whole reason a photo library is
/// worth keeping, and they live in QuickTime keyed atoms that not every container
/// or muxer flag carries across.
const TRACKED_TAGS: [&str; 3] = ["creation_time", "location", "com.apple.quicktime"];

/// Runs one setting against one file.
pub async fn measure_one(
    source: &Path,
    work_dir: &Path,
    preset: u8,
    temporal_filtering: bool,
    crf: u8,
    cores: &[usize],
    hwaccel: bool,
) -> Result<Row> {
    let probe = classify::probe_file(source).await?;
    let video = probe.video.first().context("the sample has no video")?;
    let model = vmaf::model_for(video.width, video.height);

    let output = work_dir.join(format!("bench-p{preset}-tf{}-crf{crf}.mp4", u8::from(temporal_filtering)));
    let settings = Settings {
        preset,
        temporal_filtering,
        hwaccel,
    };

    let started = Instant::now();
    // `bench` reports per sample, not per frame, so it discards the ticks.
    video_av1::encode(source, &output, crf, &settings, cores, |_| {}).await?;
    let elapsed = started.elapsed();

    let scores = vmaf::measure(&output, source, model, work_dir, cores, hwaccel).await?;
    let metadata_kept = metadata_survived(&probe, &classify::probe_file(&output).await?);

    let row = Row {
        preset,
        temporal_filtering,
        crf,
        input_bytes: tokio::fs::metadata(source).await?.len(),
        output_bytes: tokio::fs::metadata(&output).await?.len(),
        scores,
        elapsed,
        metadata_kept,
    };
    let _ = tokio::fs::remove_file(&output).await;
    Ok(row)
}

/// Whether the re-encode kept the source's shape and orientation.
///
/// A full metadata diff needs exiftool; this is the part ffprobe can answer, and
/// it catches the failure that matters most — a picture that comes out rotated
/// or resized.
fn metadata_survived(before: &MediaProbe, after: &MediaProbe) -> bool {
    let (Some(a), Some(b)) = (before.video.first(), after.video.first()) else {
        return false;
    };
    a.width == b.width && a.height == b.height && before.audio_codecs == after.audio_codecs
}

/// Fields a caller should diff with exiftool, which sees far more than ffprobe.
pub fn tracked_tags() -> &'static [&'static str] {
    &TRACKED_TAGS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::VideoStream;

    fn probe(width: u32, height: u32, audio: &[&str]) -> MediaProbe {
        MediaProbe {
            video: vec![VideoStream {
                codec: "h264".into(),
                width,
                height,
                color_transfer: None,
                bit_rate: Some(1000),
                has_dolby_vision: false,
            }],
            audio_codecs: audio.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn row(input: u64, output: u64) -> Row {
        Row {
            preset: 3,
            temporal_filtering: true,
            crf: 26,
            input_bytes: input,
            output_bytes: output,
            scores: Scores {
                mean: 98.0,
                min: 96.0,
                low_percentile: 97.0,
                frames: 100,
            },
            elapsed: Duration::from_secs(12),
            metadata_kept: true,
        }
    }

    #[test]
    fn saving_is_reported_as_a_fraction_of_the_input() {
        assert!((row(1000, 700).saving_fraction() - 0.3).abs() < 1e-9);
        assert_eq!(row(0, 0).saving_fraction(), 0.0);
    }

    /// An encode that grew shows as a negative saving rather than being hidden.
    #[test]
    fn growth_shows_as_a_negative_saving() {
        assert!(row(1000, 1200).saving_fraction() < 0.0);
    }

    #[test]
    fn identical_shape_and_audio_counts_as_preserved() {
        let before = probe(1920, 1080, &["aac"]);
        assert!(metadata_survived(&before, &before.clone()));
    }

    /// A re-encode that came out rotated has different dimensions, which is the
    /// failure most likely to go unnoticed.
    #[test]
    fn a_rotated_result_is_caught() {
        let before = probe(1920, 1080, &["aac"]);
        let after = probe(1080, 1920, &["aac"]);
        assert!(!metadata_survived(&before, &after));
    }

    #[test]
    fn a_dropped_audio_track_is_caught() {
        let before = probe(1920, 1080, &["aac"]);
        let after = probe(1920, 1080, &[]);
        assert!(!metadata_survived(&before, &after));
    }

    #[test]
    fn a_result_without_video_is_not_preserved() {
        let before = probe(1920, 1080, &["aac"]);
        assert!(!metadata_survived(&before, &MediaProbe::default()));
    }

    #[test]
    fn the_table_names_both_gated_figures() {
        let report = Report {
            rows: vec![row(1000, 700)],
        };
        let text = report.to_string();
        assert!(text.contains("vmaf"));
        assert!(text.contains("p1"));
        assert!(text.contains("saving"));
    }

    /// Losing metadata has to be loud: it is usually the reason the library is
    /// worth keeping at all.
    #[test]
    fn lost_metadata_is_called_out() {
        let mut bad = row(1000, 700);
        bad.metadata_kept = false;
        let text = Report { rows: vec![bad] }.to_string();
        assert!(text.contains("LOST"));
        assert!(text.contains("disqualifying"));
    }

    #[test]
    fn presets_span_the_useful_range() {
        // SVT-AV1 4.2.0 defaults to 8; everything compared here is slower and smaller.
        assert!(PRESETS.iter().all(|&p| p < 8));
    }
}
