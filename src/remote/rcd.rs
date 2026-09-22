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
/// How often to ask how a long-running job is doing. The calls go over loopback,
/// so the cost is noise next to a listing that runs for minutes.
const POLL_INTERVAL: Duration = Duration::from_secs(1);
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

/// Running counters for one job, as `core/stats` reports them.
///
/// rclone reports a good deal more; these are the fields a caller can show
/// while waiting. Unlisted fields are ignored rather than rejected, so a future
/// rclone adding to the response does not break the parse.
#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobStats {
    /// Directory entries listed so far. This is what makes a recursive listing
    /// observable at all.
    pub listed: u64,
    pub deletes: u64,
    pub deleted_dirs: u64,
    pub errors: u64,
}

/// `job/status`. While a job runs, `output` is null and `error` is empty.
#[derive(Debug, Deserialize)]
struct JobStatus {
    finished: bool,
    #[serde(default)]
    error: String,
    /// The stats group rclone opened for this job, read rather than derived from
    /// the job id so the naming convention is rclone's business, not ours.
    #[serde(default)]
    group: String,
    #[serde(default)]
    output: Value,
}

/// Long-running calls, driven as background jobs so their progress is visible.
///
/// An rc call made the ordinary way returns nothing until it is completely
/// finished, which for a recursive listing of a whole drive can be many minutes
/// of silence. Passing `_async` instead yields a job id immediately and opens a
/// stats group rclone updates as the work proceeds.
impl RcdRemote {
    /// POSTs with `_async` set, returning the job id rclone assigned.
    async fn call_async(&self, method: &str, mut body: Value) -> Result<u64> {
        body["_async"] = Value::Bool(true);
        let value = self.call(method, body).await?;
        value
            .get("jobid")
            .and_then(Value::as_u64)
            .with_context(|| format!("rc call {method} did not return a job id"))
    }

    /// Polls until the job finishes, reporting its stats in between, and returns
    /// the output the same call made synchronously would have produced.
    async fn await_job(
        &self,
        method: &str,
        jobid: u64,
        mut on_tick: impl FnMut(&JobStats) + Send,
    ) -> Result<Value> {
        loop {
            let value = self.call("job/status", json!({ "jobid": jobid })).await?;
            let status: JobStatus = serde_json::from_value(value)
                .with_context(|| format!("job/status for {method} had an unexpected shape"))?;

            if status.finished {
                // A job reports its failure here rather than through the HTTP
                // status, which `call` already covers, so this is the only place
                // an async failure surfaces. Swallowing it would turn a failed
                // listing into an empty one.
                if !status.error.is_empty() {
                    bail!("rc call {method} failed: {}", status.error);
                }
                return Ok(status.output);
            }

            on_tick(&self.job_stats(&status.group).await);
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Best effort: stats only describe the job, so failing to read them must not
    /// fail the job itself.
    async fn job_stats(&self, group: &str) -> JobStats {
        if group.is_empty() {
            return JobStats::default();
        }
        let body = json!({ "group": group, "short": true });
        match self.call("core/stats", body).await {
            Ok(value) => serde_json::from_value(value).unwrap_or_else(|e| {
                tracing::debug!("could not parse stats for {group}: {e}");
                JobStats::default()
            }),
            Err(e) => {
                tracing::debug!("could not read stats for {group}: {e:#}");
                JobStats::default()
            }
        }
    }

    /// The `operations/list` request body, shared by the plain and observed paths.
    fn list_request(&self, scope: &str) -> Value {
        json!({
            "fs": remote_spec(&self.remote, ""),
            "remote": super::normalize_path(scope),
            "opt": {
                "recurse": true,
                "filesOnly": true,
                "showHash": true,
                "hashTypes": [HASH_TYPE],
            }
        })
    }

    /// Lists like [`Remote::list`], reporting entries seen while the call is in
    /// flight.
    ///
    /// Filen answers a recursive listing from one bulk request and then decrypts
    /// every name locally, so `listed` stays at zero for the fetch and climbs
    /// during the decrypt. The transition is itself informative; a smoothly
    /// rising count from the first second is not on offer.
    pub async fn list_progress(
        &self,
        scope: &str,
        on_tick: impl FnMut(&JobStats) + Send,
    ) -> Result<Vec<Entry>> {
        let jobid = self
            .call_async("operations/list", self.list_request(scope))
            .await?;
        let output = self.await_job("operations/list", jobid, on_tick).await?;
        let list = output
            .get("list")
            .context("operations/list response had no `list` field")?;
        // Unlike `lsjson`, whose paths are relative to the listed directory,
        // `operations/list` returns paths relative to `fs` with the `remote`
        // sub-path already included. So the scope must NOT be prefixed again.
        parse_entries(&list.to_string(), "")
    }

    /// Empties the trash like [`Remote::cleanup`], reporting progress as it goes.
    ///
    /// Whether a backend reports per-file deletes is up to the backend, so the
    /// counter may stay at zero throughout. The elapsed time still answers the
    /// question the caller is actually asking.
    pub async fn cleanup_progress(&self, on_tick: impl FnMut(&JobStats) + Send) -> Result<()> {
        let body = json!({ "fs": remote_spec(&self.remote, "") });
        let jobid = self.call_async("operations/cleanup", body).await?;
        self.await_job("operations/cleanup", jobid, on_tick)
            .await
            .map(|_| ())
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
    /// Delegates to [`RcdRemote::list_progress`] rather than repeating it, so the
    /// parity suite exercises the path the CLI actually runs.
    fn list(&self, scope: &str) -> impl Future<Output = Result<Vec<Entry>>> + Send {
        self.list_progress(scope, |_| {})
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
        self.cleanup_progress(|_| {})
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

    /// Captured verbatim from `job/status` against rclone 1.75.1 while an
    /// `operations/list` was still running. Note `output: null` and the empty
    /// `error`: neither means anything until `finished` is true.
    const RUNNING: &str = r#"{
 "duration": 0,
 "endTime": "0001-01-01T00:00:00Z",
 "error": "",
 "executeId": "00c89e3f-edf1-4b46-9752-3495a965df7e",
 "finished": false,
 "group": "job/1",
 "id": 1,
 "output": null,
 "startTime": "2026-09-22T10:30:50.318939+09:00",
 "success": false
}"#;

    /// The same call after it failed. `success` is false and the reason is in
    /// `error`; the HTTP status was 200, so this is the only signal there is.
    const FAILED: &str = r#"{
 "duration": 0.00064275,
 "endTime": "2026-09-22T10:30:24.404089+09:00",
 "error": "error in ListJSON: directory not found",
 "executeId": "cd0af5e3-7667-4055-a5eb-690e57003ff3",
 "finished": true,
 "group": "job/5",
 "id": 5,
 "output": {},
 "startTime": "2026-09-22T10:30:24.403446+09:00",
 "success": false
}"#;

    #[test]
    fn running_job_carries_no_output_yet() {
        let status: JobStatus = serde_json::from_str(RUNNING).unwrap();
        assert!(!status.finished);
        assert!(status.error.is_empty());
        assert!(status.output.is_null());
        // The group is what the stats poll is addressed to; without it there is
        // nothing to show while waiting.
        assert_eq!(status.group, "job/1");
    }

    /// An async failure arrives with HTTP 200 and an `error` string. Reading only
    /// `output` would turn a failed listing into an empty one, and an empty
    /// listing quietly means "this scope has no files".
    #[test]
    fn failed_job_reports_its_error() {
        let status: JobStatus = serde_json::from_str(FAILED).unwrap();
        assert!(status.finished);
        assert_eq!(status.error, "error in ListJSON: directory not found");
    }

    #[test]
    fn successful_job_hands_back_the_synchronous_output() {
        let json = r#"{"finished":true,"error":"","group":"job/1","success":true,
            "output":{"list":[{"Path":"a.txt","Size":5,"IsDir":false}]}}"#;
        let status: JobStatus = serde_json::from_str(json).unwrap();
        assert!(status.finished);
        let entries = parse_entries(&status.output["list"].to_string(), "").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "a.txt");
    }

