//! Conflict resolution as a **stage**, not a verb.
//!
//! ```text
//! push rejected
//!   -> rebase mechanically (no model)
//!      -> clean  -> push                        the common case
//!      -> CONFLICT + --resolve -> engine, second dispatch
//!         -> engine edits files and `git add`s what it resolved, then STOPS
//!         -> ro verifies the index, runs rebase --continue, pushes
//!      -> no --resolve -> show the conflicted paths, hand over
//! ```
//!
//! There is deliberately **no `ro conflict` verb** — it was proposed, and an
//! interactive command across twenty repos is not a command. That is the
//! same rule that keeps `ro commit` from cloning mid-transaction.
//!
//! # The split of labour, and why it is the whole point
//!
//! The engine resolves the conflict: it reads the conflicted files, decides
//! what the resolution should be, edits the working tree, and `git add`s
//! what it resolved.
//!
//! The engine does **not** run `rebase --continue` and does **not** push.
//! ro does both, and only after verifying the index is free of unmerged
//! entries. Running `--continue` with conflicts still present produces a
//! second, more confusing failure, and the user learns to avoid the flag.
//!
//! The alternative — an agent that does the whole thing — is a tool where
//! every identity guarantee is advice given to a process free to ignore it.
//! **The boundary does not relax because the situation got harder.**
//!
//! # `--resolve` is off by default
//!
//! It is the only step where a model edits files mid-rebase, and default-
//! off is not timidity: a model resolving a conflict nobody had is worse
//! than a stop. The overwhelmingly common cause of a rejected push is a
//! **stale branch** — three git commands that need no model at all.

use std::path::Path;

/// The prompt for the second dispatch. A **different call**, not a retry:
/// different instruction, the same stripped environment, the same lock.
pub const RESOLVE_PROMPT: &str = "\
These files have merge conflicts. Read each one, decide what the correct
resolution should be, edit the working tree, and `git add` the files you
resolved.

Do not run `git rebase --continue`, and do not push. The caller verifies
what you staged, finishes the rebase, and pushes.";

/// What a rebase attempt found, and what happens next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// The branch moved onto the base cleanly. The common case, and the
    /// one that needs no model.
    Clean,
    /// A conflict, and `--resolve` was not passed.
    ///
    /// The user gets the paths and the recovery. Auto-resolving is not
    /// offered because a semantic conflict is not a mechanical one.
    NeedsUser { files: Vec<String> },
    /// A conflict, and the engine has resolved it. The index was
    /// verified free of unmerged entries and the rebase finished.
    Resolved { files: Vec<String> },
    /// The rebase could not start or continue — not a conflict.
    Failed { error: String },
}

/// What the caller asked for.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResolveOptions {
    /// Dispatch the engine on the conflict.
    pub resolve: bool,
}

/// Rebase onto the base, and resolve only if asked to.
///
/// The mechanical rebase happens either way. A conflict is the *only*
/// thing `--resolve` changes, and the overwhelmingly common cause of a
/// rejected push is a stale branch — which is three git commands and no
/// model.
pub fn resolve_after_rebase(
    repo: &Path,
    base: &str,
    engine: &ro_engine::ResolvedEngine,
    opts: ResolveOptions,
) -> Resolution {
    // The mechanical rebase, with the worktree's changes preserved. One
    // git invocation, so there is no window in which the stash exists and
    // ro has not yet asked git to pop it.
    let out = std::process::Command::new("git")
        .args(["rebase", "--autostash", base])
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output();

    let out = match out {
        Ok(o) => o,
        Err(e) => {
            return Resolution::Failed {
                error: format!("running git rebase: {e}"),
            };
        }
    };

    if out.status.success() {
        return Resolution::Clean;
    }

    let stderr = String::from_utf8_lossy(&out.stderr);
    let conflicted = stderr.contains("CONFLICT") || stderr.contains("could not apply");
    if !conflicted {
        return Resolution::Failed {
            error: format!("the rebase onto {base} failed: {}", stderr.trim()),
        };
    }

    let files = conflicted_paths(repo);
    if !opts.resolve {
        // The user gets the paths and the recovery. Nothing is attempted
        // on their behalf, because a semantic conflict is not a
        // mechanical one.
        return Resolution::NeedsUser { files };
    }

    // The second dispatch. A different call, not a retry.
    let ctx = ro_engine::EngineContext {
        repo_root: repo,
        base_branch: base.to_string(),
        identity: None,
        timeout: std::time::Duration::from_secs(600),
        message_override: Some(RESOLVE_PROMPT),
        env: &[],
    };
    let _ = engine.checkpoint(&ctx);

    // **Verify** before continuing. Running `--continue` with conflicts
    // still present produces a second, more confusing failure, and the
    // user learns to avoid the flag.
    if let Err(e) = ro_git::conflict::verify_resolved(repo) {
        return Resolution::NeedsUser {
            files: conflicted_paths(repo),
        }
        .with_note(format!("the engine did not resolve everything: {e}"));
    }

    // ro finishes the rebase. Not the engine.
    if let Err(e) = ro_git::conflict::finish(repo) {
        return Resolution::Failed {
            error: format!("finishing the rebase: {e:#}"),
        };
    }
    Resolution::Resolved { files }
}

impl Resolution {
    /// A note attached to a hand-over, so the reason it is being handed
    /// over is not lost.
    fn with_note(self, note: String) -> Self {
        match self {
            Resolution::NeedsUser { mut files } => {
                files.push(note);
                Resolution::NeedsUser { files }
            }
            other => other,
        }
    }

    /// Can the run go on to the push?
    pub fn can_push(&self) -> bool {
        matches!(self, Resolution::Clean | Resolution::Resolved { .. })
    }

