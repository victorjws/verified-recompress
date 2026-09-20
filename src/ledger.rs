//! Durable job state, so a multi-day run over a multi-terabyte drive can be
//! interrupted and resumed without reprocessing or double-processing anything.
//!
//! `rusqlite` is synchronous and SQLite tolerates exactly one writer, so the
//! connection lives in a dedicated task that serialises commands arriving over a
//! channel. That makes `SQLITE_BUSY` structurally impossible rather than something
//! to retry around, and keeps blocking I/O off the async worker threads.

use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::{mpsc, oneshot};

use crate::remote::Entry;

/// Where a file sits in the pipeline. Stored as text so the database stays readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Inventoried, not yet examined.
    Pending,
    /// Taken by a worker. Recovered back to `Pending` on the next start.
    Claimed,
    /// Converted, verified, and the original replaced.
    Done,
    /// Deliberately not converted. `skip_reason` says why.
    Skipped,
    /// Something went wrong. Safe to retry.
    Failed,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Pending => "pending",
            State::Claimed => "claimed",
            State::Done => "done",
            State::Skipped => "skipped",
            State::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "pending" => State::Pending,
            "claimed" => State::Claimed,
            "done" => State::Done,
            "skipped" => State::Skipped,
            "failed" => State::Failed,
            other => bail!("unknown state `{other}` in ledger"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    pub path: String,
    pub size: u64,
    pub mod_time: Option<String>,
    pub blake3: Option<String>,
    pub state: State,
    pub skip_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub pending: u64,
    pub claimed: u64,
    pub done: u64,
    pub skipped: u64,
    pub failed: u64,
    pub total_bytes: u64,
}

/// Commands the actor understands. Each carries a channel for its reply.
enum Cmd {
    Upsert {
        entries: Vec<Entry>,
        reply: oneshot::Sender<Result<usize>>,
    },
    Get {
        path: String,
        reply: oneshot::Sender<Result<Option<FileRow>>>,
    },
    Counts {
        reply: oneshot::Sender<Result<Counts>>,
    },
    ClaimNext {
        reply: oneshot::Sender<Result<Option<FileRow>>>,
    },
    SetState {
        path: String,
        state: State,
        skip_reason: Option<String>,
        reply: oneshot::Sender<Result<()>>,
    },
    RecoverClaimed {
        reply: oneshot::Sender<Result<usize>>,
    },
}

/// Handle to the ledger actor. Cloning it is cheap and safe across tasks.
#[derive(Clone)]
pub struct Ledger {
    tx: mpsc::Sender<Cmd>,
}

impl Ledger {
    /// Opens (or creates) the database and starts the actor task.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open ledger at {}", path.display()))?;
        Self::from_connection(conn)
    }

    /// An ephemeral ledger that vanishes with the process. Useful for tests and for
    /// read-only reporting runs that should leave nothing behind.
    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        init_schema(&conn)?;
        let (tx, rx) = mpsc::channel(256);
        // A blocking thread, not a tokio task: every statement is synchronous.
        std::thread::Builder::new()
            .name("ledger".into())
            .spawn(move || actor_loop(conn, rx))
            .context("failed to spawn the ledger thread")?;
        Ok(Self { tx })
    }

    async fn send<T>(&self, make: impl FnOnce(oneshot::Sender<Result<T>>) -> Cmd) -> Result<T> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(make(reply))
            .await
            .map_err(|_| anyhow::anyhow!("ledger actor has stopped"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("ledger actor dropped the reply"))?
    }

    /// Inserts new files and refreshes metadata on known ones. Returns the row count
    /// touched. Files whose size and hash are unchanged keep their state, so a rescan
    /// does not undo completed work.
    pub async fn upsert(&self, entries: Vec<Entry>) -> Result<usize> {
        self.send(|reply| Cmd::Upsert { entries, reply }).await
    }

    pub async fn get(&self, path: &str) -> Result<Option<FileRow>> {
        let path = path.to_string();
        self.send(|reply| Cmd::Get { path, reply }).await
    }

    pub async fn counts(&self) -> Result<Counts> {
        self.send(|reply| Cmd::Counts { reply }).await
    }

    /// Atomically takes the next pending file. Because the actor is the only writer,
    /// two concurrent callers can never receive the same row.
    pub async fn claim_next(&self) -> Result<Option<FileRow>> {
        self.send(|reply| Cmd::ClaimNext { reply }).await
    }

    pub async fn set_state(
        &self,
        path: &str,
        state: State,
        skip_reason: Option<String>,
    ) -> Result<()> {
        let path = path.to_string();
        self.send(|reply| Cmd::SetState {
            path,
            state,
            skip_reason,
            reply,
        })
        .await
    }

    /// Returns rows stranded in `claimed` by a crash back to `pending`.
    pub async fn recover_claimed(&self) -> Result<usize> {
        self.send(|reply| Cmd::RecoverClaimed { reply }).await
    }
}

