//! Exit codes, machine output, and messages that point at flags.
//!
//! # What is under test, and why these are one file
//!
//! Three families of defect, one shape: a value the tool computed and then
//! dropped, or a string it printed that names something other than what it
//! did. They were found by running the binary, not by reading it, and each one
//! had a comment nearby describing the fix as already made.
//!
//!  * **Exit code.** `exit.rs` documents `EX_USAGE` (64) for "unknown flag,
//!    unknown repo name, a name matching nothing, an invalid glob" and
//!    `EX_FATAL` (70) for "bad config". Anything raised as a bare
//!    `anyhow::bail!` reaches the top-level handler, which finds no
//!    `FatalError` to downcast to and reports 70 — so a mistyped command line
//!    read as a broken install, on three different verbs.
//!  * **`--format json`.** `json` is a document; the `Commands::Sync` arm
//!    re-serialised and re-printed the whole fleet once per repo, so N repos
//!    produced N concatenated copies of one array and `json.load()` raised
//!    "Extra data: line 2 column 1". The identical bug had already been found
//!    and fixed for `ro list`, with a comment naming that exact error.
//!  * **Flag names in messages.** `--repos` is not a flag on any verb —
//!    clap rejects it as an unexpected argument and `ro schema`'s long-flag
//!    list does not contain it — yet it was named in two user-facing messages,
//!    alongside a suggestion to pass `--all` to a command that already had it.
//!
//! Every test here runs the **real binary**, because the exit code and the
//! stdout bytes are the only things a library test cannot see.

use assert_cmd::Command;
use ro_testkit::{BareRemote, Worktree};
use tempfile::TempDir;

struct Test {
    config_dir: TempDir,
    state_dir: TempDir,
}

impl Test {
    fn new() -> Self {
        Self {
            config_dir: TempDir::new().unwrap(),
            state_dir: TempDir::new().unwrap(),
        }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("ro").expect("the ro binary compiles");
        cmd.arg("--config-dir")
            .arg(self.config_dir.path())
            .arg("--state-dir")
            .arg(self.state_dir.path());
        cmd
    }

    fn initialised() -> Self {
        let t = Self::new();
        t.cmd().arg("init").assert().success();
        t
    }

    /// Register a checkout and return its `owner/name` label.
    fn register(&self, path: &std::path::Path) -> String {
        self.cmd().arg("add").arg(path).assert().success();
        let out = self
            .cmd()
            .args(["list", "--format", "ndjson"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let wanted = path.to_str().unwrap();
        let mut labels = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).expect("each line is a JSON object")
            })
            .filter(|v| v["local_path"].as_str() == Some(wanted))
            .map(|v| {
                format!(
                    "{}/{}",
                    v["owner"].as_str().unwrap(),
                    v["name"].as_str().unwrap()
                )
            });
        labels.next().unwrap_or_else(|| {
            panic!("no listed row has local_path {wanted:?}; ro list said:\n{text}")
        })
    }
}

/// A checkout with an upstream that is in line with a real bare remote.
fn enrolled(t: &Test) -> (Worktree, BareRemote, String) {
    let repo = Worktree::with_one_commit();
    let remote = BareRemote::ephemeral();
    repo.add_remote("origin", remote.path());
    ro_testkit::worktree::run(repo.path(), &["push", "-q", "origin", "HEAD:main"]);
    let label = t.register(repo.path());
    (repo, remote, label)
}

// ── Exit codes ──────────────────────────────────────────────────────────
//
// 64 is `EX_USAGE` and 70 is `EX_FATAL`, per `crates/ro/src/exit.rs`. The
// table there also says the split exists so a mistyped command line is not
// mistaken for a broken install — which is exactly what 70 says.

