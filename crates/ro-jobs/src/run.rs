//! Run record management.
//!
//! Every command that mutates state creates a run record. The record
//! is opened at the start of the command, finalised when the command
//! exits, and surfaces through the audit / timeline / context views.
//!
//! Fields per PLAN.md §13: id, command, started_at, ended_at, exit_code,
//! args_json, user, host.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A run record as stored in the `runs` table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunRecord {
    /// UUIDv4 string identifying this run.
    pub id: String,
    /// The top-level command, e.g. `sync`, `commit`, `pr`.
    pub command: String,
    /// Unix epoch seconds when the run was opened.
    pub started_at: i64,
    /// Unix epoch seconds when the run was finalised. `None` while in flight.
    pub ended_at: Option<i64>,
    /// Process exit code. `None` while in flight.
    pub exit_code: Option<i32>,
    /// JSON-encoded argv (sans secrets).
    pub args_json: String,
    /// Operating-system user, redacted to `Some(name)` if discoverable.
    pub user: Option<String>,
    /// Hostname, if discoverable.
    pub host: Option<String>,
}

impl RunRecord {
    /// Returns the run duration in seconds, or `None` if the run is
    /// still open.
    pub fn duration_secs(&self) -> Option<i64> {
        Some(self.ended_at? - self.started_at)
    }

    /// Returns true if the run has been finalised.
    pub fn is_finished(&self) -> bool {
        self.ended_at.is_some()
    }
}

/// Open a new run record. Returns the freshly generated UUID.
///
/// `args` is serialised as JSON and stored verbatim — callers are
/// responsible for redacting secrets before passing the slice.
pub fn open_run(conn: &Connection, command: &str, args: &[String]) -> Result<RunRecord> {
    let id = Uuid::new_v4().to_string();
    let started_at = now_secs();
    let args_json = serde_json::to_string(args).context("serialising run args to JSON")?;
    let user = current_user();
    let host = current_host();

    conn.execute(
        "INSERT INTO runs (id, command, started_at, ended_at, exit_code, args_json, user, host)
         VALUES (?1, ?2, ?3, NULL, NULL, ?4, ?5, ?6)",
        params![id, command, started_at, args_json, user, host],
    )
    .context("inserting run record")?;

    Ok(RunRecord {
        id,
        command: command.to_string(),
        started_at,
        ended_at: None,
        exit_code: None,
        args_json,
        user,
        host,
    })
}

/// Finalise a run with an exit code. Idempotent: re-finalising the same
/// run overwrites `ended_at` / `exit_code`, which is the right behaviour
/// for retry-from-checkpoint flows.
pub fn finalize_run(conn: &Connection, run_id: &str, exit_code: i32) -> Result<()> {
    let ended_at = now_secs();
    let n = conn
        .execute(
            "UPDATE runs SET ended_at = ?1, exit_code = ?2 WHERE id = ?3",
            params![ended_at, exit_code, run_id],
        )
        .context("finalising run record")?;
    if n == 0 {
        anyhow::bail!("run id {run_id} not found");
    }
    Ok(())
}

fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn current_user() -> Option<String> {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
}

fn current_host() -> Option<String> {
    // Try `HOSTNAME` env var first (set by most shells), then
    // `COMPUTERNAME` (Windows), fall back to /etc/hostname on Unix.
    if let Ok(h) = std::env::var("HOSTNAME") {
        if !h.is_empty() {
            return Some(h);
        }
    }
    #[cfg(windows)]
    if let Ok(h) = std::env::var("COMPUTERNAME") {
        if !h.is_empty() {
            return Some(h);
        }
    }
    #[cfg(unix)]
    {
        if let Ok(h) = std::fs::read_to_string("/etc/hostname") {
            let h = h.trim().to_string();
            if !h.is_empty() {
                return Some(h);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use ro_state::open_memory;

    fn db() -> Connection {
        open_memory().expect("memory db")
    }

    /// Read a run row back directly.
    ///
    /// `get_run` was removed along with the rest of the crate's read side —
    /// it had no caller outside this file — so the tests that assert on
    /// persisted state query the table themselves. That is the point of a
    /// test: it should not depend on the same function it is checking.
    fn stored_run(c: &Connection, id: &str) -> RunRecord {
        c.query_row(
            "SELECT id, command, started_at, ended_at, exit_code, args_json, user, host
             FROM runs WHERE id = ?1",
            [id],
            |row| {
                Ok(RunRecord {
                    id: row.get(0)?,
                    command: row.get(1)?,
                    started_at: row.get(2)?,
                    ended_at: row.get(3)?,
                    exit_code: row.get(4)?,
                    args_json: row.get(5)?,
                    user: row.get(6)?,
                    host: row.get(7)?,
                })
            },
        )
        .expect("run row should exist")
    }

    #[test]
    fn open_run_writes_record() {
        let c = db();
        let r = open_run(&c, "sync", &["--all".into(), "--dry-run".into()]).unwrap();
        assert!(!r.id.is_empty());
        assert_eq!(r.command, "sync");
        assert!(r.ended_at.is_none());
        assert_eq!(r.exit_code, None);
        assert!(!r.is_finished());
        assert_eq!(stored_run(&c, &r.id), r);
    }

    #[test]
    fn finalize_sets_exit_code_and_duration() {
        let c = db();
        let r = open_run(&c, "commit", &[]).unwrap();
        finalize_run(&c, &r.id, 0).unwrap();
        let stored = stored_run(&c, &r.id);
        assert_eq!(stored.exit_code, Some(0));
        assert!(stored.ended_at.is_some());
        assert!(stored.is_finished());
        assert!(stored.duration_secs().unwrap() >= 0);
    }

    #[test]
    fn finalize_unknown_run_errors() {
        let c = db();
        let err = finalize_run(&c, "no-such-run", 1).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn run_args_serialise_to_json() {
        let c = db();
        let r = open_run(&c, "sync", &["--branch".into(), "main".into()]).unwrap();
        let parsed: Vec<String> = serde_json::from_str(&r.args_json).unwrap();
        assert_eq!(parsed, vec!["--branch", "main"]);
    }
}
