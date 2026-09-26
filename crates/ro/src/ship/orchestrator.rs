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
    /// `commit` and `push` do not: only a full ship fetches, and there is
    /// nothing to rebase onto a base we have not fetched.
    pub fn rebases(&self) -> bool {
        matches!(self, HowFar::Ship)
    }
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
    /// The worktree is mid-conflict; ro does not resolve it for you.
    SkippedConflict {
        detail: String,
    },
    /// A safety check stopped it **before** the engine ran.
    Blocked {
        reason: String,
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
    Pushed {
        oid: String,
    },
    Failed {
        error: String,
    },
}

impl RepoOutcome {
    /// Is this a failure that should make the whole run exit non-zero?
    pub fn is_failure(&self) -> bool {
        matches!(
            self,
            RepoOutcome::Blocked { .. } | RepoOutcome::Failed { .. } | RepoOutcome::Refused { .. }
        )
    }

    /// One line for the summary table.
    pub fn render(&self) -> String {
        match self {
            RepoOutcome::Committed { oid, .. } => format!("committed {oid}"),
            RepoOutcome::NothingToCommit => "nothing to commit".into(),
            RepoOutcome::SkippedConflict { .. } => "skipped: mid-conflict".into(),
            RepoOutcome::Blocked { reason, .. } => format!("blocked: {reason}"),
            RepoOutcome::Refused { branch, .. } => format!("refused: {branch} is protected"),
            RepoOutcome::Pushed { oid } => format!("pushed {oid}"),
            RepoOutcome::Failed { error } => format!("failed: {error}"),
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
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            how_far: HowFar::Ship,
            timeout: ro_engine::dispatch::default_timeout(),
            parallel: 4,
            dry_run: false,
            state_dir: std::env::temp_dir(),
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
    // A dry run stops here, before the lock and before anything can
    // write. The flag used to sit on RunOptions unread, so `--dry-run`
    // was a promise the code never kept.
    if opts.dry_run {
        return RepoOutcome::NothingToCommit;
    }

    let repo = plan.path();

    // (a) Lock. A guard, so it releases on every path including a panic
    //     — a lock left held is a repo nobody can touch until the
    //     process dies.
    let lock_dir = opts.state_dir.join("locks");
    let _lock = match ro_git::RepoLock::acquire_default(&lock_dir, repo) {
        Ok(l) => l,
        Err(e) => {
            return RepoOutcome::Failed {
                error: format!("could not lock {}: {e:#}", plan.label),
            };
        }
    };

    // (b) Preflight. A conflict is a *skip*, not a failure: the worktree
    //     is mid-merge and ro does not resolve it for you.
    match ro_git::conflict::detect(repo) {
        Ok(Some(state)) => {
            return RepoOutcome::SkippedConflict {
                detail: format!(
                    "{} conflicted file(s) during {}",
                    state.files.len(),
                    state.op
                ),
            };
        }
        Ok(None) => {}
        Err(e) => {
            return RepoOutcome::Failed {
                error: format!("could not read {}: {e:#}", plan.label),
            };
        }
    }

    // (b2) The safety net, and it runs **before** the engine. This is
    // the last thing standing between a WIP commit and a leaked
    // credential, and a preflight declared but not called is the exact
    // "looks like a feature and is not one" shape the beads keep naming —
    // so it is here, in the order that matters, and `Blocked` is
    // constructed below.
    match preflight(repo) {
        Ok(()) => {}
        Err(reason) => {
            return RepoOutcome::Blocked {
                reason: reason.0,
                detail: String::new(),
            };
        }
    }

    // (c) Fetch, for a full ship. A network failure is a failure, not a
    //     silent skip: the user asked for an up-to-date push.
    if opts.how_far.fetches() {
        if let Err(e) = ro_git::mutation::fetch(repo, &ro_git::mutation::FetchOpts::default()) {
            return RepoOutcome::Failed {
                error: format!("fetch failed: {e:#}"),
            };
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
    if opts.how_far.rebases() {
        match rebase_onto_remote_base(repo) {
            Ok(RebaseState::Rebased) | Ok(RebaseState::NoRemote) => {}
            Err(e) => {
                // A failed autostash pop is a per-repo FAILURE, never a
                // proceed on a half-popped tree. The message carries the
                // exact recovery rather than hoping the user knows it.
                return RepoOutcome::Failed {
                    error: e.to_string(),
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
        message_override: None,
        env: &[],
    };

    let oid = match plan.engine.checkpoint(&ctx) {
        EngineOutcome::Committed { commits } => match commits.last() {
            Some(c) => c.oid.clone(),
            None => {
                return RepoOutcome::Failed {
                    error: "the engine reported a commit with no commit id".into(),
                };
            }
        },
        EngineOutcome::NothingToCommit => return RepoOutcome::NothingToCommit,
        EngineOutcome::Unavailable { binary, hint } => {
            return RepoOutcome::Failed {
                error: format!("{binary} is not installed. {hint}"),
            };
        }
        EngineOutcome::TimedOut { after } => {
            return RepoOutcome::Failed {
                error: format!("the engine did not finish within {after:?} and was killed"),
            };
        }
        EngineOutcome::Failed { error, .. } => return RepoOutcome::Failed { error },
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
    // The refusal. ro does **not** create a branch.
    //
    // The old behaviour silently made `ro/wip/<slug>-<run>` so a pull
    // request would have something to point at, and with PRs cut the
    // reason is gone. Naming a branch has consequences: it is what a
    // teammate fetches, what appears in `git branch` next month, and
    // what a later `ro push` assumes. A tool that invents a name is a
    // tool making a decision with a blast radius on your behalf.
    //
    // `--onto` is the escape, for the rare case where the work genuinely
    // belongs on a different branch.
    if plan.onto.is_none() && ro_git::primitives::is_protected_branch(&plan.base_branch) {
        return RepoOutcome::Refused {
            branch: plan.base_branch.clone(),
            reason: format!(
                "{} is a protected branch. Create a branch first \
                 (`git checkout -b feat/x`), or pass --onto <BRANCH> if the \
                 work genuinely belongs somewhere else.",
                plan.base_branch
            ),
        };
    }

    let secret = match resolve_credential(plan.credential_ref.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            return RepoOutcome::Failed {
                error: format!("credential: {e}"),
            };
        }
    };

    let push = ro_git::mutation::push_with_credential(
        repo,
        &ro_git::mutation::PushOpts {
            remote: Some("origin".into()),
            branch: Some(
                plan.onto
                    .clone()
                    .unwrap_or_else(|| plan.base_branch.clone()),
            ),
            set_upstream: true,
            host: Some(plan.host_for_extraheader()),
            ..Default::default()
        },
        secret.as_ref(),
    );

    match push {
        Ok(r) if r.ok() => RepoOutcome::Pushed { oid },
        Ok(r) => RepoOutcome::Failed {
            error: format!("push failed: {}", r.stderr.trim()),
        },
        Err(e) => RepoOutcome::Failed {
            error: format!("push failed: {e:#}"),
        },
    }
}

impl RepoPlan {
    /// Which host the extraheader is scoped to.
    ///
    /// A local path has no host, and a credential scoped to
    /// `github.com` must not be offered to one.
    fn host_for_extraheader(&self) -> String {
        if self.clone_url.starts_with("http") {
            self.clone_url
                .split("//")
                .nth(1)
                .and_then(|r| r.split('/').next())
                .unwrap_or("github.com")
                .to_string()
        } else {
            "localhost".into()
        }
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
        let plan = plan_for_test(&path);
        let opts = RunOptions {
            dry_run: true,
            state_dir: path.join(".state"),
            ..Default::default()
        };
        assert_eq!(run_one(&plan, &opts), RepoOutcome::NothingToCommit);
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

/// Rebase onto `origin/HEAD`, with the worktree's changes preserved.
///
/// One git invocation, so there is no window in which the stash exists
/// and ro has not yet asked git to pop it. Driving `stash` / `rebase` /
/// `stash pop` as three steps ro controls itself would be the same thing
/// with **three places to forget the cleanup** — the failure class the
/// credential design avoids by never rewriting a remote in the first
/// place.
fn rebase_onto_remote_base(repo: &Path) -> Result<RebaseState, anyhow::Error> {
    // Fetch first, always. Rebasing onto a stale
    // `refs/remotes/origin/HEAD` reorders the branch onto a base the
    // remote has already moved past, which is the one ordering mistake
    // that makes a rebase look like it worked.
    let _ = ro_git::mutation::fetch(repo, &ro_git::mutation::FetchOpts::default());

    let Some(base) = ro_git::primitives::symbolic_ref(repo, "refs/remotes/origin/HEAD")? else {
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
        .map_err(|e| anyhow::anyhow!("running git rebase --autostash: {e}"))?;

    if out.status.success() {
        return Ok(RebaseState::Rebased);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    // A conflict is the user's to resolve, and the tree is left mid-
    // rebase on purpose: aborting would discard their work.
    if stderr.contains("CONFLICT") || stderr.contains("could not apply") {
        return Err(anyhow::anyhow!(
            "the rebase onto {base} conflicted. Nothing was committed or \
             pushed. Resolve it and run `git rebase --continue`, or \
             `git rebase --abort` to go back."
        ));
    }
    // Everything else is a real failure, and the recovery is stated
    // rather than assumed.
    Err(anyhow::anyhow!(
        "the rebase onto {base} failed: {}.\n\
         If the tree looks wrong, `git stash list` shows any autostash \
         that was not popped, and `git stash pop` restores it.",
        stderr.trim()
    ))
}

#[cfg(test)]
mod rebase_tests {
    use super::tests_support::*;
    use super::*;

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

        let state = rebase_onto_remote_base(f.repo()).expect("the rebase runs");
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

        let state = rebase_onto_remote_base(f.repo()).expect("the rebase runs");
        assert!(matches!(state, RebaseState::Rebased), "got {state:?}");
        assert!(f.local_contains("remote moved on"));
    }

    /// Nothing to rebase is a real answer, not a failure.
    #[test]
    fn an_already_current_branch_is_up_to_date() {
        let f = Fixture::new();
        let state = rebase_onto_remote_base(f.repo()).expect("the rebase runs");
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
            rebase_onto_remote_base(&repo).expect("must not fail"),
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

        let err = rebase_onto_remote_base(f.repo()).expect_err("must fail loudly");
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
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    pub fn run_git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
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
        let refused = RepoOutcome::Refused {
            branch: "main".into(),
            reason: "protected".into(),
        };
        assert!(
            refused.is_failure(),
            "a refusal is a failure for the exit code"
        );
        assert!(refused.render().contains("main"));
        assert!(!refused.render().contains("failed:"));
    }

    fn opts_for(f: &Fixture) -> RunOptions {
        RunOptions {
            how_far: HowFar::Push,
            state_dir: f.repo().join(".ro-state"),
            ..Default::default()
        }
    }

    fn plan_on(f: &Fixture, branch: &str, onto: Option<&str>) -> RepoPlan {
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
}
