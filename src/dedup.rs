//! Finding files stored more than once.
//!
//! This costs nothing to compute: the inventory already carries each file's
//! blake3, because rclone reports it for the Filen backend, so duplicates fall
//! out of the listing without downloading anything.
//!
//! Nothing is deleted. Which copy of a duplicate matters is a question about
//! intent — a file in `Archive` and the same file in `Inbox` may both be
//! deliberate — and that is not a judgement this tool should make on its own.

use std::collections::HashMap;
use std::fmt;

use humansize::{DECIMAL, format_size};

use crate::ledger::FileRow;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub blake3: String,
    pub size: u64,
    /// Every path holding these bytes, in a stable order.
    pub paths: Vec<String>,
}

impl Group {
    /// Bytes that would come back if all but one copy went away.
    pub fn recoverable(&self) -> u64 {
        self.size * (self.paths.len() as u64 - 1)
    }
}

#[derive(Debug, Default)]
pub struct Report {
    /// Groups, largest recoverable first.
    pub groups: Vec<Group>,
    /// Files the inventory holds no hash for, so they could not be compared.
    ///
    /// `scan` does not record hashes unless asked, because on Filen that makes
    /// the listing far slower. Reporting "no duplicates" when the truth is "we
    /// never looked" would be the worst answer available, so the count is kept
    /// and shown.
    pub unhashed: usize,
}

impl Report {
    pub fn recoverable(&self) -> u64 {
        self.groups.iter().map(Group::recoverable).sum()
    }

    pub fn redundant_files(&self) -> usize {
        self.groups.iter().map(|g| g.paths.len() - 1).sum()
    }
}

/// Groups files that share a hash.
///
/// Files the remote could not hash are ignored rather than guessed at: equal
/// sizes are not equal contents.
pub fn find(rows: &[FileRow]) -> Report {
    let mut by_hash: HashMap<(&str, u64), Vec<&str>> = HashMap::new();
    let mut unhashed = 0usize;
    for row in rows {
        // A zero-length file is not an interesting duplicate.
        if row.size == 0 {
            continue;
        }
        match row.blake3.as_deref() {
            Some(hash) => by_hash
                .entry((hash, row.size))
                .or_default()
                .push(&row.path),
            None => unhashed += 1,
        }
    }

    let mut groups: Vec<Group> = by_hash
        .into_iter()
        .filter(|(_, paths)| paths.len() > 1)
        .map(|((hash, size), mut paths)| {
            paths.sort_unstable();
            Group {
                blake3: hash.to_string(),
                size,
                paths: paths.into_iter().map(str::to_string).collect(),
            }
        })
        .collect();

    // Biggest win first, then by path so the output is stable run to run.
    groups.sort_by(|a, b| {
        b.recoverable()
            .cmp(&a.recoverable())
            .then_with(|| a.paths[0].cmp(&b.paths[0]))
    });
    Report { groups, unhashed }
}

impl Report {
    /// Says how much of the inventory could not be compared at all.
    fn note_unhashed(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.unhashed == 0 {
            return Ok(());
        }
        writeln!(
            f,
            "\n  {} file(s) have no recorded hash and were not compared.\n  \
             Re-run `scan --hash` to record them; it is slower, which is why it \n  \
             is not the default.",
            self.unhashed
        )
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.groups.is_empty() {
            writeln!(f, "No duplicate files found.")?;
            return self.note_unhashed(f);
        }
        writeln!(
            f,
            "{} duplicate group(s), {} redundant file(s), {} recoverable\n",
            self.groups.len(),
            self.redundant_files(),
            format_size(self.recoverable(), DECIMAL)
        )?;
        for group in &self.groups {
            writeln!(
                f,
                "  {} x{}  ({} each)",
                format_size(group.recoverable(), DECIMAL),
                group.paths.len(),
                format_size(group.size, DECIMAL)
            )?;
            for path in &group.paths {
                writeln!(f, "      {path}")?;
            }
        }
        writeln!(
            f,
            "\n  Nothing has been deleted. Which copy to keep depends on what you\n  \
             meant by having both, so that decision is left to you."
        )?;
        self.note_unhashed(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::State;

    fn row(path: &str, size: u64, hash: Option<&str>) -> FileRow {
        FileRow {
            path: path.to_string(),
            size,
            mod_time: None,
            blake3: hash.map(str::to_string),
            state: State::Pending,
            skip_reason: None,
        }
    }

    #[test]
    fn identical_files_are_grouped() {
        let rows = vec![
            row("a/photo.jpg", 100, Some("aa")),
            row("b/photo.jpg", 100, Some("aa")),
            row("c/other.jpg", 200, Some("bb")),
        ];
        let report = find(&rows);
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].paths, vec!["a/photo.jpg", "b/photo.jpg"]);
        // One of the two copies is redundant.
        assert_eq!(report.recoverable(), 100);
        assert_eq!(report.redundant_files(), 1);
    }

