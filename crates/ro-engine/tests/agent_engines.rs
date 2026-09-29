//! The agent engines, exercised as real processes.
//!
//! Every assertion here is about a **spawn**, because that is where the
//! design's claims either hold or do not: a missing binary must be data
//! rather than an exit, a hang must be killed and called a timeout, and
//! the prompt must arrive as one argument rather than through a shell.

use std::time::Duration;

use ro_engine::{AgentEngine, BUILTIN_PROMPT, Engine, EngineContext, EngineOutcome};
use ro_testkit::{TestEnv, Worktree};

/// A shim that writes `filler_line` until the stream is at least `bytes`,
/// then `tail`, then exits 0.
///
/// # Why this exists, and why it is not a hypothetical
///
/// An OS pipe buffer is 64 KiB. A child that writes more than that before
/// exiting blocks in `write` and never exits at all — so the poll loop in
/// `run_with_deadline`, which only reaped the pipes *after* `try_wait`
/// returned `Some`, sat there until the deadline and reported a timeout for
/// an agent that was making perfectly good progress.
///
/// The real `claude` on this machine emits 73–86 KB of stream-json from a
/// single `commands_changed` event listing every skill, so `ro commit` with
/// the default engine sits right at that edge and intermittently hangs for
/// ten minutes. This fixture puts it well past the edge so the test does not
/// depend on where the boundary happens to be on the day it runs.
///
/// # The filler is valid stream-json, not arbitrary bytes
///
/// A reader that kept only the first 64 KiB and threw the rest away would
/// still return promptly, so "did not hang" on its own is satisfied by a
/// fix that truncates. The filler is therefore made of lines the parser
/// accepts and that carry no plan, and the plan goes in `tail` at the very
/// end: a truncated stream then parses cleanly and finds *nothing*, which
/// is a different answer from the one the fixed code gives. Bytes the parser
/// would have rejected anyway would hide that distinction completely.
///
/// The shim is a `cat` of a real file rather than a generated `printf` loop:
/// it is the same fixture on both platforms, and `FakeBinary::at` is the
/// constructor for a body the caller writes.
fn big_output_agent(
    dir: &std::path::Path,
    name: &str,
    filler_line: &str,
    bytes: usize,
    tail: &str,
) -> ro_testkit::FakeBinary {
    let mut payload = String::with_capacity(bytes + tail.len() + filler_line.len());
    while payload.len() < bytes {
        payload.push_str(filler_line);
        payload.push('\n');
    }
    payload.push_str(tail);

    let data = dir.join("payload.bin");
    std::fs::write(&data, payload).expect("the payload is writable");

    let body = if cfg!(windows) {
        format!("@echo off\r\ntype \"{}\"\r\nexit /b 0\r\n", data.display())
    } else {
        format!("#!/bin/sh\ncat '{}'\nexit 0\n", data.display())
    };
    let file = if cfg!(windows) {
        dir.join(format!("{name}.cmd"))
    } else {
        dir.join(name)
    };
    std::fs::write(&file, body).expect("the shim is writable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&file).expect("the shim exists").permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&file, p).expect("the mode is settable");
    }
    ro_testkit::FakeBinary::at(dir.to_path_buf(), file, name)
}

/// A shim that commits the work itself, as an identity that is **not** the
/// one ro resolved.
///
/// The prompt says "Do NOT run `git commit`". Some agent runs it anyway.
/// `BUILTIN_PROMPT` is a *request*; the control that is supposed to hold is
/// ro's own, and this is the case it did not cover: ro re-asserts
/// `-c user.name/-c user.email` on every commit **ro makes**, so an agent's
/// own commit arrived at the remote carrying whatever identity the agent's
/// own config had.
///
/// # Why `-c` on the commit rather than a `.git/config` write
///
/// Because `-c` is the stronger case, and the one that leaves nothing
/// behind. A `git config user.email …` write persists in `.git/config` and
/// is *visible* to ro — that is item 3's report. A per-invocation `-c`
/// touches no file at all, so ro has nothing to notice unless it looks at
/// the commit itself, which is exactly what did not happen. It is also
/// literally what an agent reaching for "commit under my own name" would
/// type.
///
/// # Why the tree is left clean
///
/// The detection this exercises is gated on a clean tree: `checkpoint`
/// reads `is_dirty` first, and only a clean tree with a moved HEAD means
/// "the agent committed it itself". A shim that staged but did not commit
/// leaves the index dirty and takes the other branch, which is the
/// ordinary path. So the fixture commits, and the tree is clean.
fn self_committing_agent(dir: &std::path::Path, name: &str) -> ro_testkit::FakeBinary {
    let body = if cfg!(windows) {
        "@echo off\r\n\
         git add -A\r\n\
         git -c user.name=Agent -c user.email=agent@elsewhere.invalid commit -q -m \"agent own commit\"\r\n\
         exit /b 0\r\n"
            .to_string()
    } else {
        "#!/bin/sh\n\
         git add -A\n\
         git -c user.name=Agent -c user.email=agent@elsewhere.invalid commit -q -m 'agent own commit'\n\
         exit 0\n"
            .to_string()
    };
    let file = if cfg!(windows) {
        dir.join(format!("{name}.cmd"))
    } else {
        dir.join(name)
    };
    std::fs::write(&file, body).expect("the shim is writable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&file).expect("the shim exists").permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&file, p).expect("the mode is settable");
    }
    ro_testkit::FakeBinary::at(dir.to_path_buf(), file, name)
}