/// Two mutually exclusive selectors is the canonical usage error.
#[test]
fn two_conflicting_selectors_exit_usage_not_fatal() {
    let t = Test::initialised();
    let out = t
        .cmd()
        .args(["sync", "--clone-only", "--pull-only"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(64),
        "two mutually exclusive flags on the command line is a usage error; \
         got {}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The negative control: `EX_FATAL` is a different code, so the assertion
/// above is not passing because everything exits 64.
#[test]
fn a_broken_config_still_exits_fatal() {
    let t = Test::initialised();
    std::fs::write(
        t.config_dir.path().join("config.toml"),
        "this is not [ valid toml",
    )
    .unwrap();
    let out = t.cmd().args(["config", "print"]).output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(70),
        "a config that will not parse is EX_FATAL; got {}",
        out.status
    );
}

/// An unknown repo name, on three verbs, is the same class of mistake as the
/// one above and gets the same code as its siblings.
#[test]
fn an_unknown_repo_name_on_tag_verbs_exits_usage_not_fatal() {
    let t = Test::initialised();
    for args in [
        vec!["tag", "nosuchrepo", "x"],
        vec!["untag", "nosuchrepo", "x"],
        vec!["tags", "nosuchrepo"],
    ] {
        let out = t.cmd().args(&args).output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(64),
            "`ro {}` on an unknown repo must be EX_USAGE like `ro status`; got {}\n{}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// And the sibling it has to agree with.
#[test]
fn an_unknown_repo_name_on_status_exits_usage() {
    let t = Test::initialised();
    t.cmd().args(["status", "nosuchrepo"]).assert().code(64);
}

/// A missing `=` is the simplest possible mistyped command line.
#[test]
fn a_malformed_config_set_argument_exits_usage() {
    let t = Test::initialised();
    let out = t.cmd().args(["config", "set", "foo"]).output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(64),
        "a missing `=` is a usage error; got {}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("KEY=VALUE"),
        "the message must say what was expected: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── `--format json` is a document ───────────────────────────────────────
//
// The bug was quadratic: the `Json` arm sat inside `for r in &results` and
// re-printed the *entire* fleet once per repo. One repo hides it — one array
// is valid JSON — which is why a two-repo case is the smallest that fails.

#[test]
fn sync_json_is_one_parseable_document_over_a_fleet() {
    let t = Test::initialised();
    for _ in 0..3 {
        enrolled(&t);
    }

    let out = t.cmd().args(["sync", "--format", "json"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);

    let value: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| {
        panic!(
            "`ro sync --format json` must emit ONE JSON document, but \
             json.load() failed: {e}\n\
             stdout was {} bytes with {} top-level arrays — the Json arm is \
             inside the per-repo loop, so N repos print N copies of the fleet.",
            text.len(),
            text.lines()
                .filter(|l| l.trim_start().starts_with('['))
                .count(),
        )
    });
    let rows = value.as_array().expect("a fleet of results is an array");
    assert_eq!(rows.len(), 3, "one row per repo: {text}");
}

/// `ndjson` is a stream and is the negative control: one object per line,
/// which is why the two arms cannot be the same code.
#[test]
fn sync_ndjson_is_one_object_per_line() {
    let t = Test::initialised();
    for _ in 0..3 {
        enrolled(&t);
    }
    let out = t
        .cmd()
        .args(["sync", "--format", "ndjson"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 3, "one line per repo: {text}");
    for line in &lines {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|e| panic!("ndjson line is not a JSON object: {e}\n{line}"));
    }
}

// ── Every status word reaches a reader ──────────────────────────────────
//
// `skipped_dirty` and `skipped_unpushed` are skips, not errors, so their
// reason lived only in the `sync_results` row's `error` column. Every
// renderer reads `SyncResult`, so a plain `ro sync` printed the word and
// nothing else — and the word does not tell you that `--autostash` is the
// flag that changes the outcome.

#[test]
fn a_dirty_repo_says_how_to_stop_it_being_skipped() {
    let t = Test::initialised();
    let (repo, _remote, label) = enrolled(&t);
    repo.write("unsaved.txt", "work in progress\n");

    let out = t.cmd().args(["sync"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains(&label),
        "the dirty repo must have a row: {text}"
    );
    assert!(
        text.contains("skipped_dirty"),
        "a dirty repo is skipped: {text}"
    );
    assert!(
        text.contains("--autostash"),
        "the row must name the flag that makes this repo syncable. The \
         reason existed in the database and reached no reader: {text}"
    );
}

/// The same for the other skip, and the negative control for the assertion
/// above: a clean repo has no reason to print.
#[test]
fn a_clean_fleet_prints_no_reason() {
    let t = Test::initialised();
    let (_repo, _remote, label) = enrolled(&t);
    let out = t.cmd().args(["sync"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains(&label), "the repo has a row: {text}");
    assert!(
        !text.contains("--autostash"),
        "a clean repo must not be told about --autostash: {text}"
    );
}

// ── Messages must not name flags that do not exist ──────────────────────

#[test]
fn the_invalid_glob_message_names_the_flag_that_exists() {
    let t = Test::initialised();
    let (_repo, _remote, _label) = enrolled(&t);
    let out = t.cmd().args(["sync", "--pattern", "["]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stderr);

    assert!(
        text.contains("--pattern"),
        "the message must name the flag the value arrived on: {text}"
    );
    assert!(
        !text.contains("--repos"),
        "`--repos` is not a flag on any verb — clap rejects it with \
         'unexpected argument' and `ro schema` does not list it. A user whose \
         glob has a typo was told to fix a flag they never typed: {text}"
    );
}

#[test]
fn the_empty_registry_message_suggests_nothing_impossible() {
    let t = Test::initialised();
    let out = t.cmd().args(["ship", "--all"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stderr);

    assert!(
        !text.contains("--repos"),
        "`--repos` is not a flag on any verb: {text}"
    );
    assert!(
        !text.contains("--all"),
        "`--all` is the flag the user just typed; suggesting it as the remedy \
         for a command that already contained it is the defect this message's \
         own comment records as fixed once: {text}"
    );
}

/// The schema is the reference whose stated job is that you do not have to
/// parse `--help`. A flag the tool names in a message and does not have is
/// not a documentation nit — it is a user following the tool's own advice
/// and getting "unexpected argument".
#[test]
fn the_schema_does_not_advertise_a_flag_the_binary_rejects() {
    let out = Command::cargo_bin("ro")
        .expect("the ro binary compiles")
        .arg("schema")
        .output()
        .unwrap();
    let schema: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("ro schema is one JSON document");

    let mut flags = Vec::new();
    fn collect(v: &serde_json::Value, into: &mut Vec<String>) {
        // `args` is an array of objects with a `long`; `commands` is an array
        // of objects whose own `args` has to be found. Both are arrays, so
        // the recursion cannot stop at the first level.
        if let Some(arr) = v.as_array() {
            for item in arr {
                if let Some(long) = item.get("long").and_then(|l| l.as_str()) {
                    into.push(long.to_string());
                } else {
                    collect(item, into);
                }
            }
        }
        if let Some(obj) = v.as_object() {
            for child in obj.values() {
                collect(child, into);
            }
        }
    }
    collect(&schema, &mut flags);
    assert!(
        flags.len() > 10,
        "the schema lists the whole surface: {flags:?}"
    );

    for flag in &flags {
        assert_ne!(
            flag, "--repos",
            "the schema advertises --repos, which no verb has"
        );
    }

    // And the control that makes the assertion mean something: the binary
    // really does reject it.
    let rejected = Command::cargo_bin("ro")
        .unwrap()
        .args(["list", "--repos", "x"])
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&rejected.stdout),
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert!(
        text.contains("unexpected argument"),
        "`--repos` must genuinely be absent, or this test proves nothing: {text}"
    );
}

// ── `agent.engine` is validated at write time ───────────────────────────
//
// Every sibling key is: `core.layout="bogus"`, `github.auth="bogus"`,
// `core.parallel=0` and `core.timeout_secs=0` are all refused by
// `ro config set`. `[agent].engine` was the one that was not — accepted at
// exit 0, and it failed at the next fleet run, once per repo per run.

#[test]
fn an_out_of_range_enum_value_is_refused_at_write_time() {
    let t = Test::initialised();

    // The siblings, as the control: they are already refused.
    t.cmd()
        .args(["config", "set", "core.layout=\"bogus\""])
        .assert()
        .failure();
    t.cmd()
        .args(["config", "set", "github.auth=\"bogus\""])
        .assert()
        .failure();

    // And the one that was not.
    let out = t
        .cmd()
        .args(["config", "set", "agent.engine=\"bogus\""])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "`agent.engine` is documented as `claude | codex | git`; a value \
         outside that domain must be refused where it is written, not once \
         per repo at the next run. Got {}",
        out.status
    );

    // Nothing was written: a refused set leaves the file alone.
    let printed = t.cmd().args(["config", "print"]).output().unwrap();
    let text = String::from_utf8_lossy(&printed.stdout);
    assert!(
        !text.contains("bogus"),
        "a refused value must not be written: {text}"
    );
}

// ── The shipped config keys ro reads are actually read ──────────────────
//
// `core.parallel` and `core.timeout_secs` were accepted, echoed by
// `ro config`, and ignored — and the `runs` row then recorded
// `parallel=8`, a number the user never chose, in the audit trail.

#[test]
fn a_shipped_config_key_round_trips_into_the_file() {
    let t = Test::initialised();
    for pair in [
        "core.parallel=2",
        "core.timeout_secs=45",
        "core.layout=\"nested\"",
        "github.auth=\"env\"",
    ] {
        t.cmd().args(["config", "set", pair]).assert().success();
    }
    let out = t.cmd().args(["config", "print"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    for expected in ["parallel = 2", "timeout_secs = 45"] {
        assert!(
            text.contains(expected),
            "a key ro ships and reads must round-trip; missing {expected:?}: {text}"
        );
    }

    // And the **runs** row records the number the user actually chose.
    //
    // This is the half that was invisible. `config set core.parallel=1` was
    // accepted and echoed, `ro sync` called `sync_all` (which hardcodes the
    // compiled-in 8), and the `runs` row recorded `parallel=8` — a number
    // the user never chose, written into the audit trail for a fleet they had
    // asked to make one repo at a time.
    let (_repo, _remote, label) = enrolled(&t);
    let out = t.cmd().args(["sync"]).output().unwrap();
    assert!(
        out.status.success(),
        "a clean fleet sync exits 0: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    let db = t.state_dir.path().join("state.db");
    let conn = rusqlite::Connection::open(&db).expect("the state db opens");
    let args: String = conn
        .query_row(
            "SELECT args_json FROM runs WHERE command = 'sync' ORDER BY started_at DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .expect("the sync run recorded its args");
    assert!(
        args.contains("parallel=2"),
        "the runs row must record the width the user configured, not the \
         compiled-in default; got {args}"
    );
    assert!(
        args.contains("timeout_secs=45"),
        "and the deadline the user configured, not the literal 30 that was \
         hardcoded in main.rs; got {args}"
    );
    let _ = label;
}
