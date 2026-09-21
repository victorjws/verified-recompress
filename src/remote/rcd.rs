//! Primary [`Remote`]: one long-lived `rclone rcd` daemon driven over its HTTP
//! control API.
//!
//! This avoids forking a process per file, reuses connections, and returns
//! structured JSON. The daemon is a child process owned by [`RcdRemote`] and is shut
//! down on drop.
//!
//! Security: rclone's own documentation states that access to the rc API is
//! equivalent to shell access as the user running rclone. The listener is therefore
//! bound to loopback only and protected by a password generated fresh at startup;
//! `--rc-no-auth` is never used.

use std::future::Future;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
// rand 0.10 moved `random_range` off `Rng` onto the `RngExt` extension trait.
use rand::RngExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::{Child, Command};

use super::{About, Entry, HASH_TYPE, Remote, parse_about, parse_entries, remote_spec};

/// How long to wait for the daemon's listener to come up.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
/// Grace period for a polite `core/quit` before the child is killed.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(1500);
const RC_USER: &str = "verified-recompress";

pub struct RcdRemote {
    remote: String,
    base_url: String,
    password: String,
    http: reqwest::Client,
    child: Option<Child>,
}

impl RcdRemote {
    /// Spawns a daemon on a free loopback port and waits for it to answer.
    pub async fn spawn(remote: impl Into<String>, config_path: Option<&str>) -> Result<Self> {
        let remote = remote.into();
        let port = free_loopback_port().context("could not reserve a loopback port for rclone")?;
        let password = random_password();
        let addr = format!("127.0.0.1:{port}");

        let mut cmd = Command::new("rclone");
        cmd.args([
            "rcd",
            "--rc-addr",
            &addr,
            "--rc-user",
            RC_USER,
            "--rc-pass",
            &password,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
        if let Some(path) = config_path {
            cmd.env("RCLONE_CONFIG", path);
        }

        let child = cmd
            .spawn()
            .context("failed to start `rclone rcd`; is rclone installed and on PATH?")?;

        let this = Self {
            remote,
            base_url: format!("http://{addr}"),
            password,
            http: reqwest::Client::new(),
            child: Some(child),
        };
        this.await_ready().await?;
        Ok(this)
    }

    async fn await_ready(&self) -> Result<()> {
        let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
        let mut last: Option<anyhow::Error> = None;
        while tokio::time::Instant::now() < deadline {
            match self.call("core/version", json!({})).await {
                Ok(_) => return Ok(()),
                Err(e) => last = Some(e),
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        match last {
            Some(e) => Err(e).context("rclone rcd did not become ready"),
            None => bail!("rclone rcd did not become ready"),
        }
    }

    /// POSTs to one rc method and returns its JSON body.
    async fn call(&self, method: &str, body: Value) -> Result<Value> {
        let response = self
            .http
            .post(format!("{}/{method}", self.base_url))
            .basic_auth(RC_USER, Some(&self.password))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("rc call {method} could not be sent"))?;

        let status = response.status();
        let value: Value = response
            .json()
            .await
            .with_context(|| format!("rc call {method} returned a non-JSON body"))?;

        // Failures come back as {"error": "...", "status": 404, ...}.
        if !status.is_success() {
            let detail = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("no detail");
            bail!("rc call {method} failed ({status}): {detail}");
        }
        Ok(value)
    }
}

impl Drop for RcdRemote {
    fn drop(&mut self) {
        // Ask politely first, then make sure. `core/quit` returns before the process
        // has actually exited, and a blocking HTTP call is not possible here, so rely
        // on kill_on_drop plus an explicit start_kill.
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
    }
}

impl RcdRemote {
    /// Requests a clean shutdown, falling back to a kill.
    pub async fn shutdown(mut self) -> Result<()> {
        let _ = self.call("core/quit", json!({})).await;
        if let Some(mut child) = self.child.take() {
            let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => return Ok(()),
                    Ok(None) if tokio::time::Instant::now() < deadline => {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    // core/quit acknowledges before the process is gone, so a kill here
                    // is expected rather than exceptional.
                    Ok(None) => {
                        let _ = child.kill().await;
                        return Ok(());
                    }
                    Err(e) => return Err(e).context("failed to wait on rclone rcd"),
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct StatResponse {
    item: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct HashResponse {
    hash: Option<String>,
}

impl Remote for RcdRemote {
    fn list(&self, scope: &str) -> impl Future<Output = Result<Vec<Entry>>> + Send {
        let fs = remote_spec(&self.remote, "");
        let remote_path = super::normalize_path(scope);
        async move {
            let value = self
                .call(
                    "operations/list",
                    json!({
                        "fs": fs,
                        "remote": remote_path,
                        "opt": {
                            "recurse": true,
                            "filesOnly": true,
                            "showHash": true,
                            "hashTypes": [HASH_TYPE],
                        }
                    }),
                )
                .await?;
            let list = value
                .get("list")
                .context("operations/list response had no `list` field")?;
            // Unlike `lsjson`, whose paths are relative to the listed directory,
            // `operations/list` returns paths relative to `fs` with the `remote`
            // sub-path already included. So the scope must NOT be prefixed again.
            parse_entries(&list.to_string(), "")
        }
    }

    fn download(&self, path: &str, local: &Path) -> impl Future<Output = Result<()>> + Send {
        let fs = remote_spec(&self.remote, "");
        let remote_path = super::normalize_path(path);
        let split = split_local(local);
        async move {
            let (dir, name) = split?;
            self.call(
                "operations/copyfile",
                json!({
                    "srcFs": fs, "srcRemote": remote_path,
                    "dstFs": dir, "dstRemote": name,
                }),
            )
            .await
            .map(|_| ())
        }
    }

    fn upload(&self, local: &Path, path: &str) -> impl Future<Output = Result<()>> + Send {
        let fs = remote_spec(&self.remote, "");
        let remote_path = super::normalize_path(path);
        let split = split_local(local);
        async move {
            let (dir, name) = split?;
            self.call(
                "operations/copyfile",
                json!({
                    "srcFs": dir, "srcRemote": name,
                    "dstFs": fs, "dstRemote": remote_path,
                }),
            )
            .await
            .map(|_| ())
        }
    }

    fn delete(&self, path: &str) -> impl Future<Output = Result<()>> + Send {
        let fs = remote_spec(&self.remote, "");
        let remote_path = super::normalize_path(path);
        async move {
            self.call(
                "operations/deletefile",
                json!({ "fs": fs, "remote": remote_path }),
            )
            .await
            .map(|_| ())
        }
    }

    fn move_to(&self, from: &str, to: &str) -> impl Future<Output = Result<()>> + Send {
        let fs = remote_spec(&self.remote, "");
        let from = super::normalize_path(from);
        let to = super::normalize_path(to);
        async move {
            self.call(
                "operations/movefile",
                json!({
                    "srcFs": fs, "srcRemote": from,
                    "dstFs": fs, "dstRemote": to,
                }),
            )
            .await
            .map(|_| ())
        }
    }

    fn cleanup(&self) -> impl Future<Output = Result<()>> + Send {
        let fs = remote_spec(&self.remote, "");
        async move {
            self.call("operations/cleanup", json!({ "fs": fs }))
                .await
                .map(|_| ())
        }
    }

    fn about(&self) -> impl Future<Output = Result<About>> + Send {
        let fs = remote_spec(&self.remote, "");
        async move {
            let value = self.call("operations/about", json!({ "fs": fs })).await?;
            parse_about(&value.to_string())
        }
    }

    fn stat(&self, path: &str) -> impl Future<Output = Result<Option<Entry>>> + Send {
        let fs = remote_spec(&self.remote, "");
        let remote_path = super::normalize_path(path);
        let path = path.to_string();
        async move {
            let value = self
                .call(
                    "operations/stat",
                    json!({
                        "fs": fs,
                        "remote": remote_path,
                        "opt": { "showHash": true, "hashTypes": [HASH_TYPE] }
                    }),
                )
                .await?;
            // A missing object is reported as {"item": null} with HTTP 200, so absence
            // is distinguishable from a transport failure, which `call` already raised.
            let response: StatResponse = serde_json::from_value(value)
                .context("operations/stat response had an unexpected shape")?;
            let Some(item) = response.item else {
                return Ok(None);
            };
            let mut entries = parse_entries(&Value::Array(vec![item]).to_string(), "")?;
            Ok(entries.pop().map(|mut e| {
                e.path = super::normalize_path(&path);
                e
            }))
        }
    }

    fn hashsum(&self, path: &str) -> impl Future<Output = Result<Option<String>>> + Send {
        let fs = remote_spec(&self.remote, "");
        let remote_path = super::normalize_path(path);
        async move {
            let value = self
                .call(
                    "operations/hashsumfile",
                    json!({ "fs": fs, "remote": remote_path, "hashType": HASH_TYPE }),
                )
                .await?;
            let response: HashResponse = serde_json::from_value(value)
                .context("operations/hashsumfile response had an unexpected shape")?;
            Ok(response.hash.filter(|h| !h.is_empty()))
        }
    }
}

/// Splits a local file path into the (directory, filename) pair the rc API wants.
///
/// `operations/copyfile` addresses both ends as a filesystem plus a name within it;
/// a bare local directory path is a valid filesystem, so no remote name is needed.
fn split_local(path: &Path) -> Result<(String, String)> {
    let dir = path
        .parent()
        .context("local path has no parent directory")?
        .to_string_lossy()
        .into_owned();
    let name = path
        .file_name()
        .context("local path has no file name")?
        .to_string_lossy()
        .into_owned();
    Ok((dir, name))
}

/// Binds port 0 to let the OS pick a free port, then releases it.
///
/// This races in principle, but the window is small and the alternative — a fixed
/// port — collides with any other instance on the machine.
fn free_loopback_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

fn random_password() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::rng();
    (0..32)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_are_long_and_unpredictable() {
        let a = random_password();
        let b = random_password();
        assert_eq!(a.len(), 32);
        assert_ne!(a, b, "two generated passwords must not collide");
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn allocates_distinct_ports() {
        let a = free_loopback_port().unwrap();
        let b = free_loopback_port().unwrap();
        assert!(a > 0 && b > 0);
    }

    /// `{"item": null}` is how a missing object is reported, and it arrives with a
    /// 200 status, so it must not be conflated with a transport failure.
    #[test]
    fn stat_response_distinguishes_absent_from_present() {
        let absent: StatResponse = serde_json::from_str(r#"{"item": null}"#).unwrap();
        assert!(absent.item.is_none());
        let present: StatResponse =
            serde_json::from_str(r#"{"item": {"Path":"a.txt","Size":5,"IsDir":false}}"#).unwrap();
        assert!(present.item.is_some());
    }

    #[test]
    fn hash_response_parses() {
        let r: HashResponse =
            serde_json::from_str(r#"{"hash":"ea8f16","hashType":"blake3"}"#).unwrap();
        assert_eq!(r.hash.as_deref(), Some("ea8f16"));
    }
}