/// A stream-json line the parser accepts, carrying text and no plan.
///
/// Shaped like a real Claude turn so the test exercises the production
/// parse path rather than a special case. The text must not contain a
/// fence, or the filler would itself carry a plan and the "is the tail
/// present" question would have no answer.
const FILLER_LINE: &str =
    r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working on it"}]}}"#;

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

/// An agent that writes more than a pipe buffer before exiting must not
/// hang the run, and its whole stream must survive.
///
/// This is the test that proves the pipe-drain fix. On the code before it,
/// `run_with_deadline` polled `try_wait()` and only read the pipes *after*
/// the child exited, so a child that filled the 64 KiB pipe buffer blocked
/// in `write` forever, `try_wait` never returned `Some`, and the run was
/// reported as a timeout after the full deadline — for an agent that had
/// already done all its work.
///
/// Two claims, and the second is the one that is easy to fake. **The run
/// returns promptly** is asserted on the elapsed time. **The whole stream
/// arrives** is asserted by putting the plan at the *end* of 5 MB of
/// filler: a reader that kept only the first 64 KiB would return promptly
/// and find no plan, so `NothingToCommit` here is the failure of a fix that
/// drains to the first buffer boundary and stops.
#[test]
fn an_agent_writing_more_than_a_pipe_buffer_does_not_hang_the_run() {
    let w = Worktree::with_one_commit();
    w.write("a.txt", "x\n");

    // 5 MB, against a 64 KiB pipe buffer. Not a boundary case: the point is
    // that the child cannot possibly fit its output in the buffer, so the
    // only way this returns promptly is if something drained the pipes while
    // the child was still running.
    // The tail is a stream-json *line*, not a bare fence: the Claude
    // parser reads `message.content[].text` and nothing else, so a raw
    // fenced block on its own line is invisible to it. A tail the parser
    // could never read would make this assertion vacuous.
    //
    // Raw strings throughout, so the JSON escapes below are the bytes the
    // child actually writes. Doubling them in a raw string would emit a
    // literal backslash and produce a line no JSON parser accepts, which
    // is the same vacuous assertion wearing a different hat.
    let plan = concat!(
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done\n\n```ro-commits\n"#,
        r#"[{\"subject\":\"the plan at the very end\",\"files\":[\"a.txt\"]}]\n```"}]}}"#,
        "\n",
    );
    let dir = tempfile::TempDir::new().expect("the shim dir is creatable");
    let shim = big_output_agent(dir.path(), "claude", FILLER_LINE, 5 * 1024 * 1024, plan);
    let engine = engine_for(&shim, ro_engine::EngineKind::Claude);

    // A deadline long enough that a wedged run would be caught by the
    // harness rather than by this assertion, and short enough that a wedged
    // run fails the test rather than the suite.
    let ctx = EngineContext::new(w.path(), "main").with_timeout(Duration::from_secs(30));

    let started = std::time::Instant::now();
    let outcome = unsafe { TestEnv::new().shim(&shim).run(|| engine.checkpoint(&ctx)) };
    let elapsed = started.elapsed();

    assert!(
        !matches!(outcome, EngineOutcome::TimedOut { .. }),
        "an agent that finished its work must not be reported as a timeout; \
         the pipes were not drained while it ran. Outcome: {outcome:?}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the run took {elapsed:?}; a run that blocked on a full pipe buffer \
         would take the whole deadline"
    );
    match outcome {
        EngineOutcome::Committed { commits } => {
            assert_eq!(
                commits.len(),
                1,
                "5 MB of filler then one group is one commit"
            );
            assert_eq!(
                commits[0].message, "the plan at the very end",
                "the plan is the LAST thing the agent wrote, so finding it \
                 proves the whole stream arrived rather than the first \
                 buffer's worth"
            );
        }
        other => panic!(
            "the plan at the end of a 5 MB stream must still be found; a \
             reader that stopped at the 64 KiB boundary would report \
             {other:?}"
        ),
    }
}

