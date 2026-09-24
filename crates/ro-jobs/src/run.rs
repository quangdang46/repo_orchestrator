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

/// Fetch a run record by id, or `None` if it doesn't exist.
pub fn get_run(conn: &Connection, run_id: &str) -> Result<Option<RunRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, command, started_at, ended_at, exit_code, args_json, user, host
             FROM runs WHERE id = ?1",
        )
        .context("preparing run lookup")?;
    let mut rows = stmt
        .query_map(params![run_id], row_to_run)
        .context("querying run by id")?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

/// List the most recent `limit` runs, newest first.
pub fn recent_runs(conn: &Connection, limit: usize) -> Result<Vec<RunRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, command, started_at, ended_at, exit_code, args_json, user, host
             FROM runs ORDER BY started_at DESC LIMIT ?1",
        )
        .context("preparing recent runs query")?;
    let rows = stmt
        .query_map(params![limit as i64], row_to_run)
        .context("querying recent runs")?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// List runs that are still open (no `ended_at`).
pub fn open_runs(conn: &Connection) -> Result<Vec<RunRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, command, started_at, ended_at, exit_code, args_json, user, host
             FROM runs WHERE ended_at IS NULL ORDER BY started_at ASC",
        )
        .context("preparing open runs query")?;
    let rows = stmt
        .query_map([], row_to_run)
        .context("querying open runs")?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn row_to_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunRecord> {
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
    use crate::events::{EventLevel, append_event, events_for_run};
    use ro_state::open_memory;

    fn db() -> Connection {
        open_memory().expect("memory db")
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
        let stored = get_run(&c, &r.id).unwrap().unwrap();
        assert_eq!(stored, r);
    }

    #[test]
    fn finalize_sets_exit_code_and_duration() {
        let c = db();
        let r = open_run(&c, "commit", &[]).unwrap();
        finalize_run(&c, &r.id, 0).unwrap();
        let stored = get_run(&c, &r.id).unwrap().unwrap();
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
    fn recent_runs_returns_newest_first() {
        let c = db();
        let r1 = open_run(&c, "sync", &[]).unwrap();
        // Bump started_at on second run to guarantee ordering even when
        // the test runs in <1s.
        let r2 = open_run(&c, "commit", &[]).unwrap();
        c.execute(
            "UPDATE runs SET started_at = ?1 WHERE id = ?2",
            rusqlite::params![r2.started_at + 5, r2.id],
        )
        .unwrap();
        let recent = recent_runs(&c, 10).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].id, r2.id);
        assert_eq!(recent[1].id, r1.id);
    }

    #[test]
    fn open_runs_filters_finished() {
        let c = db();
        let r1 = open_run(&c, "sync", &[]).unwrap();
        let r2 = open_run(&c, "commit", &[]).unwrap();
        finalize_run(&c, &r1.id, 0).unwrap();
        let open = open_runs(&c).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, r2.id);
    }

    #[test]
    fn run_args_serialise_to_json() {
        let c = db();
        let r = open_run(&c, "sync", &["--branch".into(), "main".into()]).unwrap();
        let parsed: Vec<String> = serde_json::from_str(&r.args_json).unwrap();
        assert_eq!(parsed, vec!["--branch", "main"]);
    }

    #[test]
    fn events_attach_to_run() {
        let c = db();
        let r = open_run(&c, "sync", &[]).unwrap();
        append_event(&c, &r.id, EventLevel::Info, "starting fetch", None).unwrap();
        append_event(
            &c,
            &r.id,
            EventLevel::Warn,
            "remote slow",
            Some(&serde_json::json!({"latency_ms": 1234})),
        )
        .unwrap();
        let events = events_for_run(&c, &r.id).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].level, EventLevel::Info);
        assert_eq!(events[0].message, "starting fetch");
        assert!(events[0].data_json.is_none());
        assert_eq!(events[1].level, EventLevel::Warn);
        assert!(events[1].data_json.is_some());
        assert!(events[1].data_json.as_ref().unwrap().contains("1234"));
    }

    #[test]
    fn event_level_parses_aliases() {
        assert_eq!("warning".parse::<EventLevel>().unwrap(), EventLevel::Warn);
        assert_eq!("ERR".parse::<EventLevel>().unwrap(), EventLevel::Error);
        assert!("nope".parse::<EventLevel>().is_err());
    }
}
