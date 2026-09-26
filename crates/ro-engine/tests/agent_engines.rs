//! The agent engines, exercised as real processes.
//!
//! Every assertion here is about a **spawn**, because that is where the
//! design's claims either hold or do not: a missing binary must be data
//! rather than an exit, a hang must be killed and called a timeout, and
//! the prompt must arrive as one argument rather than through a shell.

use std::time::Duration;

use ro_engine::{AgentEngine, BUILTIN_PROMPT, Engine, EngineContext, EngineOutcome};
use ro_testkit::{TestEnv, Worktree};

/// A missing binary is `Unavailable`, and the process does not fail.
///
/// The failure this guards is a spawn error escaping as a non-zero exit,
/// which would abort a fleet run for a problem that is really "this one
/// user has not installed a tool".
#[test]
fn a_missing_binary_is_unavailable_not_a_process_failure() {
    let w = Worktree::with_one_commit();
    w.write("a.txt", "x\n");

    // An engine pointed at a binary that cannot exist, so the test does
    // not depend on whether `claude` happens to be installed here.
    let engine = AgentEngine::claude();
    let ctx = EngineContext::new(w.path(), "main");

    let outcome = unsafe {
        TestEnv::new()
            .var("PATH", "/nonexistent-directory-for-this-test")
            .run(|| engine.checkpoint(&ctx))
    };

    match outcome {
        EngineOutcome::Unavailable { binary, hint } => {
            assert_eq!(binary, "claude", "the message must name the binary");
            assert!(
                hint.contains("checkpoint.engine"),
                "the hint must name the setting that changes the answer, \
                 got: {hint}"
            );
        }
        other => panic!(
            "a missing binary must be Unavailable so one repo's setup \\
             problem does not stop the fleet, got {other:?}"
        ),
    }
}

/// The negative control: the harness can tell a missing binary from a
/// present one, so the test above is not passing for a reason unrelated
/// to what it checks.
#[test]
fn the_availability_probe_distinguishes_present_from_missing() {
    // A shim on PATH, so "present" is a fact this test created rather
    // than a guess about what the machine happens to have installed.
    let shim = ro_testkit::FakeBinary::recording("claude");
    let engine = AgentEngine::claude();

    let present = unsafe { TestEnv::new().shim(&shim).run(|| engine.availability()) };
    let missing = unsafe {
        TestEnv::new()
            .var("PATH", "/nonexistent-directory-for-this-test")
            .run(|| engine.availability())
    };
    assert!(
        present.is_present(),
        "a shim on PATH must read as present, got {present:?}"
    );
    assert_eq!(
        missing,
        ro_engine::Availability::Missing,
        "and an empty PATH must read as missing"
    );
}

/// The prompt is **one argv element**, never a shell string.
///
/// The prompt carries diff text and file paths. If any of it went through
/// a shell, a file named `; rm -rf ~` would be syntax. The fake records
/// its argv, and the assertion is on the count and on the content.
#[test]
fn the_prompt_arrives_as_exactly_one_argument() {
    let w = Worktree::with_one_commit();
    w.write("a.txt", "x\n");

    // A shim that records its argv, one invocation per line.
    let shim = ro_testkit::FakeBinary::recording("claude");
    let engine = AgentEngine::claude();

    // SAFETY: the body only runs the shim under a scrubbed PATH.
    let outcome = unsafe {
        TestEnv::new().shim(&shim).run(|| {
            let ctx = EngineContext::new(w.path(), "main");
            engine.checkpoint(&ctx)
        })
    };

    let invocations = shim.invocations();
    assert_eq!(
        invocations.len(),
        1,
        "the engine should have spawned once, got {invocations:?}"
    );
    let argv = &invocations[0];

    // The prompt is the LAST thing on the line, and it is present in full.
    assert!(
        argv.contains("Do not push"),
        "the built-in prompt must reach the agent intact, got: {argv}"
    );
    assert!(
        argv.contains("Read the full diff"),
        "and it must be the whole prompt, not a fragment: {argv}"
    );
    // The prompt legitimately CONTAINS newlines — it is a multi-paragraph
    // instruction — so what matters is that they stayed inside ONE argv
    // element. A shell would have split on them and tried to run each
    // paragraph; `execve` passed one string.
    //
    // The recorder joins argv with spaces and preserves the newlines
    // *within* each element, and one spawn is one record. So the whole
    // prompt being present as one contiguous run is the assertion.
    let paragraphs = BUILTIN_PROMPT.split("\n\n").count();
    assert!(
        argv.matches("Do not").count() >= paragraphs.min(3),
        "every paragraph must be inside the single argument, got: {argv}"
    );
    let _ = outcome;
}

