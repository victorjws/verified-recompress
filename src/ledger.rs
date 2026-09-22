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

use crate::config::Order;
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
    ListByState {
        state: State,
        reply: oneshot::Sender<Result<Vec<FileRow>>>,
    },
    ClaimNext {
        prefixes: Vec<String>,
        order: Order,
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
    ReopenSkipped {
        reasons: Vec<String>,
        reply: oneshot::Sender<Result<usize>>,
    },
    RecordConversion {
        record: Conversion,
        reply: oneshot::Sender<Result<()>>,
    },
    PendingReclaim {
        reply: oneshot::Sender<Result<Reclaim>>,
    },
    MarkReclaimed {
        reply: oneshot::Sender<Result<u64>>,
    },
    Savings {
        reply: oneshot::Sender<Result<Savings>>,
    },
    Completed {
        limit: Option<usize>,
        reply: oneshot::Sender<Result<Vec<Completed>>>,
    },
    CompletedFor {
        path: String,
        reply: oneshot::Sender<Result<Option<Completed>>>,
    },
}

/// A finished replacement, as recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completed {
    /// Where the original used to be.
    pub path: String,
    pub output_path: String,
    pub original_size: u64,
    pub output_size: u64,
    /// blake3 of the original, from the inventory. What a restore is checked against.
    pub original_blake3: Option<String>,
    pub recipe: String,
    pub fidelity: String,
}

/// A completed replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversion {
    pub path: String,
    pub output_path: String,
    pub output_size: u64,
    pub recipe: String,
    pub fidelity: String,
}

/// Originals sitting in the trash, still counted against the quota.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reclaim {
    pub files: u64,
    pub bytes: u64,
}

/// What the conversions have achieved so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Savings {
    pub files: u64,
    pub original_bytes: u64,
    pub output_bytes: u64,
}

impl Savings {
    /// Bytes removed by the conversions themselves.
    ///
    /// This is not the same as the drop in quota: until the trash is emptied the
    /// originals are still there, and the quota has in fact gone up.
    pub fn logical_bytes(&self) -> u64 {
        self.original_bytes.saturating_sub(self.output_bytes)
    }
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

    /// All rows in one state, largest first. Used by `plan` to project a whole run.
    pub async fn list_by_state(&self, state: State) -> Result<Vec<FileRow>> {
        self.send(|reply| Cmd::ListByState { state, reply }).await
    }

