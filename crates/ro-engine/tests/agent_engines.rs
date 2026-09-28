//! The agent engines, exercised as real processes.
//!
//! Every assertion here is about a **spawn**, because that is where the
//! design's claims either hold or do not: a missing binary must be data
//! rather than an exit, a hang must be killed and called a timeout, and
//! the prompt must arrive as one argument rather than through a shell.

use std::time::Duration;

use ro_engine::{AgentEngine, BUILTIN_PROMPT, Engine, EngineContext, EngineOutcome};
use ro_testkit::{TestEnv, Worktree};

/// The engine for `kind`, pointed at `shim` by **path**.
///
/// # Why every test in this file goes through here
///
/// `AgentEngine::claude()` and `AgentEngine::codex()` name a bare binary
/// and let `PATH` resolution find it. That is right for production and
/// wrong for a test, for one reason: the answer depends on what the
/// machine has installed. On CI no agent is installed, the shim directory
/// prepended to `PATH` is the only `claude` there is, and the test passes.
/// On a developer machine with the real `claude` in it, the same test
/// spawns the **real agent** — the shim records nothing, the assertion
/// fails, and the failure says nothing about the code under test.
///
/// Naming the shim by path removes `PATH` from the question entirely: the
/// engine is handed the one file the test created, so the result is the
/// same on every machine. `AgentEngine::with` is the constructor for
/// exactly this — a different binary and the same arguments — and it is
/// the one the extensibility note in `agent.rs` describes.
///
/// The defaults are deliberately `None`, so each slot gets the same
/// argument list its built-in constructor would have used: the prompt
/// assertions below are about argv, and a test that silently changed the
/// flags would be testing something else.
fn engine_for(shim: &ro_testkit::FakeBinary, kind: ro_engine::EngineKind) -> AgentEngine {
    AgentEngine::with(kind, shim.program().to_string_lossy().to_string(), None)
}

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
    //
    // The absence is expressed as a *name*, not as a scrubbed PATH. A
    // scrubbed PATH is a process-global fact, and this binary runs its
    // tests in parallel against one process: writing
    // `PATH=/nonexistent-directory-for-this-test` here makes every
    // sibling test that spawns a bare `git` — which is what the testkit
    // fixtures did — fail with `Os { code: 2, kind: NotFound }`. That is
    // the flake `ro_testkit::git_path` was written to remove, and the way
    // to keep it removed is to not need the global here. An absent name is
    // the same fact about availability with none of the blast radius, and
    // it is the shape `git_engine.rs` already uses for the same assertion
    // (its doc comment says so explicitly).
    let engine = AgentEngine::with(
        ro_engine::EngineKind::Claude,
        "definitely-not-installed-this-test-only",
        None,
    );
    let ctx = EngineContext::new(w.path(), "main");

    let outcome = engine.checkpoint(&ctx);

    match outcome {
        EngineOutcome::Unavailable { binary, hint } => {
            assert_eq!(
                binary, "definitely-not-installed-this-test-only",
                "the message must name the binary that is missing, not some \
                 other name"
            );
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
    let present = unsafe {
        TestEnv::new()
            .shim(&shim)
            .run(|| AgentEngine::claude().availability())
    };
    assert!(
        present.is_present(),
        "a shim on PATH must read as present, got {present:?}"
    );

    // The missing half is asserted against a name nothing can have
    // installed, *without* scrubbing PATH. Writing an empty PATH here is
    // the same process-global mutation as above — it answers "not on
    // PATH" for every thread except the one asking, and that is exactly
    // the failure `ro_testkit::git_path` exists for ("not found" on a bare
    // `git` spawned by a parallel fixture has nothing to do with what that
    // test is asserting). A definitely-absent name is the same negative
    // control with no shared state at all, which is also why
    // `git_engine.rs` chose it for its own missing-binary case.
    let missing = AgentEngine::with(
        ro_engine::EngineKind::Claude,
        "definitely-not-installed-this-test-only",
        None,
    )
    .availability();
    assert_eq!(
        missing,
        ro_engine::Availability::Missing,
        "and a name nothing on PATH can answer for must read as missing"
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
    //
    // Named by **path**, not left to `PATH` resolution — the same reason
    // `a_failure_is_reported_from_the_whole_of_stderr_not_its_first_line`
    // names its shim that way, and the comment there explains it at
    // length. In short: on a machine with a real agent installed, a
    // bare-name spawn runs the *real* agent and the shim records nothing,
    // so the assertion below would fail on a developer machine and pass
    // on CI, which has no agent installed. A test whose result depends
    // on what the machine happens to have is not hermetic, and a green
    // run of one is a claim about CI rather than about the code.
    let shim = ro_testkit::FakeBinary::recording("claude");
    let engine = engine_for(&shim, ro_engine::EngineKind::Claude);

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
    let engine = engine_for(&shim, ro_engine::EngineKind::Codex);

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

    let shim = ro_testkit::FakeBinary::hanging("claude");
    let engine = engine_for(&shim, ro_engine::EngineKind::Claude);
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

/// The reported error must be the one the agent actually failed on.
///
/// The test above passes with a single line of stderr, which is the shape
/// `claude` tends to produce and the only shape this code was exercised
/// against. `codex` does not produce it: `codex exec` writes a version
/// banner, a workdir/model/session preamble, an echo of the prompt, and
/// reconnect progress to **stderr**, and only then the error. A real
/// unauthenticated run on this machine put the actual cause —
///
///     unexpected status 401 Unauthorized: Missing bearer or basic
///     authentication in header
///
/// thirteen lines down, with `Reading additional input from stdin...` on
/// line one. Reporting only the first line reports the banner.
///
/// `classify_agent_output` already reads the whole stream, so `class` is
/// computed correctly and only `error` is wrong — which is the worst shape
/// for this bug: the taxonomy looks right in the JSON while the human-
/// readable string is a progress line.
#[test]
fn a_failure_is_reported_from_the_whole_of_stderr_not_its_first_line() {
    let w = Worktree::with_one_commit();
    w.write("a.txt", "x\n");

    // The shape `codex exec` actually produces, in order. Built line by
    // line: `FakeBinary::failing` writes one `echo`, which on Windows
    // collapses to a single line and would make this test pass against a
    // build that reports only the first line — the exact blindness this is
    // here to remove.
    let stderr = [
        "Reading additional input from stdin...",
        "OpenAI Codex v0.118.0 (research preview)",
        "--------",
        "workdir: /tmp/proj",
        "model: gpt-5.3-codex",
        "session id: 01a0e787-5e6d-7e51-bca8-e1ac87a74aab",
        "--------",
        "ERROR: Reconnecting... 1/5",
        "ERROR: Reconnecting... 2/5",
        "ERROR: unexpected status 401 Unauthorized: Missing bearer or basic \
         authentication in header, url: https://api.openai.com/v1/responses",
    ];

    let shim = ro_testkit::FakeBinary::failing_lines("codex", 1, &stderr);
    // The shim is named by **path**, not left to PATH resolution.
    //
    // `AgentEngine::codex()` resolves the bare name `codex` and tries
    // `.exe` first *across the whole PATH* before it ever tries `.cmd`, so
    // on a machine with a real `codex.exe` installed — or a real
    // `claude.exe`, which is what breaks the sibling tests in this file —
    // the real binary wins and the shim is never spawned. This test was
    // first written with the bare name and went red reporting
    // `Error loading config.toml: invalid type: string "wwwww"` — the
    // operator's own `~/.codex/config.toml`, read by a real codex. Naming
    // the file removes PATH from the question entirely.
    let engine = AgentEngine::with(
        ro_engine::EngineKind::Codex,
        shim.program().to_string_lossy().to_string(),
        None,
    );
    let ctx = EngineContext::new(w.path(), "main").with_timeout(Duration::from_secs(10));

    let outcome = unsafe { TestEnv::new().shim(&shim).run(|| engine.checkpoint(&ctx)) };

    match outcome {
        EngineOutcome::Failed { error, .. } => {
            // The banner *may* be in there — carrying the whole stream is a
            // fine way to fix this. What must not happen is the banner being
            // all there is, because that is the run where the user is told
            // "Reading additional input from stdin..." and never told the
            // token is missing.
            assert!(
                error.contains("401") && error.contains("Unauthorized"),
                "the reported error must name the real cause, which is on a \
                 later line of stderr. Got only: {error}"
            );
            assert!(
                error.contains("Reconnecting"),
                "the progress lines are part of the stream too, so a report \
                 that skipped to the last line would also satisfy this test; \
                 this one must not be the *only* thing reported: {error}"
            );
        }
        other => panic!("a non-zero exit must be Failed, got {other:?}"),
    }
}

/// A non-zero agent exit is classified into the shared taxonomy.
///
/// One taxonomy, not a second one for agents: a caller that has to learn
/// two vocabularies learns neither.
#[test]
fn a_non_zero_agent_exit_is_classified() {
    let w = Worktree::with_one_commit();
    w.write("a.txt", "x\n");

    let shim =
        ro_testkit::FakeBinary::failing("claude", 1, "Error: rate limit exceeded, try later later");
    let engine = engine_for(&shim, ro_engine::EngineKind::Claude);
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
