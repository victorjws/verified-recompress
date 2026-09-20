//! Exercises both [`Remote`] implementations against a real rclone `local` remote.
//!
//! No Filen account is involved: `rclone config create testlocal local` gives a
//! remote that behaves like any other as far as our code is concerned, so the whole
//! read path can be verified offline.
//!
//! The two implementations must agree. `CliRemote` is the simple reference; if
//! `RcdRemote` ever drifts from it, these tests fail rather than the difference
//! showing up as mysterious behaviour against the real drive.

use std::path::Path;
use std::process::Command;

use storage_optimizer::ledger::{Ledger, State};
use storage_optimizer::remote::rclone_cli::CliRemote;
use storage_optimizer::remote::rcd::RcdRemote;
use storage_optimizer::remote::{Entry, Remote};

/// Skips the test (rather than failing) when rclone is absent, so `cargo test`
/// still works on a machine that has not been set up yet.
fn rclone_available() -> bool {
    Command::new("rclone")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

macro_rules! require_rclone {
    () => {
        if !rclone_available() {
            eprintln!("skipping: rclone is not installed");
            return;
        }
    };
}

struct Fixture {
    _dir: tempfile::TempDir,
    config: String,
    /// Remote spec rooted at the sample tree, e.g. `testlocal:/tmp/xyz/tree`.
    remote: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("rclone.conf");
        std::fs::write(&config, "").unwrap();

        let status = Command::new("rclone")
            .env("RCLONE_CONFIG", &config)
            .args(["config", "create", "testlocal", "local"])
            .output()
            .unwrap();
        assert!(status.status.success(), "failed to create the test remote");

        let tree = dir.path().join("tree");
        std::fs::create_dir_all(tree.join("sub")).unwrap();
        std::fs::create_dir_all(tree.join("empty")).unwrap();
        write(&tree.join("a.txt"), b"hello");
        write(&tree.join("sub/b.txt"), b"world!!");
        write(&tree.join("sub/c.bin"), &[0u8; 1024]);

        Self {
            config: config.to_string_lossy().into_owned(),
            remote: format!("testlocal:{}", tree.to_string_lossy()),
            _dir: dir,
        }
    }

    fn cli(&self) -> CliRemote {
        CliRemote::new(&self.remote).with_config(&self.config)
    }

    async fn rcd(&self) -> RcdRemote {
        RcdRemote::spawn(&self.remote, Some(&self.config))
            .await
            .expect("failed to start rclone rcd")
    }
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
}

fn sorted(mut entries: Vec<Entry>) -> Vec<Entry> {
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries
}

#[tokio::test]
async fn cli_lists_the_tree() {
    require_rclone!();
    let fx = Fixture::new();
    let entries = sorted(fx.cli().list("").await.unwrap());

    assert_eq!(entries.len(), 3, "directories must not appear: {entries:#?}");
    assert_eq!(entries[0].path, "a.txt");
    assert_eq!(entries[0].size, 5);
    assert_eq!(entries[1].path, "sub/b.txt");
    assert_eq!(entries[2].path, "sub/c.bin");
    assert_eq!(entries[2].size, 1024);
    assert!(
        entries.iter().all(|e| e.blake3.is_some()),
        "every entry should carry a blake3 hash"
    );
}

#[tokio::test]
async fn listing_a_subtree_yields_root_relative_paths() {
    require_rclone!();
    let fx = Fixture::new();
    let entries = sorted(fx.cli().list("sub").await.unwrap());
    assert_eq!(entries.len(), 2);
    // Not "b.txt": the ledger keys on remote-root-relative paths, so scanning a
    // subtree must produce the same key as scanning the whole drive.
    assert_eq!(entries[0].path, "sub/b.txt");
    assert_eq!(entries[1].path, "sub/c.bin");
}

