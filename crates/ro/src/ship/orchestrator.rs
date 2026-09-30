//! The per-repo pipeline, and the coordinator that runs it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ro_core::{CommitIdentity, SecretString};
use ro_engine::{EngineContext, EngineOutcome, ResolvedEngine};

/// How far the pipeline runs.
///
/// A shorter verb stops earlier rather than being a different code path:
/// three paths that mostly agree drift, and the drift shows up as "commit
/// blocked but push did it anyway".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HowFar {
    /// Commit only. Stops after the engine.
    Commit,
    /// Commit and push, without fetching or rebasing first.
    Push,
    /// The whole thing: fetch, rebase, commit, push.
    Ship,
}

impl HowFar {
    /// Does this run fetch first?
    pub fn fetches(&self) -> bool {
        matches!(self, HowFar::Ship)
    }

    /// Does this run push?
    pub fn pushes(&self) -> bool {
        !matches!(self, HowFar::Commit)
    }

    /// Does this run rebase onto the remote's base first?
    ///
    /// Everything that touches the remote, and `commit` does not. A push
    /// onto a branch the remote has moved past is rejected as
    /// non-fast-forward, and the only two ways out are a rebase — which
    /// rewrites the branch and needs `--force-with-lease` — or failing and
    /// handing the user a problem they have to know the answer to. Rebasing
    /// first makes the common case an ordinary forward push, which is the
    /// whole reason the step exists.
    ///
    /// `ro push` skipping it was not a design decision, it was an omission:
    /// `ro ship` rebased, `ro push` did not, and both are on the same
    /// pipeline with the same arguments. `ro commit` stays out because it
    /// writes nothing to the remote, and rewriting local history for a
    /// request that was only "commit this" is its own surprise.
    pub fn rebases(&self) -> bool {
        matches!(self, HowFar::Ship | HowFar::Push)
    }
}

/// The checks that stop a run before anything can write.
///
/// Returns the outcome that stops it, or `None` to proceed. Split out so
/// the dry run and the real run cannot drift apart: a guard stated once
/// and consulted by both is the only way "the plan agrees with the run"
/// survives either one being edited.
///
/// All three are **reads**. That is what lets the dry run call them: a
/// guard that wrote would be the one thing `--dry-run` could not do.
fn guards(plan: &RepoPlan, repo: &std::path::Path, opts: &RunOptions) -> Option<RepoOutcome> {
    // (b) A conflict is a *skip*, not a failure: the worktree is
    //     mid-merge and ro does not resolve it for you.
    match ro_git::conflict::detect(repo) {
        Ok(Some(state)) => {
            // …unless it is a **stale marker**, which is not a conflict at
            // all. A successful `git rebase --continue` leaves
            // `REBASE_HEAD` behind on git 2.53: the rebase directory is
            // gone, the branch has moved, the tree is clean, and
            // `detect` still reports a rebase in progress — with zero
            // conflicted files.
            //
            // That matters here more than anywhere, because this is the
            // path the resolve stage leaves behind. Skipping on it would
            // answer "0 conflicted file(s) during rebase" for this repo on
            // every run, from here on: a tool that just resolved the
            // conflict would never ship the result.
            let stale_marker = state.op == ro_git::conflict::ConflictOp::Rebase
                && state.is_empty()
                && !rebase_still_running(repo);
            if !stale_marker {
                return Some(RepoOutcome::SkippedConflict {
                    detail: format!(
                        "{} conflicted file(s) during {}",
                        state.files.len(),
                        state.op
                    ),
                });
            }
        }
        Ok(None) => {}
        Err(e) => {
            return Some(RepoOutcome::Failed {
                error: format!("could not read {}: {e:#}", plan.label),
                class: ro_core::FailureClass::MissingProvider,
            });
        }
    }

    // (b2) The safety net: denylisted paths and a secret scan. This is the
    // last thing standing between a WIP commit and a leaked credential, so
    // it runs before the engine and not merely before the push.
    if let Err(reason) = preflight(repo) {
        return Some(RepoOutcome::Blocked {
            reason: reason.0,
            detail: String::new(),
        });
    }

    // (b3) Protected branches, **before** the engine.
    //
    // This check used to run after the engine had committed, so `ro ship`
    // on `main` made the commit and *then* declined to push it — leaving
    // a WIP commit behind while the summary reported `refused` and
    // "0 committed". A refusal that mutates is not a refusal.
    //
    // It also tested only the **base** branch and was skipped outright
    // whenever `--onto` was present (`plan.onto.is_none() && …`), which made
    // `--onto` a one-flag escape from the whole rule: naming `main`,
    // `staging`, `production` or `release/1.0` as the destination pushed
    // straight onto it, exit 0, from any checkout.
    //
    // Both the branch being left and the branch being written to are
    // checked, because the protection is a property of the **destination**,
    // not of where you happen to be standing. `--onto` remains the escape
    // from a protected *checkout* — that is what it is for — but it cannot
    // be used to write to a protected branch.
    //
    // Naming a branch has consequences: it is what a teammate fetches and
    // what appears in `git branch` next month, so a tool that invents one is
    // deciding something with a blast radius on the user's behalf.
    let protected_base = ro_git::primitives::is_protected_branch(&plan.base_branch);
    // One normalisation, used by **both** checks below. An `--onto` that is
    // empty or all whitespace is no `--onto` at all — every writer filters it
    // — so it must not read as "the user aimed somewhere else" and let a
    // protected checkout through. Reading the raw field here and the trimmed
    // one three lines down is how `--onto ' main '` came to pass a guard that
    // `push_refspec` then did not.
    let onto = plan
        .onto
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty());
    if protected_base && onto.is_none() {
        return Some(RepoOutcome::Refused {
            branch: plan.base_branch.clone(),
            reason: format!(
                "{} is a protected branch. Create a branch first \
                 (`git checkout -b feat/x`), or pass --onto <BRANCH> if the \
                 work genuinely belongs somewhere else.",
                plan.base_branch
            ),
        });
    }
    // Trimmed **here**, at the guard, and not only at the writers. Every
    // writer of `onto` trims — `push_refspec`, `onto_created_warning`,
    // `base_for`, `written_branch` — so the guard was the one place the
    // value was read raw, and the check and the write disagreed about what
    // the branch was called. `--onto ' main '` therefore passed the
    // protection and pushed onto `refs/heads/main`, which is the exact
    // bypass FEATURES.md promises the guard closes.
    if let Some(onto) = onto {
        if ro_git::primitives::is_protected_branch(onto) {
            return Some(RepoOutcome::Refused {
                branch: onto.to_string(),
                reason: format!(
                    "--onto {onto} names a protected branch. Protection applies \
                     to the branch being written to, not to the branch you are \
                     standing on, so --onto cannot be used to bypass it. \
                     Choose an unprotected destination, or push there yourself."
                ),
            });
        }
        // A name git itself would refuse is refused here, with a reason that
        // names the flag, rather than at the push as a bare non-fast-forward
        // or as a branch invented from half a refspec.
        if let Err(why) = validate_onto(onto) {
            return Some(RepoOutcome::Refused {
                branch: onto.to_string(),
                reason: why,
            });
        }
    }

    // (b4) `--amend` on a commit the remote has already seen.
    //
    // The help promises this refusal — "Refused when HEAD is already on the
    // remote: amending a commit someone else has seen rewrites history under
    // a message that no longer describes it" — and it did not exist. The
    // amend ran, reported `committed`, exited 0, and left the branch
    // permanently diverged with the remote still carrying the orphaned
    // original; the follow-up push is then rejected non-fast-forward, which
    // is precisely the outcome the gate is documented to prevent.
    //
    // Not scoped to the pushing verbs. `ro commit --amend` on a published
    // commit is the case the E2E pass found: it amended the commit, reported
    // `committed`, exited 0, and left the branch diverged (`1  1` against
    // origin) with the remote still carrying the orphaned original. Whether
    // *this* run pushes is beside the point — the damage is done the moment
    // HEAD is rewritten, and the next push is where it surfaces.
    //
    // Lives here with the other guards rather than at the amend site, so the
    // dry run and the real run reach the same conclusion — the property this
    // function exists for.
    if opts.amend && head_is_on_remote(repo, &plan.base_branch) {
        return Some(RepoOutcome::Refused {
            branch: plan.base_branch.clone(),
            reason: format!(
                "{} is already on the remote. Amending it rewrites history \
                 someone else may have seen, under a message that no longer \
                 describes it. Push the new work as a new commit instead, \
                 or use --force-with-lease if you are certain nobody else \
                 has fetched it.",
                plan.base_branch
            ),
        });
    }

    None
}

/// Why a repo did not get as far as a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoOutcome {
    Committed {
        oid: String,
        message: String,
    },
    /// Nothing to do. Not a failure.
    NothingToCommit,
    /// A dry run found work it *would* commit.
    ///
    /// Distinct from `Committed`, because nothing was committed: a plan
    /// that reports a commit it did not make is the same lie as a plan
    /// that reports no work where there is work. Distinct from
    /// `NothingToCommit`, which is the true statement. Not a failure.
    WouldCommit,
    /// The worktree is mid-conflict; ro does not resolve it for you.
    SkippedConflict {
        detail: String,
    },
    /// A safety check stopped it **before** the engine ran.
    Blocked {
        reason: String,
        detail: String,
    },
    /// A conflict the user has to resolve.
    ///
    /// Not a failure: nothing is broken, and a run that exits 1 because one
    /// repo needs a human would train people to ignore the exit code. Not
    /// a success either — the work did not land — so the summary says so.
    HandedOver {
        detail: String,
    },
    /// Refused before any push was attempted.
    ///
    /// Distinct from `Blocked` and from `Failed`: nothing was wrong with
    /// the repo and nothing failed, the operation simply is not one ro
    /// does. A caller that has to tell these apart can, and a run summary
    /// that merges them loses the one that tells the user what to do.
    Refused {
        branch: String,
        reason: String,
    },
    /// The work reached the remote.
    ///
    /// `warnings` carries what the user has to be told **about the write** —
    /// a fact that is true of the ref that was just pushed and not of the
    /// run in general. It lives on the outcome rather than beside it
    /// because the summary row renders the outcome: a warning printed
    /// somewhere else in the run is a warning a script reading `json` never
    /// sees, and `--onto` naming a branch the remote does not have is
    /// precisely the thing a script is most likely to act on unobserved.
    ///
    /// Empty for every outcome but this one, and that is the scope
    /// deliberately: the only write ro makes without being asked is
    /// creating the branch `--onto` named, so it is the only one that can
    /// surprise. A push that went to a branch the remote already had
    /// surprises nobody, and a push that failed published nothing to warn
    /// about.
    Pushed {
        oid: String,
        warnings: Vec<String>,
    },
    Failed {
        error: String,
        /// The taxonomy, carried to the user.
        ///
        /// `classify_agent_output` computed it correctly and it was dropped
        /// at this boundary, so a rate-limited run ("retry in five minutes")
        /// and a timeout run ("retry now") were the same news: both rendered
        /// `failed: <error>`, and the JSON output had no class field at all.
        /// A conflict-class fake and an auth-class fake were likewise
        /// indistinguishable. The class is the difference between "wait" and
        /// "fix something", and it was being thrown away.
        class: ro_core::FailureClass,
    },
}

impl RepoOutcome {
    /// Is this a failure that should make the whole run exit non-zero?
    /// Whether this row should make the run a failure.
    ///
    /// A mid-conflict repo is deliberately **not** here. ro is not failing
    /// when it declines to act on a repository a person already left
    /// mid-merge, and that person knows. Making it a failure means a
    /// twenty-repo fleet with one long-wedged repo exits non-zero, and a
    /// signal that fires for a known, already-diagnosed condition is a
    /// signal people learn to ignore.
    ///
    /// It is still counted, and still printed — see `Summary::render`. The
    /// claim is only that the exit code answers "did ro do its job", and a
    /// skip is not a job ro declined.
    pub fn is_failure(&self) -> bool {
        matches!(
            self,
            RepoOutcome::Blocked { .. } | RepoOutcome::Failed { .. } | RepoOutcome::Refused { .. }
        )
    }

    /// What the user has to be told about this outcome.
    ///
    /// Empty for almost every outcome, and that is the point: a warning
    /// that fires on a clean run is a warning nobody reads. A borrowed
    /// slice rather than a `Vec`, so a caller cannot empty the warning by
    /// taking it — the row and the text table are two renderings of one
    /// fact, and a `take()` between them would make them disagree.
    pub fn warnings(&self) -> &[String] {
        match self {
            RepoOutcome::Pushed { warnings, .. } => warnings,
            _ => &[],
        }
    }

    /// One line for the summary table.
    pub fn render(&self) -> String {
        match self {
            RepoOutcome::Committed { oid, .. } => format!("committed {oid}"),
            RepoOutcome::NothingToCommit => "nothing to commit".into(),
            RepoOutcome::WouldCommit => "would commit".into(),
            RepoOutcome::SkippedConflict { .. } => "skipped: mid-conflict".into(),
            RepoOutcome::Blocked { reason, .. } => format!("blocked: {reason}"),
            // The reason is rendered, not assumed. A refusal used to read
            // "refused: {branch} is protected" for *every* refusal, so a
            // refusal for any other reason — amending a published commit,
            // `--onto` naming a protected branch — announced itself as a
            // protection problem it had nothing to do with. The branch is
            // dropped from the line for the same reason: it is in the
            // reason.
            RepoOutcome::Refused { reason, .. } => format!("refused: {reason}"),
            RepoOutcome::HandedOver { detail } => format!("needs you: {detail}"),
            RepoOutcome::Pushed { oid, .. } => format!("pushed {oid}"),
            RepoOutcome::Failed { error, class } => format!("failed ({class}): {error}"),
        }
    }
}

/// What the coordinator knows before any worker starts.
///
/// No `Debug` or `Clone`: it holds a `ResolvedEngine`, which holds a
/// `Box<dyn Engine>`. Deriving either would mean every engine had to be
/// printable and duplicable for the sake of a struct that is built once
/// and consumed once.
pub struct RepoPlan {
    /// The registry row id. Carried so a summary line can be traced back
    /// to its row without the coordinator holding the map.
    #[allow(dead_code)]
    pub repo_id: String,
    pub label: String,
    pub local_path: PathBuf,
    pub clone_url: String,
    /// The branch as it is **now**, read once on the coordinator.
    ///
    /// Snapshot before any worker runs, because an engine may switch
    /// branches itself and a value read afterwards is whatever the engine
    /// left behind rather than what it was asked to work from.
    pub base_branch: String,
    /// The  profile this row names, kept for the record:
    /// the resolved email is what the commit carries, and the name is
    /// what explains it afterwards.
    #[allow(dead_code)]
    /// The identity profile this row names, kept for the record: the
    /// resolved email is what the commit carries, and the name is what
    /// explains it afterwards.
    #[allow(dead_code)]
    pub author_ref: Option<String>,
    pub credential_ref: Option<String>,
    /// The branch the work should land on, when the user named one.
    ///
    /// `None` means "whatever the repo is on", which is why a protected
    /// branch is a refusal rather than a rebase onto something invented.
    pub onto: Option<String>,
    /// The commit author resolved from `author_ref` and the global
    /// identity table. `None` means "inherit git's own config".
    pub identity: Option<CommitIdentity>,
    pub engine: ResolvedEngine,
}

impl RepoPlan {
    pub fn path(&self) -> &Path {
        &self.local_path
    }
}

/// Everything a run needs, resolved before any worker starts.
#[derive(Debug, Clone)]
pub struct RunOptions {
    pub how_far: HowFar,
    pub timeout: Duration,
    /// Repos scanned in parallel. Bounded because each worker holds a
    /// checkout open and a fleet run that opens all twenty at once is a
    /// fleet run that starves itself.
    pub parallel: usize,
    pub dry_run: bool,
    /// One commit, this subject, for every repo. `--message`.
    ///
    /// This is the only way to stop an agent from splitting the work, and
    /// it is deliberately the only one: "commit it as one" is a statement
    /// about the *message*, and the engine still does the grouping work —
    /// it just has nothing to choose between.
    pub message: Option<String>,
    /// Fold the work into HEAD instead of adding a commit.
    ///
    /// Refused when HEAD is already pushed. Amending a commit the remote
    /// has seen is a history rewrite published under a name that no longer
    /// describes it, and no flag should make that one keystroke away.
    pub amend: bool,
    /// The instruction handed to the agent, replacing the built-in one.
    pub prompt: Option<String>,
    /// Where locks live. Under the state directory, never in the
    /// worktree: a lock inside the repo makes `git status` report the
    /// tool's own file as uncommitted work.
    ///
    /// Threaded rather than discovered from an env var, because an env
    /// var nothing sets means every run locks `./locks` relative to
    /// whatever directory ro was invoked from — so two processes in
    /// different directories would lock different files for the same
    /// repository, which is the TOCTOU the lock exists to prevent,
    /// reintroduced through a different door.
    pub state_dir: PathBuf,
    /// Dispatch the engine on a conflict.
    ///
    /// Off by default. It is the only step where a model edits files
    /// mid-rebase, and the overwhelmingly common cause of a rejected push
    /// is a stale branch — three git commands that need no model at all.
    pub resolve_conflicts: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            how_far: HowFar::Ship,
            timeout: ro_engine::dispatch::default_timeout(),
            parallel: 4,
            dry_run: false,
            message: None,
            amend: false,
            prompt: None,
            state_dir: std::env::temp_dir(),
            resolve_conflicts: false,
        }
    }
}

