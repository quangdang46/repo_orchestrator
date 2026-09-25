//! End-to-end test for the NDJSON emit path.
//!
//! The five tests inside `ndjson.rs` exercise `NdjsonWriter` in isolation,
//! and they were green while the production path was broken: the handler
//! printed events with `serde_json::to_string(&event)` and never constructed
//! the writer, so every NDJSON line `ro` actually emitted carried
//! `"ts": null` for the life of the crate. The writer's only callers were its
//! own unit tests, in a different crate, where nothing could see the gap.
//!
//! Moving the module into the binary is what made the compiler point at it —
//! the dead-code warning appeared the moment it sat next to its caller. That
//! is luck, not coverage, and the next wiring mistake will not produce a
//! warning.
//!
//! So this file goes through the real handler. Nothing here tests the writer
//! directly; everything here runs the command and reads its stdout.

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

struct Cli {
    config_dir: TempDir,
    state_dir: TempDir,
}

impl Cli {
    fn new() -> Self {
        Self {
            config_dir: TempDir::new().unwrap(),
            state_dir: TempDir::new().unwrap(),
        }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("ro").expect("ro binary should compile");
        cmd.arg("--config-dir").arg(self.config_dir.path());
        cmd.arg("--state-dir").arg(self.state_dir.path());
        cmd
    }

    /// Run `sweep agent --all --output json` and return one parsed object per
    /// stdout line. An empty registry still emits `batch_start` and
    /// `batch_done`, which is the cheapest fixture that reaches the emit path.
    fn sweep_ndjson(&self) -> Vec<Value> {
        let mut cmd = self.cmd();
        cmd.args(["sweep", "agent", "--all", "--output", "json"]);
        cmd.assert().success();
        let out = cmd.output().expect("sweep agent should run");
        String::from_utf8(out.stdout)
            .expect("NDJSON should be UTF-8")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                serde_json::from_str(l)
                    .unwrap_or_else(|e| panic!("every line must be valid JSON: {e}\nline: {l}"))
            })
            .collect()
    }
}

#[test]
fn every_ndjson_line_carries_a_real_timestamp() {
    // The regression. Before the fix every one of these parsed, every one was
    // structurally valid, and every one had `"ts": null` — because the test
    // suite only ever exercised the writer, and the writer was never used.
    let events = Cli::new().sweep_ndjson();
    assert!(!events.is_empty(), "sweep agent should emit at least a batch_start");

    for ev in &events {
        let kind = ev["event"].as_str().unwrap_or("<none>");
        let ts = ev
            .get("ts")
            .unwrap_or_else(|| panic!("{kind} has no `ts` field at all"));
        let ts = ts
            .as_str()
            .unwrap_or_else(|| panic!("{kind} has a non-string ts: {ts}"));

        assert!(
            ts.contains('T') && ts.ends_with('Z'),
            "{kind} ts is not RFC3339 UTC: {ts}"
        );
        assert_ne!(ts, "unknown", "{kind} fell back to the literal \"unknown\"");
    }
}

#[test]
fn the_batch_is_opened_and_closed() {
    let events = Cli::new().sweep_ndjson();
    let kinds: Vec<&str> = events
        .iter()
        .filter_map(|e| e["event"].as_str())
        .collect();

    assert_eq!(kinds.first(), Some(&"batch_start"), "a run must open a batch");
    assert_eq!(kinds.last(), Some(&"batch_done"), "a run must close a batch");
    assert!(
        events[0]["repos"].as_u64() == Some(0),
        "an empty registry should report zero repos, got {:?}",
        events[0]["repos"]
    );
}

#[test]
fn timestamps_increase_along_the_batch() {
    // A writer constructed per event, or one reusing a stale clock, would
    // still satisfy "has a ts" but not this.
    //
    // The comparison is `>=` and must stay `>=`. `NdjsonWriter` formats
    // RFC3339 at nanosecond resolution, and a batch can legitimately
    // contain two events stamped in the same nanosecond under load. Tightening
    // this to `>` reads as "make it stricter" and would produce a test that
    // fails perhaps one run in twenty on a busy CI machine — the kind of flake
    // nobody believes and everybody learns to re-run.
    let events = Cli::new().sweep_ndjson();
    let stamps: Vec<&str> = events
        .iter()
        .map(|e| e["ts"].as_str().expect("every event carries a ts"))
        .collect();

    for pair in stamps.windows(2) {
        assert!(
            pair[1] >= pair[0],
            "timestamps went backwards: {} then {}",
            pair[0],
            pair[1]
        );
    }
}