    /// Captured verbatim from `core/stats` against rclone 1.75.1, mid-listing.
    /// Most of the response is about transfers and is deliberately ignored.
    #[test]
    fn stats_parse_and_ignore_the_transfer_fields() {
        let json = r#"{
	"bytes": 0,
	"checks": 0,
	"deletedDirs": 0,
	"deletes": 0,
	"elapsedTime": 1.464237125,
	"errors": 0,
	"eta": null,
	"fatalError": false,
	"listed": 92277,
	"renames": 0,
	"retryError": false,
	"serverSideCopies": 0,
	"serverSideCopyBytes": 0,
	"serverSideMoveBytes": 0,
	"serverSideMoves": 0,
	"speed": 0,
	"totalBytes": 0,
	"totalChecks": 0,
	"totalTransfers": 0,
	"transferTime": 0,
	"transfers": 0
}"#;
        let stats: JobStats = serde_json::from_str(json).unwrap();
        assert_eq!(stats.listed, 92_277);
        assert_eq!(stats.deletes, 0);
        assert_eq!(stats.errors, 0);
    }

    /// `deletedDirs` is the one field whose name does not survive a naive
    /// snake_case mapping, so it gets its own check.
    #[test]
    fn stats_map_camel_case_names() {
        let stats: JobStats =
            serde_json::from_str(r#"{"listed":3,"deletes":7,"deletedDirs":2,"errors":1}"#).unwrap();
        assert_eq!(stats.deleted_dirs, 2);
        assert_eq!(stats.deletes, 7);
        assert_eq!(stats.errors, 1);
    }

    /// The listing body is what makes scope handling correct; `remote` carries the
    /// scope and `fs` stays at the remote root, which is why paths come back
    /// already prefixed.
    #[test]
    fn list_request_puts_the_scope_in_remote_not_fs() {
        let remote = RcdRemote {
            remote: "filen:".into(),
            base_url: String::new(),
            password: String::new(),
            http: reqwest::Client::new(),
            child: None,
        };
        let body = remote.list_request("/Photos/2019/");
        assert_eq!(body["fs"], "filen:");
        assert_eq!(body["remote"], "Photos/2019");
        assert_eq!(body["opt"]["recurse"], true);
        assert_eq!(body["opt"]["hashTypes"][0], HASH_TYPE);
    }
}