/// A commit the agent made itself is reported as something ro can push, or
/// not at all — never as a commit ro will publish under someone else's name.
///
/// # The bug
///
/// `BUILTIN_PROMPT` says "Do NOT run `git commit`". Some agent runs it
/// anyway. `checkpoint` notices (`before != after` on HEAD), collects the
/// commits, and returns `EngineOutcome::Committed` — and ro **pushes** them.
/// The `-c user.name/-c user.email` guard covers every commit *ro* makes
/// and nothing else, so the commit on the remote carries whatever identity
/// the agent's own config had. That is the exact outcome the prompt exists
/// to prevent, arriving through the door the identity guard does not cover.
///
/// # What is asserted
///
/// Not "the commit is gone" — that would lose the user's work, which is a
/// worse outcome than the one being fixed. Not "the author was rewritten",
/// which would put ro's name on something it did not write. The commit is
/// **reported as not-yours-to-push**, and the message names both the author
/// found and the identity ro resolved, so the user can decide.
///
/// A user who ran `ro commit` and got a clean "committed" out of it is the
/// harm. A user who is told "the agent committed this as Agent, under
/// another identity — look at it before it goes anywhere" has not been
/// harmed.
#[test]
fn a_commit_the_agent_made_itself_is_not_published_under_its_own_identity() {
    let w = Worktree::with_one_commit();
    w.write("a.txt", "x\n");

    let dir = tempfile::TempDir::new().expect("the shim dir is creatable");
    let shim = self_committing_agent(dir.path(), "claude");
    let engine = engine_for(&shim, ro_engine::EngineKind::Claude);

    let identity = ro_core::CommitIdentity {
        name: "Ro User".into(),
        email: "ro-user@example.com".into(),
    };
    let ctx = EngineContext::new(w.path(), "main")
        .with_identity(Some(&identity))
        .with_timeout(Duration::from_secs(30));

    let outcome = unsafe { TestEnv::new().shim(&shim).run(|| engine.checkpoint(&ctx)) };

    // The commit is on disk either way — assert that first, because a
    // fixture that never committed would make every assertion below pass
    // for the wrong reason.
    let head = std::process::Command::new(ro_testkit::git_path())
        .args(["log", "-1", "--format=%an <%ae>"])
        .current_dir(w.path())
        .output()
        .expect("git runs");
    let author = String::from_utf8_lossy(&head.stdout).trim().to_string();
    assert_eq!(
        author, "Agent <agent@elsewhere.invalid>",
        "this fixture only proves anything if the agent really committed \
         under its own identity, and the work is not destroyed by the check"
    );

    match outcome {
        EngineOutcome::Failed { error, .. } => {
            assert!(
                error.contains("Agent") || error.contains("agent@elsewhere.invalid"),
                "the report must name the author that was found, or the user \
                 cannot tell which commit is being talked about. Got: {error}"
            );
            assert!(
                error.contains("ro-user@example.com"),
                "and it must name the identity ro resolved, so the \
                 difference is visible. Got: {error}"
            );
        }
        other => panic!(
            "a commit authored by someone other than the resolved identity \
             must not be handed back for pushing; got {other:?}"
        ),
    }
}

