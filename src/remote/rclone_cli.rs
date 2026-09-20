//! Fallback [`Remote`] that shells out to the `rclone` binary once per call.
//!
//! Slower than the daemon, but it has no lifecycle to manage, so integration tests
//! use it as the reference behaviour that [`super::rcd::RcdRemote`] must match.

use std::future::Future;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

use super::{About, Entry, HASH_TYPE, Remote, parse_about, parse_entries, remote_spec};

pub struct CliRemote {
    remote: String,
    /// Overrides `RCLONE_CONFIG` when set, so tests can use a throwaway config.
    config_path: Option<String>,
}

impl CliRemote {
    pub fn new(remote: impl Into<String>) -> Self {
        Self {
            remote: remote.into(),
            config_path: None,
        }
    }

    pub fn with_config(mut self, path: impl Into<String>) -> Self {
        self.config_path = Some(path.into());
        self
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new("rclone");
        if let Some(path) = &self.config_path {
            cmd.env("RCLONE_CONFIG", path);
        }
        cmd
    }

    /// Runs rclone, returning stdout plus the exit code so callers can tell apart
    /// "the object is not there" from "rclone could not answer".
    async fn run_raw(&self, args: &[&str]) -> Result<(i32, String)> {
        let output = self
            .command()
            .args(args)
            .output()
            .await
            .context("failed to execute rclone; is it installed and on PATH?")?;

        let code = output.status.code().unwrap_or(-1);
        if code != 0 && !is_not_found(code) {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "rclone {} failed (exit {code}): {}",
                args.join(" "),
                stderr.trim()
            );
        }
        let stdout = String::from_utf8(output.stdout).context("rclone produced non-UTF-8 output")?;
        Ok((code, stdout))
    }

    /// Runs rclone and returns stdout, failing on any non-zero exit.
    ///
    /// Checking the status is not optional: a failing `lsjson` still prints a bare
    /// `[` to stdout, which would otherwise parse as a truncated document.
    async fn run(&self, args: &[&str]) -> Result<String> {
        let (code, stdout) = self.run_raw(args).await?;
        if code != 0 {
            bail!("rclone {} failed (exit {code})", args.join(" "));
        }
        Ok(stdout)
    }
}

/// rclone signals a missing directory with 3 and a missing file with 4. Every other
/// non-zero code is a genuine failure (1 = usage, 2 = uncategorised, 5 = temporary,
/// 7 = fatal), and must never be mistaken for absence: treating a network error as
/// "the file is gone" could let the replace step delete an original it should not.
fn is_not_found(code: i32) -> bool {
    matches!(code, 3 | 4)
}

impl Remote for CliRemote {
    fn list(&self, scope: &str) -> impl Future<Output = Result<Vec<Entry>>> + Send {
        let spec = remote_spec(&self.remote, scope);
        let scope = scope.to_string();
        async move {
            let json = self
                .run(&[
                    "lsjson",
                    "--recursive",
                    "--files-only",
                    // Narrow the hash set explicitly; a bare --hash makes some backends
                    // compute every algorithm they support.
                    "--hash-type",
                    HASH_TYPE,
                    // Filen supports ListR, so one recursive call beats per-directory walks.
                    "--fast-list",
                    &spec,
                ])
                .await?;
            parse_entries(&json, &scope)
        }
    }

    fn about(&self) -> impl Future<Output = Result<About>> + Send {
        let spec = self.remote.clone();
        async move {
            let json = self.run(&["about", "--json", &spec]).await?;
            parse_about(&json)
        }
    }

    fn stat(&self, path: &str) -> impl Future<Output = Result<Option<Entry>>> + Send {
        let spec = remote_spec(&self.remote, path);
        let path = path.to_string();
        async move {
            // `lsjson --stat` prints one object. A missing object exits 3/4 with empty
            // stdout; any other failure propagates rather than reading as absence.
            let (code, json) = self
                .run_raw(&["lsjson", "--stat", "--hash-type", HASH_TYPE, &spec])
                .await?;
            if code != 0 {
                return Ok(None);
            }
            // Wrap the single object so the shared array parser applies. The entry's
            // own Path is just the basename here, so re-attach the full path.
            let wrapped = format!("[{json}]");
            let mut entries = parse_entries(&wrapped, "")?;
            Ok(entries.pop().map(|mut e| {
                e.path = path;
                e
            }))
        }
    }

    async fn hashsum(&self, path: &str) -> Result<Option<String>> {
        Ok(self.stat(path).await?.and_then(|e| e.blake3))
    }
}