/// Run the pipeline for one repo.
///
/// Returns a **value**. A `Result` here would be the fleet abort this
/// module exists to prevent: the caller has no way to skip one repo's
/// failure and carry on if the function hands back a `Result`, and one
/// `?` inside a worker turns a per-repo failure into a whole-run abort.
pub fn run_one(plan: &RepoPlan, opts: &RunOptions) -> RepoOutcome {
    let repo = plan.path();

    // The guards first, and stated **once**, because the dry run and the
    // real run have to reach the same conclusion. They used not to:
    // `--dry-run` returned `NothingToCommit` without looking at the repo
    // at all, and the protection check sat *after* the engine had already
    // committed. So a dry run on a dirty tree printed "nothing to
    // commit", and the real run then committed and refused — the plan
    // said one thing and the run did another. That is the one thing a dry
    // run must never do, and the plan names it as the single most
    // important property to keep.
    if let Some(stop) = guards(plan, repo, opts) {
        return stop;
    }

    if opts.dry_run {
        // No lock, no engine, no write. The engine is deliberately not
        // dispatched: a plan that runs a model in order to report what it
        // would have run is not a plan, and what the engine *would* commit
        // is its decision to make. What the dry run can answer honestly
        // is whether there is anything to do at all.
        return match ro_git::read::is_dirty(repo) {
            Ok(true) => RepoOutcome::WouldCommit,
            // An unreadable tree is not evidence of a clean one, and
            // claiming "nothing to commit" about a repo ro could not read
            // is the same lie in a quieter voice.
            Ok(false) => RepoOutcome::NothingToCommit,
            Err(e) => RepoOutcome::Failed {
                error: format!("could not read {}: {e:#}", plan.label),
                class: ro_core::FailureClass::MissingProvider,
            },
        };
    }

    // (a) Lock. A guard, so it releases on every path including a panic
    //     — a lock left held is a repo nobody can touch until the
    //     process dies.
    let lock_dir = opts.state_dir.join("locks");
    let _lock = match ro_git::RepoLock::acquire_default(&lock_dir, repo) {
        Ok(l) => l,
        Err(e) => {
            return RepoOutcome::Failed {
                error: format!("could not lock {}: {e:#}", plan.label),
                class: ro_core::FailureClass::MissingProvider,
            };
        }
    };

    // (b) The conflict check, the denylist/secret-scan preflight and the
    //     protected-branch rule all ran in `guards` above, before this
    //     lock and before anything can write. They are not repeated here:
    //     a second copy is a second answer waiting to disagree.

    // (c) Fetch, for a full ship. A network failure is a failure, not a
    //     silent skip: the user asked for an up-to-date push.
    //
    //     The check reads the **result**, not just the `Result`. `fetch`
    //     returns `Ok(GitCommandResult { status: 128, .. })` for a non-zero
    //     exit — `Err` is reserved for a spawn failure — so matching only on
    //     `Err` could never fire. `ro ship` therefore committed and pushed
    //     onto a base it never fetched, reported success, and printed
    //     nothing of git's `fatal:`. FEATURES.md's "fetch, rebase, commit,
    //     push" was not what happened.
    //
    //     The credential is resolved **here**, before the first network call,
    //     and threaded into the fetch. It used to be resolved at the push
    //     alone, so a private HTTPS repo — the only case where a per-row
    //     credential is needed at all — fetched anonymously, failed at the
    //     fetch with `fatal: Authentication failed`, and never reached the push
    //     that did carry the token. Resolving once up front also means a
    //     bad reference is reported before any work is committed, rather
    //     than after.
    let credential = match resolve_credential(plan.credential_ref.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            return RepoOutcome::Failed {
                error: format!("credential: {e}"),
                class: ro_core::FailureClass::MissingProvider,
            };
        }
    };
    if opts.how_far.fetches() {
        match ro_git::mutation::fetch_with_credential(
            repo,
            &ro_git::mutation::FetchOpts {
                // `None` for a local path or an SSH remote, which is correct:
                // the header is an HTTP mechanism and SSH authenticates with
                // keys. Inventing a host for it would only aim the token at
                // nothing — or, worse, at github.com.
                host: plan.host_for_extraheader(),
                ..Default::default()
            },
            credential.as_ref(),
        ) {
            Ok(r) if r.ok() => {}
            Ok(r) => {
                return RepoOutcome::Failed {
                    error: format!(
                        "fetch failed: {}",
                        if r.stderr.trim().is_empty() {
                            r.stdout.trim()
                        } else {
                            r.stderr.trim()
                        }
                    ),
                    class: ro_core::FailureClass::MissingProvider,
                };
            }
            Err(e) => {
                return RepoOutcome::Failed {
                    error: format!("fetch failed: {e:#}"),
                    class: ro_core::FailureClass::MissingProvider,
                };
            }
        }
    }

    // (c2) Rebase onto the remote's base, **before** the engine.
    //
    // The tree is dirty, so git stashes, rebases and pops; it comes back
    // dirty and already on top of the remote's base. The engine then
    // commits once and the push is an ordinary fast-forward — so the
    // happy path never rewrites the branch, and a `--force-with-lease`
    // is needed only when a commit made earlier is pushed after the
    // remote moved, which is the honest limit of reordering.
    //
    // `--onto` names the base, and the base is what the push will land on
    // — so passing one and rebasing onto the other is the flag being
    // ignored, which is what used to happen: the base was read from
    // `origin/HEAD` no matter what the user typed.
    if opts.how_far.rebases() {
        match rebase_onto_remote_base(
            repo,
            plan.onto.as_deref(),
            plan.host_for_extraheader().as_deref(),
            credential.as_ref(),
        ) {
            Ok(RebaseState::Rebased) | Ok(RebaseState::NoRemote) => {}
            // (c3) A conflict is a **stage**, not a dead end — and the
            // base travels with it, because the stage is about *that*
            // rebase and a base recomputed here is a second answer
            // waiting to disagree.
            //
            // This arm did not exist. A conflicting rebase returned the
            // same `Err` as a broken repository and the `Err` arm below
            // returned `Failed` from the middle of this block — so the
            // resolve stage, which lives between them, was unreachable
            // for exactly the case it exists to handle. `--resolve`
            // printed "the rebase conflicted" and dispatched nothing.
            Err(RebaseError::Conflicted { base }) => {
                let resolution = crate::ship::resolve::resolve_conflict(
                    repo,
                    &base,
                    &plan.engine,
                    crate::ship::resolve::ResolveOptions {
                        resolve: opts.resolve_conflicts,
                    },
                );
                // The stage decides, and the pipeline asks two questions
                // of it: may the run continue, and if not, what does the
                // user need. Both are the enum's own answer rather than a
                // match repeated here.
                if !resolution.can_push() {
                    let summary = resolution.render();
                    return match resolution {
                        crate::ship::resolve::Resolution::NeedsUser { files } => {
                            RepoOutcome::HandedOver {
                                detail: format!("{summary}: {}", files.join(", ")),
                            }
                        }
                        crate::ship::resolve::Resolution::Failed { error } => RepoOutcome::Failed {
                            error,
                            class: ro_core::FailureClass::MissingProvider,
                        },
                        _ => RepoOutcome::Failed {
                            error: "the rebase did not finish".to_string(),
                            class: ro_core::FailureClass::MissingProvider,
                        },
                    };
                }
            }
            // A failed autostash pop, a base that does not exist, a
            // broken repo: a real per-repo failure, never a proceed on a
            // half-popped tree. The message carries the exact recovery
            // rather than hoping the user knows it.
            Err(e) => {
                return RepoOutcome::Failed {
                    error: e.to_string(),
                    class: ro_core::FailureClass::MissingProvider,
                };
            }
        }
    }

    // (d) The engine. `ro commit` returns after this.
    let ctx = EngineContext {
        repo_root: repo,
        base_branch: plan.base_branch.clone(),
        identity: plan.identity.as_ref(),
        timeout: opts.timeout,
        // Two flags, two fields. `--prompt` is the agent's brief and
        // `--message` is the subject; one field could only mean one of
        // them, and the other engine silently got the wrong thing.
        message_override: opts.prompt.as_deref(),
        subject_override: opts.message.as_deref(),
        env: &[],
    };

    // Remembered before the engine runs, so "amend" can tell an actual
    // rewrite from a no-op, and so a push that has work to send can be
    // told from one that does not. Without the second, the rebase that
    // runs *above* has no way to be seen: a resolved conflict leaves the
    // worktree clean and HEAD one commit ahead, the engine has nothing
    // left to commit, and returning early reported `nothing to commit`
    // for a run that had just produced the commit it was asked to ship.
    let head_before = ro_git::read::head_oid(repo).ok().flatten();
    let head_before = head_before.as_deref();
    // What the push is measured against, for the same reason and taken
    // at the same point: the remote's branch, not the local one, so a
    // local-only branch does not read as "ahead of nothing" and justify a
    // push of a branch that never existed remotely.
    let push_target = push_refspec(plan);

    let oid = match plan.engine.checkpoint(&ctx) {
        EngineOutcome::Committed { commits } => match commits.last() {
            Some(c) => c.oid.clone(),
            None => {
                return RepoOutcome::Failed {
                    error: "the engine reported a commit with no commit id".into(),
                    class: ro_core::FailureClass::MissingProvider,
                };
            }
        },
        // Nothing for the *engine* to do is not nothing for the *run*.
        // The rebase above may have rewritten the branch onto the remote's
        // base, and that commit is on the remote only if we push it.
        EngineOutcome::NothingToCommit => {
            if !opts.how_far.pushes() {
                return RepoOutcome::NothingToCommit;
            }
            if !has_commits_to_push(repo, &push_target, head_before) {
                return RepoOutcome::NothingToCommit;
            }
            match ro_git::read::head_oid(repo) {
                Ok(Some(oid)) => oid,
                Ok(None) => {
                    return RepoOutcome::Failed {
                        error: "there is work to push but HEAD does not resolve".into(),
                        class: ro_core::FailureClass::MissingProvider,
                    };
                }
                Err(e) => {
                    return RepoOutcome::Failed {
                        error: format!("could not read HEAD: {e:#}"),
                        class: ro_core::FailureClass::MissingProvider,
                    };
                }
            }
        }
        EngineOutcome::Unavailable { binary, hint } => {
            return RepoOutcome::Failed {
                error: format!("{binary} is not installed. {hint}"),
                class: ro_core::FailureClass::MissingProvider,
            };
        }
        EngineOutcome::TimedOut { after } => {
            return RepoOutcome::Failed {
                error: format!("the engine did not finish within {after:?} and was killed"),
                class: ro_core::FailureClass::MissingProvider,
            };
        }
        EngineOutcome::Failed { error, .. } => {
            return RepoOutcome::Failed {
                error,
                class: ro_core::FailureClass::MissingProvider,
            };
        }
    };

    // `--amend`: fold what the engine just committed into the commit
    // underneath, so the branch moves by one commit holding the new tree
    // rather than by N. Applied after the engine, so it is one code path for
    // the `git` engine and for an agent that produced several.
    let oid = if opts.amend {
        // The subject comes from the commit being **amended**, which is
        // `head_before` — not from `oid`, the commit the engine just made.
        // Reading `oid` returned the engine's own placeholder
        // (`wip on <branch>`), so `--amend` with no `--message` silently
        // replaced a subject the user had written. `git commit --amend`
        // keeps it; the doc comment on `subject_of` says that is the point.
        let subject = opts
            .message
            .clone()
            .or_else(|| head_before.and_then(|h| subject_of(repo, h)))
            .unwrap_or_else(|| "amend".to_string());
        let author = plan
            .identity
            .as_ref()
            .map(|i| (i.name.as_str(), i.email.as_str()));
        match ro_git::primitives::amend_tree(repo, head_before, &subject, author) {
            Ok(new) => new,
            Err(e) => {
                return RepoOutcome::Failed {
                    error: format!("amending HEAD failed: {e:#}"),
                    class: ro_core::FailureClass::MissingProvider,
                };
            }
        }
    } else {
        oid
    };

    if !opts.how_far.pushes() {
        return RepoOutcome::Committed {
            oid,
            message: String::new(),
        };
    }

    // (e) Push, with the credential resolved from the row. `None` means
    //     "use the machine's own" — the right answer for a repo whose SSH
    //     key is already correct and needs no configuration.
    //
    // No protection check here: it ran in `guards`, before the engine, and
    // this point is after the commit. A refusal that arrives after the
    // work is already on disk is not a refusal.
    //
    // The secret is the one resolved at (c), not a second lookup: two
    // resolutions of one reference is two answers waiting to disagree, and
    // a credential that changed between them is a push as an account the
    // fetch never saw.
    //
    // The `--onto` warning is computed **before** the push, which is the
    // only moment the answer is both true and useful. A successful push
    // updates `refs/remotes/origin/<onto>` in this very repository, so the
    // same check afterwards answers "the remote has it" about a branch
    // this run invented and the warning never fires. Before the push the
    // tracking refs are as fresh as the fetch above made them — the same
    // state `base_for` decided the rebase base from, so the two answers
    // agree.
    //
    // Attached only on success, so a push that failed published nothing and
    // says nothing about publishing.
    let onto_warning = onto_created_warning(repo, plan.onto.as_deref());

    let push = ro_git::mutation::push_with_credential(
        repo,
        &ro_git::mutation::PushOpts {
            remote: Some("origin".into()),
            branch: Some(push_target),
            set_upstream: true,
            // `None` for an SSH remote, which is correct: the header is an
            // HTTP mechanism and SSH authenticates with keys. Inventing a
            // host for it would only aim the token at nothing.
            host: plan.host_for_extraheader(),
            ..Default::default()
        },
        credential.as_ref(),
    );

    match push {
        Ok(r) if r.ok() => RepoOutcome::Pushed {
            oid,
            warnings: onto_warning,
        },
        Ok(r) => RepoOutcome::Failed {
            error: format!("push failed: {}", r.stderr.trim()),
            class: ro_core::FailureClass::MissingProvider,
        },
        Err(e) => RepoOutcome::Failed {
            error: format!("push failed: {e:#}"),
            class: ro_core::FailureClass::MissingProvider,
        },
    }
}

/// What the user has to be told about a push that is about to **create**
/// the branch it lands on.
///
/// `--onto` naming a branch the remote does not have is not an error — it
/// is the first push, and the flag is the documented way to aim work at a
/// branch that does not exist yet. It is also the one ship flag that can
/// write anywhere on the remote with no gate: `--onto` to a protected
/// branch is refused, but `--onto` to a branch nobody has heard of is
/// accepted, and the push that follows **publishes** it. The help says
/// only "The branch the work should land on, when it is not the current
/// one", so a user who typed `--onto` and got a published branch they
/// never asked for was told nothing until they looked at the remote.
///
/// Warned about, not refused. Refusing would break the legitimate case
/// above, and the branch is created by the user's own flag either way —
/// the difference is only whether they knew it was going to happen.
///
/// The negative control is the point: a branch the remote already had is
/// not a surprise, and without this early return the warning fires on
/// every `--onto` run and stops being read.
fn onto_created_warning(repo: &Path, onto: Option<&str>) -> Vec<String> {
    let Some(onto) = onto.map(str::trim).filter(|b| !b.is_empty()) else {
        return Vec::new();
    };
    // A branch the remote already had is not a surprise, and it is the
    // negative control the whole warning rests on: without this early return
    // the warning fires on every `--onto` run and stops being read.
    if remote_branch_exists(repo, onto) {
        return Vec::new();
    }
    vec![format!(
        "--onto {onto} names a branch the remote does not have. ro created \
         it and pushed to it: {onto} is now a published branch, visible to \
         anyone who can see this repository. If that was not the branch you \
         meant, delete it (`git push origin --delete {onto}`) and re-run \
         with the right name."
    )]
}

impl RepoPlan {
    /// Which host the extraheader is scoped to, from this row's clone URL.
    fn host_for_extraheader(&self) -> Option<String> {
        extraheader_host(&self.clone_url)
    }
}

/// The **scheme and host** the credential's extraheader is scoped to, from a
/// clone URL.
///
/// A local path has no host, and a credential scoped to `github.com` must not
/// be offered to one.
///
/// Scheme included, and that is the whole point: the header used to be
/// assembled as `http.https://{host}/`, so a plain-HTTP remote had its
/// credential scoped to a URL that is never requested. The header was
/// dropped, git fell back to the machine's credential helper, and the request
/// either failed or succeeded as the *wrong account* — plus a modal dialog on
/// the user's screen.
///
/// A non-HTTP remote (SSH) returns `None` rather than a fake origin: SSH
/// authenticates with keys, not this header, so there is nothing to scope and
/// a made-up host would only send the token somewhere it has no business going.
///
/// A free function rather than a method on `RepoPlan` because it is a rule
/// about a **URL**, and the rebase step's fetch needs the same rule for the
/// same row without a `RepoPlan` in hand. Two spellings of this is how the
/// fetch ends up scoped somewhere the push is not.
fn extraheader_host(clone_url: &str) -> Option<String> {
    let (scheme, rest) = clone_url.split_once("://")?;
    // Case-insensitively, and emitted **lower-cased**. A URL scheme is
    // case-insensitive by RFC 3986, so `HTTP://host/…` names the same origin
    // as `http://host/…` — but this matched `scheme` exactly, so an uppercase
    // scheme returned `None`, no header was scoped, and the row's credential
    // was dropped before git's own refusal of `remote-HTTP` was ever reached.
    // The error the user saw then named neither the scheme nor the
    // credential, which is the part that costs them the hour.
    let scheme = scheme.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https") {
        return None;
    }
    let host = rest.split('/').next().unwrap_or_default();
    if host.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{host}"))
}

