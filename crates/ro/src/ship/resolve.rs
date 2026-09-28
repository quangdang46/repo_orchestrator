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
//! The mechanical rebase is the **caller's** — `run_one` does it, because
//! the rebase is where the base is chosen and `--onto` is honoured. This
//! module is entered with a conflict already in the index, and it never
//! rebases: a second `git rebase` against a mid-rebase tree is `fatal: It
//! seems that there is already a rebase-merge directory`, exit 128, and a
//! stage that dispatches no engine at all.
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

use ro_engine::EngineOutcome;

/// The prompt for the second dispatch. A **different call**, not a retry:
/// different instruction, the same stripped environment, the same lock.
pub const RESOLVE_PROMPT: &str = "\
These files have merge conflicts. Read each one, decide what the correct
resolution should be, edit the working tree, and `git add` the files you
resolved.

Do not run `git rebase --continue`, and do not push. The caller verifies
what you staged, finishes the rebase, and pushes.";

/// What the conflict stage found, and what happens next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// A conflict, and `--resolve` was not passed.
    ///
    /// The user gets the paths and the recovery. Auto-resolving is not
    /// offered because a semantic conflict is not a mechanical one.
    NeedsUser { files: Vec<String> },
    /// A conflict, and the engine has resolved it. The index was
    /// verified free of unmerged entries and the rebase finished.
    Resolved { files: Vec<String> },
    /// The engine could not run, or the rebase could not continue — not a
    /// conflict, and not the user's to resolve by hand.
    Failed { error: String },
}

/// What the caller asked for.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResolveOptions {
    /// Dispatch the engine on the conflict.
    pub resolve: bool,
}

/// What the tree looks like when this stage gives up, and what to do about
/// it.
///
/// Every `Failed` after the dispatch leaves a mid-rebase tree, and a
/// message that does not say so leaves the user finding it out from the
/// next `git status` — the one moment when a message is not on screen.
const MID_REBASE: &str = " Nothing was committed or pushed, and the tree is \
     still mid-rebase — resolve it and run `git rebase --continue`, or \
     `git rebase --abort` to go back.";