fn init_schema(conn: &Connection) -> Result<()> {
    // WAL lets readers proceed while the single writer works.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS files (
            path        TEXT PRIMARY KEY,
            size        INTEGER NOT NULL,
            mod_time    TEXT,
            blake3      TEXT,
            state       TEXT NOT NULL DEFAULT 'pending',
            skip_reason TEXT,
            seen_at     TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS files_state ON files(state);
        -- Intake order is by projected savings, so the quota drops fastest early.
        CREATE INDEX IF NOT EXISTS files_state_size ON files(state, size DESC);
        "#,
    )?;
    Ok(())
}

fn actor_loop(mut conn: Connection, mut rx: mpsc::Receiver<Cmd>) {
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            Cmd::Upsert { entries, reply } => {
                let _ = reply.send(do_upsert(&mut conn, &entries));
            }
            Cmd::Get { path, reply } => {
                let _ = reply.send(do_get(&conn, &path));
            }
            Cmd::Counts { reply } => {
                let _ = reply.send(do_counts(&conn));
            }
            Cmd::ClaimNext { reply } => {
                let _ = reply.send(do_claim_next(&mut conn));
            }
            Cmd::SetState {
                path,
                state,
                skip_reason,
                reply,
            } => {
                let _ = reply.send(do_set_state(&conn, &path, state, skip_reason.as_deref()));
            }
            Cmd::RecoverClaimed { reply } => {
                let _ = reply.send(do_recover_claimed(&conn));
            }
        }
    }
}

fn do_upsert(conn: &mut Connection, entries: &[Entry]) -> Result<usize> {
    let tx = conn.transaction()?;
    let mut count = 0;
    {
        // Re-inventorying must not reset progress. Only when the file actually
        // changed on the remote does it go back to pending for reassessment.
        let mut stmt = tx.prepare(
            r#"
            INSERT INTO files (path, size, mod_time, blake3, state)
            VALUES (?1, ?2, ?3, ?4, 'pending')
            ON CONFLICT(path) DO UPDATE SET
                size     = excluded.size,
                mod_time = excluded.mod_time,
                blake3   = excluded.blake3,
                seen_at  = datetime('now'),
                state = CASE
                    WHEN files.size != excluded.size THEN 'pending'
                    ELSE files.state
                END,
                skip_reason = CASE
                    WHEN files.size != excluded.size THEN NULL
                    ELSE files.skip_reason
                END
            "#,
        )?;
        for entry in entries {
            stmt.execute(params![
                entry.path,
                entry.size as i64,
                entry.mod_time,
                entry.blake3,
            ])?;
            count += 1;
        }
    }
    tx.commit()?;
    Ok(count)
}

/// A row exactly as stored, before the state text is validated.
struct RawRow {
    path: String,
    size: i64,
    mod_time: Option<String>,
    blake3: Option<String>,
    state: String,
    skip_reason: Option<String>,
}

fn row_to_file(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok(RawRow {
        path: row.get(0)?,
        size: row.get(1)?,
        mod_time: row.get(2)?,
        blake3: row.get(3)?,
        state: row.get(4)?,
        skip_reason: row.get(5)?,
    })
}

fn build_row(raw: RawRow) -> Result<FileRow> {
    Ok(FileRow {
        path: raw.path,
        size: u64::try_from(raw.size).unwrap_or(0),
        mod_time: raw.mod_time,
        blake3: raw.blake3,
        state: State::parse(&raw.state)?,
        skip_reason: raw.skip_reason,
    })
}

