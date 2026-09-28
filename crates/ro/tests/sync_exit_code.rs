//! `ro sync` must set its process exit code.
//!
//! # The property under test
//!
//! `ro_sync::sync::run_exit_code` computes the run-level verdict and
//! `ro_jobs::finalize_run` writes it to the `runs` table — and then the
//! verdict was dropped on the floor, because the `Commands::Sync` arm in
//! `main.rs` printed its rows and fell off the end of `run()`. `doctor` and
//! `ship` both call `process::exit`; `sync` was the only fleet verb that did
//! not.
//!
//! So a run whose only bad repo was an autostash conflict — a tree full of
//! conflict markers with the user's work parked in a stash — exited 0. That
//! is a "summary disagrees with the rows" defect in its most consequential
//! form: a fleet-wide failure is indistinguishable from a clean run to any
//! script, and it contradicts FEATURES.md's "The exit code carries the
//! run-level verdict".

use assert_cmd::Command;
use predicates::prelude::*;
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
}

/// A repo on a real bare remote, with the working copy **diverged** from the
/// remote: one local commit the remote lacks, and one remote commit the
/// working copy lacks.
///
/// Both commits touch the **same line of the same file**, because that is
/// the only shape that produces a conflict. A local commit to `local.txt`
/// and a remote commit to `remote.txt` rebase cleanly, so a fixture that
/// used different files would report `status=success` and the run-level
/// verdict would be 0 — a green test that proves nothing.
fn diverged(name: &str) -> (Worktree, BareRemote) {
    let repo = Worktree::with_one_commit();
    let remote = BareRemote::ephemeral();
    repo.add_remote("origin", remote.path());
    // A shared file both sides will rewrite, committed on the base.
    repo.write("shared.txt", &format!("base {name}\n"));
    ro_testkit::worktree::run(repo.path(), &["add", "-A"]);
    ro_testkit::worktree::run(repo.path(), &["commit", "-q", "-m", "base"]);
    // Push the base so the remote has a main.
    ro_testkit::worktree::run(repo.path(), &["push", "-q", "origin", "HEAD:main"]);

    // Local commit the remote has not seen, rewriting the shared line.
    repo.write("shared.txt", &format!("LOCAL SIDE {name}\n"));
    ro_testkit::worktree::run(repo.path(), &["add", "-A"]);
    ro_testkit::worktree::run(repo.path(), &["commit", "-q", "-m", "local"]);

    // Remote commit the working copy has not seen, rewriting the same line,
    // pushed from a second clone so the working copy's own history is
    // untouched. The clone is made with `git clone` rather than
    // `Worktree::empty()` because a bare clone's checkout lands on a
    // detached HEAD, and a commit there is not on `main` to push.
    let other_dir = TempDir::new().expect("a temp dir is creatable");
    let other_path = other_dir.path().join("other");
    let out = Command::new(ro_testkit::git_path())
        .arg("clone")
        .arg("-q")
        .arg(remote.path())
        .arg(&other_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git clone failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    ro_testkit::worktree::run(&other_path, &["config", "user.email", "t@example.invalid"]);
    ro_testkit::worktree::run(&other_path, &["config", "user.name", "T"]);
    // A bare remote's HEAD names a branch that does not exist, so the
    // clone checks out nothing and lands on an unborn `master`. Putting
    // the clone on the remote's `main` first is what makes the push below
    // a fast-forward rather than a rejected non-fast-forward.
    ro_testkit::worktree::run(&other_path, &["fetch", "-q", "origin", "main"]);
    ro_testkit::worktree::run(&other_path, &["checkout", "-q", "-B", "main", "FETCH_HEAD"]);
    std::fs::write(other_path.join("shared.txt"), format!("REMOTE SIDE {name}\n"))
        .expect("the file is writable");
    ro_testkit::worktree::run(&other_path, &["add", "-A"]);
    ro_testkit::worktree::run(&other_path, &["commit", "-q", "-m", "remote"]);
    ro_testkit::worktree::run(&other_path, &["push", "-q", "origin", "HEAD:main"]);

    // The working copy is now 1 ahead / 1 behind, and a pull over it
    // conflicts. Asserted here so a silently non-diverging fixture fails
    // at the fixture rather than reading as a passing sync test. The
    // fetch is what makes the count meaningful: `origin/main` is a local
    // ref and only moves when something fetches.
    ro_testkit::worktree::run(repo.path(), &["fetch", "-q", "origin"]);
    let counts = ro_testkit::worktree::run(
        repo.path(),
        &["rev-list", "--left-right", "--count", "origin/main...HEAD"],
    );
    assert_eq!(
        counts.trim(),
        "1\t1",
        "the fixture must be 1 ahead / 1 behind, got {counts:?}"
    );

    (repo, remote)
}

/// Register a checkout and return **its own** `owner/name` label.
///
/// Matched on `local_path`, not "the first row": two temp dirs in one test
/// can produce the same owner, and picking the first row made both labels
/// name the same repo — so the fleet test synced one healthy repo twice and
/// read exit 0 as a clean run.
fn register(t: &Test, path: &std::path::Path) -> String {
    t.cmd().arg("add").arg(path).assert().success();
    let out = t
        .cmd()
        .args(["list", "--format", "json"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let wanted = path.to_str().unwrap();
    let mut labels = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("each line is a JSON object"))
        .filter(|v| v["local_path"].as_str() == Some(wanted))
        .map(|v| format!("{}/{}", v["owner"].as_str().unwrap(), v["name"].as_str().unwrap()));
    let label = labels.next().unwrap_or_else(|| {
        panic!("no listed row has local_path {wanted:?}; ro list said:\n{text}")
    });
    assert!(
        labels.next().is_none(),
        "two rows share local_path {wanted:?}"
    );
    label
}

/// A run that produced a conflict must exit non-zero.
#[test]
fn a_conflicting_sync_exits_nonzero() {
    let t = Test::initialised();
    let (repo, _remote) = diverged("delta");
    let label = register(&t, repo.path());

    t.cmd()
        .args(["sync", "--strategy", "rebase", &label])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("status=conflict"));
}

/// A clean run must still exit 0 — the fix must not make every sync fail.
#[test]
fn a_clean_sync_exits_zero() {
    let t = Test::initialised();
    let repo = Worktree::with_one_commit();
    let remote = BareRemote::ephemeral();
    repo.add_remote("origin", remote.path());
    ro_testkit::worktree::run(repo.path(), &["push", "-q", "origin", "HEAD:main"]);
    let label = register(&t, repo.path());

    t.cmd()
        .args(["sync", &label])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("status=success"));
}

/// A mixed fleet — one success, one conflict — is a partial run, so it exits
/// 1. This is the headline case: two repos failed and the process exited 0.
#[test]
fn a_mixed_fleet_exits_nonzero() {
    let t = Test::initialised();
    let (good, _r1) = diverged("good");
    // Bring `good` into line with its remote by hand, so the fleet is
    // genuinely mixed: one repo in sync, one diverged. Done with a plain
    // `reset` rather than a pull because the two sides rewrote the same
    // line, so a pull would conflict — and a conflict here would be the
    // fixture's, not the run's.
    ro_testkit::worktree::run(good.path(), &["fetch", "-q", "origin"]);
    ro_testkit::worktree::run(good.path(), &["reset", "-q", "--hard", "origin/main"]);
    let (bad, _r2) = diverged("bad");

    let good_label = register(&t, good.path());
    let bad_label = register(&t, bad.path());

    let out = t
        .cmd()
        .args(["sync", "--strategy", "rebase", &good_label, &bad_label])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "a mixed fleet must exit 1, got {}: {}",
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("status=success"),
        "the healthy repo must still report success: {stdout}"
    );
    assert!(
        stdout.contains("status=conflict"),
        "the diverged repo must report conflict: {stdout}"
    );
}
