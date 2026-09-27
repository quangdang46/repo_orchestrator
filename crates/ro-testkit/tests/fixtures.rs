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
use std::sync::Barrier;
use std::time::Duration;

use ro_testkit::{BareRemote, Captured, FakeBinary, RemotePair, TestEnv, Worktree};

/// Run `args` through `shim`, returning what the shim saw.
///
/// The shim is spawned **by path**, not by the bare name — and that is the
/// whole reason this helper exists in this shape. On Windows the loader does
/// not consult `PATHEXT` for a bare name, so `Command::new("gh")` skipped the
/// `gh.cmd` sitting first on `PATH` and ran the runner's real `gh.exe`
/// instead. These tests then asserted on GitHub's CLI rather than on the
/// fixture, and failed on a missing `GH_TOKEN` — which says nothing about
/// whether a shim records what it was given. `FakeBinary::program` is the
/// documented answer, and the rest of this file already used it.
fn run_with_shim(shim: &FakeBinary, args: &[&str]) -> String {
    let program = shim.program();
    // SAFETY: the body only spawns a process and reads its output.
    unsafe { shim.with_on_path(|| spawn(&program, args)) }
}

/// The spawn itself, so `with_on_path` can own the lock and the guard.
fn spawn(program: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawning {}: {e}", program.display()));
    assert!(
        out.status.success(),
        "{} failed: {}",
        program.display(),
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
    run_with_shim(&shim, &["pr", "list", "--head", "feature/x"]);

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
    let out = run_with_shim(&shim, &["pr", "list"]);
    assert_eq!(out.trim(), "[]", "gh's caller must see the canned answer");
}

#[test]
fn a_failing_shim_exits_with_its_code() {
    let shim = FakeBinary::failing("gh", 4, "not logged in");
    let out =
        // SAFETY: the body only spawns a process and reads its output.
        unsafe {
            shim.with_on_path(|| {
                Command::new(shim.program())
                    .arg("pr")
                    .arg("list")
                    .output()
                    .unwrap()
            })
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

    run_with_shim(&shim, &["--version"]);

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
    let shim = FakeBinary::recording("gh");

    // `before` must be a snapshot of PATH **as it was before the guard**,
    // so it is read under the lock but outside `with_on_path`. Reading it
    // inside would capture the already-swapped value and compare it
    // against an unrelated PATH afterwards — which fails or passes for a
    // reason that has nothing to do with the restore.
    let before = {
        let _l = ro_testkit::path_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::var("PATH").unwrap_or_default()
    };

    // SAFETY: the body only reads PATH.
    let during = unsafe { shim.with_on_path(|| std::env::var("PATH").unwrap_or_default()) };
    assert_ne!(
        during, before,
        "the shim must be on PATH while the guard is alive"
    );

    let _l = ro_testkit::path_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
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
    let shim = FakeBinary::recording("gh");
    let before = {
        let _l = ro_testkit::path_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::var("PATH").unwrap_or_default()
    };

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
    let _l = ro_testkit::path_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
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
                Command::new(engine.program())
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

/// Fixtures keep building while another thread scrubs `PATH`.
///
/// The flake, reproduced: the availability test in `ro-engine` used to write
/// `PATH=/nonexistent-directory-for-this-test` under the one global lock
/// (`TestEnv::run`), and — because a bare `git` resolves that name at spawn
/// *time* — a `Worktree` built on any sibling thread failed with
/// `git runs: Os { code: 2, kind: NotFound }`. Nothing in the failing test has
/// anything to do with `git`; the error names a fixture. That is the whole
/// shape of the bug, so this is the test that pins it.
///
/// The invariant under test: a fixture spawn consults no process-global
/// state. `ro_testkit::git_path` resolves `git` once to an absolute path and
/// caches it, so after the first resolution a scrubbed `PATH` is irrelevant
/// — by construction, and here by observation.
///
/// Why this shape, and not the deadlocks it avoids:
///
/// - The **scrubber** takes the real lock and holds it for a whole
///   `TestEnv::run` body, exactly like the engine test it reproduces.
///   It never touches fixture state.
/// - The **builder** takes **no lock at all**. That is the point: the
///   competing design — the fixture holding `path_lock` — self-deadlocks,
///   because the lock is a non-reentrant `Mutex` and `TestEnv::run` holds
///   it for a body that routinely builds a fixture *inside* it.
/// - **Bounded loops, no retries.** A fixed number of iterations on each
///   side; the assertion is "every one of those N built", not "it built
///   eventually". A retry loop would make this test hang rather than fail
///   under contention — the wrong lesson from a slow machine.
/// - The builder asserts the *files exist* after each build, not merely that
///   no error was thrown: a fixture that half-built would otherwise read as a
///   pass.
///
/// Kept modest on purpose: each fixture is a `git init` plus a commit, so
/// `K` is the number of commits this test pays for, and the cost is
/// seconds, not minutes.
#[test]
fn fixtures_build_while_path_is_scrubbed_on_another_thread() {
    // Bounded, so the test adds seconds rather than minutes and — more
    // importantly — so a pre-fix failure *count* is a number, not a guess.
    // 30 is enough that a fixture built against a scrubbed global PATH at
    // any point during the run is overwhelmingly likely to be hit.
    const K: usize = 30;

    // One barrier, released by both threads together, so the two loops
    // overlap for the whole run rather than serially. Without it the builder
    // could finish before the scrubber started, and the test would be a
    // green nothing — the same failure mode the recorder guards exist for.
    let start = Barrier::new(2);

    // A thread *scope*: both threads must finish before the assertion, and
    // `join` is the only way to know that. A detached thread would make the
    // pass/fail depend on the test harness happening to wait.
    //
    // `scrubs` is the vacuity guard for the whole test. If the two loops ever
    // stopped overlapping, the builder would simply build K fixtures under
    // a healthy `PATH` — green, and proving nothing, which is the exact
    // failure mode the negative controls in this file exist to prevent. The
    // scrubber counts what it actually did, so a test that never raced
    // cannot read as a test that raced and passed.
    let scrubs = std::sync::atomic::AtomicUsize::new(0);
    let scrubs = &scrubs;
    std::thread::scope(|scope| {
        let builder = scope.spawn(|| {
            start.wait();
            for _ in 0..K {
                let w = Worktree::with_one_commit();
                // Assert the artifact, not the absence of a panic: the
                // pre-fix failure was `git runs: ... NotFound` on exactly
                // this call, and a fixture that failed to build must be
                // counted, not skipped.
                assert!(
                    w.path().join(".git").exists(),
                    "the worktree was not really built"
                );
                let remote = BareRemote::ephemeral();
                assert!(
                    remote.path().join("HEAD").exists(),
                    "the bare remote was not really built"
                );
                assert!(remote.branches().is_empty());
            }
        });

        let scrubber = scope.spawn(|| {
            start.wait();
            // The pre-fix engine test's scrub, reproduced as the same
            // lock-held global mutation for the same duration shape.
            for _ in 0..K {
                // SAFETY: the body mutates nothing and sleeps; the
                // global `PATH` swap is installed and restored by the
                // `TestEnv` under its own lock. The sleep is what
                // makes the window overlap the builder's spawns rather
                // than flash past them.
                unsafe {
                    TestEnv::new()
                        .var("PATH", "/nonexistent-directory-for-this-test")
                        .run(|| std::thread::sleep(Duration::from_millis(1)))
                };
                // Counted only after the restore, so a count of K means K
                // *completed* scrubs, not K attempts.
                scrubs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });

        // `join` rather than `is_finished`: a panicked thread must fail
        // this test, and the `expect` on the `Result` is the assertion. A
        // timeout would be a worse test — the fix must not trade a flake
        // for a hang, so a hang is a failure here, loudly.
        builder.join().expect("the builder thread must not panic");
        scrubber.join().expect("the scrubber thread must not panic");

        // The overlap really happened, so the builder really did race a
        // scrubbed `PATH` K times. Without this the test would pass on a
        // run where the two threads never met.
        assert_eq!(
            scrubs.load(std::sync::atomic::Ordering::SeqCst),
            K,
            "every scrub must have run: a test whose two threads never \
             overlap passes for a reason unrelated to what it claims"
        );
    });
}