/// Is this a value that can name a **branch**?
///
/// `git check-ref-format --branch` is the authority, but two things are
/// refused before it is asked, because both are the shape that turns one flag
/// into two different refspecs:
///
///  * anything containing `:`, which git splits at the first colon into a
///    source and a destination — `--onto 'feat:refs/heads/evil'` silently
///    dropped the destination and pushed to `feat`
///  * anything starting with `refs/`, which git will happily create as a
///    branch *literally named* `refs/heads/refs/heads/feat`
///
/// `Some(err)` is the reason to show, phrased for someone who typed this.
fn validate_onto(onto: &str) -> Result<(), String> {
    if onto.contains(':') {
        return Err(format!(
            "--onto {onto:?} looks like a refspec, not a branch name. \
             git splits it at the first `:`, so the part after the colon would \
             be discarded and the push would land somewhere you did not name. \
             Pass the branch on its own, e.g. --onto feat/x."
        ));
    }
    if onto.starts_with("refs/") {
        return Err(format!(
            "--onto {onto:?} is a fully-qualified ref, not a branch name. \
             ro would create a branch literally called {onto:?}. \
             Pass the branch on its own, e.g. --onto {}.",
            onto.trim_start_matches("refs/heads/")
        ));
    }
    let out = std::process::Command::new("git")
        .args(["check-ref-format", "--branch", onto])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output();
    // An unreadable answer is `Ok`: this gate exists to refuse names git
    // itself would refuse, and refusing every `--onto` on a machine where
    // `git` cannot be spawned would be a far worse failure.
    match out {
        Ok(o) if !o.status.success() => Err(format!(
            "--onto {onto:?} is not a valid branch name. \
             `git check-ref-format --branch` rejects it."
        )),
        _ => Ok(()),
    }
}

/// Turn a `credential_ref` into a secret.
///
/// `None` means "no reference configured", which is **not** an error: a
/// repo whose SSH key is already correct needs no configuration at all.
fn resolve_credential(reference: Option<&str>) -> anyhow::Result<Option<SecretString>> {
    let Some(r) = reference else {
        return Ok(None);
    };
    // Parsed first, so a malformed reference is a clear error rather than
    // a fall-back to the machine's own credential — which would be a push
    // as the wrong person, silently.
    let parsed: ro_core::CredentialRef = r
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid credential reference: {e}"))?;
    let resolved =
        ro_core::credential_resolve::resolve(&parsed).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(Some(resolved))
}

/// The safety net: denylist and secret scan, over the dirty files.
///
/// Returns the **reason** a repo is blocked, not a bool, because the
/// message is what a user acts on — "blocked" with no reason is a dead
/// end, and the reason is what distinguishes "your `.env` is not
/// committable" from "there is a credential in this file".
struct BlockReason(String);

fn preflight(repo: &Path) -> Result<(), BlockReason> {
    let dirty = dirty_files(repo);

    // The denylist, checked before staging so a denied file is never
    // even in the index. A silently skipped `.env` teaches the user
    // that ro committed everything.
    let denylist = ro_sweep::denylist::Denylist::new_default()
        .map_err(|e| BlockReason(format!("denylist unavailable: {e:#}")))?;
    let denied: Vec<String> = dirty
        .iter()
        .filter_map(|f| {
            let rel = f.strip_prefix(repo).ok()?;
            let s = rel.to_string_lossy().replace('\\', "/");
            denylist.is_denied(&s).then_some(s)
        })
        .collect();
    if !denied.is_empty() {
        return Err(BlockReason(format!(
            "denylisted path(s): {}",
            denied.join(", ")
        )));
    }

    // The secret scan. `block` is the default for the same reason the
    // preflight is here at all: a default that permits the leak is not a
    // default anyone would pick knowingly.
    let refs: Vec<&Path> = dirty.iter().map(|p| p.as_path()).collect();
    let findings = ro_sweep::secret_scan::scan_files(&refs)
        .map_err(|e| BlockReason(format!("secret scan failed: {e:#}")))?;
    if !findings.is_empty() {
        // The file and the rule, never the matched text: the finding
        // matched on the *shape* of a secret, so echoing it would repeat
        // the leak in the message reporting it.
        let where_ = findings
            .iter()
            .map(|f| format!("{} ({})", f.path, f.rule))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(BlockReason(format!("possible secret in {where_}")));
    }
    Ok(())
}

/// The dirty files in a checkout, as absolute paths.
fn dirty_files(repo: &Path) -> Vec<PathBuf> {
    let Ok(out) = std::process::Command::new("git")
        .args(["status", "--porcelain", "-z", "-uall"])
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
    // `-z` and `-uall` together: the first makes the parse NUL-safe so a
    // filename with a space or a quote cannot be misread, and the second
    // expands an untracked directory into its files, which is what makes
    // "untracked files are included" true.
    ro_git::primitives::parse_porcelain(&String::from_utf8_lossy(&out.stdout))
        .into_iter()
        .map(|e| repo.join(e.path))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A real repository, because the preflight runs git in it.
    fn repo() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().to_path_buf();
        run(&path, &["init", "-q", "-b", "main"]);
        run(&path, &["config", "user.email", "t@e.com"]);
        run(&path, &["config", "user.name", "T"]);
        (tmp, path)
    }

    fn run(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The safety net, and it is the reason the preflight exists.
    ///
    /// `BlockedBySecrets` was unreachable for two releases: the call site
    /// hardcoded `Warn`, and `should_block` only returns true for
    /// `Block`. A variant nothing constructs is a variant nothing tests,
    /// so this constructs it.
    #[test]
    fn a_credential_shaped_file_is_blocked_before_anything_runs() {
        let (_tmp, path) = repo();
        std::fs::write(
            path.join("config.py"),
            b"token = 'ghp_1234567890abcdef1234567890abcdef1234'\n",
        )
        .unwrap();

        let err = preflight(&path).expect_err("a PAT-shaped file must block");
        let reason = &err.0;
        assert!(
            reason.contains("possible secret"),
            "the reason must say what was found, got: {reason}"
        );
        assert!(
            reason.contains("config.py"),
            "and name the file, got: {reason}"
        );
        // Never the matched text: the finding matched on the *shape* of a
        // secret, so echoing it would repeat the leak in the message
        // reporting it.
        assert!(
            !reason.contains("ghp_1234567890"),
            "the message must not echo the secret it found: {reason}"
        );
    }

    /// A denylisted path is reported by name, never dropped in silence.
    ///
    /// A silently skipped `.env` teaches the user that ro committed
    /// everything, which is the belief that gets a real secret committed
    /// next.
    #[test]
    fn a_denylisted_path_is_named_not_skipped() {
        let (_tmp, path) = repo();
        std::fs::write(path.join(".env"), b"SECRET=x\n").unwrap();

        let err = preflight(&path).expect_err("a .env must be blocked");
        assert!(
            err.0.contains(".env"),
            "the reason must name the path, got: {}",
            err.0
        );
    }

    /// And a clean tree passes.
    #[test]
    fn a_clean_file_passes_the_preflight() {
        let (_tmp, path) = repo();
        std::fs::write(path.join("readme.md"), b"# hi\n").unwrap();
        assert!(
            preflight(&path).is_ok(),
            "an ordinary file must not be blocked"
        );
    }

    #[test]
    fn a_directory_with_files_inside_is_scanned() {
        // `-uall` is what makes "untracked files are included" true: it
        // expands an untracked directory into its files. Without it the
        // scan sees a directory and reads nothing inside it.
        let (_tmp, path) = repo();
        std::fs::create_dir_all(path.join("pkg")).unwrap();
        std::fs::write(
            path.join("pkg/creds.py"),
            b"token = 'ghp_1234567890abcdef1234567890abcdef1234'\n",
        )
        .unwrap();

        let err = preflight(&path).expect_err("a file inside a new dir must be scanned");
        assert!(err.0.contains("creds.py"), "got: {}", err.0);
    }

    /// A dry run stops before anything can write.
    ///
    /// The flag used to sit on `RunOptions` unread, so `--dry-run` was a
    /// promise the code never kept.
    #[test]
    fn a_dry_run_returns_without_touching_the_repo() {
        let (_tmp, path) = repo();
        // The fixture is on `main`, which is protected, so the honest
        // answer is `Refused` — the same answer the real run gives. The
        // old expectation was `NothingToCommit` because `--dry-run` used
        // to return that without looking at the repo at all.
        let plan = plan_for_test(&path);
        let opts = RunOptions {
            dry_run: true,
            state_dir: path.join(".state"),
            ..Default::default()
        };
        assert!(
            matches!(run_one(&plan, &opts), RepoOutcome::Refused { .. }),
            "a dry run on a protected branch must preview the refusal, not \
             report an empty plan"
        );
        let porcelain = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&path)
                .output()
                .unwrap()
                .stdout,
        )
        .into_owned();
        assert!(
            porcelain.trim().is_empty(),
            "a dry run must leave the tree exactly as it found it, got: {porcelain}"
        );
    }

    /// The property the plan names as the single most important one: the
    /// printed plan and the real outcome **agree**.
    ///
    /// The stub version of `--dry-run` returned `NothingToCommit`
    /// unconditionally, so a dry run on a dirty tree promised there was
    /// nothing to do and the real run then committed. A dry run that
    /// disagrees with the run is worse than no dry run, because it is
    /// trusted precisely when it is wrong.
    #[test]
    fn a_dry_run_says_what_the_run_would_do() {
        let (_tmp, path) = repo();
        run(&path, &["checkout", "-q", "-b", "feat/x"]);

        // `plan_for_test` names `main` as the base whatever the repo is
        // on, which is right for a fixture and wrong here: the whole
        // subject is a branch that is *not* protected. The plan's base
        // branch is a stored fact about the repo, so that is the field to
        // set.
        let plan = RepoPlan {
            base_branch: "feat/x".into(),
            ..plan_for_test(&path)
        };
        let dry = RunOptions {
            dry_run: true,
            state_dir: path.join(".state"),
            ..Default::default()
        };
        assert_eq!(run_one(&plan, &dry), RepoOutcome::NothingToCommit);

        // Dirty, and not protected: there IS work, and the plan has to
        // say so.
        std::fs::write(path.join("a.txt"), "x\n").unwrap();
        assert_eq!(
            run_one(&plan, &dry),
            RepoOutcome::WouldCommit,
            "a dry run on a dirty tree must not claim there is nothing to commit"
        );

        // …and it still wrote nothing.
        let porcelain = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&path)
                .output()
                .unwrap()
                .stdout,
        )
        .into_owned();
        assert!(
            porcelain.contains("a.txt") && !porcelain.contains("A "),
            "a dry run must leave the change uncommitted, got: {porcelain}"
        );
    }

    /// A refusal must not mutate. The protection check used to sit *after*
    /// the engine, so `ro ship` on a protected branch made the WIP commit
    /// and then declined to push it — leaving work on disk while the
    /// summary said `refused` and "0 committed".
    #[test]
    fn a_refusal_leaves_the_tree_untouched() {
        let (_tmp, path) = repo();
        std::fs::write(path.join("a.txt"), "x\n").unwrap();
        let plan = plan_for_test(&path);
        let opts = RunOptions {
            state_dir: path.join(".state"),
            ..Default::default()
        };

        assert!(
            matches!(run_one(&plan, &opts), RepoOutcome::Refused { .. }),
            "a protected branch must be refused"
        );
        let head = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&path)
                .output()
                .unwrap()
                .stdout,
        )
        .into_owned();
        assert!(
            !head.trim().is_empty(),
            "a refused repo must have gained no commit; HEAD does not resolve"
        );
        let porcelain = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&path)
                .output()
                .unwrap()
                .stdout,
        )
        .into_owned();
        assert!(
            porcelain.contains("a.txt"),
            "a refused repo must keep its uncommitted work, not lose it: {porcelain}"
        );
    }

    /// A plan that needs no database, for the per-repo tests.
    fn plan_for_test(path: &Path) -> RepoPlan {
        RepoPlan {
            repo_id: "r1".into(),
            label: "acme/api".into(),
            local_path: path.to_path_buf(),
            clone_url: String::new(),
            base_branch: "main".into(),
            author_ref: None,
            credential_ref: None,
            onto: None,
            identity: None,
            engine: ro_engine::resolve("git", &ro_engine::EngineSlots::default(), None)
                .expect("git is one of the three built-ins"),
        }
    }
}

/// What a rebase attempt found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebaseState {
    /// The branch is now on the remote's base.
    ///
    /// One answer rather than two: git reports nothing differently for
    /// "already current" and "moved", and a caller given two answers
    /// has to invent a distinction it cannot act on.
    Rebased,
    /// No remote, so no base to rebase onto. A local-only repo, which is
    /// an ordinary state and not a failure.
    NoRemote,
}

/// Why a rebase did not leave the branch on the base.
///
/// The two cases look identical from outside — git exited non-zero — and
/// they are opposites. A conflict is a **stage**: the tree is mid-rebase on
/// purpose and the next thing that happens is `--resolve` or a hand-over.
/// Everything else is a dead end, and a run that treats it as a stage
/// would carry on committing and pushing from a tree nobody has looked at.
///
/// One type rather than a bool, because the caller cannot recover the
/// distinction from the message and must not have to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseError {
    /// The rebase stopped on a conflict. The tree is left mid-rebase:
    /// aborting would discard the work that caused the conflict.
    ///
    /// Carries the base, because the stage that follows is about *this*
    /// rebase and re-deriving the base is a second answer waiting to
    /// disagree with the first.
    Conflicted { base: String },
    /// The rebase failed for some other reason: an autostash that would
    /// not pop, a base that does not exist, a repository git cannot read.
    Failed { base: String, stderr: String },
}

impl std::fmt::Display for RebaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RebaseError::Conflicted { base } => write!(
                f,
                "the rebase onto {base} conflicted. Nothing was committed or \
                 pushed. Resolve it and run `git rebase --continue`, or \
                 `git rebase --abort` to go back."
            ),
            RebaseError::Failed { base, stderr } => write!(
                f,
                "the rebase onto {base} failed: {}.\n\
                 If the tree looks wrong, `git stash list` shows any autostash \
                 that was not popped, and `git stash pop` restores it.",
                stderr.trim()
            ),
        }
    }
}

/// Is the rebase actually still running?
///
/// `REBASE_HEAD` alone is not the answer: git leaves it behind after a
/// successful `--continue`, so a repository that just finished a resolved
/// rebase looks mid-rebase forever. The rebase directory is the answer —
/// it is what git itself checks before it will start another one.
fn rebase_still_running(repo: &Path) -> bool {
    // Two mechanisms rather than one, because they disagree exactly where
    // it matters. A linked worktree's `.git` is a *file*, so
    // `is_rebase_in_progress` reads the wrong place and answers "no" for a
    // live rebase; `rev-parse --absolute-git-dir` resolves the real one.
    // The first is the cheap path and answers for every ordinary clone.
    ro_git::primitives::is_rebase_in_progress(repo)
        || match absolute_git_dir(repo) {
            Some(dir) => dir.join("rebase-merge").exists() || dir.join("rebase-apply").exists(),
            // Cannot tell. Treat the marker as real: skipping a repo that
            // turns out to be fine is a delay, and committing into a live
            // rebase is a mess nobody wants to unpick.
            None => true,
        }
}

/// The real git directory, for a checkout where `.git` is a file.
fn absolute_git_dir(repo: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--absolute-git-dir"])
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!dir.is_empty()).then(|| PathBuf::from(dir))
}

/// The branch to rebase onto.
///
/// `--onto` names it, and names it as a **remote** branch — the work is
/// going to land there, and rebasing onto anything else builds a push the
/// remote will reject. Without `--onto` it is the remote's own HEAD: the
/// branch the clone was made from, and therefore the one a plain `ro ship`
/// means.
///
/// `None` means there is nothing to rebase onto. A local-only repo is that,
/// and it is an ordinary state rather than a failure.
fn base_for(repo: &Path, onto: Option<&str>) -> Option<String> {
    let remote_head = ro_git::primitives::symbolic_ref(repo, "refs/remotes/origin/HEAD")
        .ok()
        .flatten();
    let Some(onto) = onto.map(str::trim).filter(|b| !b.is_empty()) else {
        return remote_head;
    };
    // A branch the remote does not have yet is not an error: the push is
    // what creates it, and there is nothing to rebase onto. Falling back
    // to the remote's HEAD keeps the branch current with where the work
    // came from, which is the best answer available.
    if remote_branch_exists(repo, onto) {
        Some(format!("origin/{onto}"))
    } else {
        remote_head
    }
}

/// Does the **remote** have this branch?
///
/// Two questions have been asked here under this name, and the wrong one is
/// the dangerous one. `git show-ref --verify refs/remotes/origin/<branch>`
/// reads the **local remote-tracking refs**, so on any clone that is not a
/// full clone — `--single-branch`, `--depth`, or merely stale — the answer
/// was "no" for a branch the remote has had since before the clone was made.
/// Both callers then act on that answer: `base_for` rebases onto the wrong
/// base and the push is rejected non-fast-forward, and the "remote does not
/// have it" warning fires on a branch that was **overwritten** rather than
/// created, advising `git push origin --delete <branch>` on somebody else's
/// work.
///
/// `ls-remote --heads` asks the remote. It is one more round trip on a
/// network the caller is already using, and it is the only question whose
/// answer is about the remote.
///
/// A remote that cannot be reached answers `false`. That is the old answer,
/// and it is the conservative one for the warning — claiming a branch was
/// created when the remote was unreachable would be the claim that cannot be
/// walked back.
fn remote_branch_exists(repo: &Path, branch: &str) -> bool {
    std::process::Command::new("git")
        // The two global `-c` options, and the reason this is not a raw
        // `ls-remote`: without them git consults the machine's credential
        // helper, which on Windows is Git Credential Manager and opens a
        // **window** asking for a password. This is the one question here
        // that touches the network, so it was the one place on this path
        // where `GIT_TERMINAL_PROMPT=0` did not help — that silences a
        // terminal, not a helper. An unreachable remote answers `false`
        // below either way, so refusing to authenticate costs this check
        // nothing and saves the user a dialog.
        .args(["-c", "credential.helper=", "-c", "core.askPass="])
        .args([
            "ls-remote",
            "--heads",
            "origin",
            &format!("refs/heads/{branch}"),
        ])
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .env("GCM_UI", "Never")
        .env("LC_ALL", "C")
        .output()
        .map(|o| o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false)
}