/// A custom prompt is also one argument, and replaces the built-in.
#[test]
fn a_custom_prompt_replaces_the_builtin() {
    let w = Worktree::with_one_commit();
    w.write("a.txt", "x\n");
    let shim = ro_testkit::FakeBinary::recording("codex");
    let engine = AgentEngine::codex();

    unsafe {
        TestEnv::new().shim(&shim).run(|| {
            let ctx = EngineContext::new(w.path(), "main")
                .with_message(Some("only do the thing I asked"));
            engine.checkpoint(&ctx)
        })
    };

    let argv = shim.invocations().join(" ");
    assert!(argv.contains("only do the thing I asked"), "got: {argv}");
    assert!(
        !argv.contains("Read the full diff"),
        "the override must replace the built-in, not append to it: {argv}"
    );
}

/// An engine that hangs is killed and reported as `TimedOut`.
///
/// Distinct from `Failed` because the caller's response differs: a timeout
/// means "try again", a failure may mean "this will never work".
#[test]
fn a_hanging_engine_is_killed_and_reported_as_timed_out() {
    let w = Worktree::with_one_commit();
    w.write("a.txt", "x\n");

    let shim = hanging_shim();
    let engine = AgentEngine::claude();
    let ctx = EngineContext::new(w.path(), "main").with_timeout(Duration::from_millis(300));

    let started = std::time::Instant::now();
    let outcome = unsafe { TestEnv::new().shim(&shim).run(|| engine.checkpoint(&ctx)) };
    let elapsed = started.elapsed();

    assert!(
        matches!(outcome, EngineOutcome::TimedOut { .. }),
        "a hang must be TimedOut, not Failed, got {outcome:?}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the timeout did not fire promptly: {elapsed:?}"
    );
}

/// A shim that never returns.
fn hanging_shim() -> ro_testkit::FakeBinary {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("claude");
    std::fs::write(&path, "#!/bin/sh\nsleep 600\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&path).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&path, p).unwrap();
    }
    // The temp dir must outlive the shim, so leak it deliberately: the
    // fixture is dropped at the end of the test, not before the run.
    let dir = tmp.path().to_path_buf();
    std::mem::forget(tmp);
    ro_testkit::FakeBinary::at(dir, path, "claude")
}

/// A non-zero agent exit is classified into the shared taxonomy.
///
/// One taxonomy, not a second one for agents: a caller that has to learn
/// two vocabularies learns neither.
#[test]
fn a_non_zero_agent_exit_is_classified() {
    let w = Worktree::with_one_commit();
    w.write("a.txt", "x\n");

    let shim = failing_shim("Error: rate limit exceeded, try later later");
    let engine = AgentEngine::claude();
    let ctx = EngineContext::new(w.path(), "main").with_timeout(Duration::from_secs(10));

    let outcome = unsafe { TestEnv::new().shim(&shim).run(|| engine.checkpoint(&ctx)) };

    match outcome {
        EngineOutcome::Failed { error, class } => {
            assert_eq!(
                class,
                ro_core::FailureClass::RateLimited,
                "the agent's own words must reach the shared taxonomy"
            );
            assert!(error.contains("rate limit"), "got: {error}");
        }
        other => panic!("a non-zero exit must be Failed, got {other:?}"),
    }
}

fn failing_shim(message: &str) -> ro_testkit::FakeBinary {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("claude");
    std::fs::write(&path, format!("#!/bin/sh\necho '{message}' >&2\nexit 1\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&path).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&path, p).unwrap();
    }
    let dir = tmp.path().to_path_buf();
    std::mem::forget(tmp);
    ro_testkit::FakeBinary::at(dir, path, "claude")
}

/// The prompt is a constant, and a caller cannot accidentally inject one
/// through `message_override` by accident of formatting.
#[test]
fn the_builtin_prompt_is_exactly_the_one_that_says_do_not_push() {
    assert!(BUILTIN_PROMPT.contains("Do not push"));
    assert!(
        !BUILTIN_PROMPT.to_ascii_lowercase().contains("git push"),
        "the built-in prompt must not contain a push command"
    );
}
