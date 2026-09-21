//! Remote storage access, via rclone.
//!
//! Two implementations exist. [`rcd::RcdRemote`] drives a single long-lived
//! `rclone rcd` daemon over its HTTP control API and is what production runs use:
//! it avoids forking a process per file and returns structured JSON.
//! [`rclone_cli::CliRemote`] shells out to the `rclone` binary per call. It is
//! slower but has no daemon to manage, so it serves as a fallback and as the
//! reference implementation that integration tests compare against.
//!
//! Both speak the same entry schema — `rclone lsjson` and the RC `operations/list`
//! method emit byte-identical objects — so [`parse_entries`] serves both.

pub mod rclone_cli;
pub mod rcd;

use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// The hash Filen exposes. Asking for one specific type matters: on some backends
/// (the `local` one, for instance) an unqualified `--hash` computes every algorithm
/// rclone knows, which is needlessly slow.
pub const HASH_TYPE: &str = "blake3";

/// One file on the remote. Directories are filtered out during listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Path relative to the remote root, without a leading slash.
    pub path: String,
    pub size: u64,
    /// RFC3339, as reported by the backend.
    pub mod_time: Option<String>,
    pub blake3: Option<String>,
}

/// Quota figures. Backends may report any subset; Filen reports at least used and total.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct About {
    pub total: Option<u64>,
    pub used: Option<u64>,
    pub free: Option<u64>,
    pub trashed: Option<u64>,
}

pub trait Remote: Send + Sync {
    /// Recursively lists files under `scope`, a path relative to the remote root.
    /// Returned paths are also relative to the remote root, not to `scope`.
    fn list(&self, scope: &str) -> impl Future<Output = Result<Vec<Entry>>> + Send;

    /// Fetches one file to a local path.
    fn download(&self, path: &str, local: &Path) -> impl Future<Output = Result<()>> + Send;

    /// Stores a local file at `path`, replacing whatever is there.
    fn upload(&self, local: &Path, path: &str) -> impl Future<Output = Result<()>> + Send;

    /// Removes one file. On Filen this moves it to the trash, which still counts
    /// against the quota until [`Remote::cleanup`] runs.
    fn delete(&self, path: &str) -> impl Future<Output = Result<()>> + Send;

    /// Renames within the remote. Server-side, so it costs no transfer.
    fn move_to(&self, from: &str, to: &str) -> impl Future<Output = Result<()>> + Send;

    /// Empties the trash, which is what actually reclaims quota.
    fn cleanup(&self) -> impl Future<Output = Result<()>> + Send;

    /// Quota for the remote. Free space here gates uploads.
    fn about(&self) -> impl Future<Output = Result<About>> + Send;

    /// Metadata for one file, or `None` if it does not exist.
    fn stat(&self, path: &str) -> impl Future<Output = Result<Option<Entry>>> + Send;

    /// Server-side hash of one file, computed without downloading it.
    fn hashsum(&self, path: &str) -> impl Future<Output = Result<Option<String>>> + Send;
}

/// The JSON object `lsjson` and `operations/list` both produce.
#[derive(Debug, Deserialize)]
struct RawEntry {
    #[serde(rename = "Path")]
    path: String,
    #[serde(rename = "Size")]
    size: i64,
    #[serde(rename = "ModTime")]
    mod_time: Option<String>,
    #[serde(rename = "IsDir", default)]
    is_dir: bool,
    #[serde(rename = "Hashes", default)]
    hashes: BTreeMap<String, String>,
}

/// Parses a JSON array of rclone entries, dropping directories.
///
/// `scope` is prefixed onto each path so callers get remote-root-relative paths
/// regardless of which subtree was listed.
pub fn parse_entries(json: &str, scope: &str) -> Result<Vec<Entry>> {
    let raw: Vec<RawEntry> =
        serde_json::from_str(json).context("failed to parse rclone listing as JSON")?;
    Ok(entries_from_raw(raw, scope))
}