/// Is the current `HEAD` already reachable from the remote-tracking branch
/// for `branch`?
///
/// `--amend` on such a commit rewrites published history, so the answer is
/// the gate. `merge-base --is-ancestor` is the right question: it is true
/// when the remote's tip is an ancestor of HEAD, which covers both "HEAD is
/// exactly the published commit" and "HEAD is ahead of it" — the second
/// being a normal new commit on top of something already published, which
/// amending would also rewrite.
///
/// A failure to answer is **false**, not a refusal: an unreadable ref store
/// must not turn every `--amend` into a dead end. The amend then proceeds
/// and the push reports the truth, which is where it would have been found
/// anyway.
fn head_is_on_remote(repo: &Path, branch: &str) -> bool {
    let Ok(Some(head)) = ro_git::read::head_oid(repo) else {
        return false;
    };
    std::process::Command::new("git")
        .args([
            "merge-base",
            "--is-ancestor",
            &head,
            &format!("refs/remotes/origin/{branch}"),
        ])
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Rebase onto the remote's base — or onto the one `--onto` named — with
/// the worktree's changes preserved.
///
/// One git invocation, so there is no window in which the stash exists
/// and ro has not yet asked git to pop it. Driving `stash` / `rebase` /
/// `stash pop` as three steps ro controls itself would be the same thing
/// with **three places to forget the cleanup** — the failure class the
/// credential design avoids by never rewriting a remote in the first
/// place.
fn rebase_onto_remote_base(
    repo: &Path,
    onto: Option<&str>,
    host: Option<&str>,
    credential: Option<&SecretString>,
) -> Result<RebaseState, RebaseError> {
    // Fetch first, always. Rebasing onto a stale
    // `refs/remotes/origin/HEAD` reorders the branch onto a base the
    // remote has already moved past, which is the one ordering mistake
    // that makes a rebase look like it worked.
    //
    // The credential travels with it, for the reason the fetch in `run_one`
    // does: this fetch used to go out anonymously too, so a private repo
    // failed here and the rebase never ran. The result is deliberately
    // ignored — a fetch that cannot authenticate is not a reason to skip
    // the rebase, and the push that follows reports the real failure —
    // but the header has to be on the request.
    let _ = ro_git::mutation::fetch_with_credential(
        repo,
        &ro_git::mutation::FetchOpts {
            host: host.map(str::to_string),
            ..Default::default()
        },
        credential,
    );

    let Some(base) = base_for(repo, onto) else {
        return Ok(RebaseState::NoRemote);
    };
    // `--autostash` is the whole mechanism: git stashes, rebases, and pops
    // in one command, so a failure to pop fails the whole rebase rather
    // than leaving a half-popped tree behind for ro to walk away from.
    let out = std::process::Command::new("git")
        .args(["rebase", "--autostash", &base])
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| RebaseError::Failed {
            base: base.clone(),
            stderr: format!("running git rebase --autostash: {e}"),
        })?;

    if out.status.success() {
        // **The exit code is not the outcome.** `git rebase --autostash`
        // returns 0 when the rebase itself succeeds and the *pop* that
        // follows conflicts — verified against the real git, not assumed:
        // stderr says "Applying autostash resulted in conflicts." and the
        // status is 0. So the arm below, which only read stderr when
        // `!out.status.success()`, never saw it, returned `Rebased`, and the
        // engine then committed a tree full of conflict markers and the push
        // published them as a clean `pushed`. The user's work was not lost —
        // it was in `stash@{0}` — but the row said `pushed` and the remote
        // held markers.
        //
        // The question is asked of the **tree**, not of the streams, and it is
        // the same question `ro_git::mutation::pull_in` asks for the same
        // flag: `git rebase --autostash` and `git pull --autostash` share the
        // contract, so they share the fix.
        let unmerged = ro_git::mutation::unmerged_paths(repo, &ro_git::mutation::RunOpts::none());
        if !unmerged.is_empty() {
            return Err(RebaseError::Conflicted { base });
        }
        return Ok(RebaseState::Rebased);
    }
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    // A conflict is a stage, not a failure, and the two are told apart
    // here rather than by the caller reading the message.
    if stderr.contains("CONFLICT") || stderr.contains("could not apply") {
        return Err(RebaseError::Conflicted { base });
    }
    // Everything else is a real failure, and the recovery is stated
    // rather than assumed.
    Err(RebaseError::Failed { base, stderr })
}

/// `--resolve` is a stage, not a dead end.
///
/// The bug this module exists for: a conflicting rebase returned the same
/// `Err` as a broken repository, and that `Err` hard-returned `Failed`
/// from the middle of the rebase block — so the resolve stage below it was
/// unreachable for exactly the case it exists to handle. The second engine
/// dispatch never ran, and `--resolve` printed "the rebase conflicted"
/// and did nothing.
#[cfg(test)]
mod resolve_stage_tests {
    use super::tests_support::*;
    use super::*;

    /// A conflicting rebase **with** `--resolve` dispatches the engine.
    ///
    /// The engine is a script standing in for an agent: it takes one side
    /// and stages it, which is what a resolver is asked to do. What is
    /// being tested is that ro *asks* — the marker file is the proof, and
    /// on the old code it is never written because the dispatch is
    /// unreachable.
    #[cfg(unix)]
    #[test]
    fn a_conflicting_rebase_with_resolve_dispatches_the_engine() {
        let f = Fixture::new();
        // Not `main`: the fixture starts there, and `main` is refused
        // before the pipeline runs at all. The rebase base is still
        // `origin/main` — it is `origin/HEAD` that decides — so a feature
        // branch conflicts exactly the same way.
        run_git(f.repo(), &["checkout", "-q", "-b", "feat/x"]);
        f.make_conflict();
        let marker = f
            .repo()
            .parent()
            .unwrap()
            .join("ro-test-resolve-ran.marker");
        let plan = RepoPlan {
            engine: resolving_engine(&f, &marker),
            ..plan_on(&f, "feat/x", None)
        };
        let opts = RunOptions {
            how_far: HowFar::Push,
            resolve_conflicts: true,
            state_dir: f.repo().join(".ro-state"),
            ..Default::default()
        };

        let outcome = run_one(&plan, &opts);

        assert!(
            marker.exists(),
            "the engine must be dispatched on a conflicting rebase with \
             --resolve; the marker is its proof. Outcome: {outcome:?}"
        );
        // And the run is not a plain `Failed` — a conflict that was
        // handed to the engine is not a failure of the repo.
        assert!(
            !matches!(outcome, RepoOutcome::Failed { .. }),
            "a conflict handed to the engine is not a repo failure, got {outcome:?}"
        );
    }

    /// Without `--resolve` the same conflict is a hand-over, and the tree
    /// is left mid-rebase on purpose.
    ///
    /// This is the existing intent and the reason the flag is opt-in: a
    /// model editing files mid-rebase that nobody asked about is worse than
    /// a stop. Aborting would discard the work that caused the conflict.
    #[test]
    fn a_conflicting_rebase_without_resolve_hands_over_and_pushes_nothing() {
        let f = Fixture::new();
        // Not `main`: the fixture starts there, and `main` is refused
        // before the pipeline runs at all. The rebase base is still
        // `origin/main` — it is `origin/HEAD` that decides — so a feature
        // branch conflicts exactly the same way.
        run_git(f.repo(), &["checkout", "-q", "-b", "feat/x"]);
        f.make_conflict();
        let plan = plan_on(&f, "feat/x", None);
        let opts = RunOptions {
            how_far: HowFar::Push,
            state_dir: f.repo().join(".ro-state"),
            ..Default::default()
        };

        let outcome = run_one(&plan, &opts);

        match &outcome {
            RepoOutcome::HandedOver { detail } => {
                assert!(
                    detail.contains("shared.txt"),
                    "the hand-over must name the conflicted paths, got: {detail}"
                );
            }
            other => panic!("a conflict with no --resolve must be handed over, got {other:?}"),
        }

        // The tree is mid-rebase, and that is deliberate: aborting would
        // discard the work that caused the conflict.
        assert!(
            ro_git::primitives::is_rebase_in_progress(f.repo()),
            "the tree must be left mid-rebase, not aborted"
        );
        // And nothing reached the remote.
        assert!(
            !f.remote_has("feat/x"),
            "a hand-over must not push: {:?}",
            f.remote_subjects()
        );
    }

    /// An engine that is not installed is a **failure**, not a conflict.
    ///
    /// The old code threw the engine's outcome away, so a missing binary
    /// and a semantic conflict both ended up as "conflict: N file(s) need
    /// you" — telling the user to go and read conflict markers that a
    /// missing binary created.
    #[test]
    fn an_unavailable_engine_is_a_failure_not_a_conflict() {
        let f = Fixture::new();
        // Not `main`: the fixture starts there, and `main` is refused
        // before the pipeline runs at all. The rebase base is still
        // `origin/main` — it is `origin/HEAD` that decides — so a feature
        // branch conflicts exactly the same way.
        run_git(f.repo(), &["checkout", "-q", "-b", "feat/x"]);
        f.make_conflict();
        // A binary on no PATH anywhere, so this is `Unavailable` on every
        // machine rather than on the one without `claude`.
        let engine = ro_engine::resolve(
            "claude",
            &ro_engine::EngineSlots::default(),
            Some("ro-test-no-such-agent-binary-8f3a"),
        )
        .expect("claude is one of the three");
        let plan = RepoPlan {
            engine,
            ..plan_on(&f, "feat/x", None)
        };
        let opts = RunOptions {
            how_far: HowFar::Push,
            resolve_conflicts: true,
            state_dir: f.repo().join(".ro-state"),
            ..Default::default()
        };

        let outcome = run_one(&plan, &opts);

        match &outcome {
            RepoOutcome::Failed {
                error,
                class: ro_core::FailureClass::MissingProvider,
            } => {
                assert!(
                    error.contains("ro-test-no-such-agent-binary-8f3a"),
                    "the engine's own message must reach the user, got: {error}"
                );
                assert!(
                    !error.contains("conflicted"),
                    "a missing engine is not a conflicting rebase, and must not \
                     be reported as one, got: {error}"
                );
            }
            other => panic!("a missing engine is a failure, not a conflict, got {other:?}"),
        }
    }

    /// A conflict the engine settled is finished, not left mid-rebase.
    ///
    /// The whole point of the stage: ro verifies the index, runs
    /// `rebase --continue`, and pushes. Leaving the tree mid-rebase would
    /// strand the work and make the next run skip the repo forever.
    #[cfg(unix)]
    #[test]
    fn a_resolved_conflict_finishes_the_rebase_and_pushes() {
        let f = Fixture::new();
        // Not `main`: the fixture starts there, and `main` is refused
        // before the pipeline runs at all. The rebase base is still
        // `origin/main` — it is `origin/HEAD` that decides — so a feature
        // branch conflicts exactly the same way.
        run_git(f.repo(), &["checkout", "-q", "-b", "feat/x"]);
        f.make_conflict();
        let marker = f
            .repo()
            .parent()
            .unwrap()
            .join("ro-test-resolve-ran.marker");
        let plan = RepoPlan {
            engine: resolving_engine(&f, &marker),
            ..plan_on(&f, "feat/x", None)
        };
        let opts = RunOptions {
            how_far: HowFar::Push,
            resolve_conflicts: true,
            state_dir: f.repo().join(".ro-state"),
            ..Default::default()
        };

        let outcome = run_one(&plan, &opts);

        assert!(marker.exists(), "the engine must have run");
        assert!(
            !ro_git::primitives::is_rebase_in_progress(f.repo()),
            "the rebase must be finished, not left mid-rebase: {outcome:?}"
        );
        assert!(
            matches!(outcome, RepoOutcome::Pushed { .. }),
            "a resolved conflict must land on the remote, got {outcome:?}"
        );
    }

    /// An engine that resolves a conflict the way one is asked to.
    #[cfg(unix)]
    fn resolving_engine(f: &Fixture, marker: &std::path::Path) -> ResolvedEngine {
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
}

#[cfg(test)]
mod rebase_tests {
    use super::tests_support::*;
    use super::*;

    /// A conflicting `--autostash` pop is a **conflict**, not a rebase.
    ///
    /// `git rebase --autostash` exits 0 when the rebase succeeds and the
    /// *pop* that follows conflicts — verified against the real git, not
    /// assumed. `rebase_onto_remote_base` read the exit code and only looked
    /// at stderr when it was non-zero, so the `CONFLICT` string it matched for
    /// was never reached, the function returned `Rebased`, and the engine then
    /// committed a tree full of conflict markers and the push published them
    /// as a clean `pushed`. The user's work was not lost — it was in
    /// `stash@{0}` — but the row said `pushed` and the remote held markers.
    #[test]
    fn a_conflicting_autostash_pop_is_a_conflict_not_a_clean_rebase() {
        let f = Fixture::new();
        // Both sides start from a shared tree containing `shared.txt`.
        run_git(f.repo(), &["checkout", "-q", "-b", "work"]);
        f.write_local("shared.txt", "base version\n");
        run_git(f.repo(), &["add", "-A"]);
        run_git(f.repo(), &["commit", "-q", "-m", "add shared"]);
        run_git(f.repo(), &["push", "-q", "origin", "HEAD:main"]);

        // The local side now has an **uncommitted** edit of that file…
        f.write_local("shared.txt", "local version\n");
        // …and the remote changes it too. So `git rebase --autostash` stashes
        // the local edit, the rebase itself is a no-op (work is already on
        // `origin/main`), and the POP conflicts — the exact case git exits 0
        // for. Tracked on both sides on purpose: an untracked file is not
        // stashed by `--autostash` and git refuses the checkout instead.
        f.force_remote_file("shared.txt", "remote version\n");
        run_git(f.repo(), &["fetch", "-q", "origin"]);

        let state = rebase_onto_remote_base(f.repo(), None, None, None);

        match state {
            Err(RebaseError::Conflicted { .. }) => {}
            other => panic!(
                "a conflicting autostash pop must be a conflict, not a rebase; \
                 got {other:?}"
            ),
        }
        // And the tree really does hold conflict markers — the
        // classification is not being made on a tree that happens to be
        // clean. This is the content the push used to publish as `pushed`.
        let tree = std::fs::read_to_string(f.repo().join("shared.txt")).unwrap_or_default();
        assert!(
            tree.contains("<<<<<<<") && tree.contains(">>>>>>>"),
            "the worktree must hold conflict markers, or nothing was at stake; got: {tree:?}"
        );
    }

    /// The negative control: a clean pop is a rebase.
    ///
    /// Without this, the test above would pass for a function that reported
    /// `Conflicted` for every rebase.
    #[test]
    fn a_clean_autostash_pop_is_a_rebase() {
        let f = Fixture::new();
        f.write_local("feature.txt", "work in progress\n");
        f.other_clone_commits("remote moved on");
        run_git(f.repo(), &["fetch", "-q", "origin"]);

        let state = rebase_onto_remote_base(f.repo(), None, None, None);
        assert!(
            matches!(state, Ok(RebaseState::Rebased)),
            "a clean pop must be a rebase; got {state:?}"
        );
        assert!(
            !f.local_porcelain().contains("UU"),
            "no conflict markers may be left behind: {}",
            f.local_porcelain()
        );
    }

    /// The rebase happens **before** the engine, so the tree comes back
    /// already on the remote's base and the push is a fast-forward.
    ///
    /// The conventional order — commit, push, get rejected, rebase,
    /// force-push — rewrites the branch. A force-push across a fleet is
    /// the one operation here that can destroy someone else's work.
    #[test]
    fn a_dirty_tree_is_rebased_onto_the_remote_base_before_committing() {
        let f = Fixture::new();
        // The remote moves on.
        f.other_clone_commits("remote moved on");

        // Locally there is uncommitted work — the shape `ro ship` is
        // normally invoked in.
        f.write_local("feature.txt", "work in progress\n");

        let state = rebase_onto_remote_base(f.repo(), None, None, None).expect("the rebase runs");
        assert!(
            matches!(state, RebaseState::Rebased),
            "a dirty tree on a moved remote must rebase, got {state:?}"
        );

        // The local work survived the stash/rebase/pop cycle.
        assert!(
            f.local_porcelain().contains("feature.txt"),
            "the autostash must pop the work back, got: {}",
            f.local_porcelain()
        );
        // And the local branch now contains the remote's commit.
        assert!(
            f.local_contains("remote moved on"),
            "the branch must sit on the remote's base"
        );
    }

    /// A clean tree with the remote moved is a plain rebase.
    #[test]
    fn a_clean_tree_behind_the_remote_rebases_without_a_stash() {
        let f = Fixture::new();
        f.other_clone_commits("remote moved on");
        // Nothing local is dirty.
        assert!(f.local_porcelain().trim().is_empty());

        let state = rebase_onto_remote_base(f.repo(), None, None, None).expect("the rebase runs");
        assert!(matches!(state, RebaseState::Rebased), "got {state:?}");
        assert!(f.local_contains("remote moved on"));
    }

    /// Nothing to rebase is a real answer, not a failure.
    #[test]
    fn an_already_current_branch_is_up_to_date() {
        let f = Fixture::new();
        let state = rebase_onto_remote_base(f.repo(), None, None, None).expect("the rebase runs");
        assert!(
            matches!(state, RebaseState::Rebased | RebaseState::NoRemote),
            "a fresh fixture must not fail, got {state:?}"
        );
    }

    /// No remote, no base, no failure.
    ///
    /// A repo with no remote is a local-only repo, which is an ordinary
    /// state — not an error, and not a reason to stop.
    #[test]
    fn a_repo_with_no_remote_reports_it_rather_than_failing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("solo");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q", "-b", "main"]);
        assert_eq!(
            rebase_onto_remote_base(&repo, None, None, None).expect("must not fail"),
            RebaseState::NoRemote,
            "a local-only repo has no base to rebase onto, and that is a fact"
        );
    }

