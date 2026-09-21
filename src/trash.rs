//! When to actually reclaim the space.
//!
//! Deleting an original on Filen moves it to the trash, and the trash counts
//! against the quota. So a converted drive does not get smaller until the trash is
//! emptied — and while the originals are still in there, they can be restored from
//! Filen's own web interface. That window is the real safety net for conversions
//! that are otherwise irreversible, which is why emptying it is a separate,
//! deliberate act rather than something a run does on its own.

use std::fmt;

use humansize::{DECIMAL, format_size};

use crate::config::TrashPolicy;
use crate::ledger::Reclaim;

/// Whether a run should empty the trash when it finishes.
pub fn purges_after_run(policy: TrashPolicy) -> bool {
    matches!(policy, TrashPolicy::PurgeNow)
}

/// Whether the given policy ever empties the trash without being asked.
pub fn purges_automatically(policy: TrashPolicy) -> bool {
    !matches!(policy, TrashPolicy::Keep)
}

/// The three quantities that must never be conflated in a report.
///
/// With the default policy `logical` grows while `quota_change` goes the wrong
/// way, because the originals are still present alongside their replacements. A
/// report that showed only one number would look like the tool was broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accounting {
    /// Bytes the conversions removed from the content itself.
    pub logical: u64,
    /// Bytes sitting in the trash, recoverable and still billed.
    pub pending: Reclaim,
    /// Free space the remote reports right now.
    pub remote_free: Option<u64>,
}

impl fmt::Display for Accounting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "  logical saving      {:>12}",
            format_size(self.logical, DECIMAL)
        )?;
        writeln!(
            f,
            "  waiting in trash    {:>12}  ({} file(s), still counted against quota)",
            format_size(self.pending.bytes, DECIMAL),
            self.pending.files
        )?;
        if let Some(free) = self.remote_free {
            writeln!(
                f,
                "  remote free now     {:>12}",
                format_size(free, DECIMAL)
            )?;
        }
        Ok(())
    }
}

/// Explains why the quota has not moved, when that is the case.
pub fn advice(policy: TrashPolicy, pending: Reclaim) -> Option<String> {
    if pending.bytes == 0 {
        return None;
    }
    match policy {
        TrashPolicy::Keep => Some(format!(
            "{} of originals are in the trash. Your quota has not dropped yet, and it \
             will not until you empty it. While they are there you can restore any of \
             them from the Filen web app, so check the results first, then run \
             `cleanup --execute`.",
            format_size(pending.bytes, DECIMAL)
        )),
        TrashPolicy::PurgeAfterDays => Some(format!(
            "{} of originals are in the trash, waiting out the retention period.",
            format_size(pending.bytes, DECIMAL)
        )),
        TrashPolicy::PurgeNow => Some(format!(
            "{} of originals are in the trash and were not purged; run \
             `cleanup --execute`.",
            format_size(pending.bytes, DECIMAL)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(bytes: u64) -> Reclaim {
        Reclaim { files: 2, bytes }
    }

    #[test]
    fn only_purge_now_empties_the_trash_at_the_end_of_a_run() {
        assert!(purges_after_run(TrashPolicy::PurgeNow));
        assert!(!purges_after_run(TrashPolicy::Keep));
        assert!(!purges_after_run(TrashPolicy::PurgeAfterDays));
    }

    /// The default must never throw away the recovery window on its own.
    #[test]
    fn keep_never_purges_automatically() {
        assert!(!purges_automatically(TrashPolicy::Keep));
        assert!(purges_automatically(TrashPolicy::PurgeNow));
        assert!(purges_automatically(TrashPolicy::PurgeAfterDays));
    }

    #[test]
    fn nothing_pending_needs_no_explanation() {
        assert!(advice(TrashPolicy::Keep, Reclaim::default()).is_none());
    }

    /// The most likely confusion is "I converted everything and my drive is the
    /// same size", so the default policy must say exactly why.
    #[test]
    fn keep_explains_why_the_quota_has_not_moved() {
        let message = advice(TrashPolicy::Keep, pending(5_000_000)).unwrap();
        assert!(message.contains("has not dropped"));
        assert!(message.contains("cleanup --execute"));
        assert!(message.contains("restore"));
    }

    #[test]
    fn the_report_keeps_the_three_figures_apart() {
        let text = Accounting {
            logical: 1_000_000,
            pending: pending(4_000_000),
            remote_free: Some(9_000_000),
        }
        .to_string();
        assert!(text.contains("logical saving"));
        assert!(text.contains("waiting in trash"));
        assert!(text.contains("remote free now"));
        // The trash line has to say why it matters.
        assert!(text.contains("still counted against quota"));
    }

    #[test]
    fn a_remote_without_quota_reporting_omits_the_line() {
        let text = Accounting {
            logical: 1,
            pending: Reclaim::default(),
            remote_free: None,
        }
        .to_string();
        assert!(!text.contains("remote free now"));
    }
}
