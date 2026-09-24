//! Typed NDJSON event stream for multi-repo runs.
//!
//! Each event is a single-line JSON object, newline-delimited, suitable
//! for streaming to stdout or piping to `jq`, `ru`, or any NDJSON consumer.
//!
//! Schema loosely compatible with `ru` NDJSON output; each line is
//! self-describing.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Every event written to the NDJSON stream carries this envelope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NdjsonEvent {
    /// Event kind — machine-actionable discriminator.
    #[serde(rename = "event")]
    pub kind: String,
    /// ISO-8601 RFC 3339 timestamp of the event. Added by the writer if
    /// omitted by the caller.
    pub ts: Option<String>,
    /// Flat key/value payload. Keys are domain-specific (repo, plan_id,
    /// status, etc.). Values are plain JSON scalars so consumers can
    /// index without deep inspection.
    #[serde(flatten)]
    pub payload: HashMap<String, serde_json::Value>,
}

/// Convenience constructors for the eight canonical event kinds from
/// ADDITION.md §A4.
impl NdjsonEvent {
    /// Batch-level start event.
    pub fn batch_start(repos: u32, action: &str, dry_run: bool) -> Self {
        let mut payload = HashMap::new();
        payload.insert("repos".into(), repos.into());
        payload.insert("action".into(), action.into());
        payload.insert("dry_run".into(), dry_run.into());
        Self {
            kind: "batch_start".into(),
            ts: None,
            payload,
        }
    }

    /// Single-repo begin event.
    pub fn repo_start(repo: &str) -> Self {
        let mut payload = HashMap::new();
        payload.insert("repo".into(), repo.into());
        Self {
            kind: "repo_start".into(),
            ts: None,
            payload,
        }
    }

    /// Plan was created for a repo.
    pub fn plan_created(repo: &str, plan_id: &str, risk: &str) -> Self {
        let mut payload = HashMap::new();
        payload.insert("repo".into(), repo.into());
        payload.insert("plan_id".into(), plan_id.into());
        payload.insert("risk".into(), risk.into());
        Self {
            kind: "plan_created".into(),
            ts: None,
            payload,
        }
    }

    /// All quality gates passed for a repo/plan.
    pub fn gates_passed(repo: &str, plan_id: &str) -> Self {
        let mut payload = HashMap::new();
        payload.insert("repo".into(), repo.into());
        payload.insert("plan_id".into(), plan_id.into());
        Self {
            kind: "gates_passed".into(),
            ts: None,
            payload,
        }
    }

    /// A mutating command was applied to a repo.
    pub fn applied(repo: &str, run_id: &str) -> Self {
        let mut payload = HashMap::new();
        payload.insert("repo".into(), repo.into());
        payload.insert("run_id".into(), run_id.into());
        Self {
            kind: "applied".into(),
            ts: None,
            payload,
        }
    }

    /// Single-repo completion event.
    pub fn repo_done(repo: &str, status: &str) -> Self {
        let mut payload = HashMap::new();
        payload.insert("repo".into(), repo.into());
        payload.insert("status".into(), status.into());
        Self {
            kind: "repo_done".into(),
            ts: None,
            payload,
        }
    }

    /// Quality gates failed for a repo.
    pub fn gates_failed(repo: &str, reason: &str) -> Self {
        let mut payload = HashMap::new();
        payload.insert("repo".into(), repo.into());
        payload.insert("reason".into(), reason.into());
        Self {
            kind: "gates_failed".into(),
            ts: None,
            payload,
        }
    }

    /// Batch-level completion event.
    pub fn batch_done(applied: u32, skipped: u32, failed: u32) -> Self {
        let mut payload = HashMap::new();
        payload.insert("applied".into(), applied.into());
        payload.insert("skipped".into(), skipped.into());
        payload.insert("failed".into(), failed.into());
        Self {
            kind: "batch_done".into(),
            ts: None,
            payload,
        }
    }
}