    /// A conflicting rebase is a per-repo FAILURE with the recovery
    /// stated — never a proceed on a half-popped tree.
    #[test]
    fn a_conflicting_rebase_fails_with_the_recovery_in_the_message() {
        let f = Fixture::new();
        // Both sides change the same line, so the rebase conflicts.
        f.write_local("shared.txt", "local version\n");
        run_git(f.repo(), &["add", "-A"]);
        run_git(f.repo(), &["commit", "-q", "-m", "local change"]);
        f.other_clone_commits("different change");
        // Move the remote's version of shared.txt to conflict.
        f.force_remote_file("shared.txt", "remote version\n");
        f.write_local("shared.txt", "local version\n");

        let err =
            rebase_onto_remote_base(f.repo(), None, None, None).expect_err("must fail loudly");
        let msg = err.to_string();
        assert!(
            msg.contains("rebase --abort") || msg.contains("rebase --continue"),
            "the message must state the recovery, got: {msg}"
        );
        assert!(
            msg.contains("Nothing was committed or pushed"),
            "and say plainly that nothing was written, got: {msg}"
        );
    }
}

/// Shared fixtures for the per-repo tests.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::{HowFar, RepoPlan, RunOptions};
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    /// A plan for this fixture, on `branch`, optionally aimed elsewhere by
    /// `--onto`, driven by the raw git backend.
    ///
    /// `base_branch` is a **stated fact about the repo**, not a reading of
    /// it, so a test that puts the checkout on a feature branch and still
    /// wants to exercise the protected-branch rule passes `main` here and
    /// gets the refusal. That is the whole point of the field.
    pub fn plan_on(f: &Fixture, branch: &str, onto: Option<&str>) -> RepoPlan {
        RepoPlan {
            repo_id: "r1".into(),
            label: "acme/api".into(),
            local_path: f.repo().to_path_buf(),
            clone_url: f.remote_path().to_string_lossy().into_owned(),
            base_branch: branch.to_string(),
            author_ref: None,
            credential_ref: None,
            onto: onto.map(str::to_string),
            identity: None,
            engine: ro_engine::resolve("git", &ro_engine::EngineSlots::default(), None)
                .expect("git is one of the three built-ins"),
        }
    }

    /// The ordinary `ro push` run against this fixture.
    pub fn opts_for(f: &Fixture) -> RunOptions {
        RunOptions {
            how_far: HowFar::Push,
            state_dir: f.repo().join(".ro-state"),
            ..Default::default()
        }
    }

    pub fn run_git(dir: &Path, args: &[&str]) {
        let out = run_git_out(dir, args);
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The same command, with its stdout returned.
    ///
    /// Separate from `run_git` rather than a flag on it, because a caller
    /// that wants the output is asking a different question from one that
    /// wants a side effect, and a `bool` parameter that changes what the
    /// function returns is how the two get confused.
    ///
    /// **One retry, and only on a transport failure.** A local-path remote
    /// cannot be "inaccessible" in the way a network one can — there is no
    /// network involved — so `unable to access the remote` here is git
    /// failing to spawn `git-upload-pack`, which on a memory-constrained
    /// machine means the fork did not succeed. The test suite runs many test
    /// binaries in parallel, and this one failed intermittently at exactly
    /// that point while passing every time it ran alone.
    ///
    /// The retry is deliberately narrow: only that one signature, only once,
    /// and the failure is still reported if it repeats. A blanket retry would
    /// hide a real failure behind a second attempt, which is the cost every
    /// other test in this file has been paying to avoid.
    pub fn run_git_out(dir: &Path, args: &[&str]) -> std::process::Output {
        let out = run_git_once(dir, args);
        let transport_failure =
            String::from_utf8_lossy(&out.stderr).contains("unable to access the remote");
        if out.status.success() || !transport_failure {
            return out;
        }
        run_git_once(dir, args)
    }

    fn run_git_once(dir: &Path, args: &[&str]) -> std::process::Output {
        // `credential.helper=` / `core.askPass=` on argv, and the reason is
        // this file specifically: several tests here repoint a fixture repo's
        // `origin` at `DemandingRemote`, a real HTTP server on loopback that
        // **demands** an Authorization header. Any later fixture `git` call in
        // such a repo is an unauthenticated HTTP request, and with the
        // machine's credential helper left in place git handed that to Git
        // Credential Manager on Windows — which opens a password dialog on the
        // developer's screen and the test sits there waiting for an answer
        // nobody is going to type. The dialog named the loopback port, which
        // is how it was traced back to here rather than to the code under
        // test.
        //
        // `GIT_TERMINAL_PROMPT=0` below never helped: it silences a
        // **terminal**, not a helper, and a helper is a separate program git
        // runs first.
        std::process::Command::new("git")
            .args(["-c", "credential.helper=", "-c", "core.askPass="])
            .args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "Never")
            .env("GCM_UI", "Never")
            .env("LC_ALL", "C")
            .output()
            .expect("git runs")
    }

    /// Clone `remote` into `root/name`, already on `main` with an identity
    /// configured, so no caller has to remember the order these must
    /// happen in.
    fn clone_at(root: &Path, name: &str, remote: &Path) -> PathBuf {
        run_git(root, &["clone", "-q", &remote.to_string_lossy(), name]);
        let c = root.join(name);
        run_git(&c, &["config", "user.email", "t@e.com"]);
        run_git(&c, &["config", "user.name", "T"]);
        // A bare remote's clone can land on a branch whose name came
        // from the local git config; the fixture must not.
        run_git(&c, &["checkout", "-q", "-B", "main"]);
        c
    }

    /// A local clone with an upstream it can fall behind.
    pub struct Fixture {
        // Kept alive so the temp directory outlives every clone in it.
        _tmp: TempDir,
        remote: PathBuf,
        other: PathBuf,
        local: PathBuf,
    }

    impl Fixture {
        pub fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let root = tmp.path().to_path_buf();

            // A bare remote with an explicit default branch. `git init
            // --bare` takes the name from the local git config, and a
            // fixture whose branch name depends on the machine is a
            // fixture that fails on somebody else's laptop.
            let remote = root.join("remote.git");
            std::fs::create_dir_all(&remote).unwrap();
            run_git(&remote, &["init", "--bare", "-q", "--initial-branch=main"]);

            // ONE root commit, made in `other` and pushed. If each clone
            // made its own, the two would sit on divergent roots and
            // every later rebase would be a real conflict about nothing —
            // the fixture would be testing the wrong thing entirely.
            let other = clone_at(&root, "other", &remote);
            run_git(&other, &["commit", "-q", "--allow-empty", "-m", "init"]);
            run_git(&other, &["push", "-q", "-u", "origin", "main"]);

            // `local` is cloned AFTER, so it starts from that commit with
            // its upstream already tracking origin/main.
            let local = clone_at(&root, "local", &remote);

            // The base is read from `refs/remotes/origin/HEAD`, and a
            // clone leaves it dangling — so without this every repo looks
            // like it has no remote at all, which is a real failure mode
            // precisely because the base must come from the repository
            // and not from configuration.
            run_git(&local, &["remote", "set-head", "origin", "-a"]);

            Self {
                _tmp: tmp,
                remote,
                other,
                local,
            }
        }

        pub fn repo(&self) -> &Path {
            &self.local
        }

        /// The *other* clone, which is how the remote is moved.
        pub fn other(&self) -> &Path {
            &self.other
        }

        pub fn write_local(&self, name: &str, contents: &str) {
            let p = self.local.join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, contents).unwrap();
        }

        /// Commit and push from the *other* clone, so the remote moves.
        pub fn other_clone_commits(&self, message: &str) {
            std::fs::write(self.other.join("remote.txt"), message).unwrap();
            run_git(&self.other, &["add", "-A"]);
            run_git(&self.other, &["commit", "-q", "-m", message]);
            run_git(&self.other, &["push", "-q", "origin", "HEAD:main"]);
        }

        /// Put both sides on the same branch, on the same line, so a
        /// rebase genuinely conflicts.
        ///
        /// The local side is **committed** rather than left dirty on
        /// purpose. A dirty tree is stashed by `--autostash` and restored
        /// only when the rebase *finishes*, so an uncommitted change is not
        /// in the worktree while the conflict is being resolved — and a
        /// test about what the resolver sees would be reading an empty
        /// tree and passing for the wrong reason.
        pub fn make_conflict(&self) {
            std::fs::write(self.local.join("shared.txt"), "local version\n").unwrap();
            run_git(&self.local, &["add", "-A"]);
            run_git(&self.local, &["commit", "-q", "-m", "local change"]);
            // The remote side lands on `main`, which is what
            // `refs/remotes/origin/HEAD` points at — the base a plain
            // rebase uses.
            self.force_remote_file("shared.txt", "remote version\n");
            run_git(&self.local, &["fetch", "-q", "origin"]);
        }

        /// Put a file's content directly into the remote, so a rebase
        /// conflicts on it.
        pub fn force_remote_file(&self, name: &str, contents: &str) {
            run_git(&self.other, &["fetch", "-q", "origin"]);
            std::fs::write(self.other.join(name), contents).unwrap();
            run_git(&self.other, &["add", "-A"]);
            run_git(&self.other, &["commit", "-q", "-m", "remote change"]);
            run_git(
                &self.other,
                &["push", "-q", "--force", "origin", "HEAD:main"],
            );
        }

        /// Is the checkout dirty? A precondition assertion needs a question
        /// that is about the *worktree*, not about a lock file.
        pub fn local_is_dirty(&self) -> bool {
            let out = std::process::Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&self.local)
                .output()
                .expect("git runs");
            !String::from_utf8_lossy(&out.stdout).trim().is_empty()
        }

        pub fn local_porcelain(&self) -> String {
            String::from_utf8_lossy(
                &std::process::Command::new("git")
                    .args(["status", "--porcelain"])
                    .current_dir(&self.local)
                    .output()
                    .expect("git runs")
                    .stdout,
            )
            .into_owned()
        }

        pub fn local_contains(&self, message: &str) -> bool {
            let out = std::process::Command::new("git")
                .args(["log", "--format=%s"])
                .current_dir(&self.local)
                .output()
                .expect("git runs");
            String::from_utf8_lossy(&out.stdout).contains(message)
        }

        pub fn remote_path(&self) -> &Path {
            &self.remote
        }

        /// Does the remote have a branch with this name?
        pub fn remote_has(&self, branch: &str) -> bool {
            self.remote_refs().iter().any(|r| r == branch)
        }

        /// Every branch on the remote.
        pub fn remote_refs(&self) -> Vec<String> {
            let out = std::process::Command::new("git")
                .args(["for-each-ref", "--format=%(refname:short)", "refs/heads"])
                .current_dir(&self.remote)
                .output()
                .expect("git runs");
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::to_string)
                .collect()
        }

        /// Commit subjects on a remote branch, for a failure message.
        pub fn remote_subjects(&self) -> Vec<String> {
            self.remote_refs()
        }
    }
}

#[cfg(test)]
mod protected_tests {
    use super::tests_support::*;
    use super::*;
    use tempfile::TempDir;

    /// `--amend` on a HEAD the remote has already seen is **refused**.
    ///
    /// The help promises it: "Refused when HEAD is already on the remote:
    /// amending a commit someone else has seen rewrites history under a
    /// message that no longer describes it." The refusal did not exist —
    /// the amend ran, reported `committed`, exited 0, and left the branch
    /// permanently diverged with the remote still carrying the orphaned
    /// original. The next push is then rejected non-fast-forward, which is
    /// exactly the outcome the gate is documented to prevent.
    #[test]
    fn amend_on_a_pushed_head_is_refused() {
        let f = Fixture::new();
        run_git(f.repo(), &["checkout", "-q", "-b", "feat/amend"]);
        run_git(f.repo(), &["push", "-q", "-u", "origin", "feat/amend"]);
        f.write_local("f.txt", "work\n");
        run_git(f.repo(), &["add", "-A"]);
        run_git(f.repo(), &["commit", "-q", "-m", "published"]);
        run_git(f.repo(), &["push", "-q", "origin", "feat/amend"]);

        // HEAD is on the remote, which is the condition the gate names.
        assert!(
            f.remote_has("feat/amend"),
            "the fixture must have published the commit"
        );

        f.write_local("g.txt", "more work\n");
        let plan = plan_on(&f, "feat/amend", None);
        let mut opts = opts_for(&f);
        opts.amend = true;
        let outcome = run_one(&plan, &opts);

        match &outcome {
            RepoOutcome::Refused { branch, reason } => {
                assert_eq!(branch, "feat/amend");
                assert!(
                    reason.contains("already on the remote"),
                    "the message must name the reason, got: {reason}"
                );
            }
            other => panic!("amending a published commit must be refused, got {other:?}"),
        }

        // And the remote still carries the original, un-rewritten: its tip
        // is still the `published` commit, not the amended one. Read off the
        // bare remote directly, because that is the ground truth and
        // `remote_subjects` lists branch names rather than commits.
        let tip = run_git_out(
            f.remote_path(),
            &["log", "-1", "--format=%s", "refs/heads/feat/amend"],
        );
        let tip = String::from_utf8_lossy(&tip.stdout);
        assert_eq!(
            tip.trim(),
            "published",
            "the remote's tip must be the original commit, got {tip:?}"
        );
    }

    /// The same refusal on `ro commit`, which never pushes.
    ///
    /// This is the case the E2E pass found: `ro commit --amend` amended a
    /// published commit, reported `committed`, exited 0, and left the branch
    /// diverged with the remote still carrying the orphaned original. The
    /// damage is done the moment HEAD is rewritten, so "this run does not
    /// push" is not a reason to allow it — the next push is where it
    /// surfaces, as a non-fast-forward rejection.
    #[test]
    fn amend_on_a_pushed_head_is_refused_for_commit_too() {
        let f = Fixture::new();
        run_git(f.repo(), &["checkout", "-q", "-b", "feat/amendc"]);
        run_git(f.repo(), &["push", "-q", "-u", "origin", "feat/amendc"]);
        f.write_local("f.txt", "work\n");
        run_git(f.repo(), &["add", "-A"]);
        run_git(f.repo(), &["commit", "-q", "-m", "published"]);
        run_git(f.repo(), &["push", "-q", "origin", "feat/amendc"]);

        f.write_local("g.txt", "more work\n");
        let plan = plan_on(&f, "feat/amendc", None);
        let mut opts = opts_for(&f);
        opts.how_far = HowFar::Commit;
        opts.amend = true;
        let outcome = run_one(&plan, &opts);

        assert!(
            matches!(outcome, RepoOutcome::Refused { .. }),
            "ro commit --amend on a published commit must be refused, got {outcome:?}"
        );
    }

    /// The negative control: amending a commit the remote has never seen is
    /// still allowed, so the fix is not "never amend".
    #[test]
    fn amend_on_an_unpushed_head_is_allowed() {
        let f = Fixture::new();
        run_git(f.repo(), &["checkout", "-q", "-b", "feat/amend2"]);
        f.write_local("f.txt", "work\n");
        run_git(f.repo(), &["add", "-A"]);
        run_git(f.repo(), &["commit", "-q", "-m", "local only"]);
        // Deliberately NOT pushed.

        f.write_local("g.txt", "more work\n");
        let plan = plan_on(&f, "feat/amend2", None);
        let mut opts = opts_for(&f);
        opts.amend = true;
        let outcome = run_one(&plan, &opts);

        assert!(
            !matches!(outcome, RepoOutcome::Refused { .. }),
            "amending an unpublished commit must be allowed, got {outcome:?}"
        );
    }