    /// Atomically takes the next pending file within `prefixes`, in `order`.
    ///
    /// The filter is applied here rather than after claiming, so a scoped run can
    /// never pick up a file outside its scope even momentarily. An empty slice
    /// means the whole remote.
    pub async fn claim_next(&self, prefixes: &[String], order: Order) -> Result<Option<FileRow>> {
        let prefixes = prefixes.to_vec();
        self.send(|reply| Cmd::ClaimNext {
            prefixes,
            order,
            reply,
        })
        .await
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

    /// Returns files skipped for the given reasons to `pending`.
    ///
    /// Used when a setting changes that could alter the verdict, so that enabling
    /// the video tier or raising the size cap actually reconsiders the files those
    /// limits excluded, instead of leaving them skipped forever.
    pub async fn reopen_skipped(&self, reasons: &[&str]) -> Result<usize> {
        let reasons = reasons.iter().map(|r| r.to_string()).collect();
        self.send(|reply| Cmd::ReopenSkipped { reasons, reply }).await
    }

    /// Marks a file replaced, recording what replaced it and when the original
    /// went to the trash.
    pub async fn record_conversion(&self, record: Conversion) -> Result<()> {
        self.send(|reply| Cmd::RecordConversion { record, reply })
            .await
    }

    /// Originals still in the trash, whose space has not yet come back.
    pub async fn pending_reclaim(&self) -> Result<Reclaim> {
        self.send(|reply| Cmd::PendingReclaim { reply }).await
    }

    /// Records that the trash has been emptied. Returns the bytes reclaimed.
    pub async fn mark_reclaimed(&self) -> Result<u64> {
        self.send(|reply| Cmd::MarkReclaimed { reply }).await
    }

    pub async fn savings(&self) -> Result<Savings> {
        self.send(|reply| Cmd::Savings { reply }).await
    }

    /// Completed conversions, largest original first.
    pub async fn completed(&self, limit: Option<usize>) -> Result<Vec<Completed>> {
        self.send(|reply| Cmd::Completed { limit, reply }).await
    }

    /// The conversion recorded for one original path.
    pub async fn completed_for(&self, path: &str) -> Result<Option<Completed>> {
        let path = path.to_string();
        self.send(|reply| Cmd::CompletedFor { path, reply }).await
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
        -- One index per intake order, so `claim_next` never sorts the table.
        CREATE INDEX IF NOT EXISTS files_state_size ON files(state, size DESC);
        "#,
    )?;
    migrate(conn)?;
    Ok(())
}

/// Adds columns introduced after a ledger was first created.
///
/// Additive rather than a rebuild: the conversion history is the only record of
/// what was replaced with what, and is not recoverable from a rescan.
fn migrate(conn: &Connection) -> Result<()> {
    let existing: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('files')")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;

    let wanted = [
        // Where the replacement lives, and what it cost.
        ("output_path", "TEXT"),
        ("output_size", "INTEGER"),
        ("recipe", "TEXT"),
        // Which guarantee actually held: byte-exact or content-exact.
        ("fidelity", "TEXT"),
        // When the original was moved to the trash. Until the trash is emptied
        // those bytes still count against the quota.
        ("trashed_at", "TEXT"),
        // Set once the space has genuinely been reclaimed.
        ("reclaimed", "INTEGER NOT NULL DEFAULT 0"),
        // What `Order::Savings` sorts on. A ledger predating this column reads as
        // zero everywhere, which degrades that order to the size tiebreaker until
        // the next scan rather than producing a wrong one.
        ("projected_saving", "INTEGER NOT NULL DEFAULT 0"),
    ];
    for (name, decl) in wanted {
        if !existing.iter().any(|c| c == name) {
            conn.execute(&format!("ALTER TABLE files ADD COLUMN {name} {decl}"), [])?;
        }
    }
    // Created here rather than in `init_schema` because it needs a migrated column.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS files_state_saving
         ON files(state, projected_saving DESC, size DESC);",
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
            Cmd::ListByState { state, reply } => {
                let _ = reply.send(do_list_by_state(&conn, state));
            }
            Cmd::ClaimNext {
                prefixes,
                order,
                reply,
            } => {
                let _ = reply.send(do_claim_next(&mut conn, &prefixes, order));
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
            Cmd::ReopenSkipped { reasons, reply } => {
                let _ = reply.send(do_reopen_skipped(&conn, &reasons));
            }
            Cmd::RecordConversion { record, reply } => {
                let _ = reply.send(do_record_conversion(&conn, &record));
            }
            Cmd::PendingReclaim { reply } => {
                let _ = reply.send(do_pending_reclaim(&conn));
            }
            Cmd::MarkReclaimed { reply } => {
                let _ = reply.send(do_mark_reclaimed(&conn));
            }
            Cmd::Savings { reply } => {
                let _ = reply.send(do_savings(&conn));
            }
            Cmd::Completed { limit, reply } => {
                let _ = reply.send(do_completed(&conn, limit));
            }
            Cmd::CompletedFor { path, reply } => {
                let _ = reply.send(do_completed_for(&conn, &path));
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
            INSERT INTO files (path, size, mod_time, blake3, state, projected_saving)
            VALUES (?1, ?2, ?3, ?4, 'pending', ?5)
            ON CONFLICT(path) DO UPDATE SET
                size     = excluded.size,
                mod_time = excluded.mod_time,
                blake3   = excluded.blake3,
                seen_at  = datetime('now'),
                projected_saving = excluded.projected_saving,
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
                crate::policy::projected_saving(&entry.path, entry.size) as i64,
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

fn do_list_by_state(conn: &Connection, state: State) -> Result<Vec<FileRow>> {
    let mut stmt = conn.prepare(
        "SELECT path, size, mod_time, blake3, state, skip_reason
         FROM files WHERE state = ?1 ORDER BY size DESC, path",
    )?;
    let rows = stmt.query_map(params![state.as_str()], row_to_file)?;
    rows.map(|r| r.map_err(anyhow::Error::from).and_then(build_row))
        .collect()
}

fn do_claim_next(
    conn: &mut Connection,
    prefixes: &[String],
    order: Order,
) -> Result<Option<FileRow>> {
    let tx = conn.transaction()?;
    let (filter, params) = prefix_filter(prefixes);
    let sql = format!(
        "SELECT path, size, mod_time, blake3, state, skip_reason
         FROM files WHERE state = 'pending'{filter} ORDER BY {} LIMIT 1",
        order_clause(order)
    );
    let raw = tx
        .query_row(
            &sql,
            rusqlite::params_from_iter(params.iter()),
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

/// The `ORDER BY` for one intake order.
///
/// Every variant ends in `path` so the sequence is total: two files of identical
/// size must not swap places between runs, or `--limit` would cover a different
/// set each time.
fn order_clause(order: Order) -> &'static str {
    match order {
        // Projected saving first, but a tie falls back to size so a ledger that
        // predates the column (all zeroes) still orders sensibly.
        Order::Savings => "projected_saving DESC, size DESC, path",
        Order::Size => "size DESC, path",
        Order::Path => "path",
    }
}

/// Builds a `path` restriction matching any of `prefixes`, plus its parameters.
///
/// Prefixes are matched with `LIKE 'prefix/%'` alongside an exact match, so a
/// prefix lines up with a path segment and `Photos` cannot pick up
/// `PhotosBackup`. LIKE wildcards inside a prefix are escaped, since folder names
/// may legitimately contain `%` or `_`.
fn prefix_filter(prefixes: &[String]) -> (String, Vec<String>) {
    if prefixes.is_empty() {
        return (String::new(), Vec::new());
    }
    let mut clauses = Vec::new();
    let mut params = Vec::new();
    for prefix in prefixes {
        clauses.push("(path = ? OR path LIKE ? ESCAPE '\\')".to_string());
        params.push(prefix.clone());
        params.push(format!("{}/%", escape_like(prefix)));
    }
    (format!(" AND ({})", clauses.join(" OR ")), params)
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
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

fn do_reopen_skipped(conn: &Connection, reasons: &[String]) -> Result<usize> {
    if reasons.is_empty() {
        return Ok(0);
    }
    // rusqlite has no list binding, so build one placeholder per reason rather
    // than interpolating values into the statement.
    let placeholders = std::iter::repeat_n("?", reasons.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "UPDATE files SET state = 'pending', skip_reason = NULL
         WHERE state = 'skipped' AND skip_reason IN ({placeholders})"
    );
    let params = rusqlite::params_from_iter(reasons.iter());
    Ok(conn.execute(&sql, params)?)
}

fn do_record_conversion(conn: &Connection, record: &Conversion) -> Result<()> {
    let changed = conn.execute(
        "UPDATE files SET
            state       = 'done',
            skip_reason = NULL,
            output_path = ?2,
            output_size = ?3,
            recipe      = ?4,
            fidelity    = ?5,
            trashed_at  = datetime('now'),
            reclaimed   = 0
         WHERE path = ?1",
        params![
            record.path,
            record.output_path,
            record.output_size as i64,
            record.recipe,
            record.fidelity,
        ],
    )?;
    if changed == 0 {
        bail!("no ledger row for `{}`", record.path);
    }
    Ok(())
}

fn do_pending_reclaim(conn: &Connection) -> Result<Reclaim> {
    let (files, bytes): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(size), 0) FROM files
         WHERE state = 'done' AND trashed_at IS NOT NULL AND reclaimed = 0",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(Reclaim {
        files: u64::try_from(files).unwrap_or(0),
        bytes: u64::try_from(bytes).unwrap_or(0),
    })
}

fn do_mark_reclaimed(conn: &Connection) -> Result<u64> {
    let pending = do_pending_reclaim(conn)?;
    conn.execute(
        "UPDATE files SET reclaimed = 1
         WHERE state = 'done' AND trashed_at IS NOT NULL AND reclaimed = 0",
        [],
    )?;
    Ok(pending.bytes)
}

fn do_savings(conn: &Connection) -> Result<Savings> {
    let (files, original, output): (i64, i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(size), 0), COALESCE(SUM(output_size), 0)
         FROM files WHERE state = 'done' AND output_size IS NOT NULL",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    Ok(Savings {
        files: u64::try_from(files).unwrap_or(0),
        original_bytes: u64::try_from(original).unwrap_or(0),
        output_bytes: u64::try_from(output).unwrap_or(0),
    })
}

const COMPLETED_COLUMNS: &str =
    "path, output_path, size, output_size, blake3, recipe, fidelity";

fn row_to_completed(row: &rusqlite::Row<'_>) -> rusqlite::Result<Completed> {
    Ok(Completed {
        path: row.get(0)?,
        output_path: row.get(1)?,
        original_size: u64::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
        output_size: u64::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
        original_blake3: row.get(4)?,
        recipe: row.get(5)?,
        fidelity: row.get(6)?,
    })
}

fn do_completed(conn: &Connection, limit: Option<usize>) -> Result<Vec<Completed>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM files
         WHERE state = 'done' AND output_path IS NOT NULL
         ORDER BY size DESC, path LIMIT ?1",
        COLUMNS = COMPLETED_COLUMNS
    );
    let cap = limit.map_or(-1i64, |n| i64::try_from(n).unwrap_or(i64::MAX));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![cap], row_to_completed)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn do_completed_for(conn: &Connection, path: &str) -> Result<Option<Completed>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM files
         WHERE state = 'done' AND output_path IS NOT NULL AND (path = ?1 OR output_path = ?1)",
        COLUMNS = COMPLETED_COLUMNS
    );
    Ok(conn
        .query_row(&sql, params![path], row_to_completed)
        .optional()?)
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
        assert_eq!(ledger.claim_next(&[], Order::Size).await.unwrap().unwrap().path, "big");
        assert_eq!(ledger.claim_next(&[], Order::Size).await.unwrap().unwrap().path, "mid");
        assert_eq!(ledger.claim_next(&[], Order::Size).await.unwrap().unwrap().path, "small");
        assert!(ledger.claim_next(&[], Order::Size).await.unwrap().is_none());
    }

    /// `Order::Savings` is the default, and it is not the same as size order: a
    /// large JPEG projects a 20% saving where a smaller PNG projects 35%, so the
    /// smaller file can legitimately come first. This is the behaviour `--order`
    /// was always documented to have and never actually had.
    #[tokio::test]
    async fn savings_order_is_not_size_order() {
        let ledger = Ledger::open_in_memory().unwrap();
        // jpeg -> jxl keeps 80% (saves 20%); png -> jxl keeps 65% (saves 35%).
        // 100 * 0.20 = 20 against 70 * 0.35 = 24.5, so the PNG wins on savings
        // while the JPEG wins on size.
        ledger
            .upsert(vec![entry("a.jpg", 100_000), entry("b.png", 70_000)])
            .await
            .unwrap();

        let by_size = ledger.claim_next(&[], Order::Size).await.unwrap().unwrap();
        assert_eq!(by_size.path, "a.jpg", "size order takes the bigger file");

        let ledger = Ledger::open_in_memory().unwrap();
        ledger
            .upsert(vec![entry("a.jpg", 100_000), entry("b.png", 70_000)])
            .await
            .unwrap();
        let by_savings = ledger
            .claim_next(&[], Order::Savings)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(by_savings.path, "b.png", "savings order takes the better ratio");
    }

    #[tokio::test]
    async fn path_order_is_lexicographic() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger
            .upsert(vec![entry("c.jpg", 300), entry("a.jpg", 100), entry("b.jpg", 200)])
            .await
            .unwrap();
        for expected in ["a.jpg", "b.jpg", "c.jpg"] {
            let row = ledger.claim_next(&[], Order::Path).await.unwrap().unwrap();
            assert_eq!(row.path, expected);
        }
    }

    /// A file nothing can convert projects no saving, so it sorts last under
    /// `Savings` rather than jumping the queue on size alone.
    #[tokio::test]
    async fn unconvertible_files_sort_last_by_savings() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger
            .upsert(vec![entry("huge.bin", 9_000_000), entry("small.jpg", 50_000)])
            .await
            .unwrap();
        let first = ledger
            .claim_next(&[], Order::Savings)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.path, "small.jpg");
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
                while let Some(row) = ledger.claim_next(&[], Order::Size).await.unwrap() {
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

    /// Scoping is a safety boundary: a run limited to one folder must not be able
    /// to pick up a file in a sibling folder, even momentarily.
    #[tokio::test]
    async fn claims_are_confined_to_the_given_prefixes() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger
            .upsert(vec![
                entry("photos/a.jpg", 100),
                entry("photos/2019/b.jpg", 90),
                entry("audio/tone.wav", 80),
                entry("videos/clip.mp4", 70),
            ])
            .await
            .unwrap();

        let scope = vec!["photos".to_string()];
        let mut claimed = Vec::new();
        while let Some(row) = ledger.claim_next(&scope, Order::Size).await.unwrap() {
            claimed.push(row.path);
        }
        claimed.sort();
        assert_eq!(claimed, vec!["photos/2019/b.jpg", "photos/a.jpg"]);

        // Everything outside the scope is still available afterwards.
        assert_eq!(ledger.counts().await.unwrap().pending, 2);
    }

    /// A prefix must align with a path segment, or `photos` would sweep up
    /// `photos-backup` as well.
    #[tokio::test]
    async fn a_prefix_does_not_match_a_longer_sibling_directory() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger
            .upsert(vec![entry("photos/a.jpg", 10), entry("photos-backup/b.jpg", 20)])
            .await
            .unwrap();
        let scope = vec!["photos".to_string()];
        assert_eq!(
            ledger.claim_next(&scope, Order::Size).await.unwrap().unwrap().path,
            "photos/a.jpg"
        );
        assert!(ledger.claim_next(&scope, Order::Size).await.unwrap().is_none());
    }

    /// Folder names may contain LIKE wildcards; they must be matched literally.
    #[tokio::test]
    async fn like_wildcards_in_a_folder_name_are_escaped() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger
            .upsert(vec![entry("100%_done/a.jpg", 10), entry("100Xdone/b.jpg", 20)])
            .await
            .unwrap();
        let scope = vec!["100%_done".to_string()];
        assert_eq!(
            ledger.claim_next(&scope, Order::Size).await.unwrap().unwrap().path,
            "100%_done/a.jpg"
        );
        assert!(ledger.claim_next(&scope, Order::Size).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn several_prefixes_are_unioned_in_the_claim() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger
            .upsert(vec![
                entry("photos/a.jpg", 30),
                entry("camera/b.jpg", 20),
                entry("docs/c.pdf", 10),
            ])
            .await
            .unwrap();
        let scope = vec!["photos".to_string(), "camera".to_string()];
        let mut claimed = Vec::new();
        while let Some(row) = ledger.claim_next(&scope, Order::Size).await.unwrap() {
            claimed.push(row.path);
        }
        assert_eq!(claimed, vec!["photos/a.jpg", "camera/b.jpg"]);
    }

    #[tokio::test]
    async fn crashed_claims_are_recovered() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger
            .upsert(vec![entry("a", 1), entry("b", 2)])
            .await
            .unwrap();
        ledger.claim_next(&[], Order::Size).await.unwrap();
        assert_eq!(ledger.counts().await.unwrap().claimed, 1);

        assert_eq!(ledger.recover_claimed().await.unwrap(), 1);
        let counts = ledger.counts().await.unwrap();
        assert_eq!(counts.claimed, 0);
        assert_eq!(counts.pending, 2);
    }

    /// Enabling the video tier must reconsider the files that were skipped only
    /// because it was off, while leaving genuinely unsuitable files alone.
    #[tokio::test]
    async fn settings_dependent_skips_can_be_reopened() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger
            .upsert(vec![entry("gated", 1), entry("hdr", 2), entry("big", 3)])
            .await
            .unwrap();
        ledger
            .set_state("gated", State::Skipped, Some("video_tier_disabled".into()))
            .await
            .unwrap();
        ledger
            .set_state("hdr", State::Skipped, Some("video_hdr".into()))
            .await
            .unwrap();
        ledger
            .set_state("big", State::Skipped, Some("too_large_for_budget".into()))
            .await
            .unwrap();

        let reopened = ledger
            .reopen_skipped(&["video_tier_disabled", "too_large_for_budget"])
            .await
            .unwrap();
        assert_eq!(reopened, 2);

        assert_eq!(ledger.get("gated").await.unwrap().unwrap().state, State::Pending);
        assert_eq!(ledger.get("big").await.unwrap().unwrap().state, State::Pending);
        // An HDR file is unsuitable no matter how the run is configured.
        assert_eq!(ledger.get("hdr").await.unwrap().unwrap().state, State::Skipped);
    }

    #[tokio::test]
    async fn reopening_clears_the_stale_reason() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger.upsert(vec![entry("a", 1)]).await.unwrap();
        ledger
            .set_state("a", State::Skipped, Some("video_tier_disabled".into()))
            .await
            .unwrap();
        ledger.reopen_skipped(&["video_tier_disabled"]).await.unwrap();
        assert_eq!(ledger.get("a").await.unwrap().unwrap().skip_reason, None);
    }

    #[tokio::test]
    async fn reopening_nothing_is_harmless() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger.upsert(vec![entry("a", 1)]).await.unwrap();
        ledger.set_state("a", State::Done, None).await.unwrap();
        assert_eq!(ledger.reopen_skipped(&[]).await.unwrap(), 0);
        assert_eq!(ledger.reopen_skipped(&["video_hdr"]).await.unwrap(), 0);
        assert_eq!(ledger.get("a").await.unwrap().unwrap().state, State::Done);
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

    fn conversion(path: &str, output_size: u64) -> Conversion {
        Conversion {
            path: path.to_string(),
            output_path: format!("{path}.jxl"),
            output_size,
            recipe: "jxl-from-jpeg".into(),
            fidelity: "byte-exact".into(),
        }
    }

    #[tokio::test]
    async fn recording_a_conversion_marks_it_done_and_stores_the_result() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger.upsert(vec![entry("a.jpg", 1000)]).await.unwrap();
        ledger.record_conversion(conversion("a.jpg", 800)).await.unwrap();

        let row = ledger.get("a.jpg").await.unwrap().unwrap();
        assert_eq!(row.state, State::Done);

        let savings = ledger.savings().await.unwrap();
        assert_eq!(savings.files, 1);
        assert_eq!(savings.original_bytes, 1000);
        assert_eq!(savings.output_bytes, 800);
        assert_eq!(savings.logical_bytes(), 200);
    }

    /// The gap between "we converted things" and "the drive is smaller" is the
    /// trash, and reporting has to be able to show it.
    #[tokio::test]
    async fn originals_stay_pending_reclaim_until_the_trash_is_emptied() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger
            .upsert(vec![entry("a.jpg", 1000), entry("b.jpg", 500)])
            .await
            .unwrap();
        ledger.record_conversion(conversion("a.jpg", 800)).await.unwrap();
        ledger.record_conversion(conversion("b.jpg", 400)).await.unwrap();

        let pending = ledger.pending_reclaim().await.unwrap();
        assert_eq!(pending.files, 2);
        // The originals' bytes, not the savings: that is what the trash holds.
        assert_eq!(pending.bytes, 1500);

        assert_eq!(ledger.mark_reclaimed().await.unwrap(), 1500);
        assert_eq!(ledger.pending_reclaim().await.unwrap().bytes, 0);
    }

    #[tokio::test]
    async fn reclaiming_twice_does_not_double_count() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger.upsert(vec![entry("a.jpg", 1000)]).await.unwrap();
        ledger.record_conversion(conversion("a.jpg", 800)).await.unwrap();
        assert_eq!(ledger.mark_reclaimed().await.unwrap(), 1000);
        assert_eq!(ledger.mark_reclaimed().await.unwrap(), 0);
    }

    /// Savings survive a reclaim; only the pending figure changes.
    #[tokio::test]
    async fn savings_persist_after_reclaiming() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger.upsert(vec![entry("a.jpg", 1000)]).await.unwrap();
        ledger.record_conversion(conversion("a.jpg", 800)).await.unwrap();
        ledger.mark_reclaimed().await.unwrap();
        assert_eq!(ledger.savings().await.unwrap().logical_bytes(), 200);
    }

    #[tokio::test]
    async fn recording_a_conversion_for_an_unknown_file_errors() {
        let ledger = Ledger::open_in_memory().unwrap();
        assert!(ledger.record_conversion(conversion("ghost", 1)).await.is_err());
    }

    /// A ledger written before the result columns existed must keep working.
    #[tokio::test]
    async fn an_older_ledger_is_migrated_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.sqlite");
        {
            // The original schema, without any of the result columns.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE files (
                    path TEXT PRIMARY KEY, size INTEGER NOT NULL, mod_time TEXT,
                    blake3 TEXT, state TEXT NOT NULL DEFAULT 'pending',
                    skip_reason TEXT, seen_at TEXT NOT NULL DEFAULT (datetime('now')));
                 INSERT INTO files (path, size) VALUES ('legacy.jpg', 4242);",
            )
            .unwrap();
        }

        let ledger = Ledger::open(&path).unwrap();
        assert_eq!(ledger.get("legacy.jpg").await.unwrap().unwrap().size, 4242);
        ledger
            .record_conversion(conversion("legacy.jpg", 3000))
            .await
            .unwrap();
        assert_eq!(ledger.savings().await.unwrap().logical_bytes(), 1242);
    }

    /// A row that predates `projected_saving` reads as zero, so savings order must
    /// still hand it out rather than skipping it or failing the query.
    #[tokio::test]
    async fn a_pre_savings_ledger_still_claims_in_savings_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE files (
                    path TEXT PRIMARY KEY, size INTEGER NOT NULL, mod_time TEXT,
                    blake3 TEXT, state TEXT NOT NULL DEFAULT 'pending',
                    skip_reason TEXT, seen_at TEXT NOT NULL DEFAULT (datetime('now')));
                 INSERT INTO files (path, size) VALUES ('old_small.jpg', 10_000);
                 INSERT INTO files (path, size) VALUES ('old_big.jpg', 900_000);",
            )
            .unwrap();
        }

        let ledger = Ledger::open(&path).unwrap();
        // Both migrated rows carry saving 0, so they fall through to the size
        // tiebreaker; a freshly scanned row outranks them on its real estimate.
        ledger.upsert(vec![entry("fresh.jpg", 500_000)]).await.unwrap();

        let order: Vec<String> = {
            let mut seen = Vec::new();
            while let Some(row) = ledger.claim_next(&[], Order::Savings).await.unwrap() {
                seen.push(row.path);
            }
            seen
        };
        assert_eq!(order, ["fresh.jpg", "old_big.jpg", "old_small.jpg"]);
    }

    #[tokio::test]
    async fn completed_conversions_can_be_listed_and_looked_up() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger
            .upsert(vec![entry("a.jpg", 1000), entry("b.jpg", 20), entry("c.jpg", 5)])
            .await
            .unwrap();
        ledger.record_conversion(conversion("a.jpg", 800)).await.unwrap();
        ledger.record_conversion(conversion("b.jpg", 10)).await.unwrap();

        let all = ledger.completed(None).await.unwrap();
        assert_eq!(all.len(), 2, "only converted files are listed");
        assert_eq!(all[0].path, "a.jpg", "largest original first");
        assert_eq!(all[0].output_path, "a.jpg.jxl");
        assert_eq!(all[0].original_size, 1000);
        assert_eq!(all[0].output_size, 800);
        assert_eq!(all[0].original_blake3.as_deref(), Some("hash-of-a.jpg"));

        assert_eq!(ledger.completed(Some(1)).await.unwrap().len(), 1);
    }

    /// A restore may be asked for by either name, since the original no longer
    /// exists and the converted file is what the user can see.
    #[tokio::test]
    async fn a_conversion_is_findable_by_either_path() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger.upsert(vec![entry("a.jpg", 100)]).await.unwrap();
        ledger.record_conversion(conversion("a.jpg", 80)).await.unwrap();

        assert!(ledger.completed_for("a.jpg").await.unwrap().is_some());
        assert!(ledger.completed_for("a.jpg.jxl").await.unwrap().is_some());
        assert!(ledger.completed_for("nothing").await.unwrap().is_none());
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