/// The conflict stage, for a rebase that **has** conflicted.
///
/// The mechanical rebase is the caller's, and it has already stopped: by
/// the time a conflict needs handling the tree is mid-rebase, and asking
/// git to rebase it again is not a second attempt — it is `fatal: It seems
/// that there is already a rebase-merge directory`, exit 128, and a stage
/// that dispatches no engine at all.
///
/// The tree is left mid-rebase on purpose. Aborting would discard the work
/// that caused the conflict, which is the one thing a tool holding
/// someone's uncommitted changes must not do.
pub fn resolve_conflict(
    repo: &Path,
    base: &str,
    engine: &ro_engine::ResolvedEngine,
    opts: ResolveOptions,
) -> Resolution {
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
        subject_override: None,
        env: &[],
    };

    // **The engine's own answer**, not `let _ =`. An engine that is not
    // installed, that timed out, or that could not be spawned is not a
    // conflict, and reporting it as one tells the user to go and read
    // conflict markers that a missing binary created. A hand-over is a
    // request for a *decision*; this is a broken tool, and the two must
    // never render as the same line.
    match engine.checkpoint(&ctx) {
        // It ran. Whether it changed anything is the index's business,
        // not the engine's: both of these fall through to the check.
        EngineOutcome::Committed { .. } | EngineOutcome::NothingToCommit => {}
        EngineOutcome::Unavailable { binary, hint } => {
            return Resolution::Failed {
                error: format!("resolving the conflict: {binary} is not installed. {hint}{MID_REBASE}"),
            };
        }
        EngineOutcome::TimedOut { after } => {
            return Resolution::Failed {
                error: format!(
                    "resolving the conflict: the engine did not finish within \
                     {after:?} and was killed.{MID_REBASE}"
                ),
            };
        }
        EngineOutcome::Failed { error, .. } => {
            return Resolution::Failed {
                error: format!("resolving the conflict: {error}.{MID_REBASE}"),
            };
        }
    }

    // **Verify** before continuing. Running `--continue` with conflicts
    // still present produces a second, more confusing failure, and the
    // user learns to avoid the flag.
    if let Err(e) = index_is_clear(repo) {
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

/// Is the index ready for `rebase --continue`?
///
/// Deliberately **not** `ro_git::conflict::verify_resolved`. That answers
/// "is this repository finished", and the honest answer here is no: the
/// rebase is in progress, which is the entire point of the stage. It
/// rejected a perfectly resolved conflict with `OperationStillInProgress`,
/// so every engine that did its job was reported as having done nothing
/// and the user was handed a conflict the engine had already settled.
///
/// What `--continue` actually needs is a clean index — no unmerged entries,
/// and no conflict markers left behind in the worktree.
fn index_is_clear(repo: &Path) -> Result<(), String> {
    let unmerged = conflicted_paths(repo);
    if !unmerged.is_empty() {
        return Err(format!(
            "{} unmerged path(s) remain: {}",
            unmerged.len(),
            unmerged.join(", ")
        ));
    }
    let marked = files_with_conflict_markers(repo);
    if !marked.is_empty() {
        return Err(format!(
            "{} file(s) still hold conflict markers: {}",
            marked.len(),
            marked.join(", ")
        ));
    }
    Ok(())
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
    ///
    /// One answer, and that is the point: this stage is only reached with
    /// a conflict already in the index, so "no conflict" is not an
    /// outcome it can return. Every answer that is not `Resolved` stops
    /// the run for that repo — a hand-over for the user, a failure for
    /// the tool.
    pub fn can_push(&self) -> bool {
        matches!(self, Resolution::Resolved { .. })
    }

    /// One line for the summary.
    pub fn render(&self) -> String {
        match self {
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

/// The tracked files whose worktree copy still holds conflict markers.
///
/// The index is not the only place a marker survives. An engine that
/// resolves a file by writing the markers back, or that `git add`s a file
/// it did not actually settle, leaves the markers in the worktree — and
/// `rebase --continue` will commit them, because a staged file is a
/// resolved file as far as git is concerned. The failure is silent and it
/// is the worst one available: a conflict marker published to everyone.
///
/// `ls-files` gives the tracked set, so an untracked scratch file parked
/// in the tree is never read, and a repository full of them costs one
/// `ls-files` rather than a walk.
fn files_with_conflict_markers(repo: &Path) -> Vec<String> {
    let Ok(out) = std::process::Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    out.stdout
        .split(|&b| b == 0)
        .filter(|raw| !raw.is_empty())
        .filter_map(|raw| {
            let name = String::from_utf8_lossy(raw).into_owned();
            has_conflict_markers(&repo.join(&name)).then_some(name)
        })
        .collect()
}

/// Does this file hold an unresolved conflict?
///
/// The **full triple**, anchored at the start of a line. A single `<<<<<<<`
/// is not evidence of anything: documentation, a fixture, and source code
/// that mentions the marker all contain one, and a check that flagged those
/// would hand back a conflict nobody has.
fn has_conflict_markers(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    // Binary. A NUL in the first block means this was never a merge of
    // text, whatever its bytes happen to spell.
    if bytes.iter().take(512).any(|&b| b == 0) {
        return false;
    }
    text_has_conflict_markers(&String::from_utf8_lossy(&bytes))
}

/// The marker test, on the text alone, so a test can state the shapes
/// without building a repository around each one.
fn text_has_conflict_markers(text: &str) -> bool {
    let mut start = false;
    let mut separated = false;
    for line in text.lines() {
        if line.starts_with("<<<<<<<") {
            start = true;
            separated = false;
        } else if start && line.starts_with("=======") {
            separated = true;
        } else if separated && line.starts_with(">>>>>>>") {
            return true;
        }
    }
    false
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
        let base = conflicting(&f);

        let engine = ro_engine::resolve("git", &ro_engine::EngineSlots::default(), None)
            .expect("git is one of the three");
        let outcome = resolve_conflict(f.repo(), &base, &engine, ResolveOptions::default());

        match &outcome {
            Resolution::NeedsUser { files } => {
                assert!(
                    files.iter().any(|f| f == "shared.txt"),
                    "the hand-over must name the conflicted paths, got {files:?}"
                );
            }
            other => panic!("a conflict with no --resolve must be handed over, got {other:?}"),
        }
        assert!(!outcome.can_push(), "nothing may be pushed after a hand-over");
    }

    /// With `--resolve`, the engine is dispatched exactly once and the
    /// result is verified before the rebase continues.
    ///
    /// The engine used is the raw backend, so the resolution it performs is
    /// a mechanical "stage whatever is on disk" — enough to prove the
    /// *stage* runs, without a model being involved in a test that claims
    /// to be about the boundary. And because that resolution keeps the
    /// conflict markers, the stage refuses it: the index is clean, the
    /// worktree is not, and `rebase --continue` would have published
    /// `<<<<<<<` to everyone.
    #[test]
    fn resolve_dispatches_once_and_refuses_a_poisones_the_worktree() {
        let f = Fixture::new();
        let base = conflicting(&f);

        let engine = ro_engine::resolve("git", &ro_engine::EngineSlots::default(), None)
            .expect("git is one of the three");
        let outcome =
            resolve_conflict(f.repo(), &base, &engine, ResolveOptions { resolve: true });

        match &outcome {
            // The raw backend stages the markers; ro does not continue.
            Resolution::NeedsUser { files } => {
                assert!(
                    files
                        .iter()
                        .any(|f| f.contains("conflict markers")),
                    "the hand-over must say the markers are still there, got {files:?}"
                );
            }
            other => panic!("the stage must reach a decision, got {other:?}"),
        }
        assert!(!outcome.can_push());
    }

    /// ro does not continue the rebase with conflicts still in the index.
    ///
    /// This is the guarantee that keeps the flag usable: running
    /// `--continue` with conflicts present produces a second, more
    /// confusing failure, and the user learns to avoid the flag.
    #[test]
    fn the_index_is_verified_before_the_rebase_continues() {
        let f = Fixture::new();
        let _ = conflicting(&f);

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

        // The check the stage actually makes must **fail** — that is what
        // stops ro running `rebase --continue` into a second, more
        // confusing failure.
        let err = index_is_clear(f.repo()).expect_err("an unmerged index is not ready");
        assert!(
            err.contains("shared.txt"),
            "and it must name what is still unmerged, got: {err}"
        );
    }

    /// The engine is asked, and its answer is what comes back.
    ///
    /// An engine that is not installed did not have an opinion about the
    /// conflict. Reporting it as a hand-over tells the user to go and read
    /// conflict markers that a missing binary created — and the user,
    /// who has a rebase to finish, is the one who pays for the confusion.
    #[test]
    fn an_unavailable_engine_is_a_failure_not_a_conflict() {
        let f = Fixture::new();
        let base = conflicting(&f);

        // A binary that is on no PATH anywhere, so this is `Unavailable`
        // on every machine rather than on the one without `claude`.
        let engine = ro_engine::resolve(
            "claude",
            &ro_engine::EngineSlots::default(),
            Some("ro-test-no-such-agent-binary-8f3a"),
        )
        .expect("claude is one of the three");
        let outcome =
            resolve_conflict(f.repo(), &base, &engine, ResolveOptions { resolve: true });

        match &outcome {
            Resolution::Failed { error } => {
                assert!(
                    error.contains("ro-test-no-such-agent-binary-8f3a"),
                    "the engine's own message must reach the user, got: {error}"
                );
                assert!(
                    error.contains("rebase --abort"),
                    "and the tree is still mid-rebase, so the recovery goes with it, got: {error}"
                );
            }
            other => panic!("a missing engine is a failure, not a conflict, got {other:?}"),
        }
        assert!(!outcome.can_push());
    }

    /// The stage finishes the rebase itself, and the tree comes back out
    /// of the conflict.
    ///
    /// The engine that resolves is a script standing in for an agent: it
    /// takes one side and stages it, which is what a resolver is asked to
    /// do. What is being tested is ro's half — verify, continue, and do
    /// not leave a rebase behind.
    #[cfg(unix)]
    #[test]
    fn a_resolved_conflict_finishes_the_rebase() {
        let f = Fixture::new();
        let base = conflicting(&f);

        // A marker file the resolver writes, so the test can tell that the
        // engine ran at all.
        let marker = f.repo().parent().unwrap().join("ro-test-resolved.marker");
        let engine = shim_engine(&f, &marker);

        let outcome =
            resolve_conflict(f.repo(), &base, &engine, ResolveOptions { resolve: true });

        assert!(
            marker.exists(),
            "the engine must have been dispatched, and its work kept"
        );
        assert!(
            matches!(outcome, Resolution::Resolved { .. }),
            "a conflict the engine settled must be reported as resolved, got {outcome:?}"
        );
        assert!(outcome.can_push());
        assert!(
            !ro_git::primitives::is_rebase_in_progress(f.repo()),
            "the rebase must be finished, not left mid-rebase"
        );
    }

    /// A single `<<<<<<<` is not a conflict.
    ///
    /// Documentation, fixtures and source code all mention the marker, and
    /// a check that flagged those would hand back a conflict nobody has —
    /// and would do it on every repository that quotes one.
    #[test]
    fn a_mention_of_a_marker_is_not_a_conflict() {
        assert!(
            !text_has_conflict_markers("a conflict looks like:\n<<<<<<< HEAD\nand then some\n"),
            "a lone marker must not read as a conflict"
        );
        assert!(text_has_conflict_markers(
            "<<<<<<< HEAD\nmine\n=======\ntheirs\n>>>>>>> origin/main\n"
        ));
        // The pair without the opener is a merge that finished, or a
        // document about one.
        assert!(!text_has_conflict_markers("=======\ntheirs\n>>>>>>> origin/main\n"));
    }

    /// An engine that resolves a conflict the way one is asked to.
    #[cfg(unix)]
    fn shim_engine(f: &Fixture, marker: &std::path::Path) -> ro_engine::ResolvedEngine {
        use std::os::unix::fs::PermissionsExt;
        let shim = f.repo().parent().unwrap().join("ro-test-resolver.sh");
        std::fs::write(
            &shim,
            format!(
                "#!/bin/sh\n\
                 : > '{marker}'\n\
                 for f in $(git diff --name-only --diff-filter=U); do\n\
                 \x20 git checkout --theirs -- \"$f\" && git add -- \"$f\" || exit 1\n\
                 done\n\
                 exit 0\n",
                marker = marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        ro_engine::resolve(
            "claude",
            &ro_engine::EngineSlots::default(),
            Some(&shim.to_string_lossy()),
        )
        .expect("claude is one of the three")
    }

    /// Make the local branch and the remote edit the same line, so a
    /// rebase genuinely conflicts. Returns the base that conflicts.
    fn conflicting(f: &Fixture) -> String {
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

        // …and run it, because the stage is entered with a conflict
        // already in the index. It never rebases for itself.
        let out = Command::new("git")
            .args(["rebase", "--autostash", "origin/main"])
            .current_dir(f.repo())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "the fixture must produce a real conflict, or these tests prove \
             nothing: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        "origin/main".to_string()
    }
}
