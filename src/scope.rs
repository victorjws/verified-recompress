//! Which files a command is allowed to touch.
//!
//! Scoping is a safety boundary, not a convenience. Someone who writes
//! `--path /Photos/2019` is stating that nothing outside that folder should be
//! modified, and the guardrail that demands an explicit `--path` or `--all` before
//! a write means nothing unless the restriction is actually enforced at the point
//! files are picked up.

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::remote::normalize_path;

#[derive(Debug, Clone)]
pub struct Scope {
    /// Remote-root-relative prefixes. Empty means the whole remote.
    prefixes: Vec<String>,
    excludes: GlobSet,
    exclude_patterns: Vec<String>,
}

impl Scope {
    pub fn new(paths: &[String], excludes: &[String]) -> Result<Self> {
        let mut builder = GlobSetBuilder::new();
        for pattern in excludes {
            builder.add(
                Glob::new(pattern)
                    .with_context(|| format!("invalid exclude pattern `{pattern}`"))?,
            );
        }
        Ok(Self {
            prefixes: paths
                .iter()
                .map(|p| normalize_path(p))
                .filter(|p| !p.is_empty())
                .collect(),
            excludes: builder.build().context("failed to compile exclude patterns")?,
            exclude_patterns: excludes.to_vec(),
        })
    }

    /// Prefixes for the ledger query. Empty means no restriction.
    pub fn prefixes(&self) -> &[String] {
        &self.prefixes
    }

    pub fn exclude_patterns(&self) -> &[String] {
        &self.exclude_patterns
    }

    /// Whether `path` is inside one of the scoped folders.
    ///
    /// A prefix must line up with a path segment: `Photos` covers `Photos/a.jpg`
    /// but not `PhotosBackup/a.jpg`.
    pub fn contains(&self, path: &str) -> bool {
        if self.prefixes.is_empty() {
            return true;
        }
        let path = normalize_path(path);
        self.prefixes.iter().any(|prefix| {
            path == *prefix
                || path
                    .strip_prefix(prefix.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    }

    pub fn is_excluded(&self, path: &str) -> bool {
        self.excludes.is_match(normalize_path(path))
    }

    /// Whether this command may act on `path`.
    pub fn allows(&self, path: &str) -> bool {
        self.contains(path) && !self.is_excluded(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(paths: &[&str], excludes: &[&str]) -> Scope {
        Scope::new(
            &paths.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            &excludes.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[test]
    fn no_paths_means_the_whole_remote() {
        let s = scope(&[], &[]);
        assert!(s.allows("anything/at/all.jpg"));
        assert!(s.prefixes().is_empty());
    }

    #[test]
    fn a_prefix_limits_to_its_subtree() {
        let s = scope(&["/Photos"], &[]);
        assert!(s.allows("Photos/a.jpg"));
        assert!(s.allows("Photos/2019/deep/a.jpg"));
        assert!(!s.allows("Videos/a.mp4"));
        assert!(!s.allows("a.jpg"));
    }

    /// This is the bug that made scoping meaningless in practice: a run scoped to
    /// one folder must not reach into a sibling.
    #[test]
    fn a_sibling_folder_is_out_of_scope() {
        let s = scope(&["/photos"], &[]);
        assert!(s.allows("photos/shot.jpg"));
        assert!(!s.allows("audio/tone.wav"));
        assert!(!s.allows("videos/clip.mp4"));
    }

    /// Prefix matching must respect segment boundaries.
    #[test]
    fn a_prefix_does_not_match_a_longer_sibling_name() {
        let s = scope(&["Photos"], &[]);
        assert!(s.allows("Photos/a.jpg"));
        assert!(!s.allows("PhotosBackup/a.jpg"));
        assert!(!s.allows("PhotosOld"));
    }

    #[test]
    fn the_scoped_folder_itself_is_included() {
        assert!(scope(&["Photos/a.jpg"], &[]).allows("Photos/a.jpg"));
    }

    #[test]
    fn several_prefixes_are_unioned() {
        let s = scope(&["/Photos", "/Camera"], &[]);
        assert!(s.allows("Photos/a.jpg"));
        assert!(s.allows("Camera/b.jpg"));
        assert!(!s.allows("Videos/c.mp4"));
    }

    #[test]
    fn leading_and_trailing_slashes_are_irrelevant() {
        let s = scope(&["/Photos/"], &[]);
        assert!(s.allows("/Photos/a.jpg"));
        assert!(s.allows("Photos/a.jpg"));
    }

    #[test]
    fn excludes_override_inclusion() {
        let s = scope(&["/Photos"], &["**/.thumbnails/**"]);
        assert!(s.allows("Photos/a.jpg"));
        assert!(!s.allows("Photos/.thumbnails/a.jpg"));
        assert!(!s.allows("Photos/2019/.thumbnails/deep/a.jpg"));
    }

    #[test]
    fn excludes_can_match_by_extension() {
        let s = scope(&[], &["**/*.RAW"]);
        assert!(!s.allows("Photos/a.RAW"));
        assert!(s.allows("Photos/a.jpg"));
    }

    #[test]
    fn a_malformed_pattern_is_reported() {
        let err = Scope::new(&[], &["[unclosed".to_string()]).unwrap_err();
        assert!(err.to_string().contains("invalid exclude pattern"), "{err}");
    }
}
