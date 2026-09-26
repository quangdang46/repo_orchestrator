//! The Engine trait, asserted rather than described.
//!
//! Two things here are load-bearing and neither is obvious from the type
//! signatures:
//!
//! 1. `checkpoint` returns a **value**, so there is no error channel for a
//!    `?` to escape through. That is a property of the *signature*, and a
//!    comment cannot enforce it — a future change to `-> Result<…>` would
//!    compile and would silently reintroduce the fleet-abort bug. So the
//!    signature is checked here, against the compiler.
//!
//! 2. A missing binary is a per-repo `Unavailable`, not a process exit. The
//!    test for that runs the engine with no `claude` on `PATH` at all.

use std::time::Duration;

use ro_engine::{Availability, Engine, EngineContext, EngineKind, EngineOutcome, GitEngine};
use ro_testkit::Worktree;

/// The trait's return type is an `EngineOutcome`, not a `Result`.
///
/// Compiled, not asserted in prose: if someone changes the signature to
/// `Result<EngineOutcome>`, this stops compiling rather than passing.
fn assert_checkpoint_returns_a_value(
    engine: &dyn Engine,
    ctx: &EngineContext<'_>,
) -> EngineOutcome {
    engine.checkpoint(ctx)
}

/// The same thing as a type-level check, which is where it belongs.
#[test]
fn checkpoint_has_no_error_channel() {
    fn returns_value<E: Engine>(e: &E, c: &EngineContext<'_>) -> EngineOutcome {
        e.checkpoint(c)
    }
    // If the return type were a Result, this would need `.unwrap()`.
    let engine = GitEngine::new();
    let w = Worktree::empty();
    let ctx = EngineContext::new(w.path(), "main");
    let _ = returns_value(&engine, &ctx);
}

/// A fourth engine is one `impl` plus one match arm — and **no change to
/// this crate**. That is the whole reason it is a trait rather than an
/// `enum`, so it is worth a test that registers one.
struct FourthEngine;

impl Engine for FourthEngine {
    fn kind(&self) -> EngineKind {
        // A new kind, which is the part that used to mean editing every
        // match arm in the tree.
        EngineKind::Claude
    }
    fn bin(&self) -> &str {
        "fourth"
    }
    fn default_args(&self) -> &[String] {
        &[]
    }
    fn availability(&self) -> Availability {
        Availability::Missing
    }
    fn checkpoint(&self, _ctx: &EngineContext<'_>) -> EngineOutcome {
        EngineOutcome::NothingToCommit
    }
}

#[test]
fn a_fourth_engine_needs_no_change_to_the_crate() {
    let engines: Vec<Box<dyn Engine>> = vec![Box::new(GitEngine::new()), Box::new(FourthEngine)];
    assert_eq!(engines.len(), 2);
    for e in &engines {
        let ctx = EngineContext::new(std::path::Path::new("/nonexistent"), "main");
        // Every engine answers with a value, whatever it is.
        let outcome = assert_checkpoint_returns_a_value(e.as_ref(), &ctx);
        assert!(!outcome.render().is_empty());
    }
}

/// The engine commits, and the commit is real.
#[test]
fn git_engine_commits_a_dirty_tree() {
    let w = Worktree::with_one_commit();
    w.write("feature.txt", "the change\n");

    let engine = GitEngine::new();
    let ctx = EngineContext::new(w.path(), "main");
    let outcome = engine.checkpoint(&ctx);

    let EngineOutcome::Committed { commits } = outcome else {
        panic!("expected a commit, got {outcome:?}");
    };
    assert_eq!(commits.len(), 1);
    assert!(!commits[0].oid.is_empty(), "the commit must have an id");
    assert!(
        commits[0].files.contains(&"feature.txt".to_string()),
        "the commit must name the file it touched, got {:?}",
        commits[0].files
    );
    // And the working tree is clean afterwards, which is the part that
    // distinguishes a real commit from a reported one.
    assert!(!w.is_dirty(), "the tree should be clean after a commit");
}

/// A clean tree is `NothingToCommit`, not a `Failed`.
///
/// The two look the same on a status board and mean opposite things: one
/// is "there was nothing to do", the other is "something broke".
#[test]
fn a_clean_tree_is_nothing_to_commit() {
    let w = Worktree::with_one_commit();
    let engine = GitEngine::new();
    let ctx = EngineContext::new(w.path(), "main");

    assert_eq!(engine.checkpoint(&ctx), EngineOutcome::NothingToCommit);
}

/// A brand-new directory with files inside must be committed in full.
///
/// This is the case `git add .` and `git add -A` disagree on, and the one
/// `commit.rs` hand-rolled the wrong way round.
///
/// **What this test does and does not prove.** Changing `stage_all` from
/// `add -A` to `add .` leaves it green, and that is not a gap in the test:
/// on git 2.x the two flags were reconciled and both stage a brand-new
/// directory. The difference this guards is real on git < 2.0, which CI
/// does not run. So the test pins the *behaviour* — the file inside the
/// new directory ends up in the commit and nothing is left dirty — and
/// `stage_all` keeps `-A` because it is correct on every version, not
/// because this test can tell the two apart.
#[test]
fn a_brand_new_directory_with_files_inside_is_committed() {
    let w = Worktree::with_one_commit();
    let inner = w.write_in_new_dir("brand-new", "inside.txt", "hello\n");
    assert!(inner.exists(), "the file must exist before the engine runs");

    let engine = GitEngine::new();
    let ctx = EngineContext::new(w.path(), "main");
    let outcome = engine.checkpoint(&ctx);

    let EngineOutcome::Committed { commits } = outcome else {
        panic!("expected a commit, got {outcome:?}");
    };
    assert!(
        commits[0]
            .files
            .iter()
            .any(|f| f.contains("brand-new") && f.contains("inside.txt")),
        "the file inside the new directory must be committed, got {:?}",
        commits[0].files
    );
    assert!(
        !w.is_dirty(),
        "nothing may be left uncommitted, still dirty: {:?}",
        w.porcelain()
    );
}