/// A thread-safe, buffering NDJSON writer backed by any `Write`.
#[derive(Debug)]
pub struct NdjsonWriter<W: std::io::Write> {
    inner: std::io::BufWriter<W>,
}

impl<W: std::io::Write> NdjsonWriter<W> {
    /// Wrap any `Write` — typically `std::io::stdout()`.
    pub fn new(inner: W) -> Self {
        Self {
            inner: std::io::BufWriter::new(inner),
        }
    }

    /// Write a single event. The `ts` field is populated automatically
    /// if the caller hasn't set one.
    pub fn write_event(&mut self, mut event: NdjsonEvent) -> anyhow::Result<()> {
        use anyhow::Context;
        if event.ts.is_none() {
            let ts = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| "unknown".into());
            event.ts = Some(ts);
        }
        let json = serde_json::to_string(&event).context("serialising NDJSON event")?;
        use std::io::Write;
        writeln!(self.inner, "{json}").context("writing NDJSON line")?;
        Ok(())
    }

    /// Flush the buffer. Call at the end of a batch.
    pub fn flush(&mut self) -> anyhow::Result<()> {
        use anyhow::Context;
        use std::io::Write;
        self.inner.flush().context("flushing NDJSON stream")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_start_event_serialises_correctly() {
        let e = NdjsonEvent::batch_start(3, "train", false);
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"event\":\"batch_start\""));
        assert!(json.contains("\"repos\":3"));
        assert!(json.contains("\"action\":\"train\""));
        assert!(json.contains("\"dry_run\":false"));
    }

    #[test]
    fn writer_populates_timestamp() {
        let buf = std::io::Cursor::new(Vec::new());
        let mut w = NdjsonWriter::new(buf);
        w.write_event(NdjsonEvent::repo_start("acme/widget"))
            .unwrap();
        w.flush().unwrap();

        let inner = w.inner.into_inner().unwrap();
        let line = String::from_utf8(inner.into_inner()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(parsed["event"], "repo_start");
        assert_eq!(parsed["repo"], "acme/widget");
        assert!(
            parsed["ts"].as_str().unwrap().contains('T'),
            "timestamp should be RFC 3339-like"
        );
    }

    #[test]
    fn writer_keeps_existing_timestamp() {
        let mut e = NdjsonEvent::batch_done(1, 0, 0);
        e.ts = Some("2025-01-01T00:00:00Z".into());
        let buf = std::io::Cursor::new(Vec::new());
        let mut w = NdjsonWriter::new(buf);
        w.write_event(e).unwrap();
        w.flush().unwrap();

        let inner = w.inner.into_inner().unwrap();
        let line = String::from_utf8(inner.into_inner()).unwrap();
        assert!(line.contains("2025-01-01T00:00:00Z"));
    }

    #[test]
    fn each_line_is_valid_json_no_embedded_newlines() {
        let buf = std::io::Cursor::new(Vec::new());
        let mut w = NdjsonWriter::new(buf);
        w.write_event(NdjsonEvent::gates_passed("a/b", "p1"))
            .unwrap();
        w.write_event(NdjsonEvent::gates_failed("a/b", "secret_detected"))
            .unwrap();
        w.flush().unwrap();

        let inner = w.inner.into_inner().unwrap();
        let bytes = inner.into_inner();
        let text = String::from_utf8(bytes).unwrap();
        let lines: Vec<&str> = text.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            assert!(serde_json::from_str::<serde_json::Value>(line).is_ok());
        }
    }

    #[test]
    fn custom_payload_roundtrips() {
        let mut e = NdjsonEvent {
            kind: "custom".into(),
            ts: None,
            payload: HashMap::new(),
        };
        e.payload.insert("score".into(), 42.into());
        e.payload
            .insert("tags".into(), serde_json::json![["a", "b"]]);
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"score\":42"));
        assert!(json.contains("\"tags\":[\"a\",\"b\"]"));
    }
}