    /// `--amend` with no `--message` keeps the subject the commit already
    /// had.
    ///
    /// It used to read `subject_of(repo, &oid)` where `oid` is the commit
    /// the engine *just* created — so it read the engine's own placeholder
    /// (`wip on <branch>`) rather than the commit being amended, and
    /// silently replaced a subject the user wrote. `git commit --amend`
    /// keeps it; the doc comment above `subject_of` says that is the point.
    #[test]
    fn amend_without_a_message_keeps_the_original_subject() {
        let f = Fixture::new();
        run_git(f.repo(), &["checkout", "-q", "-b", "feat/amend3"]);
        f.write_local("f.txt", "work\n");
        run_git(f.repo(), &["add", "-A"]);
        run_git(
            f.repo(),
            &["commit", "-q", "-m", "feat(real): a meaningful subject"],
        );

        f.write_local("g.txt", "more work\n");
        let plan = plan_on(&f, "feat/amend3", None);
        let mut opts = opts_for(&f);
        // Commit, not push: the subject rides on the `Committed` outcome,
        // and this test is about the message, not the ref.
        opts.how_far = HowFar::Commit;
        opts.amend = true;
        let outcome = run_one(&plan, &opts);

        assert!(
            matches!(outcome, RepoOutcome::Committed { .. }),
            "the amend must land, got {outcome:?}"
        );

        // Read the subject off the repository rather than off the outcome:
        // `Committed.message` is an empty string at its only construction
        // site, so asserting on it would be asserting on nothing. The commit
        // is the ground truth.
        let out = run_git_out(f.repo(), &["log", "-1", "--format=%s"]);
        let subject = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            subject.trim(),
            "feat(real): a meaningful subject",
            "the original subject must survive the amend, got {subject:?}"
        );
    }

    /// `main` refuses, names the fix, and **attempts no push**.
    ///
    /// The reflog check is the part that matters: "it printed a refusal"
    /// and "it did not push" are different claims, and only the second is
    /// the one a user needs.
    #[test]
    fn a_protected_branch_refuses_and_pushes_nothing() {
        let f = Fixture::new();
        f.write_local("f.txt", "work\n");

        let plan = plan_on(&f, "main", None);
        let outcome = run_one(&plan, &opts_for(&f));

        match &outcome {
            RepoOutcome::Refused { branch, reason } => {
                assert_eq!(branch, "main");
                assert!(
                    reason.contains("checkout -b"),
                    "the message must name the fix, got: {reason}"
                );
            }
            other => panic!("a protected branch must be refused, got {other:?}"),
        }

        // The remote's ref is untouched.
        assert!(
            !f.remote_has("work"),
            "nothing may reach the remote; the remote has: {:?}",
            f.remote_subjects()
        );
    }

    /// The case-insensitivity, which the previous exact match missed.
    #[test]
    fn the_protected_set_is_case_insensitive() {
        for b in ["main", "Main", "MAIN", "master", "MASTER", "Master"] {
            assert!(
                ro_git::primitives::is_protected_branch(b),
                "{b} must be protected"
            );
        }
        for b in [
            "production",
            "PRODUCTION",
            "staging",
            "release/1.2",
            "RELEASE/9",
        ] {
            assert!(
                ro_git::primitives::is_protected_branch(b),
                "{b} must be protected"
            );
        }
        // And the negative, so the check is not simply always true.
        for b in ["feature/x", "mainline", "release-notes", "fix/main"] {
            assert!(
                !ro_git::primitives::is_protected_branch(b),
                "{b} must NOT be protected"
            );
        }
    }

    /// `--onto` is the escape, and it must actually work.
    #[test]
    fn onto_is_the_escape_from_a_protected_branch() {
        let f = Fixture::new();
        run_git(f.repo(), &["checkout", "-q", "-b", "feat/x"]);
        run_git(f.repo(), &["push", "-q", "-u", "origin", "feat/x"]);
        f.write_local("f.txt", "work\n");

        // The repo is on a feature branch, and the work is aimed at
        // another one: exactly what `--onto` is for.
        let plan = plan_on(&f, "main", Some("feat/x"));
        let outcome = run_one(&plan, &opts_for(&f));

        assert!(
            !matches!(outcome, RepoOutcome::Refused { .. }),
            "--onto must lift the refusal, got {outcome:?}"
        );
    }

    /// `--onto` naming a **protected** branch must not be a bypass.
    ///
    /// The guard was `if plan.onto.is_none() && is_protected_branch(&base)`,
    /// so it tested the *checkout* and was skipped entirely whenever `--onto`
    /// was present. Naming `main`, `staging`, `production` or `release/1.0`
    /// as the destination pushed straight onto it, exit 0, from any checkout.
    /// FEATURES.md: "Push is gated three ways — the branch must not be
    /// protected. Any gate failing means no push." The protection is a
    /// property of the branch being **written to**, not of where you happen
    /// to be standing.
    #[test]
    fn onto_a_protected_branch_is_refused() {
        for onto in ["main", "staging", "production", "release/1.0"] {
            let f = Fixture::new();
            run_git(f.repo(), &["checkout", "-q", "-b", "feat/x"]);
            run_git(f.repo(), &["push", "-q", "-u", "origin", "feat/x"]);
            f.write_local("f.txt", "work\n");

            let plan = plan_on(&f, "main", Some(onto));
            let outcome = run_one(&plan, &opts_for(&f));

            match &outcome {
                RepoOutcome::Refused { branch, .. } => {
                    assert_eq!(
                        branch, onto,
                        "the refusal must name the protected destination, got {branch}"
                    );
                }
                other => panic!("--onto {onto} must be refused, got {other:?}"),
            }

            // And nothing **new** reached the remote under that name. The
            // fixture's setup push already created the branch, so the
            // assertion is on the commit count, not on the ref's existence:
            // a refusal that pushed would have advanced it.
            let before = f.remote_subjects();
            let plan = plan_on(&f, "main", Some(onto));
            let _ = run_one(&plan, &opts_for(&f));
            assert_eq!(
                f.remote_subjects(),
                before,
                "--onto {onto} must not write to the remote; it went from \
                 {before:?} to {:?}",
                f.remote_subjects()
            );
        }
    }

    /// The negative control: `--onto` naming an ordinary branch still works.
    /// The fix must not be "never honour --onto".
    #[test]
    fn onto_an_unprotected_branch_still_works() {
        let f = Fixture::new();
        run_git(f.repo(), &["checkout", "-q", "-b", "feat/x"]);
        run_git(f.repo(), &["push", "-q", "-u", "origin", "feat/x"]);
        f.write_local("f.txt", "work\n");

        let plan = plan_on(&f, "main", Some("feat/other"));
        let outcome = run_one(&plan, &opts_for(&f));

        assert!(
            !matches!(outcome, RepoOutcome::Refused { .. }),
            "--onto to an unprotected branch must be allowed, got {outcome:?}"
        );
    }

    /// A failed fetch is a **failure**, not a silent success.
    ///
    /// The code says "A network failure is a failure, not a silent skip: the
    /// user asked for an up-to-date push" and then only matches `Err`. But
    /// `ro_git::mutation::fetch` returns
    /// `Ok(GitCommandResult { status: 128, .. })` for a non-zero exit —
    /// `Err` is reserved for a spawn failure — so the check could never
    /// fire. `ro ship` committed and pushed onto a base it never fetched and
    /// reported success, with git's fatal message appearing nowhere.
    #[test]
    fn a_failed_fetch_is_reported_as_a_failure() {
        let f = Fixture::new();
        run_git(f.repo(), &["checkout", "-q", "-b", "feat/fetch"]);
        run_git(f.repo(), &["push", "-q", "-u", "origin", "feat/fetch"]);
        f.write_local("f.txt", "work\n");

        // The fetch fails because `origin` now names a path that is not
        // there. No shim, and so no platform in the fixture.
        //
        // This used to be a `git` shim on `PATH` that exited 128 on `fetch`
        // and exec'd the real git for everything else. That fixture was
        // Unix-only in a way nothing declared: the shim was a `#!/bin/sh`
        // script, and on Windows `Command::new("git")` resolves through the
        // OS loader, which will not execute an extensionless file. The shim
        // was therefore never invoked there, the real fetch succeeded, and
        // the test asserted "a failed fetch must be a failure" over a push
        // that had actually landed — green on Linux, red on Windows, and
        // testing nothing at all in between.
        //
        // An unreachable remote is the same condition with nothing
        // platform-specific in it. Real git fails the fetch on every OS,
        // which is all the gate under test ever cared about: the exit status
        // of the fetch, not who produced it.
        let missing = f.remote_path().with_file_name("no-such-remote.git");
        run_git(
            f.repo(),
            &["remote", "set-url", "origin", &missing.to_string_lossy()],
        );

        let plan = plan_on(&f, "feat/fetch", None);
        let mut opts = opts_for(&f);
        // Ship, not Push: the fetch step is `HowFar::fetches()`, which is
        // Ship alone. Testing it on Push would pass without ever running a
        // git fetch, which is the vacuous shape this test exists to avoid.
        opts.how_far = HowFar::Ship;
        let outcome = run_one(&plan, &opts);

        match &outcome {
            RepoOutcome::Failed {
                error,
                class: ro_core::FailureClass::MissingProvider,
            } => {
                assert!(
                    error.contains("fetch failed"),
                    "the failure must name the fetch, got: {error}"
                );
            }
            other => panic!("a failed fetch must be a failure, got {other:?}"),
        }
    }

    /// A real HTTP git remote that demands a credential, and reports the
    /// `Authorization` header it was actually handed.
    ///
    /// **Why a server and not a `git` shim.** The obvious fixture is a shim
    /// on `PATH` that records the env it was given, and it is the wrong one
    /// here: `PATH` is process-global, and only the tests that *install* a
    /// shim take the testkit lock — every other test in this binary runs
    /// `git` by bare name and is not holding it. So while a shim is first on
    /// `PATH`, a test on another thread can resolve `git` to that script and
    /// then have the directory deleted underneath it, which fails as
    /// `cannot open /tmp/.tmpXXXX/bin/git: No such file` in a test that has
    /// nothing to do with credentials. Leaking the directory stops the
    /// not-found and leaves a different race — a concurrent test runs the
    /// shim, which `exec`s the real git for everything that is not a fetch —
    /// and that one is not benign: it makes the *fixture* the thing under
    /// test, so the assertion reads whatever the other thread's fetch
    /// happened to carry.
    ///
    /// A loopback server has no process-global state at all. The header is
    /// read off the wire, which is the same evidence the original refutation
    /// used, and the server is bound to an ephemeral port so two of them can
    /// run at once.
    struct DemandingRemote {
        /// Kept alive: dropping the `TempDir` would delete the bare repo the
        /// server is about while a fetch is still in flight.
        _tmp: TempDir,
        /// One entry per request, appended as it arrives.
        seen: std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>>,
        /// Set to ask the accept loop to stop, so the thread joins instead of
        /// outliving the test and holding the port.
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl DemandingRemote {
        /// A bare repo on loopback that answers every request with 401 and
        /// records the `Authorization` header it was given.
        ///
        /// The 401 is the point: a fetch that is not authenticated **fails**,
        /// so "the credential reached the remote" and "the fetch succeeded"
        /// are the same observation rather than two that can disagree.
        ///
        /// The listener stays open for the whole test rather than answering
        /// once. `ro ship` fetches twice — the explicit fetch and the one
        /// inside the rebase — and both have to be observed; a one-shot
        /// server would answer the first and leave the second to a connection
        /// that is refused, which is a *different* failure and reads as one.
        fn start() -> (Self, String) {
            use std::io::{Read, Write};
            use std::net::TcpListener;

            let tmp = TempDir::new().unwrap();
            let bare = tmp.path().join("remote.git");
            std::fs::create_dir_all(&bare).unwrap();
            run_git(&bare, &["init", "--bare", "-q", "--initial-branch=main"]);

            let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port is bindable");
            listener
                .set_nonblocking(true)
                .expect("the listener can poll instead of blocking");
            let port = listener.local_addr().expect("the socket is bound").port();
            let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let seen_thread = seen.clone();
            let stop_thread = stop.clone();
            let worker = std::thread::spawn(move || {
                while !stop_thread.load(std::sync::atomic::Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let _ = stream.set_nonblocking(false);
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                            let mut chunk = [0u8; 8192];
                            // The request head is the whole of what is under
                            // test; a request body, if one follows, is not.
                            let Ok(n) = stream.read(&mut chunk) else {
                                continue;
                            };
                            let head = String::from_utf8_lossy(&chunk[..n]).into_owned();
                            let header = head
                                .lines()
                                .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                                .and_then(|l| l.split_once(':'))
                                .map(|(_, v)| v.trim().to_string());
                            seen_thread
                                .lock()
                                .expect("the lock is poison-free")
                                .push(header);
                            let _ = stream.write_all(
                                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n",
                            );
                            let _ = stream.flush();
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            });

            let remote = Self {
                _tmp: tmp,
                seen,
                stop,
            };
            let url = format!("http://127.0.0.1:{port}");
            // `worker` is detached: `Drop` sets `stop` and joins it, so the
            // accept loop ends and the port is released with the `Self`.
            std::mem::forget(worker);
            (remote, url)
        }

        /// Every `Authorization` header the remote was handed, in order.
        fn headers(&self) -> Vec<Option<String>> {
            self.seen.lock().expect("the lock is poison-free").clone()
        }
    }

    impl Drop for DemandingRemote {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// The fetch a `ro ship` runs carries the row's credential.
    ///
    /// **This is the bug.** The credential was resolved at the push and
    /// nowhere else, so a private HTTPS repo — the one case that needs a
    /// per-row credential at all — fetched anonymously, failed at the fetch,
    /// and never reached the push that did carry the token. A refutation run
    /// against a real HTTP remote with header capture saw exactly this: two
    /// `git-upload-pack` requests with no `Authorization`, two
    /// `git-receive-pack` requests with one.
    ///
    /// The remote is a real HTTP server on loopback, so the header is read
    /// off the wire rather than out of a child's environment — see
    /// [`DemandingRemote`] for why a `git` shim cannot be used here.
    ///
    /// Ship, not Push: `HowFar::fetches()` is Ship alone, so this would
    /// never run a fetch at all.
    #[test]
    fn the_ship_fetch_carries_the_rows_credential() {
        let f = Fixture::new();
        run_git(f.repo(), &["checkout", "-q", "-b", "feat/cred"]);
        run_git(f.repo(), &["push", "-q", "-u", "origin", "feat/cred"]);
        f.write_local("f.txt", "work\n");

        let (remote, url) = DemandingRemote::start();

        // The row names the demanding remote, so the fetch has to authenticate
        // or fail. The local origin is untouched: the fetch goes to `url`,
        // and the rebase's fetch goes there too.
        let mut plan = plan_on(&f, "feat/cred", None);
        plan.clone_url = format!("{url}/acme/api.git");
        plan.credential_ref = Some("env:RO_SHIP_FETCH_CREDENTIAL_TEST".to_string());
        // The remote is a bare repo with no refs, so `origin/HEAD` is dangling
        // and the rebase has nothing to land on. Without this the run stops
        // at `NoRemote` and never makes the second fetch — the one inside the
        // rebase, which had the same hole.
        run_git(f.repo(), &["remote", "set-url", "origin", &plan.clone_url]);

        let mut opts = opts_for(&f);
        opts.how_far = HowFar::Ship;
        let outcome = unsafe {
            ro_testkit::TestEnv::new()
                .var("RO_SHIP_FETCH_CREDENTIAL_TEST", "ghp_ship_fetch_marker")
                .run(|| run_one(&plan, &opts))
        };

        // The remote demands a credential and the fetch carries one, so the
        // fetch is refused by the remote rather than by ro — and that is the
        // honest outcome: the header reached the wire, which is what this
        // test exists to prove. What must NOT happen is a silent success.
        let headers = remote.headers();
        assert!(
            !headers.is_empty(),
            "the remote must have been asked for something, got nothing"
        );
        for header in &headers {
            let header = header.as_deref().expect(
                "every request to a remote that demands a credential must be authenticated",
            );
            assert!(
                header.starts_with("basic "),
                "the header must be a basic Authorization, got: {header}"
            );
            // Base64, because that is what an Authorization header carries —
            // and because a failure message that printed the token in the
            // clear would put a credential in the test log.
            assert!(
                header.contains("eC1hY2Nlc3MtdG9rZW46Z2hwX3NoaXBfZmV0Y2hfbWFya2Vy"),
                "the row's own credential must be the one on the wire, got: {header}"
            );
        }
        // And the run did not report a success it did not have.
        assert!(
            !matches!(outcome, RepoOutcome::Pushed { .. }),
            "a remote that refused the fetch must not be reported as pushed, got {outcome:?}"
        );
    }

    /// A fetch with no credential fails **honestly** rather than reporting
    /// success.
    ///
    /// The other half of the same rule, and the reason the positive test
    /// above is not self-deceiving: a remote that demands a credential and a
    /// row that has none must produce a run that says so. The failure used to
    /// be invisible here — the fetch went out anonymously, the remote
    /// refused it, and the run reported whatever the push did.
    #[test]
    fn a_fetch_with_no_credential_fails_honestly() {
        let f = Fixture::new();
        run_git(f.repo(), &["checkout", "-q", "-b", "feat/nocred"]);
        run_git(f.repo(), &["push", "-q", "-u", "origin", "feat/nocred"]);
        f.write_local("f.txt", "work\n");

        let (remote, url) = DemandingRemote::start();

        let mut plan = plan_on(&f, "feat/nocred", None);
        plan.clone_url = format!("{url}/acme/api.git");
        plan.credential_ref = None;
        // Same reason as the positive test: the fetch has to go to the
        // demanding remote, not to the fixture's local origin.
        run_git(f.repo(), &["remote", "set-url", "origin", &plan.clone_url]);

        let mut opts = opts_for(&f);
        opts.how_far = HowFar::Ship;
        let outcome = run_one(&plan, &opts);

        let headers = remote.headers();
        assert!(
            !headers.is_empty(),
            "the remote must have been asked for something, got nothing"
        );
        for header in &headers {
            assert!(
                header.is_none(),
                "a row with no credential must send no Authorization at all, got: {header:?}"
            );
        }
        match &outcome {
            RepoOutcome::Failed { error, .. } => {
                assert!(
                    error.contains("fetch failed"),
                    "the failure must name the fetch, got: {error}"
                );
            }
            other => panic!("an unauthenticated fetch must be a failure, got {other:?}"),
        }
    }

    /// A plain-HTTP remote still gets its credential on the fetch, scoped to
    /// the scheme actually requested.
    ///
    /// A header scoped to `https://` is silently dropped by a remote asked
    /// over `http://`, and the fetch then falls back to the machine's
    /// credential helper — which is a fetch as the wrong account, or a modal
    /// dialog on the user's screen. The scope is asserted at
    /// [`the_extraheader_scope_is_the_scheme_and_host_or_nothing`]; this is
    /// the end-to-end half, that a plain-HTTP row reaches the wire at all.
    #[test]
    fn a_plain_http_row_still_gets_its_credential_on_the_fetch() {
        let f = Fixture::new();
        run_git(f.repo(), &["checkout", "-q", "-b", "feat/plain"]);
        run_git(f.repo(), &["push", "-q", "-u", "origin", "feat/plain"]);
        f.write_local("f.txt", "work\n");

        let (remote, url) = DemandingRemote::start();

        let mut plan = plan_on(&f, "feat/plain", None);
        plan.clone_url = format!("{url}/acme/api.git");
        plan.credential_ref = Some("env:RO_SHIP_FETCH_CREDENTIAL_TEST".to_string());
        run_git(f.repo(), &["remote", "set-url", "origin", &plan.clone_url]);

        let mut opts = opts_for(&f);
        opts.how_far = HowFar::Ship;
        let outcome = unsafe {
            ro_testkit::TestEnv::new()
                .var("RO_SHIP_FETCH_CREDENTIAL_TEST", "ghp_plain_http_marker")
                .run(|| run_one(&plan, &opts))
        };

        let headers = remote.headers();
        assert!(
            !headers.is_empty(),
            "the remote must have been asked for something, got nothing"
        );
        for header in &headers {
            let header = header
                .as_deref()
                .expect("a plain-HTTP row with a credential must authenticate its fetch");
            assert!(
                // Base64 of `x-access-token:ghp_plain_http_marker`.
                header.contains("eC1hY2Nlc3MtdG9rZW46Z2hwX3BsYWluX2h0dHBfbWFya2Vy"),
                "the row's own credential must be on the wire, got: {header}"
            );
        }
        assert!(
            !matches!(outcome, RepoOutcome::Pushed { .. }),
            "a remote that refused the fetch must not be reported as pushed, got {outcome:?}"
        );
    }

    /// The fetch **inside the rebase** carries the credential too.
    ///
    /// `rebase_onto_remote_base` fetches before every rebase, and it fetched
    /// anonymously for the same reason the ship fetch did — a second call site
    /// for the same hole, where a fix that only reached the first one would
    /// look complete. Called directly rather than through `run_one`, because
    /// `run_one`'s own fetch is refused by this remote and the run stops
    /// before the rebase is reached; what is under test is the fetch inside
    /// `rebase_onto_remote_base` and the two arguments it is handed.
    #[test]
    fn the_rebase_fetch_carries_the_credential_too() {
        let f = Fixture::new();
        let (remote, url) = DemandingRemote::start();
        let clone_url = format!("{url}/acme/api.git");
        run_git(f.repo(), &["remote", "set-url", "origin", &clone_url]);

        let token = SecretString::new("ghp_rebase_fetch_marker");
        let host = extraheader_host(&clone_url).expect("an http clone url names a host");

        // The outcome is not the point. The fetch inside it always runs, and
        // its result is deliberately ignored, so a remote that cannot be
        // authenticated is not by itself a reason to skip the rebase.
        let _ = rebase_onto_remote_base(f.repo(), None, Some(&host), Some(&token));

        let headers = remote.headers();
        assert!(
            !headers.is_empty(),
            "the rebase must have fetched, got no request at all"
        );
        for header in &headers {
            let header = header
                .as_deref()
                .expect("the rebase's fetch must authenticate against a remote that demands it");
            // Base64 of `x-access-token:ghp_rebase_fetch_marker`.
            assert!(
                header.contains("eC1hY2Nlc3MtdG9rZW46Z2hwX3JlYmFzZV9mZXRjaF9tYXJrZXI="),
                "the rebase's fetch must carry the row's own credential, got: {header}"
            );
        }
    }

    /// The scope rule itself, so a change to it is caught here rather than by
    /// a token reaching a host the row never named.
    ///
    /// **The SSH answer is the important one.** A non-HTTP remote returns
    /// `None` and that `None` is load-bearing: it is what stops a header
    /// being invented for a transport that has no use for one, which is the
    /// same wrong-account leak the extraheader exists to prevent. It is
    /// asserted here rather than against a live remote because an SSH fetch
    /// cannot be driven against a loopback HTTP server — the half that *can*
    /// be observed end to end is that `ro_git` turns a `None` host into no
    /// header at all, in
    /// `a_fetch_to_an_ssh_remote_gets_no_fabricated_header`.
    #[test]
    fn the_scope_rule_is_the_scheme_and_host_or_nothing() {
        assert_eq!(
            extraheader_host("https://github.com/acme/api.git"),
            Some("https://github.com".to_string())
        );
        assert_eq!(
            extraheader_host("http://127.0.0.1:8080/acme/api.git"),
            Some("http://127.0.0.1:8080".to_string())
        );
        assert_eq!(extraheader_host("git@github.com:acme/api.git"), None);
        assert_eq!(
            extraheader_host("ssh://git@github.com:2222/acme/api.git"),
            None
        );
        assert_eq!(extraheader_host("/srv/repos/api.git"), None);
        assert_eq!(extraheader_host("https:///acme/api.git"), None);
    }

    /// A row whose credential cannot be resolved is refused **before** the
    /// work is committed, not after.
    ///
    /// The lookup moved up so the fetch could carry the token, and that move
    /// is the improvement: the run used to burn an engine pass, write a
    /// commit, and only then discover the credential it had been resolving
    /// all along was unusable.
    #[test]
    fn an_unresolvable_credential_is_refused_before_anything_is_written() {
        let f = Fixture::new();
        run_git(f.repo(), &["checkout", "-q", "-b", "feat/nocred"]);
        run_git(f.repo(), &["push", "-q", "-u", "origin", "feat/nocred"]);
        f.write_local("f.txt", "work\n");

        let mut plan = plan_on(&f, "feat/nocred", None);
        plan.clone_url = "https://github.com/acme/api.git".to_string();
        // An unset variable, under the testkit lock so nothing else can be
        // reading it mid-run.
        plan.credential_ref = Some("env:RO_SHIP_UNSET_CREDENTIAL_FOR_TEST".to_string());

        let mut opts = opts_for(&f);
        opts.how_far = HowFar::Ship;
        let outcome = unsafe {
            ro_testkit::TestEnv::new()
                .var("RO_SHIP_UNSET_CREDENTIAL_FOR_TEST", "")
                .run(|| run_one(&plan, &opts))
        };

        match &outcome {
            RepoOutcome::Failed {
                error,
                class: ro_core::FailureClass::MissingProvider,
            } => {
                assert!(
                    error.contains("credential"),
                    "the failure must name the credential, got: {error}"
                );
            }
            other => panic!("an unresolvable credential must be a failure, got {other:?}"),
        }
        assert!(
            !f.local_contains("work"),
            "nothing may be committed when the credential cannot be resolved"
        );
        assert!(
            !f.remote_has("feat/nocred-uncommitted"),
            "nothing may be pushed when the credential cannot be resolved"
        );
    }

    /// A plan pointed at a feature branch pushes normally.
    #[test]
    fn a_feature_branch_pushes_normally() {
        let f = Fixture::new();
        run_git(f.repo(), &["checkout", "-q", "-b", "feat/y"]);
        run_git(f.repo(), &["push", "-q", "-u", "origin", "feat/y"]);
        f.write_local("f.txt", "work\n");

        let plan = plan_on(&f, "feat/y", None);
        let outcome = run_one(&plan, &opts_for(&f));

        assert!(
            matches!(outcome, RepoOutcome::Pushed { .. }),
            "a feature branch must push, got {outcome:?}"
        );
        assert!(
            f.remote_has("feat/y"),
            "and the commit must reach the remote: {:?}",
            f.remote_subjects()
        );
    }

    /// A refusal is a distinct outcome, not folded into "failed".
    ///
    /// Nothing was wrong with the repo and nothing failed; the operation
    /// simply is not one ro does. A summary that merges them loses the
    /// one that tells the user what to do.
    #[test]
    fn a_refusal_is_its_own_outcome() {
        // The reason carries the branch, as every real constructor does, and
        // `render` shows the reason rather than assuming "is protected" —
        // a refusal can be about a published commit or an `--onto`
        // destination, and announcing those as protection problems names a
        // cause they have nothing to do with.
        let refused = RepoOutcome::Refused {
            branch: "main".into(),
            reason: "main is a protected branch".into(),
        };
        assert!(
            refused.is_failure(),
            "a refusal is a failure for the exit code"
        );
        assert!(refused.render().contains("main"));
        assert!(!refused.render().contains("failed:"));
    }
}

/// Which branch of `refs/remotes/origin` holds the push's target.
///
/// A refspec is `HEAD:refs/heads/<this>`.
fn push_destination(refspec: &str) -> String {
    match refspec.rsplit_once(":refs/heads/") {
        Some((_, dest)) => dest.to_string(),
        // Not a refspec of the shape we build. Return it whole: the
        // ahead/behind probe below is a best-effort reading of a remote
        // that does not answer the branch it was asked about, and the
        // honest answer is "unknown", which pushes.
        None => refspec.to_string(),
    }
}

/// Is there anything for the push to send?
///
/// Three answers rather than one, because "nothing to commit" and
/// "nothing to do" stopped being the same thing: a rebase onto the
/// remote's base writes a commit of its own, and that commit is on the
/// remote only if ro pushes it. Reporting `nothing to commit` for a run
/// that had just made the commit it was asked to ship is the lie the
/// summary line was invented to stop.
///
///   * a local branch exists that the remote does not have → yes, it is
///     new;
///   * the branch has commits past the one it was on before this run →
///     yes, one of them is from the rebase;
///   * the remote has not moved past what it already had → nothing to do.
///
/// `None` means the branch could not be read, and pushes: a remote that
/// cannot be asked is not a reason to strand work.
fn has_commits_to_push(repo: &Path, refspec: &str, head_before: Option<&str>) -> bool {
    let dest = push_destination(refspec);
    // No local branch named for the destination. That used to mean "this is
    // the first push of it, which creates it" — and on a **clean** tree with
    // nothing to commit, it created a remote branch and moved the checkout's
    // upstream to it. `ro ship --onto ghost` with no work published `ghost`
    // and repointed `branch.main.merge` at `refs/heads/ghost`; the warning
    // made it look deliberate, but nothing in that run asked for a branch to
    // be invented.
    //
    // The question is whether **this run** has anything to send, and
    // `head_before` is read **after** the rebase and before the engine, so
    // `moved` is "the engine committed something". It is not "the rebase
    // rewrote the branch": a rebase that moves HEAD onto a base it was
    // already level with writes nothing, and a rebase that is genuinely
    // ahead is one the `--onto` user asked for. What must not happen is a
    // branch created out of a tree with no work in it.
    //
    // An absent `head_before` is *unknown*, not *no*.
    let Some(before) = head_before else {
        // Nothing to compare against. Say there is work: inventing a skip
        // from an unreadable comparison is how work ends up stranded.
        return true;
    };
    let moved = ro_git::read::head_oid(repo).ok().flatten().as_deref() != Some(before);
    // A local branch with no upstream tracking has nothing to compare
    // against, as does a remote-tracking ref that cannot be read. Say there
    // is work in both: the push that follows resolves the truth, and
    // inventing a skip from an unreadable comparison is how work ends up
    // stranded.
    let local_has = ro_git::primitives::branch_exists(repo, &dest);
    let remote_has = remote_branch_exists(repo, &dest);
    match (local_has, remote_has) {
        (true, true) => {
            let Some(upstream) = current_branch_upstream(repo) else {
                return true;
            };
            match ro_git::read::ahead_behind(repo, &format!("refs/remotes/origin/{upstream}")) {
                Ok(ab) => ab.ahead > 0 || moved,
                Err(_) => true,
            }
        }
        // A destination neither side has is a branch this run **creates** —
        // and only out of a HEAD that moved here. Both halves matter: the
        // first one alone published a branch off `main` for a run with no
        // work in it at all.
        (false, false) => moved,
        // Undecidable, and deliberately so: the branch exists on one side
        // only, so there is no ref to compare against. A rebase the caller
        // resolved with `--resolve` lands here — the resolved commit is HEAD,
        // `head_before` was read after it, and nothing about this repo says
        // it should be pushed. Saying "there is work" and letting the push
        // resolve the truth is the same answer the old code gave, and
        // inventing a skip out of an unreadable comparison is how work ends
        // up stranded.
        _ => true,
    }
}

/// The branch this branch tracks, if it tracks a remote one.
///
/// `git rev-parse --abbrev-ref @{u}` answers with the *local* name, which
/// is not what a push needs: `feat/x` tracks `origin/feat/x`, and the
/// ahead/behind probe has to be against the remote ref.
fn current_branch_upstream(repo: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    s.strip_prefix("origin/").map(str::to_string)
}

/// The refspec that lands the work where the user asked for it.
///
/// `PushOpts.branch` is handed to `git push` as a **refspec**, and a bare
/// branch name is not one. Git reads `git push origin feat/x` as "push the
/// source `feat/x` to a destination of the same name" — which is a no-op
/// when the local branch is called something else, and an error when it
/// is not. The work is committed locally either way, so the failure is
/// the quiet kind: a commit, no push, and a summary that says `pushed`.
///
/// `HEAD` is the source rather than the branch name because the branch is
/// not always the one in the plan — a rebase onto `--onto` leaves the
/// checkout where it was, and the work is on HEAD whatever that is called.
/// The destination is the branch the work should land on, which is
/// `--onto` when it was given and the plan's own branch otherwise.
///
/// The source is `HEAD` even when the destination is a local branch, and
/// a destination that is *not* one is a plan problem the push should
/// report rather than a refspec to paper over: `src refspec <name> does
/// not match any` names the branch, where a bare name reads as a source
/// and the message says nothing about which.
fn push_refspec(plan: &RepoPlan) -> String {
    let onto = plan
        .onto
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .unwrap_or(&plan.base_branch);
    format!("HEAD:refs/heads/{onto}")
}

/// Every verb that touches the remote rebases first.
///
/// This is a one-line table, and it was wrong in the worst way a table can
/// be wrong: `ro ship` rebased, `ro push` did not, and both are the same
/// pipeline with the same flags. A user who ran `ro push` instead of
/// `ro ship` got a push onto a branch the remote had already moved past —
/// accepted by the remote as a fast-forward of the *branch*, with the base
/// divergence silently still there, and rejected outright the moment the
/// base was actually required.
/// `--onto` is a claim about where the work lands, and it has to be true
/// end to end: the rebase base moves, the commit reaches the named branch,
/// and nothing is left sitting on the local branch with a summary that
/// says it shipped.
///
/// The bug this module exists for: `--onto` was read in exactly one place,
/// the push refspec, and the rebase ignored it entirely. So `ro ship
/// --onto release` rebased onto `origin/HEAD` and then pushed
/// `refs/heads/<local>:refs/heads/release>` — a push the remote rejects
/// as non-fast-forward, because the work was never moved onto the base it
/// was being asked to land on. The commit stayed local and the row said
/// `failed`, which is at least honest, but the flag did not work.
#[cfg(test)]
mod onto_tests {
    use super::tests_support::*;
    use super::*;

    /// The case the flag exists for: work on A, land on B.
    ///
    /// Three assertions, because "it pushed" is not the claim. The rebase
    /// base has to move — or the push cannot be a fast-forward and the
    /// commit stays local. The commit has to arrive on B. And the local
    /// branch A has to end up holding B's commits, or the next run
    /// diverges again.
    #[test]
    fn onto_lands_the_commit_on_the_named_branch_not_the_local_one() {
        let f = Fixture::new();
        // A second branch on the remote, one commit ahead of main. This
        // is what makes the rebase base visible: with `origin/HEAD` as the
        // base, the local work is not descended from this commit, and the
        // push is a non-fast-forward the remote refuses.
        release_line_moves_on(&f);

        // Local work, on a branch of its own.
        run_git(f.repo(), &["checkout", "-q", "-b", "work"]);
        f.write_local("f.txt", "work\n");
        run_git(f.repo(), &["add", "-A"]);
        run_git(f.repo(), &["commit", "-q", "-m", "the work"]);

        let plan = plan_on(&f, "work", Some("release"));
        let outcome = run_one(&plan, &opts_for(&f));

        assert!(
            matches!(outcome, RepoOutcome::Pushed { .. }),
            "the work must reach the remote, got {outcome:?}"
        );
        // The rebase base moved: the local branch now sits on `release`.
        assert!(
            f.local_contains("release only moves on"),
            "the work must be rebased onto the branch it lands on, or the \
             push is a non-fast-forward the remote refuses"
        );
        // And the commit is on the remote's `release`, not just on `work`.
        assert!(
            f.remote_has("release"),
            "the named branch must exist on the remote: {:?}",
            f.remote_subjects()
        );
        assert!(
            remote_log_contains(&f, "release", "the work"),
            "the commit must land on the named branch; the remote's release \
             branch does not have it: {:?}",
            remote_subjects(&f)
        );
    }

    /// The negative half, and the reason the test above is not a tautology:
    /// the same run **without** `--onto` pushes to the local branch and
    /// the work never reaches `release`.
    #[test]
    fn without_onto_the_commit_lands_on_the_local_branch() {
        let f = Fixture::new();
        release_line_moves_on(&f);

        run_git(f.repo(), &["checkout", "-q", "-b", "work"]);
        f.write_local("f.txt", "work\n");

        let plan = plan_on(&f, "work", None);
        let outcome = run_one(&plan, &opts_for(&f));

        assert!(
            matches!(outcome, RepoOutcome::Pushed { .. }),
            "a plain push must work, got {outcome:?}"
        );
        assert!(
            remote_log_contains(&f, "work", "wip on work"),
            "the commit belongs on the branch it was made on"
        );
        assert!(
            !remote_log_contains(&f, "release", "wip on work"),
            "and must not appear on a branch nobody named"
        );
    }

    /// `--onto` names the base, so the base is the named branch and not
    /// the remote's default. Asserted on the branch's own state rather
    /// than through `run_one`, because this is the rebase step on its own.
    #[test]
    fn the_onto_branch_is_the_rebase_base() {
        let f = Fixture::new();
        release_line_moves_on(&f);

        let state = rebase_onto_remote_base(f.repo(), Some("release"), None, None)
            .expect("the rebase onto a named branch runs");
        assert!(matches!(state, RebaseState::Rebased), "got {state:?}");
        // The commit that exists **only** on `release`. Asserting on one
        // that is on `main` too would pass on code that rebased onto
        // `origin/HEAD`, which is the bug.
        assert!(
            f.local_contains("release only moves on"),
            "the rebase must be onto origin/release, not origin/HEAD"
        );
    }

    /// A named branch the remote does not have is not an error to die on.
    ///
    /// It is the first push that creates it, so there is nothing to rebase
    /// onto — and the fallback is the remote's own HEAD, which is the only
    /// base that exists. What must not happen is a hard failure, because
    /// the honest failure is `everything up-to-date` at worst and a
    /// created branch at best.
    #[test]
    fn an_onto_branch_the_remote_lacks_is_created_not_refused() {
        let f = Fixture::new();
        run_git(f.repo(), &["checkout", "-q", "-b", "work"]);
        f.write_local("f.txt", "work\n");

        let plan = plan_on(&f, "work", Some("brand-new"));
        let outcome = run_one(&plan, &opts_for(&f));

        assert!(
            matches!(outcome, RepoOutcome::Pushed { .. }),
            "the first push creates the branch, got {outcome:?}"
        );
        assert!(
            f.remote_has("brand-new"),
            "the named branch must exist after the run: {:?}",
            f.remote_subjects()
        );
    }

    /// **The warning.** `--onto` at a branch the remote has never heard of
    /// publishes that branch, and the user is told so.
    ///
    /// Nothing in the help says the flag invents a branch. `--onto` is the
    /// one ship flag with no gate on where it writes — `--onto main` is
    /// refused, but `--onto anything-at-all` is accepted and creates a
    /// branch that is then visible to anyone who can see the repository.
    /// The three things a user needs are all in the message: that the
    /// branch was not there, that ro made it, and that it is now public.
    #[test]
    fn onto_a_branch_the_remote_lacks_warns_that_it_will_be_published() {
        let f = Fixture::new();
        run_git(f.repo(), &["checkout", "-q", "-b", "work"]);
        f.write_local("f.txt", "work\n");

        let plan = plan_on(&f, "work", Some("brand-new"));
        let outcome = run_one(&plan, &opts_for(&f));

        let warnings = outcome.warnings();
        assert_eq!(
            warnings.len(),
            1,
            "publishing a new branch is worth exactly one warning, got {warnings:?}"
        );
        let warning = &warnings[0];
        assert!(
            warning.contains("brand-new"),
            "the warning must name the branch, got: {warning}"
        );
        assert!(
            warning.contains("does not have") && warning.contains("created"),
            "it must say the branch was not there and ro made it, got: {warning}"
        );
        assert!(
            warning.contains("published") && warning.contains("visible to"),
            "and that the branch is now public — a typo here is a leaked \
             ref, not a private mistake, got: {warning}"
        );

        // The fact the warning is about: the branch really is on the
        // remote, so this is not a warning about a branch that was never
        // created.
        assert!(
            f.remote_has("brand-new"),
            "the branch must exist, or the warning is lying: {:?}",
            f.remote_subjects()
        );
    }

    /// **The negative control**, and the reason the test above is not
    /// vacuous: `--onto` at a branch the remote already has says nothing.
    ///
    /// A warning that fires on every `--onto` run is a warning nobody
    /// reads, and the run above would pass unchanged on a build that
    /// warned unconditionally. Without this, "we warn" and "we warn
    /// always" are the same test result.
    #[test]
    fn onto_a_branch_the_remote_already_has_does_not_warn() {
        let f = Fixture::new();
        release_line_moves_on(&f);
        run_git(f.repo(), &["checkout", "-q", "-b", "work"]);
        f.write_local("f.txt", "work\n");

        let plan = plan_on(&f, "work", Some("release"));
        let outcome = run_one(&plan, &opts_for(&f));

        assert!(
            matches!(outcome, RepoOutcome::Pushed { .. }),
            "pushing to an existing branch must still work, got {outcome:?}"
        );
        assert!(
            f.remote_has("release"),
            "the branch must be the one it already was: {:?}",
            f.remote_subjects()
        );
        assert!(
            outcome.warnings().is_empty(),
            "a branch the remote already had is not a surprise; got {:?}",
            outcome.warnings()
        );
    }

    /// The warning is about publishing a branch, so a run that **refused**
    /// has published nothing to warn about.
    ///
    /// Ordering matters and is the part worth pinning: the protected check
    /// runs before the push, so a refused `--onto` never reaches the code
    /// that asks "did we just create this?". A warning attached to a
    /// refusal would tell a user their branch was published when the run
    /// did nothing at all.
    #[test]
    fn a_refused_onto_does_not_warn_about_publishing() {
        let f = Fixture::new();
        run_git(f.repo(), &["checkout", "-q", "-b", "work"]);
        f.write_local("f.txt", "work\n");

        let plan = plan_on(&f, "work", Some("main"));
        let outcome = run_one(&plan, &opts_for(&f));

        assert!(
            matches!(outcome, RepoOutcome::Refused { .. }),
            "--onto main must still be refused, got {outcome:?}"
        );
        assert!(
            outcome.warnings().is_empty(),
            "a refused run published nothing; got {:?}",
            outcome.warnings()
        );
    }

    /// A run with no `--onto` never invents a branch, whatever its branch
    /// is. The second half of the negative control: the warning is about
    /// the flag, not about "the checkout is on a branch the remote lacks".
    #[test]
    fn a_plain_push_never_warns_about_creating_a_branch() {
        let f = Fixture::new();
        // A branch that exists **only locally**: no push under it has
        // happened, so `origin/work` is absent. This is the case a check
        // written against the local ref store would misfire on.
        run_git(f.repo(), &["checkout", "-q", "-b", "work"]);
        f.write_local("f.txt", "work\n");

        let plan = plan_on(&f, "work", None);
        let outcome = run_one(&plan, &opts_for(&f));

        assert!(
            matches!(outcome, RepoOutcome::Pushed { .. }),
            "a plain push must work, got {outcome:?}"
        );
        assert!(
            outcome.warnings().is_empty(),
            "without --onto the destination is the branch the work was made \
             on, which is not a branch being invented; got {:?}",
            outcome.warnings()
        );
    }

    /// A `release` branch on the remote carrying a commit `main` does not.
    ///
    /// The commit has to be **only** on `release`. If the two branches
    /// were identical, a rebase onto `origin/HEAD` and a rebase onto
    /// `origin/release` produce the same history, and every assertion
    /// about the base passes on code that never read `--onto` at all — a
    /// test that cannot fail is not a test.
    fn release_line_moves_on(f: &Fixture) {
        f.other_clone_commits("release line moves on");
        run_git(f.repo(), &["fetch", "-q", "origin"]);
        run_git(
            f.repo(),
            &[
                "push",
                "-q",
                "origin",
                "refs/remotes/origin/main:refs/heads/release",
            ],
        );
        // …and one more, on `release` alone.
        run_git(f.other(), &["checkout", "-q", "-B", "release"]);
        std::fs::write(f.other().join("release-only.txt"), "only here\n").unwrap();
        run_git(f.other(), &["add", "-A"]);
        run_git(f.other(), &["commit", "-q", "-m", "release only moves on"]);
        run_git(f.other(), &["push", "-q", "origin", "HEAD:release"]);
        run_git(f.repo(), &["fetch", "-q", "origin"]);
    }

    /// Commit subjects on a branch of the bare remote.
    fn remote_subjects(f: &Fixture) -> Vec<String> {
        let out = std::process::Command::new("git")
            .args(["log", "--format=%s", "--all"])
            .current_dir(f.remote_path())
            .output()
            .expect("git runs");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Does the remote's `branch` carry a commit with this subject?
    fn remote_log_contains(f: &Fixture, branch: &str, subject: &str) -> bool {
        let out = std::process::Command::new("git")
            .args(["log", "--format=%s", branch])
            .current_dir(f.remote_path())
            .output()
            .expect("git runs");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|l| l.trim() == subject)
    }

    // ── The guard the writers disagreed with ──────────────────────────
    //
    // Every writer of `onto` trims — `push_refspec`, `onto_created_warning`,
    // `base_for`, `written_branch` — and the protected-branch guard did not.
    // So `--onto ' main '` passed the check and then wrote to
    // `refs/heads/main`, advancing a protected branch with a `wip on work`
    // commit, exit 0. FEATURES.md promises "`--onto` naming a protected
    // branch is refused too"; that promise did not survive whitespace.

    #[test]
    fn onto_a_protected_branch_padded_with_whitespace_is_still_refused() {
        for padded in [" main ", "\tmain\n", "  main"] {
            let f = Fixture::new();
            run_git(f.repo(), &["checkout", "-q", "-b", "work"]);
            f.write_local("f.txt", "work\n");
            run_git(f.repo(), &["add", "-A"]);
            run_git(f.repo(), &["commit", "-q", "-m", "the work"]);

            let plan = plan_on(&f, "work", Some(padded));
            let outcome = run_one(&plan, &opts_for(&f));

            assert!(
                matches!(outcome, RepoOutcome::Refused { .. }),
                "--onto {padded:?} must be refused exactly like `--onto main`; \
                 got {outcome:?}"
            );
        }
    }

    #[test]
    fn a_padded_protected_name_does_not_advance_the_protected_branch() {
        let f = Fixture::new();
        run_git(f.repo(), &["checkout", "-q", "-b", "work"]);
        f.write_local("f.txt", "work\n");
        run_git(f.repo(), &["add", "-A"]);
        run_git(f.repo(), &["commit", "-q", "-m", "the work"]);

        assert!(
            !remote_log_contains(&f, "main", "the work"),
            "the fixture must start clean"
        );

        let plan = plan_on(&f, "work", Some(" main "));
        let _ = run_one(&plan, &opts_for(&f));

        assert!(
            !remote_log_contains(&f, "main", "the work"),
            "refs/heads/main must not have advanced: {:?}",
            f.remote_refs()
        );
    }

    // ── `--onto` as a branch name ───────────────────────────────────────
    //
    // `--onto` was never validated as a branch name, so three shapes of the
    // same bug got through: a refspec was split at the colon and half of it
    // discarded, a fully-qualified ref invented a branch literally named
    // `refs/heads/refs/heads/feat`, and the row's warning then advised
    // deleting a branch that does not exist.

    #[test]
    fn onto_a_refspec_is_refused_rather_than_split_at_the_colon() {
        let f = Fixture::new();
        run_git(f.repo(), &["checkout", "-q", "-b", "work"]);
        f.write_local("f.txt", "work\n");
        run_git(f.repo(), &["add", "-A"]);
        run_git(f.repo(), &["commit", "-q", "-m", "the work"]);

        let plan = plan_on(&f, "work", Some("feat:refs/heads/evil"));
        let outcome = run_one(&plan, &opts_for(&f));

        match outcome {
            RepoOutcome::Refused { reason, .. } => assert!(
                reason.contains("refspec"),
                "the refusal must name the refspec as the problem: {reason}"
            ),
            other => panic!("a refspec must not be pushed; got {other:?}"),
        }
        assert!(
            !f.remote_has("evil") && !f.remote_has("feat"),
            "neither half of the refspec may be created: {:?}",
            f.remote_refs()
        );
    }

    #[test]
    fn onto_a_fully_qualified_ref_is_refused_rather_than_nested() {
        let f = Fixture::new();
        run_git(f.repo(), &["checkout", "-q", "-b", "work"]);
        f.write_local("f.txt", "work\n");
        run_git(f.repo(), &["add", "-A"]);
        run_git(f.repo(), &["commit", "-q", "-m", "the work"]);

        let plan = plan_on(&f, "work", Some("refs/heads/feat"));
        let outcome = run_one(&plan, &opts_for(&f));

        assert!(
            matches!(outcome, RepoOutcome::Refused { .. }),
            "a fully-qualified ref is not a branch name; got {outcome:?}"
        );
        assert!(
            !f.remote_refs()
                .iter()
                .any(|r| r.contains("refs/heads/refs")),
            "no branch may be invented from a ref: {:?}",
            f.remote_refs()
        );
    }

    // ── The "remote does not have it" warning asks the remote ────────────
    //
    // `remote_branch_exists` read `refs/remotes/origin/<branch>`, which is
    // the *local* remote-tracking ref. On any clone that is not a full clone
    // the answer was "no" for a branch the remote has had for ever — so the
    // warning claimed ro created a branch it had in fact **overwritten**,
    // and advised `git push origin --delete <branch>` on a colleague's work.

    #[test]
    fn a_branch_the_remote_has_since_before_the_clone_does_not_warn() {
        let f = Fixture::new();
        run_git(
            f.other(),
            &["push", "-q", "origin", "HEAD:refs/heads/ancient"],
        );
        let root = f.repo().parent().unwrap().to_path_buf();
        let shallow = root.join("shallow");
        run_git(
            root.as_path(),
            &[
                "clone",
                "-q",
                "--single-branch",
                f.remote_path().to_string_lossy().as_ref(),
                "shallow",
            ],
        );
        run_git(&shallow, &["remote", "set-head", "origin", "-a"]);

        let tracked = std::process::Command::new("git")
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                "refs/remotes/origin/ancient",
            ])
            .current_dir(&shallow)
            .output()
            .expect("git runs");
        assert!(
            !tracked.status.success(),
            "the fixture must have no tracking ref for `ancient`, or it \
             proves nothing about the local-tracking-ref question"
        );
        assert!(
            super::remote_branch_exists(&shallow, "ancient"),
            "the remote DOES have `ancient`; asking the tracking refs answers no"
        );

        let warnings = super::onto_created_warning(&shallow, Some("ancient"));
        assert!(
            warnings.is_empty(),
            "the remote HAS `ancient`; a warning here tells the user to \
             delete a branch somebody else created: {warnings:?}"
        );
    }

    // ── A run with nothing to commit creates no branch ───────────────────
    //
    // `has_commits_to_push` answered `true` for any branch the local side
    // did not have, so a clean tree still pushed, `--onto ghost` created a
    // remote branch off `main`, and the checkout's upstream was repointed
    // from `origin/main` to `origin/ghost`.

    #[test]
    fn onto_with_nothing_to_commit_creates_no_branch() {
        let f = Fixture::new();
        run_git(f.repo(), &["checkout", "-q", "-b", "work"]);
        assert!(
            !f.local_is_dirty(),
            "the fixture must start clean, or this proves nothing"
        );

        let plan = plan_on(&f, "work", Some("ghost"));
        let mut opts = opts_for(&f);
        // **Outside** the checkout. `RunOptions::state_dir` documents that a
        // lock inside the worktree makes `git status` report the tool's own
        // file as uncommitted work; putting it here would make this test
        // about the lock rather than about the branch.
        opts.state_dir = f.repo().parent().unwrap().join("state");
        let outcome = run_one(&plan, &opts);

        assert!(
            !matches!(outcome, RepoOutcome::Pushed { .. }),
            "there was nothing to push; a `pushed` row over a clean tree is \
             the bug: {outcome:?}"
        );
        assert!(
            !f.remote_has("ghost"),
            "no branch may be invented from nothing: {:?}",
            f.remote_refs()
        );
        let upstream = std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
            .current_dir(f.repo())
            .output()
            .expect("git runs");
        assert!(
            !String::from_utf8_lossy(&upstream.stdout).contains("ghost"),
            "the checkout's upstream must not be repointed: {}",
            String::from_utf8_lossy(&upstream.stdout)
        );
    }
}