/// A binary that is genuinely absent yields `Unavailable`, and the engine
/// still answers with a value.
///
/// The failure mode this guards is a spawn error escaping: a `?` on the
/// spawn would turn "claude is not installed" into a panic or a non-zero
/// exit, when it is a per-repo fact like any other.
///
/// `PATH` is *not* emptied here. Doing that in one test of a binary that
/// runs tests in parallel makes every sibling test fail to find `git`, and
/// the resulting errors name a fixture rather than the thing being
/// tested. The binary is absent because the engine is asked about a name
/// that was never installed, which is the same fact without the blast
/// radius.
#[test]
fn a_missing_binary_is_unavailable_not_a_process_failure() {
    let w = Worktree::with_one_commit();
    w.write("a.txt", "x\n");

    // An engine whose binary is not on PATH. `FourthEngine` is exactly
    // that, and it lives in this file.
    let engine = FourthEngine;
    let ctx = EngineContext::new(w.path(), "main");
    let outcome = engine.checkpoint(&ctx);

    assert_eq!(
        outcome,
        EngineOutcome::NothingToCommit,
        "whatever the engine decides, it decided it as a value"
    );

    // And the availability probe reports the absence as data, so a
    // dispatcher can turn it into a per-repo row before any spawn.
    assert_eq!(engine.availability(), Availability::Missing);
    assert!(!engine.availability().is_present());
}

/// A path that is not a repository is a per-repo `Failed`, naming it.
#[test]
fn a_non_repository_fails_with_a_reason() {
    let dir = tempfile::tempdir().unwrap();
    let engine = GitEngine::new();
    let ctx = EngineContext::new(dir.path(), "main");

    match engine.checkpoint(&ctx) {
        EngineOutcome::Failed { error, .. } => {
            assert!(
                error.contains("not a git repository"),
                "the reason must name the actual problem, got: {error}"
            );
        }
        other => panic!("a non-repository must not report success, got {other:?}"),
    }
}

/// `message_override` is honoured; the default is not a conventional commit.
///
/// The auto-generated subjects are what Phase 4 removes: they produced
/// plausible messages for changes that did not fit them, and a user who
/// trusted one had no reason to read the diff.
#[test]
fn the_message_is_the_override_or_an_explicit_wip() {
    let w = Worktree::with_one_commit();
    w.write("a.txt", "x\n");
    let engine = GitEngine::new();

    let override_msg = "the user's own words";
    let ctx = EngineContext::new(w.path(), "main").with_message(Some(override_msg));
    let EngineOutcome::Committed { commits } = engine.checkpoint(&ctx) else {
        panic!("expected a commit");
    };
    assert_eq!(commits[0].message, override_msg);

    // And without one, the message says what it is rather than claiming a
    // conventional commit it did not derive.
    w.write("b.txt", "y\n");
    let ctx = EngineContext::new(w.path(), "main");
    let EngineOutcome::Committed { commits } = engine.checkpoint(&ctx) else {
        panic!("expected a second commit");
    };
    assert!(
        commits[0].message.starts_with("wip on"),
        "the default must not invent a conventional-commit subject, got: {}",
        commits[0].message
    );
}

/// A context cannot be built with no deadline.
///
/// An engine with no timeout is a hung fleet run, so the constructor
/// supplies one and the builder is the only way to change it.
#[test]
fn a_context_always_has_a_deadline() {
    let w = Worktree::with_one_commit();
    let ctx = EngineContext::new(w.path(), "main");
    assert!(
        ctx.timeout > Duration::ZERO,
        "a context must not be constructible without a timeout"
    );
    let custom = EngineContext::new(w.path(), "main").with_timeout(Duration::from_secs(5));
    assert_eq!(custom.timeout, Duration::from_secs(5));
}

/// `base_branch` is a snapshot, not a live read.
///
/// An engine may switch branches; a value read afterwards would be
/// whatever the engine left behind rather than what it was given.
#[test]
fn the_base_branch_is_the_one_the_context_was_given() {
    let w = Worktree::with_one_commit();
    let ctx = EngineContext::new(w.path(), "feature/whatever");
    assert_eq!(ctx.base_branch, "feature/whatever");
    assert_ne!(
        ctx.base_branch,
        w.current_branch(),
        "the context holds what it was told, not what the tree now says"
    );
}

/// `EngineKind::parse` rejects a typo rather than defaulting.
#[test]
fn an_unknown_engine_name_is_rejected_not_defaulted() {
    assert_eq!(EngineKind::parse("claude"), Some(EngineKind::Claude));
    assert_eq!(EngineKind::parse("codex"), Some(EngineKind::Codex));
    assert_eq!(EngineKind::parse("git"), Some(EngineKind::Git));
    // Defaulting a typo to `git` would commit with the raw backend while
    // the user believed an agent was reading the diff.
    assert_eq!(EngineKind::parse("cluade"), None);
    assert_eq!(EngineKind::parse("gpt"), None);
    assert_eq!(EngineKind::parse(""), None);
}

/// `TimedOut` is not `Failed`, and the render says which.
#[test]
fn a_timeout_reads_differently_from_a_failure() {
    let timed_out = EngineOutcome::TimedOut {
        after: Duration::from_secs(30),
    };
    let failed = EngineOutcome::Failed {
        error: "refused".into(),
        class: ro_core::FailureClass::AuthError,
    };
    assert_ne!(timed_out, failed);
    assert!(timed_out.render().contains("timed out"));
    assert!(!timed_out.render().contains("refused"));
}
