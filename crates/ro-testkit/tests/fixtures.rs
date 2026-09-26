//! The testkit testing itself.
//!
//! A fixture that silently does not work is the worst kind of test
//! infrastructure: every test using it passes for reasons unrelated to what
//! it claims. So each part is exercised here, and — more importantly — each
//! is checked for the failure mode that makes a fixture lie: *not being
//! observed*.
//!
//! The negative controls at the bottom matter most. They assert that the
//! recorder can see a shim being run, by running one deliberately. Without
//! them, "the shim was never invoked" and "the recorder is blind" are the
//! same green.

use std::process::Command;

use ro_testkit::{BareRemote, Captured, FakeBinary, RemotePair, Worktree};

/// Run a command with `shim` first on `PATH`, returning what the shim saw.
fn run_with_shim(shim: &FakeBinary, program: &str, args: &[&str]) -> String {
    // SAFETY: the body only spawns a process and reads its output.
    unsafe { shim.with_on_path(|| spawn(program, args)) }
}

/// The spawn itself, so `with_on_path` can own the lock and the guard.
fn spawn(program: &str, args: &[&str]) -> String {
    let out = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawning {program}: {e}"));
    assert!(
        out.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The core claim: a child process really saw what the fixture recorded.
///
/// Not "the Command was built with the right argv" — the child is the only
/// witness that can testify to what it received.
#[test]
fn a_shim_really_runs_and_records_its_argv() {
    let shim = FakeBinary::recording("gh");
    run_with_shim(&shim, "gh", &["pr", "list", "--head", "feature/x"]);

    let invocations = shim.invocations();
    assert_eq!(invocations.len(), 1, "the shim should have run once");
    assert_eq!(
        invocations[0], "pr list --head feature/x",
        "the recorded argv must be what the child actually received"
    );
}

#[test]
fn a_shim_reports_stdout_to_its_caller() {
    let shim = FakeBinary::with_stdout("gh", "[]");
    let out = run_with_shim(&shim, "gh", &["pr", "list"]);
    assert_eq!(out.trim(), "[]", "gh's caller must see the canned answer");
}

#[test]
fn a_failing_shim_exits_with_its_code() {
    let shim = FakeBinary::failing("gh", 4, "not logged in");
    let out =
        // SAFETY: the body only spawns a process and reads its output.
        unsafe {
            shim.with_on_path(|| Command::new("gh").arg("pr").arg("list").output().unwrap())
        };

    assert!(
        !out.status.success(),
        "an unauthenticated gh must not look like a working one"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not logged in"),
        "the shim's stderr must reach the caller, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The negative control. Without this, every other assertion in this file
/// would also pass if the recorder were blind.
#[test]
fn the_recorder_is_not_blind() {
    let shim = FakeBinary::recording("gh");
    assert_eq!(shim.run_count(), 0, "nothing has run yet");

    run_with_shim(&shim, "gh", &["--version"]);

    assert_eq!(
        shim.run_count(),
        1,
        "the recorder must see a real invocation, or every 'was never run' \
         assertion in the suite passes for free"
    );
}

/// `run_count == 0` is only meaningful if the same fixture would have seen a
/// real run. This is that proof, in the shape the engine tests need.
#[test]
fn an_unused_shim_reports_zero_runs() {
    let shim = FakeBinary::recording("gh");
    assert!(
        shim.invocations().is_empty(),
        "a shim nobody ran must report no invocations, not a stale one"
    );
}

/// PATH must come back, or every later test in the binary spawns the shim
/// instead of the real binary — a failure that surfaces as a dozen
/// unrelated assertions at once. This happened while building the fixture.
#[test]
fn path_is_restored_after_the_guard_drops() {
    let before = std::env::var("PATH").unwrap_or_default();
    let shim = FakeBinary::recording("gh");
    // SAFETY: the body only reads PATH.
    unsafe {
        shim.with_on_path(|| {
            assert_ne!(
                std::env::var("PATH").unwrap_or_default(),
                before,
                "the shim should have been prepended while the call runs"
            );
        })
    }
    assert_eq!(
        std::env::var("PATH").unwrap_or_default(),
        before,
        "PATH must be exactly what it was"
    );
}

/// The stronger half: restored even when the body panics.
///
/// A panic between the swap and the restore is the case that produced a
/// whole binary of unrelated failures, so the property is worth
/// asserting. The panic is caught rather than resumed, and the hook is
/// silenced for its duration: a panicking test in a parallel binary
/// interleaves its message with every other test's, and that interleaving
/// is what made this test flaky for reasons unrelated to PATH.
#[test]
fn path_is_restored_even_when_the_body_panics() {
    let before = std::env::var("PATH").unwrap_or_default();
    let shim = FakeBinary::recording("gh");

    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    // `TestEnv::run` resumes the panic after restoring, so `catch_unwind`
    // here observes it and the process carries on — which is exactly
    // what a caller outside a test would not do, and why the restore is
    // asserted here rather than assumed from the absence of a crash.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: the body mutates nothing but panics, which is the point.
        unsafe { shim.with_on_path(|| panic!("deliberate panic inside the PATH guard")) }
    }));
    std::panic::set_hook(previous_hook);

    assert!(result.is_err(), "the body was supposed to panic");
    assert_eq!(
        std::env::var("PATH").unwrap_or_default(),
        before,
        "a panic inside the guard must not leave PATH swapped"
    );
}

/// A bare remote really accepts a push, and the commit is readable back.
///
/// The point of the whole module: a mocked `git push` can only assert it
/// was *called*, never *where the commit landed* — and where it landed is
/// the claim the credential design rests on.
#[test]
fn a_bare_remote_receives_a_push_and_the_commit_is_readable() {
    let remote = BareRemote::ephemeral();
    let w = Worktree::with_one_commit();
    w.add_remote("origin", remote.path());
    w.write("new.txt", "content\n");
    w.commit("a real commit");

    ro_testkit::worktree::run(w.path(), &["push", "origin", "main"]);

    assert!(
        remote.has_branch("main"),
        "the push did not land: branches are {:?}",
        remote.branches()
    );
    let messages = remote.commit_messages("main");
    assert!(
        messages.contains(&"a real commit".to_string()),
        "the right commit must be readable back, got: {messages:?}"
    );
}

/// A remote nobody pushed to reports no branches — the assertion the
/// engine-denied-push test depends on, checked here so that test is not
/// relying on an unexercised code path.
#[test]
fn an_untouched_remote_reports_no_branches() {
    let remote = BareRemote::ephemeral();
    assert!(
        remote.branches().is_empty(),
        "a fresh bare repo has no branches, got: {:?}",
        remote.branches()
    );
    assert!(!remote.has_branch("main"));
}

/// The two-remote fixture, and the property that makes it worth two: they
/// are genuinely different places, so "did not reach" is a real claim.
#[test]
fn the_remote_pair_is_two_independent_places() {
    let pair = RemotePair::new();
    assert_ne!(
        pair.ro.path(),
        pair.engine_denied.path(),
        "the pair must be two remotes, not one aliased"
    );
    assert!(pair.ro.branches().is_empty());
    assert!(pair.engine_denied.branches().is_empty());
}

/// The brand-new-directory case PLAN calls out, as a fixture guarantee.
#[test]
fn a_new_directory_with_a_file_inside_starts_untracked() {
    let w = Worktree::with_one_commit();
    let path = w.write_in_new_dir("brand-new", "inside.txt", "hello\n");
    assert!(path.exists());

    let porcelain = w.porcelain();
    assert!(
        porcelain.contains("brand-new"),
        "the new directory must show as untracked, got: {porcelain:?}"
    );
    assert!(w.is_dirty());
}

/// An engine shim makes a **real** commit. A fixture that only pretended to
/// would let the engine tests pass against a tree nothing changed.
#[test]
fn the_engine_shim_makes_a_real_commit() {
    let remote = BareRemote::ephemeral();
    let w = Worktree::with_one_commit();
    w.add_remote("origin", remote.path());
    w.write("agent.txt", "written by the agent\n");

    let engine = FakeBinary::agent_engine("claude");
    let workdir = w.path().to_str().expect("a UTF-8 temp path").to_string();
    // The engine shim first on PATH, and the workdir it commits in, both
    // installed and restored by **one** call under one lock. Composing two
    // separate guards deadlocked: they took the same non-reentrant mutex.
    // SAFETY: the body only spawns the shim.
    unsafe {
        ro_testkit::TestEnv::new()
            .shim(&engine)
            .var("RO_TESTKIT_WORKDIR", &workdir)
            .run(|| {
                Command::new("claude")
                    .arg("-p")
                    .arg("do the thing")
                    .output()
                    .expect("the engine shim runs")
            })
    };

    assert!(
        w.porcelain().trim().is_empty(),
        "the engine should have committed everything, still dirty: {:?}",
        w.porcelain()
    );
}

/// The PAT predicate must match a real token and refuse prose.
#[test]
fn the_pat_predicate_matches_a_token_and_refuses_prose() {
    assert!(ro_testkit::contains_pat(
        "token=ghp_16C7e42F292c6912E7710c838347Ae178B4a"
    ));
    assert!(
        ro_testkit::contains_pat("github_pat_11ABCDEFG0aBcDeFgHiJkLmNoPqRsT"),
        "the fine-grained form must be caught too"
    );
    assert!(!ro_testkit::contains_pat(
        "set your ghp_ token in the environment"
    ));
    assert!(!ro_testkit::contains_pat("ghp_tooshort"));
    assert!(!ro_testkit::contains_pat(""));
}

/// And the negative control for that predicate, which is what makes the
/// assertions above worth having.
#[test]
fn the_pat_predicate_would_flag_the_raw_value() {
    let raw = "ghp_16C7e42F292c6912E7710c838347Ae178B4a";
    assert!(
        ro_testkit::contains_pat(&format!("AuthToken(\"{raw}\")")),
        "the predicate must flag what a leaking Debug would print, \
         otherwise the no-leak assertions prove nothing"
    );
}

/// `Captured` reads both streams: a leak does not care which one it took.
#[test]
fn captured_combines_both_streams() {
    let captured = Captured {
        stdout: "on stdout".to_string(),
        stderr: " and on stderr".to_string(),
    };
    assert!(captured.combined().contains("on stdout"));
    assert!(captured.combined().contains("on stderr"));

    let leaky = Captured {
        stdout: String::new(),
        stderr: "leaked ghp_16C7e42F292c6912E7710c838347Ae178B4a".to_string(),
    };
    assert!(
        leaky.contains_pat(),
        "a token on stderr is leaked exactly as much as one on stdout"
    );
}