#[cfg(test)]
mod extraheader_host_tests {
    use super::extraheader_host;

    /// A URL scheme is case-insensitive, so the scoping rule must be too.
    ///
    /// `HTTP://host/…` names the same origin as `http://host/…`, but the
    /// scheme was matched exactly, so an uppercase one returned `None`: no
    /// header was scoped, the row's credential was dropped, and the user saw
    /// git's `remote-HTTP is not a git command` — which names neither the
    /// scheme nor the credential, and is the message you get when you have
    /// run out of ideas.
    #[test]
    fn the_scheme_is_matched_case_insensitively_and_emitted_lower_cased() {
        for spelling in [
            "HTTP://example.invalid/o/r.git",
            "Http://example.invalid/o/r.git",
        ] {
            assert_eq!(
                extraheader_host(spelling).as_deref(),
                Some("http://example.invalid"),
                "the scheme is case-insensitive by RFC 3986; {spelling:?} names \
                 the same origin as the lowercase spelling"
            );
        }
        assert_eq!(
            extraheader_host("HTTPS://example.invalid/o/r.git").as_deref(),
            Some("https://example.invalid")
        );
    }

    /// The negative control, and the property a case-insensitive match must
    /// not break: a non-HTTP remote still gets no header at all.
    #[test]
    fn a_non_http_remote_still_gets_no_header() {
        for url in [
            "git@example.invalid:o/r.git",
            "ssh://git@example.invalid/o/r.git",
            "/srv/local/repo.git",
            "example.invalid/o/r.git",
            "https://",
        ] {
            assert_eq!(
                extraheader_host(url),
                None,
                "{url:?} has no HTTP origin to scope a credential to"
            );
        }
    }