fn entries_from_raw(raw: Vec<RawEntry>, scope: &str) -> Vec<Entry> {
    let prefix = normalize_path(scope);
    raw.into_iter()
        .filter(|e| !e.is_dir)
        .map(|e| Entry {
            path: join_path(&prefix, &e.path),
            // A backend that cannot report a size uses -1; treat that as unknown-zero
            // rather than panicking, and let the size gate skip it later.
            size: u64::try_from(e.size).unwrap_or(0),
            mod_time: e.mod_time,
            blake3: e
                .hashes
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(HASH_TYPE))
                .map(|(_, v)| v.clone()),
        })
        .collect()
}

/// `{"total":N,"used":N,"free":N}`, any field optional.
#[derive(Debug, Deserialize)]
struct RawAbout {
    total: Option<u64>,
    used: Option<u64>,
    free: Option<u64>,
    trashed: Option<u64>,
}

pub fn parse_about(json: &str) -> Result<About> {
    let raw: RawAbout =
        serde_json::from_str(json).context("failed to parse rclone about output as JSON")?;
    Ok(About {
        total: raw.total,
        used: raw.used,
        free: raw.free,
        trashed: raw.trashed,
    })
}

/// Strips leading and trailing slashes so paths concatenate predictably.
pub fn normalize_path(path: &str) -> String {
    path.trim_matches('/').to_string()
}

/// Joins two remote-relative path segments, tolerating empty ones.
pub fn join_path(prefix: &str, suffix: &str) -> String {
    let prefix = normalize_path(prefix);
    let suffix = normalize_path(suffix);
    match (prefix.is_empty(), suffix.is_empty()) {
        (true, _) => suffix,
        (_, true) => prefix,
        _ => format!("{prefix}/{suffix}"),
    }
}