/// `--message` on an agent engine means ONE commit with that subject.
///
/// # Why this is the whole point of the flag
///
/// `--message` is documented as "One commit per repo, with this subject",
/// and the design says supplying the subject *is* the request: it is the
/// only way to stop an agent from splitting the work. `EngineContext` has
/// `subject_override` for exactly this, and `git_engine.rs` honours it —
/// but the agent path never read the field, so a user who asked for one
/// commit got N. The flag's only documented purpose was inert on the
/// flagship path.
///
/// # What is asserted
///
/// One commit, the user's own subject, and **every** changed file in it.
/// The file list is the part a weaker fix gets wrong: a single commit of
/// only the first group's files satisfies "one commit" and silently drops
/// the rest of the work.
#[test]
fn a_message_override_on_an_agent_engine_is_one_commit_with_every_file() {
    let w = Worktree::with_one_commit();
    w.write("a.txt", "x\n");
    w.write("b.txt", "y\n");
    w.write_in_new_dir("sub", "c.txt", "z\n");

    // An agent that proposes three groups, one file each. If the override
    // is honoured, this is discarded entirely.
    let dir = tempfile::TempDir::new().expect("the shim dir is creatable");
    let shim = big_output_agent(
        dir.path(),
        "claude",
        FILLER_LINE,
        0,
        concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"```ro-commits\n"#,
            r#"[{\"subject\":\"first\",\"files\":[\"a.txt\"]},"#,
            r#"{\"subject\":\"second\",\"files\":[\"b.txt\"]},"#,
            r#"{\"subject\":\"third\",\"files\":[\"sub/c.txt\"]}]"#,
            r#"\n```"}]}}"#,
            "\n",
        ),
    );
    let engine = engine_for(&shim, ro_engine::EngineKind::Claude);

    let identity = ro_core::CommitIdentity {
        name: "Ro User".into(),
        email: "ro-user@example.com".into(),
    };
    let ctx = EngineContext::new(w.path(), "main")
        .with_identity(Some(&identity))
        .with_subject(Some("the user's own words"))
        .with_timeout(Duration::from_secs(30));

    let outcome = unsafe { TestEnv::new().shim(&shim).run(|| engine.checkpoint(&ctx)) };

    let EngineOutcome::Committed { commits } = outcome else {
        panic!("expected a commit, got {outcome:?}");
    };
    assert_eq!(
        commits.len(),
        1,
        "--message is the only way to stop an agent splitting the work, and \
         the agent proposed three. Got {commits:#?}"
    );
    assert_eq!(
        commits[0].message, "the user's own words",
        "the subject is the user's, not one the agent chose"
    );

    // And the commit really carries all three files. A single commit of
    // only the first group's files would pass the assertions above and
    // throw away the rest of the work.
    let files = std::process::Command::new(ro_testkit::git_path())
        .args(["show", "--name-only", "--format=", "HEAD"])
        .current_dir(w.path())
        .output()
        .expect("git runs");
    let listed: Vec<String> = String::from_utf8_lossy(&files.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    for want in ["a.txt", "b.txt", "sub/c.txt"] {
        assert!(
            listed.iter().any(|f| f == want),
            "{want} must be in the single commit; got {listed:?}"
        );
    }
    assert!(
        std::process::Command::new(ro_testkit::git_path())
            .args(["status", "--porcelain"])
            .current_dir(w.path())
            .output()
            .expect("git runs")
            .stdout
            .is_empty(),
        "nothing may be left behind: an override that commits only the \
         first group leaves the rest dirty"
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

// ---- `.git/config` is read before and after, and a change is reported ----

/// A shim that does the one thing the prompt asks it not to, and then fails.
///
/// Failing is deliberate and is what makes the report assertable. The
/// warning itself goes to stderr on **every** path, and a test cannot read
/// that: `libtest` installs a per-test output capture on the test thread,
/// so `eprintln!` inside a test is intercepted and never reaches fd 2 —
/// which is the whole reason a test asserting on real stderr fails for a
/// reason unrelated to what it is checking. The report is *also* appended
/// to a failure that is already being reported, and that travels in the
/// `EngineOutcome`, where a test can read it. So the fixture fails on
/// purpose and the assertion is on the string the user would have seen.
fn config_rewriting_shim(
    dir: &std::path::Path,
    name: &str,
    config_command: &str,
) -> ro_testkit::FakeBinary {
    let body = if cfg!(windows) {
        format!("@echo off\r\n{config_command}\r\necho the agent gave up 1>&2\r\nexit /b 3\r\n")
    } else {
        format!("#!/bin/sh\n{config_command}\necho 'the agent gave up' 1>&2\nexit 3\n")
    };
    let file = if cfg!(windows) {
        dir.join(format!("{name}.cmd"))
    } else {
        dir.join(name)
    };
    std::fs::write(&file, body).expect("the shim is writable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&file).expect("the shim exists").permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&file, p).expect("the mode is settable");
    }
    ro_testkit::FakeBinary::at(dir.to_path_buf(), file, name)
}

/// An agent that rewrites `.git/config` is reported, not silently persisted.
///
/// # The bug
///
/// The module doc promised "ro … hashes `.git/config` before and after so a
/// change is *reported*". There was no such hash anywhere in the codebase —
/// `grep config_hash` finds nothing — so the doc described a control that
/// did not exist. An agent that ran `git config user.email something-else`
/// persisted it into `.git/config` for every commit after it, and ro had
/// already reported success.
///
/// # Why the run is not failed over it
///
/// ro's own commits carry `-c user.name/-c user.email`, so the identity the
/// agent wrote did not leak into anything this run committed. Turning a
/// good commit into a failure is the one thing that teaches users to ignore
/// the report. What was damaged is the user's own future `git commit`, and
/// that is a warning — the same channel `orchestrator.rs` uses for `--onto`.
#[test]
fn an_agent_that_rewrites_git_config_is_reported() {
    let w = Worktree::with_one_commit();
    w.write("a.txt", "x\n");

    let dir = tempfile::TempDir::new().expect("the shim dir is creatable");
    let shim = config_rewriting_shim(
        dir.path(),
        "claude",
        "git config user.email agent@elsewhere.invalid\ngit config user.name Agent",
    );
    let engine = engine_for(&shim, ro_engine::EngineKind::Claude);
    let ctx = EngineContext::new(w.path(), "main").with_timeout(Duration::from_secs(30));

    let outcome = unsafe { TestEnv::new().shim(&shim).run(|| engine.checkpoint(&ctx)) };

    let EngineOutcome::Failed { error, .. } = outcome else {
        panic!("a non-zero exit must be Failed, got {outcome:?}");
    };
    assert!(
        error.contains("user.email"),
        "the report must name the key that changed, so the user knows what to \
         look at. Got: {error}"
    );
    assert!(
        error.contains("agent@elsewhere.invalid"),
        "and what it became, so the user can undo it. Got: {error}"
    );

    // The change really is on disk — otherwise the report is a lie, and a
    // report that cries wolf is worse than no report.
    let config =
        std::fs::read_to_string(w.path().join(".git/config")).expect("the config is readable");
    assert!(
        config.contains("agent@elsewhere.invalid"),
        "the agent's write is still there, which is why it has to be \
         reported: {config}"
    );
}

/// The negative control. Without it, the test above would also pass on a
/// report that fires on every run — and a report that fires on every run is
/// a report nobody reads, which is how the original silence happened.
#[test]
fn an_agent_that_leaves_git_config_alone_is_not_reported() {
    let w = Worktree::with_one_commit();
    w.write("a.txt", "x\n");

    let dir = tempfile::TempDir::new().expect("the shim dir is creatable");
    // The same failing shim, doing nothing to the config.
    let shim = config_rewriting_shim(dir.path(), "claude", "true");
    let engine = engine_for(&shim, ro_engine::EngineKind::Claude);
    let ctx = EngineContext::new(w.path(), "main").with_timeout(Duration::from_secs(30));

    let outcome = unsafe { TestEnv::new().shim(&shim).run(|| engine.checkpoint(&ctx)) };

    let EngineOutcome::Failed { error, .. } = outcome else {
        panic!("a non-zero exit must be Failed, got {outcome:?}");
    };
    assert!(
        !error.contains(".git config"),
        "an untouched config must not be reported: {error}"
    );
    assert!(
        error.contains("the agent gave up"),
        "and the run's own failure is still reported, or the control has \
         eaten the error: {error}"
    );
}

/// A credential the agent writes into `.git/config` is never printed.
///
/// `http.extraheader` holds an `Authorization:` header and any `*.url` can
/// hold `https://user:token@host`. A report that printed those would move a
/// secret out of a file and into a terminal, a log, and quite possibly a bug
/// report — the exact leak `env.rs` exists to prevent, arriving through the
/// one door nobody thought to check.
///
/// The key is still printed. "extraheader" says what to look at, and that
/// is most of what makes the report actionable.
#[test]
fn a_credential_written_into_git_config_is_never_printed() {
    let w = Worktree::with_one_commit();
    w.write("a.txt", "x\n");

    let dir = tempfile::TempDir::new().expect("the shim dir is creatable");
    let shim = config_rewriting_shim(
        dir.path(),
        "claude",
        "git config http.https://github.com/.extraheader \
         'Authorization: Bearer ghp_16C7e42F292c6912E7710c838347Ae178B4a'",
    );
    let engine = engine_for(&shim, ro_engine::EngineKind::Claude);
    let ctx = EngineContext::new(w.path(), "main").with_timeout(Duration::from_secs(30));

    let outcome = unsafe { TestEnv::new().shim(&shim).run(|| engine.checkpoint(&ctx)) };

    let EngineOutcome::Failed { error, .. } = outcome else {
        panic!("a non-zero exit must be Failed, got {outcome:?}");
    };
    assert!(
        error.contains("extraheader"),
        "the key is safe to print and is what makes the report actionable: \
         {error}"
    );
    assert!(
        !error.contains("ghp_16C7e42F292c6912E7710c838347Ae178B4a"),
        "the token must not appear anywhere in the report, got: {error}"
    );
    assert!(
        !error.contains("Bearer"),
        "nor the header with its value: {error}"
    );

    // The token IS on disk, or the redaction is not being asked to do
    // anything — a test that passes because nothing was written proves
    // nothing.
    let config =
        std::fs::read_to_string(w.path().join(".git/config")).expect("the config is readable");
    assert!(
        config.contains("ghp_16C7e42F292c6912E7710c838347Ae178B4a"),
        "the credential really was written, so the redaction is doing the \
         work this test claims: {config}"
    );
}

// ---- a commit failure is never blank ----

/// A commit failure must carry a reason, even when git's reason is on stdout.
///
/// # The bug
///
/// `git commit` writes "nothing to commit, working tree clean" to
/// **stdout**. `ro_git::primitives::commit_all_as` builds its error from
/// stderr alone, so that one failure arrives as `git commit failed: ` and
/// nothing else — the empty string, in the one case where the cause is a
/// fact about the index rather than a git error.
///
/// # The fixture
///
/// An agent proposing two groups, the second naming a file that is already
/// committed and unchanged. The first group commits; staging the second
/// group's file is a no-op, so the index is empty and git refuses. That is
/// an ordinary thing for an agent to propose, and before the fix the report
/// was `committing the agent's work failed: git commit failed:` — a failure
/// with no cause, after the run had already half done its work.
#[test]
fn a_partial_commit_failure_carries_a_reason() {
    let w = Worktree::with_one_commit();
    w.write("a.txt", "x\n");
    // `README.md` is in the base commit and never touched, so a group that
    // names it stages nothing.
    assert!(
        std::path::Path::new(w.path().join("README.md").to_str().unwrap()).exists(),
        "the fixture needs a tracked, unchanged file to name"
    );

    let plan = concat!(
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"```ro-commits\n"#,
        r#"[{\"subject\":\"the group that works\",\"files\":[\"a.txt\"]},"#,
        r#"{\"subject\":\"the group with nothing to stage\",\"files\":[\"README.md\"]}]"#,
        r#"\n```"}]}}"#,
        "\n",
    );
    let dir = tempfile::TempDir::new().expect("the shim dir is creatable");
    let shim = big_output_agent(dir.path(), "claude", FILLER_LINE, 0, plan);
    let engine = engine_for(&shim, ro_engine::EngineKind::Claude);

    let identity = ro_core::CommitIdentity {
        name: "Ro User".into(),
        email: "ro-user@example.com".into(),
    };
    let ctx = EngineContext::new(w.path(), "main")
        .with_identity(Some(&identity))
        .with_timeout(Duration::from_secs(30));

    let outcome = unsafe { TestEnv::new().shim(&shim).run(|| engine.checkpoint(&ctx)) };

    let EngineOutcome::Failed { error, .. } = outcome else {
        panic!("the second group has nothing to commit, so this must fail, got {outcome:?}");
    };

    // The bug, stated as an assertion: the old code produced exactly this.
    assert!(
        !error.trim_end().ends_with("failed:"),
        "a failure whose text ends at the colon has no cause at all, which is \
         the bug. Got: {error:?}"
    );
    assert!(
        error.contains("nothing to commit"),
        "git's own reason must be in the report even though it arrived on \
         stdout. Got: {error}"
    );
    assert!(
        error.contains("README.md"),
        "and the paths the group named, so the user can check them: {error}"
    );

    // The first group really did land — a fix that reported the cause but
    // lost the commit would be a different bug.
    let log = std::process::Command::new(ro_testkit::git_path())
        .args(["log", "--format=%s"])
        .current_dir(w.path())
        .output()
        .expect("git runs");
    let subjects = String::from_utf8_lossy(&log.stdout);
    assert!(
        subjects.contains("the group that works"),
        "the work that did land must not be lost by the failure report: {subjects}"
    );
}

/// The negative control: a failure that *does* carry git's own reason keeps
/// it. Padding a real git error with an explanation of a different failure
/// is how a report stops being believed.
#[test]
fn a_commit_failure_with_a_reason_keeps_it() {
    let w = Worktree::with_one_commit();
    w.write("a.txt", "x\n");

    // A group naming a file that does not exist: `git add` refuses it with
    // a real, specific error of its own.
    let plan = concat!(
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"```ro-commits\n"#,
        r#"[{\"subject\":\"a group naming a file that is not there\",\"files\":[\"nope.txt\"]}]"#,
        r#"\n```"}]}}"#,
        "\n",
    );
    let dir = tempfile::TempDir::new().expect("the shim dir is creatable");
    let shim = big_output_agent(dir.path(), "claude", FILLER_LINE, 0, plan);
    let engine = engine_for(&shim, ro_engine::EngineKind::Claude);
    let ctx = EngineContext::new(w.path(), "main").with_timeout(Duration::from_secs(30));

    let outcome = unsafe { TestEnv::new().shim(&shim).run(|| engine.checkpoint(&ctx)) };

    let EngineOutcome::Failed { error, .. } = outcome else {
        panic!("staging a path that matches nothing must fail, got {outcome:?}");
    };
    assert!(
        error.contains("nope.txt"),
        "git's own reason names the path, and it must survive: {error}"
    );
    assert!(
        !error.contains("nothing to commit"),
        "and it must not be replaced by an explanation of a *different* \
         failure: {error}"
    );
}

// ---- stranded work is reported honestly ----

/// A commit the agent made itself is reported as stranded when it is, and
/// as published when it is not.
///
/// # The bug
///
/// `stranded` was asked about the **base** — "was what ro started from
/// already on a remote?" — and that is the wrong ref. A repo whose remote
/// has the base and nothing the agent added is the ordinary shape of a
/// normal `ro ship`: the base was pushed last time, and this run's work has
/// not gone anywhere. Answering that "published" tells the user their
/// stranded work went out.
///
/// # What is asserted
///
/// The same agent commit, against a remote that has the base and against a
/// remote that has nothing. The first must say the work is stranded; the
/// second must not claim it was published.
#[test]
fn an_agents_own_commit_is_reported_as_stranded_when_it_is() {
    let w = Worktree::with_one_commit();
    w.write("a.txt", "x\n");

    // A remote that has the base and nothing else — the ordinary shape of a
    // repo whose previous run pushed and whose current run has not.
    let remote = ro_testkit::BareRemote::ephemeral();
    w.add_remote("origin", remote.path());
    std::process::Command::new(ro_testkit::git_path())
        .args(["push", "-q", "origin", "main"])
        .current_dir(w.path())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .expect("git runs");

    let dir = tempfile::TempDir::new().expect("the shim dir is creatable");
    let shim = self_committing_agent(dir.path(), "claude");
    let engine = engine_for(&shim, ro_engine::EngineKind::Claude);

    let identity = ro_core::CommitIdentity {
        name: "Ro User".into(),
        email: "ro-user@example.com".into(),
    };
    let ctx = EngineContext::new(w.path(), "main")
        .with_identity(Some(&identity))
        .with_timeout(Duration::from_secs(30));

    let outcome = unsafe { TestEnv::new().shim(&shim).run(|| engine.checkpoint(&ctx)) };

    match outcome {
        EngineOutcome::Failed { error, .. } => {
            assert!(
                error.contains("stranded"),
                "the work is local and unreachable, and the report must say \
                 so. Got: {error}"
            );
            assert!(
                !error.contains("already reachable from a remote"),
                "and it must not claim the opposite: {error}"
            );
        }
        other => panic!("a foreign commit must not be handed back for pushing, got {other:?}"),
    }

    // And it really is stranded: the remote has the base and nothing above
    // it.
    assert!(
        !remote.branches().is_empty(),
        "the remote has the base, which is the whole point of this fixture"
    );
    let head = std::process::Command::new(ro_testkit::git_path())
        .args(["log", "-1", "--format=%H"])
        .current_dir(w.path())
        .output()
        .expect("git runs");
    let local_tip = String::from_utf8_lossy(&head.stdout).trim().to_string();
    let remote_tip = std::process::Command::new(ro_testkit::git_path())
        .args(["log", "-1", "--format=%H", "origin/main"])
        .current_dir(w.path())
        .output()
        .expect("git runs");
    assert_ne!(
        local_tip,
        String::from_utf8_lossy(&remote_tip.stdout).trim(),
        "the local tip must not be on the remote, or this fixture is not \
         testing what it claims to"
    );
}

/// The other half: a commit that *is* on a remote is not called stranded.
/// Reporting a published commit as stranded sends the user hunting for a
/// push that already happened.
#[test]
fn an_agents_own_commit_is_not_called_stranded_when_it_is_published() {
    let w = Worktree::with_one_commit();
    w.write("a.txt", "x\n");

    let remote = ro_testkit::BareRemote::ephemeral();
    w.add_remote("origin", remote.path());

    // The agent commits **and pushes** — the worst case, and the one the
    // "published" wording exists for. The push has to be the agent's, inside
    // the run: a push made afterwards would land after the check, so the
    // check would correctly say "stranded" and the test would be asserting
    // the wrong moment.
    let dir = tempfile::TempDir::new().expect("the shim dir is creatable");
    let body = if cfg!(windows) {
        "@echo off\r\n\
         git add -A\r\n\
         git -c user.name=Agent -c user.email=agent@elsewhere.invalid commit -q -m \"agent own commit\"\r\n\
         git push -q origin main\r\n\
         exit /b 0\r\n"
            .to_string()
    } else {
        "#!/bin/sh\n\
         git add -A\n\
         git -c user.name=Agent -c user.email=agent@elsewhere.invalid commit -q -m 'agent own commit'\n\
         git push -q origin main\n\
         exit 0\n"
            .to_string()
    };
    let file = if cfg!(windows) {
        dir.path().join("claude.cmd")
    } else {
        dir.path().join("claude")
    };
    std::fs::write(&file, body).expect("the shim is writable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&file).expect("the shim exists").permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&file, p).expect("the mode is settable");
    }
    let shim = ro_testkit::FakeBinary::at(dir.path().to_path_buf(), file, "claude");
    let engine = engine_for(&shim, ro_engine::EngineKind::Claude);

    let identity = ro_core::CommitIdentity {
        name: "Ro User".into(),
        email: "ro-user@example.com".into(),
    };
    let ctx = EngineContext::new(w.path(), "main")
        .with_identity(Some(&identity))
        .with_timeout(Duration::from_secs(30));

    let outcome = unsafe { TestEnv::new().shim(&shim).run(|| engine.checkpoint(&ctx)) };

    // The fixture only proves anything if the push really happened, so read
    // the remote back rather than trusting the shim.
    assert!(
        remote.has_branch("main"),
        "this fixture only means anything if the agent's commit reached the \
         remote"
    );

    match outcome {
        EngineOutcome::Failed { error, .. } => {
            assert!(
                error.contains("already reachable from a remote"),
                "the work has been published under that identity, and the \
                 report must say so rather than calling it stranded. Got: \
                 {error}"
            );
            assert!(
                !error.contains("stranded locally"),
                "and it must not claim the opposite: {error}"
            );
        }
        other => panic!("a foreign commit must not be handed back for pushing, got {other:?}"),
    }
}

/// HEAD moving while ro enumerates nothing is not "committed nothing".
///
/// `EngineOutcome::render` turns an empty `Committed` into exactly that
/// string, and it would be false: the agent reset the branch, or amended,
/// or moved the ref somewhere ro cannot enumerate commits from. The user is
/// told nothing happened while the ref they were working on changed
/// underneath them.
#[test]
fn a_moved_head_with_nothing_enumerable_is_not_reported_as_committed_nothing() {
    // Two commits, so `HEAD~1` exists. The agent resets back to the first:
    // HEAD moves, the tree is clean, and there is nothing between the two
    // refs for `commits_between` to enumerate.
    let w = Worktree::with_one_commit();
    w.write("a.txt", "x\n");
    w.commit("the tip the agent will undo");
    let dir = tempfile::TempDir::new().expect("the shim dir is creatable");
    let body = if cfg!(windows) {
        "@echo off\r\n\
         git reset -q --hard HEAD~1\r\n\
         exit /b 0\r\n"
            .to_string()
    } else {
        "#!/bin/sh\n\
         git reset -q --hard HEAD~1\n\
         exit 0\n"
            .to_string()
    };
    let file = if cfg!(windows) {
        dir.path().join("claude.cmd")
    } else {
        dir.path().join("claude")
    };
    std::fs::write(&file, body).expect("the shim is writable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&file).expect("the shim exists").permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&file, p).expect("the mode is settable");
    }
    let shim = ro_testkit::FakeBinary::at(dir.path().to_path_buf(), file, "claude");
    let engine = engine_for(&shim, ro_engine::EngineKind::Claude);
    let ctx = EngineContext::new(w.path(), "main").with_timeout(Duration::from_secs(30));

    let outcome = unsafe { TestEnv::new().shim(&shim).run(|| engine.checkpoint(&ctx)) };

    match outcome {
        EngineOutcome::Failed { error, class } => {
            assert!(
                error.contains("could not enumerate"),
                "the report must say that ro could not name what happened, \
                 rather than reporting nothing. Got: {error}"
            );
            assert!(
                !EngineOutcome::Failed { error, class }
                    .render()
                    .contains("committed nothing"),
                "and it must never render as a success"
            );
        }
        other => panic!(
            "a moved HEAD with nothing enumerable must not be reported as a \
             clean run, got {other:?}"
        ),
    }
}