    /// The port is part of the host, and two rows on the same host name and
    /// different ports are two different servers.
    #[test]
    fn the_port_is_part_of_the_scoped_host() {
        assert_eq!(
            extraheader_host("http://127.0.0.1:39229/team/api.git").as_deref(),
            Some("http://127.0.0.1:39229")
        );
    }
}

#[cfg(test)]
mod verb_shape_tests {
    use super::HowFar;

    #[test]
    fn every_verb_that_touches_the_remote_rebases_first() {
        assert!(HowFar::Ship.rebases(), "ship pushes, so it rebases");
        assert!(HowFar::Push.rebases(), "push pushes, so it rebases");
        assert!(
            !HowFar::Commit.rebases(),
            "commit writes nothing to the remote, and rewriting local \
             history for a request that was only 'commit this' is its own \
             surprise"
        );
    }

    /// The three predicates describe one shape, and a change to one has to
    /// agree with the others. `push` implies `rebases`; if it ever does not,
    /// the reason is written down rather than discovered by a user.
    #[test]
    fn pushing_implies_rebasing() {
        for far in [HowFar::Ship, HowFar::Push] {
            assert!(
                far.pushes() && far.rebases(),
                "{far:?} pushes but does not rebase"
            );
        }
        assert!(!HowFar::Commit.pushes(), "commit never touches the remote");
    }
}

/// The subject of a commit, for `--amend` when the user gave no `--message`.
///
/// An amend with no subject of its own must keep the one the commit already
/// had — that is what "amend" means to everyone who has used `git commit
/// --amend` without `-m`, and replacing it with a placeholder would be a
/// silent rewrite of a message the user wrote.
fn subject_of(repo: &std::path::Path, oid: &str) -> Option<String> {
    ro_git::read::commit_subject(repo, oid)
}
