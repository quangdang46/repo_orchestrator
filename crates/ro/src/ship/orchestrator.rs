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
            RepoOutcome::Blocked { .. } | RepoOutcome::Failed { .. }
        )
    }

    /// One line for the summary table.
    pub fn render(&self) -> String {
        match self {
            RepoOutcome::Committed { oid, .. } => format!("committed {oid}"),
            RepoOutcome::NothingToCommit => "nothing to commit".into(),
            RepoOutcome::SkippedConflict { .. } => "skipped: mid-conflict".into(),
            RepoOutcome::Blocked { reason, .. } => format!("blocked: {reason}"),
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
            branch: Some(plan.base_branch.clone()),
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
            identity: None,
            engine: ro_engine::resolve("git", &ro_engine::EngineSlots::default(), None)
                .expect("git is one of the three built-ins"),
        }
    }
}