#[tokio::test]
async fn both_implementations_list_identically() {
    require_rclone!();
    let fx = Fixture::new();
    let rcd = fx.rcd().await;

    let from_cli = sorted(fx.cli().list("").await.unwrap());
    let from_rcd = sorted(rcd.list("").await.unwrap());
    assert_eq!(from_cli, from_rcd);

    let from_cli = sorted(fx.cli().list("sub").await.unwrap());
    let from_rcd = sorted(rcd.list("sub").await.unwrap());
    assert_eq!(from_cli, from_rcd);

    rcd.shutdown().await.unwrap();
}

#[tokio::test]
async fn both_implementations_stat_identically() {
    require_rclone!();
    let fx = Fixture::new();
    let rcd = fx.rcd().await;

    let from_cli = fx.cli().stat("sub/b.txt").await.unwrap().unwrap();
    let from_rcd = rcd.stat("sub/b.txt").await.unwrap().unwrap();
    assert_eq!(from_cli.path, "sub/b.txt");
    assert_eq!(from_cli.size, 7);
    assert_eq!(from_cli.path, from_rcd.path);
    assert_eq!(from_cli.size, from_rcd.size);
    assert_eq!(from_cli.blake3, from_rcd.blake3);

    rcd.shutdown().await.unwrap();
}

/// Absence must read as `None` from both, and must not be confused with an error.
#[tokio::test]
async fn both_implementations_report_missing_files_as_none() {
    require_rclone!();
    let fx = Fixture::new();
    let rcd = fx.rcd().await;

    assert!(fx.cli().stat("ghost.txt").await.unwrap().is_none());
    assert!(rcd.stat("ghost.txt").await.unwrap().is_none());

    rcd.shutdown().await.unwrap();
}

/// A broken remote must surface as an error, never as "the file is not there":
/// the replace step deletes originals based on this distinction.
#[tokio::test]
async fn a_bad_remote_is_an_error_not_an_absence() {
    require_rclone!();
    let fx = Fixture::new();
    let broken = CliRemote::new("nosuchremote:").with_config(&fx.config);
    assert!(
        broken.stat("a.txt").await.is_err(),
        "an unusable remote must not be reported as a missing file"
    );
    assert!(broken.list("").await.is_err());
}

#[tokio::test]
async fn hashsum_matches_the_listed_hash() {
    require_rclone!();
    let fx = Fixture::new();
    let rcd = fx.rcd().await;

    let listed = sorted(rcd.list("").await.unwrap());
    let a = listed.iter().find(|e| e.path == "a.txt").unwrap();

    assert_eq!(rcd.hashsum("a.txt").await.unwrap(), a.blake3);
    assert_eq!(fx.cli().hashsum("a.txt").await.unwrap(), a.blake3);

    // blake3 of "hello", to catch a silent switch to another hash algorithm.
    assert_eq!(
        a.blake3.as_deref(),
        Some("ea8f163db38682925e4491c5e58d4bb3506ef8c14eb78a86e908c5624a67200f")
    );

    rcd.shutdown().await.unwrap();
}

#[tokio::test]
async fn about_reports_quota() {
    require_rclone!();
    let fx = Fixture::new();
    let about = fx.cli().about().await.unwrap();
    assert!(about.total.unwrap_or(0) > 0);
    assert!(about.free.unwrap_or(0) > 0);
}

/// The whole point of `scan`: a listing lands in the ledger and survives a rescan.
#[tokio::test]
async fn listing_feeds_the_ledger_and_rescanning_is_idempotent() {
    require_rclone!();
    let fx = Fixture::new();
    let ledger = Ledger::open_in_memory().unwrap();

    let entries = fx.cli().list("").await.unwrap();
    assert_eq!(ledger.upsert(entries.clone()).await.unwrap(), 3);
    assert_eq!(ledger.counts().await.unwrap().pending, 3);

    // Mark one done, then rescan: the verdict must stick.
    ledger.set_state("a.txt", State::Done, None).await.unwrap();
    ledger.upsert(entries).await.unwrap();

    let counts = ledger.counts().await.unwrap();
    assert_eq!(counts.done, 1);
    assert_eq!(counts.pending, 2);
    assert_eq!(counts.total_bytes, 5 + 7 + 1024);
}
