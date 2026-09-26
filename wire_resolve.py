"""One-shot: wire the resolve stage into the pipeline, and drop the verb.

The rebase step currently gives up on a conflict. It hands the run over
with a message. This replaces that with the stage the plan describes: a
**second, different** dispatch when `--resolve` is passed, the index
verified, and the rebase finished by ro.

`ro conflict` goes in the same commit — a command that exists only to
resolve conflicts is a command somebody will reach for *instead* of
`ro ship`, and the boundary it implies (a model driving a rebase) is the
one this design refuses.
"""
import io

P = "crates/ro/src/ship/orchestrator.rs"
s = io.open(P, encoding="utf-8").read()

# Replace the give-up rebase with the stage.
OLD = """            Err(e) => {
                // A failed autostash pop is a per-repo FAILURE, never a
                // proceed on a half-popped tree. The message carries the
                // exact recovery rather than hoping the user knows it.
                return RepoOutcome::Failed { error: e.to_string() };
            }"""
NEW = """            Err(e) => {
                // A failed autostash pop is a per-repo FAILURE, never a
                // proceed on a half-popped tree. The message carries the
                // exact recovery rather than hoping the user knows it.
                return RepoOutcome::Failed { error: e.to_string() };
            }
        }

        // (c3) A conflict is a **stage**, not a dead end — but only when
        // the user asked for it. `--resolve` is off by default because it
        // is the one step where a model edits files mid-rebase, and a
        // model resolving a conflict nobody had is worse than a stop.
        if ro_git::conflict::detect(repo)?.is_some() {
            let resolution = crate::ship::resolve::resolve_after_rebase(
                repo,
                &base_for(repo),
                &plan.engine,
                crate::ship::resolve::ResolveOptions {
                    resolve: opts.resolve_conflicts,
                },
            );
            match resolution {
                crate::ship::resolve::Resolution::Clean
                | crate::ship::resolve::Resolution::Resolved { .. } => {}
                crate::ship::resolve::Resolution::NeedsUser { files } => {
                    return RepoOutcome::HandedOver {
                        detail: files.join(", "),
                    };
                }
                crate::ship::resolve::Resolution::Failed { error } => {
                    return RepoOutcome::Failed { error };
                }
            }"""
assert OLD in s
s = s.replace(OLD, NEW, 1)

# The option, on RunOptions.
s = s.replace('''    pub state_dir: PathBuf,
}''', '''    pub state_dir: PathBuf,
    /// Dispatch the engine on a conflict.
    ///
    /// Off by default. It is the only step where a model edits files
    /// mid-rebase, and the overwhelmingly common cause of a rejected push
    /// is a stale branch — three git commands that need no model at all.
    pub resolve_conflicts: bool,
}''', 1)
s = s.replace('''            dry_run: false,
            state_dir: std::env::temp_dir(),
        }''', '''            dry_run: false,
            state_dir: std::env::temp_dir(),
            resolve_conflicts: false,
        }''', 1)

# The hand-over outcome.
s = s.replace('''    /// Refused before any push was attempted.''', '''    /// A conflict the user has to resolve.
    ///
    /// Not a failure: nothing is broken, and a run that exits 1 because
    /// one repo needs a human would train people to ignore the exit code.
    /// It is also not a success — the work did not land — so the summary
    /// says so.
    HandedOver { detail: String },
    /// Refused before any push was attempted.''', 1)
s = s.replace('''                | RepoOutcome::Failed { .. }
                | RepoOutcome::Refused { .. }
        )''', '''                | RepoOutcome::Failed { .. }
                | RepoOutcome::Refused { .. }
        )''', 1)
s = s.replace('''            RepoOutcome::Refused { branch, .. } => format!("refused: {branch} is protected"),''',
'''            RepoOutcome::Refused { branch, .. } => format!("refused: {branch} is protected"),
            RepoOutcome::HandedOver { detail } => {
                format!("needs you: {detail}")
            }''', 1)

# The base, read once and used by the rebase stage.
s = s.rstrip() + '''

/// The branch to rebase onto: the remote's own HEAD, or the repo's current
/// branch when there is no remote.
fn base_for(repo: &Path) -> String {
    ro_git::primitives::symbolic_ref(repo, "refs/remotes/origin/HEAD")
        .ok()
        .flatten()
        .unwrap_or_else(|| "main".to_string())
}
'''
io.open(P, "w", encoding="utf-8").write(s)
print("orchestrator ok")
