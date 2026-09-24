//! Run event log management.
//!
//! Append-only events emitted by an in-flight run. The event stream
//! drives both the user-facing timeline and the audit log.
//!
//! Fields per PLAN.md §13: run_id, ts, level, message, data_json.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Severity of a run event. Maps directly to log levels surfaced by the
/// CLI text/json output paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl fmt::Display for EventLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EventLevel::Debug => f.write_str("debug"),
            EventLevel::Info => f.write_str("info"),
            EventLevel::Warn => f.write_str("warn"),
            EventLevel::Error => f.write_str("error"),
        }
    }
}

impl FromStr for EventLevel {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "debug" => Ok(EventLevel::Debug),
            "info" => Ok(EventLevel::Info),
            "warn" | "warning" => Ok(EventLevel::Warn),
            "error" | "err" => Ok(EventLevel::Error),
            other => anyhow::bail!("unknown event level: {other}"),
        }
    }
}

/// A single row from the `run_events` table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunEvent {
    pub id: i64,
    pub run_id: String,
    pub ts: i64,
    pub level: EventLevel,
    pub message: String,
    /// Optional JSON-encoded payload.
    pub data_json: Option<String>,
}

/// Append a new event to a run. The caller-supplied `data` is
/// serialised to JSON if `Some`.
pub fn append_event(
    conn: &Connection,
    run_id: &str,
    level: EventLevel,
    message: &str,
    data: Option<&serde_json::Value>,
) -> Result<i64> {
    let ts = now_secs();
    let data_json = match data {
        Some(v) => Some(serde_json::to_string(v).context("serialising event data")?),
        None => None,
    };
    conn.execute(
        "INSERT INTO run_events (run_id, ts, level, message, data_json)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![run_id, ts, level.to_string(), message, data_json],
    )
    .context("inserting run event")?;
    Ok(conn.last_insert_rowid())
}

/// Fetch all events for a run, oldest first.
pub fn events_for_run(conn: &Connection, run_id: &str) -> Result<Vec<RunEvent>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, run_id, ts, level, message, data_json
             FROM run_events WHERE run_id = ?1 ORDER BY ts ASC, id ASC",
        )
        .context("preparing events_for_run query")?;
    let rows = stmt
        .query_map(params![run_id], row_to_event)
        .context("querying run events")?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// List all events for a run, ordered by timestamp ascending.
pub fn list_events(conn: &Connection, run_id: &str) -> Result<Vec<RunEvent>> {
    events_for_run(conn, run_id)
}

/// Fetch the most recent `limit` events across all runs, newest first.
pub fn recent_events(conn: &Connection, limit: usize) -> Result<Vec<RunEvent>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, run_id, ts, level, message, data_json
             FROM run_events ORDER BY ts DESC, id DESC LIMIT ?1",
        )
        .context("preparing recent_events query")?;
    let rows = stmt
        .query_map(params![limit as i64], row_to_event)
        .context("querying recent events")?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunEvent> {
    let level: String = row.get(3)?;
    let level = level.parse::<EventLevel>().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(e.to_string())),
        )
    })?;
    Ok(RunEvent {
        id: row.get(0)?,
        run_id: row.get(1)?,
        ts: row.get(2)?,
        level,
        message: row.get(4)?,
        data_json: row.get(5)?,
    })
}

fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ro_state::open_memory;

    fn db() -> Connection {
        open_memory().expect("memory db")
    }

    fn make_run(conn: &Connection) -> String {
        crate::run::open_run(conn, "test", &[]).unwrap().id
    }

    #[test]
    fn list_events_returns_events_oldest_first() {
        let c = db();
        let run_id = make_run(&c);
        append_event(&c, &run_id, EventLevel::Info, "first", None).unwrap();
        append_event(&c, &run_id, EventLevel::Warn, "second", None).unwrap();
        append_event(&c, &run_id, EventLevel::Error, "third", None).unwrap();
        let events = list_events(&c, &run_id).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].message, "first");
        assert_eq!(events[1].message, "second");
        assert_eq!(events[2].message, "third");
    }

    #[test]
    fn list_events_empty_for_unknown_run() {
        let c = db();
        let events = list_events(&c, "no-such-run").unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn recent_events_returns_newest_first_across_runs() {
        let c = db();
        let r1 = make_run(&c);
        let r2 = make_run(&c);
        append_event(&c, &r1, EventLevel::Info, "r1-event", None).unwrap();
        append_event(&c, &r2, EventLevel::Info, "r2-event", None).unwrap();
        let events = recent_events(&c, 10).unwrap();
        assert_eq!(events.len(), 2);
        // r2-event was inserted last, so it should be first (newest)
        assert_eq!(events[0].message, "r2-event");
        assert_eq!(events[1].message, "r1-event");
    }

    #[test]
    fn recent_events_respects_limit() {
        let c = db();
        let run_id = make_run(&c);
        for i in 0..5 {
            append_event(&c, &run_id, EventLevel::Info, &format!("evt-{i}"), None).unwrap();
        }
        let events = recent_events(&c, 3).unwrap();
        assert_eq!(events.len(), 3);
        // newest first: evt-4, evt-3, evt-2
        assert_eq!(events[0].message, "evt-4");
        assert_eq!(events[1].message, "evt-3");
        assert_eq!(events[2].message, "evt-2");
    }
}