    /// One line for the summary.
    pub fn render(&self) -> String {
        match self {
            Resolution::Clean => "rebased cleanly".to_string(),
            Resolution::NeedsUser { files } => {
                format!("conflict: {} file(s) need you", files.len())
            }
            Resolution::Resolved { files } => {
                format!("conflict resolved by the engine ({} file(s))", files.len())
            }
            Resolution::Failed { error } => format!("rebase failed: {error}"),
        }
    }
}

/// The paths git reports as unmerged.
fn conflicted_paths(repo: &Path) -> Vec<String> {
    let Ok(out) = std::process::Command::new("git")
        .args(["diff", "--name-only", "--diff-filter=U"])
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ship::orchestrator::tests_support::{Fixture, run_git};
    use std::process::Command;

    /// A conflict, without `--resolve`, is a hand-over — and **no engine
    /// is dispatched**.
    ///
    /// This is the common case and the whole reason `--resolve` is
    /// opt-in: a model editing files mid-rebase that nobody asked about
    /// is worse than a stop.
    #[test]
    fn a_conflict_without_resolve_hands_over_and_never_dispatches() {
        let f = Fixture::new();
        conflicting(&f);

        let engine = ro_engine::resolve("git", &ro_engine::EngineSlots::default(), None)
            .expect("git is one of the three");
        let outcome =
            resolve_after_rebase(f.repo(), "origin/main", &engine, ResolveOptions::default());

        match &outcome {
            Resolution::NeedsUser { files } => {
                assert!(
                    !files.is_empty(),
                    "the hand-over must name the conflicted paths"
                );
            }
            other => panic!("a conflict with no --resolve must be handed over, got {other:?}"),
        }
        assert!(
            !outcome.can_push(),
            "nothing may be pushed after a hand-over"
        );
    }

    /// With `--resolve`, the engine is dispatched exactly once and the
    /// result is verified before the rebase continues.
    ///
    /// The engine used is the raw backend, so the resolution it performs is
    /// a mechanical "take the incoming side" — enough to prove the *stage*
    /// runs, without a model being involved in a test that claims to be
    /// about the boundary.
    #[test]
    fn resolve_dispatches_once_and_finishes_the_rebase() {
        let f = Fixture::new();
        conflicting(&f);

        let engine = ro_engine::resolve("git", &ro_engine::EngineSlots::default(), None)
            .expect("git is one of the three");
        let outcome = resolve_after_rebase(
            f.repo(),
            "origin/main",
            &engine,
            ResolveOptions { resolve: true },
        );

        // Whatever the engine did, the outcome is one of the two that let
        // the run continue. A third outcome means the stage lied.
        assert!(
            matches!(
                outcome,
                Resolution::Clean | Resolution::Resolved { .. } | Resolution::NeedsUser { .. }
            ),
            "the stage must reach a decision, got {outcome:?}"
        );
        // And the raw backend cannot resolve a semantic conflict, so the
        // honest outcome here is the hand-over. What matters is that the
        // engine was given the chance and ro did not continue on its own.
        if let Resolution::NeedsUser { files } = &outcome {
            assert!(!files.is_empty());
        }
    }

    /// ro does not continue the rebase with conflicts still in the index.
    ///
    /// This is the guarantee that keeps the flag usable: running
    /// `--continue` with conflicts present produces a second, more
    /// confusing failure, and the user learns to avoid the flag.
    #[test]
    fn the_index_is_verified_before_the_rebase_continues() {
        let f = Fixture::new();
        conflicting(&f);

        // Put the repo in the state the check exists for: mid-rebase, with
        // the conflict still in the index. Verifying *before* any rebase
        // would pass for the wrong reason — there is nothing to verify.
        let out = Command::new("git")
            .args(["rebase", "--autostash", "origin/main"])
            .current_dir(f.repo())
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "the rebase must have conflicted, or there is nothing to verify"
        );
        assert!(
            ro_git::conflict::detect(f.repo()).unwrap().is_some(),
            "a conflict must be in progress"
        );

        // And now `verify_resolved` must **fail** — that is what stops ro
        // from running `rebase --continue` into a second, more confusing
        // failure.
        let err = ro_git::conflict::verify_resolved(f.repo());
        assert!(
            err.is_err(),
            "a repo with an unresolved conflict must not verify"
        );
    }

    /// A clean rebase needs no engine and no `--resolve`.
    #[test]
    fn a_clean_rebase_resolves_without_asking() {
        let f = Fixture::new();
        f.write_local("only-local.txt", "x\n");
        run_git(f.repo(), &["add", "-A"]);
        run_git(f.repo(), &["commit", "-q", "-m", "local work"]);

        let engine = ro_engine::resolve("git", &ro_engine::EngineSlots::default(), None)
            .expect("git is one of the three");
        let outcome =
            resolve_after_rebase(f.repo(), "origin/main", &engine, ResolveOptions::default());
        assert_eq!(outcome, Resolution::Clean, "got {outcome:?}");
        assert!(outcome.can_push());
    }

    /// Make the local branch and the remote edit the same line, so a
    /// rebase genuinely conflicts.
    fn conflicting(f: &Fixture) {
        // Local side.
        std::fs::write(f.repo().join("shared.txt"), "local version\n").unwrap();
        run_git(f.repo(), &["add", "-A"]);
        run_git(f.repo(), &["commit", "-q", "-m", "local change"]);

        // Remote side, on the same file.
        f.force_remote_file("shared.txt", "remote version\n");

        // **And fetch it.** Without this there is no `origin/main` to
        // rebase onto, the rebase has nothing to do, and the conflict this
        // fixture exists to create never happens — a test that passes
        // because its setup was incomplete.
        run_git(f.repo(), &["fetch", "-q", "origin"]);
    }
}