/// Builds an rclone remote spec such as `filen:Photos/2019`.
///
/// `remote` may itself carry a base path (`filen:archive`), and local-style remotes
/// may use absolute paths (`testlocal:/tmp/tree`); both must keep working.
pub fn remote_spec(remote: &str, path: &str) -> String {
    let path = normalize_path(path);
    if path.is_empty() {
        return remote.to_string();
    }
    match remote.rsplit_once(':') {
        // `filen:` with no base path.
        Some((_, "")) => format!("{remote}{path}"),
        // `filen:archive` or `testlocal:/tmp/tree`.
        Some((_, base)) => {
            let sep = if base.ends_with('/') { "" } else { "/" };
            format!("{remote}{sep}{path}")
        }
        // Bare path with no remote name at all.
        None => join_path(remote, &path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured verbatim from `rclone lsjson --recursive --files-only --hash-type blake3`
    /// against rclone 1.75.1.
    const LSJSON: &str = r#"[
{"Path":"a.txt","Name":"a.txt","Size":5,"MimeType":"text/plain; charset=utf-8","ModTime":"2026-09-21T01:04:39.250809991+09:00","IsDir":false,"Hashes":{"blake3":"ea8f163db38682925e4491c5e58d4bb3506ef8c14eb78a86e908c5624a67200f"}},
{"Path":"sub/b.txt","Name":"b.txt","Size":7,"MimeType":"text/plain; charset=utf-8","ModTime":"2026-09-21T01:04:39.250966449+09:00","IsDir":false,"Hashes":{"blake3":"8bafa24d36bc2aa6edc0d041e763cb59ebadb71b6e63ab4ac9314de95e9a0de7"}}
]"#;

    #[test]
    fn parses_real_lsjson_output() {
        let entries = parse_entries(LSJSON, "").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "a.txt");
        assert_eq!(entries[0].size, 5);
        assert_eq!(
            entries[0].blake3.as_deref(),
            Some("ea8f163db38682925e4491c5e58d4bb3506ef8c14eb78a86e908c5624a67200f")
        );
        assert_eq!(entries[1].path, "sub/b.txt");
    }

    /// Listing a subtree must still yield remote-root-relative paths, otherwise the
    /// ledger would key the same file differently depending on the scan scope.
    #[test]
    fn scope_is_prefixed_onto_listed_paths() {
        let entries = parse_entries(LSJSON, "/Photos/2019").unwrap();
        assert_eq!(entries[0].path, "Photos/2019/a.txt");
        assert_eq!(entries[1].path, "Photos/2019/sub/b.txt");
    }

    #[test]
    fn directories_are_dropped() {
        let json = r#"[
            {"Path":"sub","Name":"sub","Size":-1,"IsDir":true},
            {"Path":"sub/b.txt","Name":"b.txt","Size":7,"IsDir":false}
        ]"#;
        let entries = parse_entries(json, "").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "sub/b.txt");
    }

    #[test]
    fn missing_hashes_and_modtime_are_tolerated() {
        let json = r#"[{"Path":"a.bin","Name":"a.bin","Size":3,"IsDir":false}]"#;
        let entries = parse_entries(json, "").unwrap();
        assert_eq!(entries[0].blake3, None);
        assert_eq!(entries[0].mod_time, None);
    }

    /// Backends that cannot report a size emit -1; that must not panic.
    #[test]
    fn negative_size_becomes_zero() {
        let json = r#"[{"Path":"a.bin","Name":"a.bin","Size":-1,"IsDir":false}]"#;
        assert_eq!(parse_entries(json, "").unwrap()[0].size, 0);
    }

    #[test]
    fn hash_lookup_is_case_insensitive() {
        let json = r#"[{"Path":"a","Name":"a","Size":1,"IsDir":false,"Hashes":{"BLAKE3":"ff"}}]"#;
        assert_eq!(parse_entries(json, "").unwrap()[0].blake3.as_deref(), Some("ff"));
    }

    #[test]
    fn other_hash_types_are_ignored() {
        let json = r#"[{"Path":"a","Name":"a","Size":1,"IsDir":false,"Hashes":{"md5":"ff","sha1":"ee"}}]"#;
        assert_eq!(parse_entries(json, "").unwrap()[0].blake3, None);
    }

    #[test]
    fn parses_real_about_output() {
        let json = r#"{"total": 494384795648,"used": 468492992512,"free": 25891803136}"#;
        let about = parse_about(json).unwrap();
        assert_eq!(about.total, Some(494_384_795_648));
        assert_eq!(about.used, Some(468_492_992_512));
        assert_eq!(about.free, Some(25_891_803_136));
        assert_eq!(about.trashed, None);
    }

    #[test]
    fn about_tolerates_partial_reporting() {
        let about = parse_about(r#"{"used": 10, "trashed": 4}"#).unwrap();
        assert_eq!(about.used, Some(10));
        assert_eq!(about.trashed, Some(4));
        assert_eq!(about.total, None);
    }

    #[test]
    fn joins_paths() {
        assert_eq!(join_path("", "a.txt"), "a.txt");
        assert_eq!(join_path("Photos", ""), "Photos");
        assert_eq!(join_path("/Photos/", "/2019/a.txt"), "Photos/2019/a.txt");
        assert_eq!(join_path("", ""), "");
    }

    #[test]
    fn builds_remote_specs() {
        assert_eq!(remote_spec("filen:", "/Photos"), "filen:Photos");
        assert_eq!(remote_spec("filen:", ""), "filen:");
        assert_eq!(remote_spec("filen:archive", "/Photos"), "filen:archive/Photos");
        assert_eq!(remote_spec("testlocal:/tmp/tree", "sub"), "testlocal:/tmp/tree/sub");
        assert_eq!(remote_spec("testlocal:/tmp/tree/", "sub"), "testlocal:/tmp/tree/sub");
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(parse_entries("not json", "").is_err());
        assert!(parse_about("[").is_err());
    }

    /// rclone writes a partial `[` to stdout before failing, so a caller that ignored
    /// the exit status would see truncated JSON. Confirm that at least fails loudly.
    #[test]
    fn truncated_listing_is_an_error() {
        assert!(parse_entries("[", "").is_err());
    }
}
