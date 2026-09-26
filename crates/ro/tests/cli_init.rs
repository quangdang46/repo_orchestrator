//! End-to-end CLI tests, against the real binary.
//!
//! Every test drives `ro` with `--config-dir` and `--state-dir` overrides
//! so nothing touches a real installation, and every fixture is a local
//! git repository created with `tempfile` — never a network clone and never
//! a hard-coded `/tmp` path, so the suite runs on the whole CI matrix rather
//! than silently skipping two thirds of it.

use assert_cmd::Command;
use predicates::prelude::*;
use ro_testkit::Worktree;
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

    /// A fresh command, with the isolation flags already applied.
    fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("ro").expect("the ro binary compiles");
        cmd.arg("--config-dir")
            .arg(self.config_dir.path())
            .arg("--state-dir")
            .arg(self.state_dir.path());
        cmd
    }

    /// An initialised installation.
    fn initialised() -> Self {
        let t = Self::new();
        t.cmd().arg("init").assert().success();
        t
    }
}

/// The repos `ro list --format json` reports.
///
/// One JSON object **per line**, not a wrapping array: a fleet is a stream,
/// and a consumer can read the first repo without waiting for the last.
/// Parsing the whole output as one array is the obvious mistake and it is
/// what the previous version of this file did.
fn listed(test: &Test) -> Vec<serde_json::Value> {
    let out = test
        .cmd()
        .args(["list", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ro list failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| {
            serde_json::from_str(l)
                .unwrap_or_else(|e| panic!("each line must be a JSON object ({e}): {l}"))
        })
        .collect()
}

/// The single row `ro status` reports, for a one-repo installation.
fn status_of(test: &Test) -> serde_json::Value {
    let out = test
        .cmd()
        .args(["status", "--format", "json"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_else(|| {
            panic!(
                "ro status reported nothing: {}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
    serde_json::from_str(line).expect("each status line is a JSON object")
}

#[test]
fn init_creates_config_and_state() {
    let t = Test::new();
    t.cmd()
        .arg("init")
        .assert()
        .success()
        .stderr(predicate::str::contains("Initialized"));
    assert!(
        t.state_dir.path().join("state.db").exists(),
        "init must create the state database"
    );
}

#[test]
fn init_is_idempotent() {
    let t = Test::initialised();
    t.cmd()
        .arg("init")
        .assert()
        .success()
        .stderr(predicate::str::contains("Already"));
}

/// A local checkout, adopted rather than cloned — so the test needs no
/// network and cannot fail because a remote moved.
fn local_repo() -> Worktree {
    Worktree::with_one_commit()
}

#[test]
fn add_registers_a_local_checkout() {
    let t = Test::initialised();
    let repo = local_repo();

    t.cmd()
        .arg("add")
        .arg(repo.path())
        .assert()
        .success()
        .stderr(predicate::str::contains("Added"));

    let rows = listed(&t);
    assert_eq!(rows.len(), 1, "one repo was added, got {rows:?}");
}

/// A failed clone writes **no** row.
///
/// "Tracked but not cloned" is a state the system has to tolerate in four
/// places if it can happen at all; the reason it does not is that the
/// insert and the clone are ordered so this cannot occur. Asserted, not
/// assumed.
#[test]
fn a_failed_add_writes_no_row() {
    let t = Test::initialised();
    // A path that is not a checkout and not a remote spec: refused before
    // any row could be written.
    t.cmd()
        .arg("add")
        .arg(t.config_dir.path().join("not-a-repo"))
        .assert()
        .failure();

    assert!(
        listed(&t).is_empty(),
        "a failed add must leave no row behind"
    );
}

#[test]
fn remove_takes_the_row_away() {
    let t = Test::initialised();
    let repo = local_repo();
    t.cmd().arg("add").arg(repo.path()).assert().success();

    let rows = listed(&t);
    let alias = format!(
        "{}/{}",
        rows[0]["owner"].as_str().unwrap(),
        rows[0]["name"].as_str().unwrap()
    );

    t.cmd()
        .arg("remove")
        .arg(&alias)
        .assert()
        .success()
        .stderr(predicate::str::contains("Removed"));

    assert!(listed(&t).is_empty(), "the row must be gone");
}

/// An unknown key is a **usage** error, not a failure that looks like a
/// broken run.
#[test]
fn an_unknown_repo_is_a_usage_error() {
    let t = Test::initialised();
    t.cmd()
        .arg("remove")
        .arg("nobody/nothing")
        .assert()
        .code(64);
}

#[test]
fn status_distinguishes_clean_from_dirty() {
    let t = Test::initialised();
    let repo = local_repo();
    t.cmd().arg("add").arg(repo.path()).assert().success();

    let row = status_of(&t);
    assert_eq!(row["is_dirty"], serde_json::Value::Bool(false));

    repo.write("new.txt", "x\n");
    assert_eq!(
        status_of(&t)["is_dirty"],
        serde_json::Value::Bool(true),
        "a dirty worktree must read as dirty"
    );
}

/// A local-only repo is **in sync with nothing**, which is a fact and not
/// a measurement failure.
///
/// The distinction the two states exist for: a repo with no remote has
/// nothing to be behind, so `ahead`/`behind` are real numbers. A repo whose
/// remote exists but lacks the tracked branch is *unmeasurable* — `null`
/// plus a reason. Collapsing them would make a green board over unmeasured
/// rows look identical to a healthy fleet.
#[test]
fn a_local_only_repo_is_in_sync_not_unmeasurable() {
    let t = Test::initialised();
    let repo = local_repo();
    t.cmd().arg("add").arg(repo.path()).assert().success();

    let row = status_of(&t);
    assert_eq!(
        row["ahead"],
        serde_json::Value::from(0),
        "a repo with no remote has nothing to be behind, which is a fact"
    );
    assert_eq!(row["unmeasurable_reason"], serde_json::Value::Null);
}

/// `--dry-run` changes nothing and spawns no engine.
#[test]
fn ship_dry_run_changes_nothing() {
    let t = Test::initialised();
    let repo = local_repo();
    t.cmd().arg("add").arg(repo.path()).assert().success();
    repo.write("new.txt", "x\n");

    let before = repo.porcelain();
    t.cmd()
        .args(["ship", "--all", "--engine", "git", "--dry-run"])
        .assert()
        .success();

    assert_eq!(
        repo.porcelain(),
        before,
        "a dry run must leave the worktree exactly as it found it"
    );
    let subjects = ro_testkit::worktree::run(repo.path(), &["log", "--format=%s"]);
    assert!(
        !subjects.contains("new.txt"),
        "a dry run must not commit, log was: {subjects}"
    );
}

/// The removed names still work, for one release.
///
/// A script that breaks on a rename is a script the user has to read the
/// release notes to fix. This asserts the alias reaches the **same** place
/// as the real command, not merely that it parses — an alias that printed
/// a deprecation and then said "unrecognized" would pass a weaker test.
#[test]
fn the_removed_names_still_work() {
    let t = Test::initialised();

    // `ro robot-docs` -> `ro schema`
    let old = t.cmd().arg("robot-docs").output().unwrap();
    assert!(
        old.status.success(),
        "ro robot-docs must still resolve: {}",
        String::from_utf8_lossy(&old.stderr)
    );
    let current = t.cmd().arg("schema").output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&old.stdout),
        String::from_utf8_lossy(&current.stdout),
        "the alias must reach the same output as the command it stands for"
    );

    // `ro health` -> `ro list`
    let old = t.cmd().arg("health").output().unwrap();
    assert!(
        old.status.success(),
        "ro health must still resolve: {}",
        String::from_utf8_lossy(&old.stderr)
    );
    let current = t.cmd().arg("list").output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&old.stdout),
        String::from_utf8_lossy(&current.stdout)
    );
}

/// And the deprecation says what to type instead.
///
/// Printing "deprecated" without the replacement is a message that sends
/// the reader to the docs, which is the thing that moved.
#[test]
fn the_legacy_spelling_names_its_replacement() {
    let t = Test::initialised();
    let out = t
        .cmd()
        .args([
            "ship",
            "--commit-sweep",
            "--execute",
            "--engine",
            "git",
            "--dry-run",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ro ship"),
        "the message must name the replacement, got: {stderr}"
    );
    assert!(
        stderr.contains("removed"),
        "and say when it goes away, got: {stderr}"
    );
}
