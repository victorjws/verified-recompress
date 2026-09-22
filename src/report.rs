//! Aggregates decisions into something a person can act on.
//!
//! Three size figures are kept apart deliberately. "Logical saving" is what the
//! conversions remove, "quota change" is what the remote actually reports, and
//! "pending in trash" is the gap between them. With the default trash policy those
//! numbers diverge on purpose, and a report that blurred them would look like a bug.

use std::collections::BTreeMap;
use std::fmt;

use humansize::{DECIMAL, format_size};

use crate::policy::{Decision, Recipe, SkipReason};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Bucket {
    pub files: u64,
    pub bytes: u64,
}

impl Bucket {
    fn add(&mut self, bytes: u64) {
        self.files += 1;
        self.bytes += bytes;
    }
}

/// What a projected or completed pass amounts to.
#[derive(Debug, Clone, Default)]
pub struct Projection {
    pub convert: BTreeMap<&'static str, Bucket>,
    pub skip: BTreeMap<&'static str, Bucket>,
    /// Input bytes of everything we intend to convert.
    pub input_bytes: u64,
    /// Projected output bytes for those same files.
    pub output_bytes: u64,
    /// Files needing a probe before they can be judged.
    pub deferred: Bucket,
}

impl Projection {
    pub fn record(&mut self, decision: Decision, size: u64) {
        match decision {
            Decision::Convert(recipe) => {
                self.convert.entry(recipe.as_str()).or_default().add(size);
                self.input_bytes += size;
                self.output_bytes += (size as f64 * recipe.expected_ratio()) as u64;
            }
            Decision::Skip(SkipReason::NeedsProbe) => {
                self.deferred.add(size);
                self.skip
                    .entry(SkipReason::NeedsProbe.as_str())
                    .or_default()
                    .add(size);
            }
            Decision::Skip(reason) => {
                self.skip.entry(reason.as_str()).or_default().add(size);
            }
        }
    }

    pub fn saving_bytes(&self) -> u64 {
        self.input_bytes.saturating_sub(self.output_bytes)
    }

    pub fn saving_fraction(&self) -> f64 {
        if self.input_bytes == 0 {
            return 0.0;
        }
        self.saving_bytes() as f64 / self.input_bytes as f64
    }

    pub fn files_to_convert(&self) -> u64 {
        self.convert.values().map(|b| b.files).sum()
    }
}

impl fmt::Display for Projection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.convert.is_empty() {
            writeln!(f, "Nothing to convert.")?;
        } else {
            writeln!(f, "Planned conversions")?;
            for (recipe, bucket) in &self.convert {
                writeln!(
                    f,
                    "  {recipe:<18} {:>6} file(s)  {:>11}",
                    bucket.files,
                    format_size(bucket.bytes, DECIMAL)
                )?;
            }
            writeln!(
                f,
                "\n  {:>11} in  ->  {:>11} out   saving {} ({:.0}%)",
                format_size(self.input_bytes, DECIMAL),
                format_size(self.output_bytes, DECIMAL),
                format_size(self.saving_bytes(), DECIMAL),
                self.saving_fraction() * 100.0,
            )?;
            writeln!(f, "  Projected from per-recipe ratios, not measured.")?;
        }

        if !self.skip.is_empty() {
            writeln!(f, "\nNot converting")?;
            for (reason, bucket) in &self.skip {
                writeln!(
                    f,
                    "  {reason:<24} {:>6} file(s)  {:>11}",
                    bucket.files,
                    format_size(bucket.bytes, DECIMAL)
                )?;
            }
        }

        if self.deferred.files > 0 {
            writeln!(
                f,
                "\n  {} video file(s), {}, need a probe before they can be judged.\n  \
                 `plan` does not download, so run them through `run` to find out.",
                self.deferred.files,
                format_size(self.deferred.bytes, DECIMAL)
            )?;
        }
        Ok(())
    }
}

/// Recipes worth naming in a summary line, in the order a reader cares about.
pub const ALL_RECIPES: [Recipe; 8] = [
    Recipe::JxlFromJpeg,
    Recipe::JxlFromRaster,
    Recipe::JxlFromWebp,
    Recipe::Flac,
    Recipe::FlacRecompress,
    Recipe::TsRemux,
    Recipe::Ffv1,
    Recipe::Av1,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_savings_from_recipe_ratios() {
        let mut p = Projection::default();
        // 1000 bytes of JPEG at 0.80 leaves 800.
        p.record(Decision::Convert(Recipe::JxlFromJpeg), 1000);
        assert_eq!(p.input_bytes, 1000);
        assert_eq!(p.output_bytes, 800);
        assert_eq!(p.saving_bytes(), 200);
        assert!((p.saving_fraction() - 0.2).abs() < 1e-9);
    }

    #[test]
    fn groups_by_recipe_and_reason() {
        let mut p = Projection::default();
        p.record(Decision::Convert(Recipe::JxlFromJpeg), 100);
        p.record(Decision::Convert(Recipe::JxlFromJpeg), 200);
        p.record(Decision::Convert(Recipe::Flac), 500);
        p.record(Decision::Skip(SkipReason::AlreadyOptimal), 50);

        assert_eq!(p.convert["jxl-from-jpeg"].files, 2);
        assert_eq!(p.convert["jxl-from-jpeg"].bytes, 300);
        assert_eq!(p.convert["flac"].files, 1);
        assert_eq!(p.skip["already_optimal"].files, 1);
        assert_eq!(p.files_to_convert(), 3);
    }

    /// Files awaiting a probe are counted separately so the projection does not
    /// silently understate what a real run might achieve.
    #[test]
    fn deferred_files_are_tracked_apart() {
        let mut p = Projection::default();
        p.record(Decision::Skip(SkipReason::NeedsProbe), 900);
        assert_eq!(p.deferred.files, 1);
        assert_eq!(p.deferred.bytes, 900);
        assert_eq!(p.skip["needs_probe"].files, 1);
        // Deferred files contribute nothing to the projected saving.
        assert_eq!(p.saving_bytes(), 0);
    }

    #[test]
    fn empty_projection_reports_nothing_to_do() {
        let p = Projection::default();
        assert_eq!(p.saving_fraction(), 0.0);
        assert!(p.to_string().contains("Nothing to convert"));
    }

    #[test]
    fn output_display_separates_projection_from_measurement() {
        let mut p = Projection::default();
        p.record(Decision::Convert(Recipe::JxlFromJpeg), 1_000_000);
        let text = p.to_string();
        assert!(text.contains("Planned conversions"));
        assert!(
            text.contains("not measured"),
            "the projection must not read as a measured result"
        );
    }

    #[test]
    fn skip_reasons_have_stable_names() {
        // These strings land in the ledger, so they must not drift silently.
        assert_eq!(SkipReason::VideoHdr.as_str(), "video_hdr");
        assert_eq!(SkipReason::NeedsProbe.as_str(), "needs_probe");
        assert_eq!(SkipReason::TooLargeForBudget.as_str(), "too_large_for_budget");
    }

    #[test]
    fn every_recipe_has_a_distinct_name() {
        let mut names: Vec<_> = ALL_RECIPES.iter().map(|r| r.as_str()).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total);
    }
}