fn do_get(conn: &Connection, path: &str) -> Result<Option<FileRow>> {
    let raw = conn
        .query_row(
            "SELECT path, size, mod_time, blake3, state, skip_reason FROM files WHERE path = ?1",
            params![path],
            row_to_file,
        )
        .optional()?;
    raw.map(build_row).transpose()
}

fn do_counts(conn: &Connection) -> Result<Counts> {
    let mut counts = Counts::default();
    let mut stmt = conn.prepare("SELECT state, COUNT(*), COALESCE(SUM(size), 0) FROM files GROUP BY state")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    for row in rows {
        let (state, n, bytes) = row?;
        let n = u64::try_from(n).unwrap_or(0);
        counts.total_bytes += u64::try_from(bytes).unwrap_or(0);
        match State::parse(&state)? {
            State::Pending => counts.pending = n,
            State::Claimed => counts.claimed = n,
            State::Done => counts.done = n,
            State::Skipped => counts.skipped = n,
            State::Failed => counts.failed = n,
        }
    }
    Ok(counts)
}

fn do_claim_next(conn: &mut Connection) -> Result<Option<FileRow>> {
    let tx = conn.transaction()?;
    let raw = tx
        .query_row(
            "SELECT path, size, mod_time, blake3, state, skip_reason
             FROM files WHERE state = 'pending' ORDER BY size DESC, path LIMIT 1",
            [],
            row_to_file,
        )
        .optional()?;
    let Some(raw) = raw else {
        tx.commit()?;
        return Ok(None);
    };
    tx.execute(
        "UPDATE files SET state = 'claimed' WHERE path = ?1",
        params![raw.path],
    )?;
    tx.commit()?;
    let mut row = build_row(raw)?;
    row.state = State::Claimed;
    Ok(Some(row))
}

fn do_set_state(
    conn: &Connection,
    path: &str,
    state: State,
    skip_reason: Option<&str>,
) -> Result<()> {
    let changed = conn.execute(
        "UPDATE files SET state = ?2, skip_reason = ?3 WHERE path = ?1",
        params![path, state.as_str(), skip_reason],
    )?;
    if changed == 0 {
        bail!("no ledger row for `{path}`");
    }
    Ok(())
}