    #[test]
    fn three_copies_leave_two_redundant() {
        let rows = vec![
            row("a.jpg", 50, Some("aa")),
            row("b.jpg", 50, Some("aa")),
            row("c.jpg", 50, Some("aa")),
        ];
        let report = find(&rows);
        assert_eq!(report.recoverable(), 100);
        assert_eq!(report.redundant_files(), 2);
    }

    /// Same hash but a different size cannot be the same content, and treating it
    /// as such would be a recommendation to delete the wrong file.
    #[test]
    fn size_must_agree_as_well_as_the_hash() {
        let rows = vec![row("a.jpg", 100, Some("aa")), row("b.jpg", 999, Some("aa"))];
        assert!(find(&rows).groups.is_empty());
    }

    /// Equal sizes are not equal contents, so unhashed files are left out rather
    /// than guessed at.
    #[test]
    fn files_without_a_hash_are_ignored() {
        let rows = vec![row("a.jpg", 100, None), row("b.jpg", 100, None)];
        assert!(find(&rows).groups.is_empty());
    }

    #[test]
    fn empty_files_are_not_interesting_duplicates() {
        let rows = vec![row("a", 0, Some("e3")), row("b", 0, Some("e3"))];
        assert!(find(&rows).groups.is_empty());
    }

    #[test]
    fn unique_files_produce_nothing() {
        let rows = vec![row("a.jpg", 100, Some("aa")), row("b.jpg", 100, Some("bb"))];
        let report = find(&rows);
        assert!(report.groups.is_empty());
        assert_eq!(report.recoverable(), 0);
        assert!(report.to_string().contains("No duplicate files"));
    }

    /// An inventory with no hashes cannot find duplicates, and saying "none
    /// found" would read as "there are none". It has to say it did not look.
    #[test]
    fn an_unhashed_inventory_says_so_rather_than_reporting_nothing() {
        let rows = vec![row("a", 10, None), row("b", 10, None)];
        let report = find(&rows);
        assert!(report.groups.is_empty());
        assert_eq!(report.unhashed, 2);

        let text = report.to_string();
        assert!(text.contains("no recorded hash"), "{text}");
        assert!(text.contains("scan --hash"), "{text}");
    }

    /// The note also belongs on a report that did find something: a partial
    /// answer presented as a complete one is the same mistake.
    #[test]
    fn the_note_appears_alongside_real_findings_too() {
        let rows = vec![
            row("a", 10, Some("aa")),
            row("b", 10, Some("aa")),
            row("c", 99, None),
        ];
        let report = find(&rows);
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.unhashed, 1);
        assert!(report.to_string().contains("no recorded hash"));
    }

    /// A fully hashed inventory must not carry the caveat.
    #[test]
    fn a_hashed_inventory_carries_no_note() {
        let rows = vec![row("a", 10, Some("aa")), row("b", 10, Some("aa"))];
        let report = find(&rows);
        assert_eq!(report.unhashed, 0);
        assert!(!report.to_string().contains("no recorded hash"));
    }

    /// Output has to be stable so two runs can be compared.
    #[test]
    fn groups_are_ordered_by_what_they_would_save() {
        let rows = vec![
            row("small1", 10, Some("aa")),
            row("small2", 10, Some("aa")),
            row("big1", 1000, Some("bb")),
            row("big2", 1000, Some("bb")),
        ];
        let report = find(&rows);
        assert_eq!(report.groups[0].size, 1000);
        assert_eq!(report.groups[1].size, 10);
        assert_eq!(report.groups[0].paths, vec!["big1", "big2"]);
    }

    /// The report must not read as a list of things about to be removed.
    #[test]
    fn the_report_says_nothing_was_deleted() {
        let rows = vec![row("a.jpg", 100, Some("aa")), row("b.jpg", 100, Some("aa"))];
        let text = find(&rows).to_string();
        assert!(text.contains("Nothing has been deleted"));
        assert!(text.contains("a.jpg"));
        assert!(text.contains("b.jpg"));
    }

    #[test]
    fn empty_input_is_fine() {
        assert!(find(&[]).groups.is_empty());
    }
}