fn do_recover_claimed(conn: &Connection) -> Result<usize> {
    let n = conn.execute(
        "UPDATE files SET state = 'pending' WHERE state = 'claimed'",
        [],
    )?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, size: u64) -> Entry {
        Entry {
            path: path.to_string(),
            size,
            mod_time: Some("2026-09-21T01:00:00Z".into()),
            blake3: Some(format!("hash-of-{path}")),
        }
    }

    #[tokio::test]
    async fn upsert_then_read_back() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger
            .upsert(vec![entry("a.jpg", 100), entry("b/c.png", 200)])
            .await
            .unwrap();

        let row = ledger.get("a.jpg").await.unwrap().unwrap();
        assert_eq!(row.size, 100);
        assert_eq!(row.state, State::Pending);
        assert_eq!(row.blake3.as_deref(), Some("hash-of-a.jpg"));

        let counts = ledger.counts().await.unwrap();
        assert_eq!(counts.pending, 2);
        assert_eq!(counts.total_bytes, 300);
    }

    #[tokio::test]
    async fn unknown_path_reads_as_none() {
        let ledger = Ledger::open_in_memory().unwrap();
        assert!(ledger.get("nope").await.unwrap().is_none());
    }

    /// A rescan must not undo finished work, or every run would redo the whole drive.
    #[tokio::test]
    async fn rescan_preserves_state_for_unchanged_files() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger.upsert(vec![entry("a.jpg", 100)]).await.unwrap();
        ledger.set_state("a.jpg", State::Done, None).await.unwrap();

        ledger.upsert(vec![entry("a.jpg", 100)]).await.unwrap();
        assert_eq!(ledger.get("a.jpg").await.unwrap().unwrap().state, State::Done);
    }

    /// If the file changed on the remote, the old verdict no longer applies.
    #[tokio::test]
    async fn rescan_resets_state_when_size_changes() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger.upsert(vec![entry("a.jpg", 100)]).await.unwrap();
        ledger
            .set_state("a.jpg", State::Skipped, Some("too_small".into()))
            .await
            .unwrap();

        ledger.upsert(vec![entry("a.jpg", 999)]).await.unwrap();
        let row = ledger.get("a.jpg").await.unwrap().unwrap();
        assert_eq!(row.state, State::Pending);
        assert_eq!(row.skip_reason, None, "stale skip reason must be cleared");
        assert_eq!(row.size, 999);
    }

    #[tokio::test]
    async fn claim_takes_largest_first() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger
            .upsert(vec![entry("small", 1), entry("big", 100), entry("mid", 50)])
            .await
            .unwrap();
        assert_eq!(ledger.claim_next().await.unwrap().unwrap().path, "big");
        assert_eq!(ledger.claim_next().await.unwrap().unwrap().path, "mid");
        assert_eq!(ledger.claim_next().await.unwrap().unwrap().path, "small");
        assert!(ledger.claim_next().await.unwrap().is_none());
    }

    /// The whole point of the single-writer actor: concurrent claims cannot collide.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_claims_never_hand_out_the_same_file() {
        let ledger = Ledger::open_in_memory().unwrap();
        let entries: Vec<_> = (0..200).map(|i| entry(&format!("f{i:03}"), i)).collect();
        ledger.upsert(entries).await.unwrap();

        let mut handles = Vec::new();
        for _ in 0..8 {
            let ledger = ledger.clone();
            handles.push(tokio::spawn(async move {
                let mut mine = Vec::new();
                while let Some(row) = ledger.claim_next().await.unwrap() {
                    mine.push(row.path);
                }
                mine
            }));
        }

        let mut all = Vec::new();
        for handle in handles {
            all.extend(handle.await.unwrap());
        }
        all.sort();
        let unique = all.len();
        all.dedup();
        assert_eq!(all.len(), unique, "a file was claimed more than once");
        assert_eq!(all.len(), 200, "every file should have been claimed exactly once");
    }

    #[tokio::test]
    async fn crashed_claims_are_recovered() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger
            .upsert(vec![entry("a", 1), entry("b", 2)])
            .await
            .unwrap();
        ledger.claim_next().await.unwrap();
        assert_eq!(ledger.counts().await.unwrap().claimed, 1);

        assert_eq!(ledger.recover_claimed().await.unwrap(), 1);
        let counts = ledger.counts().await.unwrap();
        assert_eq!(counts.claimed, 0);
        assert_eq!(counts.pending, 2);
    }

    #[tokio::test]
    async fn setting_state_on_a_missing_row_errors() {
        let ledger = Ledger::open_in_memory().unwrap();
        assert!(ledger.set_state("ghost", State::Done, None).await.is_err());
    }

    #[tokio::test]
    async fn counts_break_down_by_state() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger
            .upsert(vec![entry("a", 10), entry("b", 20), entry("c", 30)])
            .await
            .unwrap();
        ledger.set_state("a", State::Done, None).await.unwrap();
        ledger
            .set_state("b", State::Skipped, Some("already_optimal".into()))
            .await
            .unwrap();

        let counts = ledger.counts().await.unwrap();
        assert_eq!(counts.done, 1);
        assert_eq!(counts.skipped, 1);
        assert_eq!(counts.pending, 1);
        assert_eq!(counts.total_bytes, 60);
    }

    #[tokio::test]
    async fn survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("ledger.sqlite");
        {
            let ledger = Ledger::open(&path).unwrap();
            ledger.upsert(vec![entry("a.jpg", 42)]).await.unwrap();
            ledger.set_state("a.jpg", State::Done, None).await.unwrap();
        }
        let ledger = Ledger::open(&path).unwrap();
        assert_eq!(ledger.get("a.jpg").await.unwrap().unwrap().state, State::Done);
    }

    #[test]
    fn state_round_trips_through_text() {
        for state in [
            State::Pending,
            State::Claimed,
            State::Done,
            State::Skipped,
            State::Failed,
        ] {
            assert_eq!(State::parse(state.as_str()).unwrap(), state);
        }
        assert!(State::parse("bogus").is_err());
    }
}
