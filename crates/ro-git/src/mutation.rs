//! Shell-out git mutation commands.
//!
//! All git mutations go through this module. Read queries belong in
//! `read.rs` (gix). The split is intentional: gix is fast for reads;
//! shelling out to `git` is safer for mutations because it matches
//! `git`'s exact behaviour, refspec handling, and credential helpers.
//!
//! Rules per PLAN.md §12.5:
//!   * `Command::arg` only — no shell interpolation.
//!   * Set `GIT_TERMINAL_PROMPT=0` and `GCM_INTERACTIVE=Never`.
//!   * Set `LC_ALL=C` for stable parsing.
//!   * Pass `--no-pager` to disable interactive paging.
//!   * Capture stdout/stderr.
//!   * Classify errors (auth, network, conflict, dirty, …).
//!
//! Timeout enforcement is the caller's responsibility for now (the
//! daemon owns long-running concerns). Synchronous API; future work
//! will wrap these in tokio for the daemon.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Result of running a git command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitCommandResult {
    pub args: Vec<String>,
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl GitCommandResult {
    pub fn ok(&self) -> bool {
        self.status == 0
    }
}

/// Options for `fetch`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FetchOpts {
    pub remote: Option<String>,
    pub prune: bool,
    pub tags: bool,
    pub depth: Option<u32>,
}

/// Options for `pull`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PullOpts {
    pub remote: Option<String>,
    pub branch: Option<String>,
    /// Pull strategy. Defaults to `--ff-only` (safest).
    pub strategy: PullStrategy,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PullStrategy {
    #[default]
    FastForwardOnly,
    Merge,
    Rebase,
}

/// Outcome of a pull.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullOutcome {
    pub result: GitCommandResult,
    pub conflict: bool,
    pub already_up_to_date: bool,
}

/// Options for `clone`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CloneOpts {
    pub depth: Option<u32>,
    pub branch: Option<String>,
    pub recurse_submodules: bool,
}

/// Outcome of a clone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloneOutcome {
    pub dest: PathBuf,
    pub result: GitCommandResult,
}

/// Options for `push`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PushOpts {
    pub remote: Option<String>,
    pub branch: Option<String>,
    pub force_with_lease: bool,
    pub set_upstream: bool,
    pub tags: bool,
}

/// Per-invocation settings for a git command.
///
/// The env list is merged **under** the hardening block, which always wins.
/// `GIT_TERMINAL_PROMPT=0` is what stops a hung credential prompt from stalling
/// a whole fleet, and a caller that could relax it would be a caller that can
/// hang the fleet — which is exactly what the credential path is.
#[derive(Debug, Clone, Default)]
pub struct RunOpts<'a> {
    /// Extra environment for this invocation only.
    ///
    /// This is how a credential reaches exactly one `git push` and no other:
    /// an `http.extraheader` on the child's env does not touch the user's
    /// `~/.gitconfig`, and does not leak into the next repository in the fleet.
    pub env: &'a [(String, String)],
    /// Kill the child after this long. `None` means wait, which is the current
    /// behaviour and is itself the bug: a hung credential prompt blocks
    /// forever.
    pub timeout: Option<Duration>,
}

impl<'a> RunOpts<'a> {
    /// The default: no extra environment, no timeout.
    pub fn none() -> Self {
        Self {
            env: &[],
            timeout: None,
        }
    }

    pub fn with_env(env: &'a [(String, String)]) -> Self {
        Self { env, timeout: None }
    }
}

/// Run an arbitrary git subcommand inside `repo`.
///
/// Use one of the typed helpers when possible; this is the escape hatch for a
/// caller that needs flags none of them expose.
#[deprecated(
    since = "0.2.0",
    note = "no per-invocation settings are reachable through this. Callers that need \
            an env var — which is every caller that hands a credential to git — must go \
            through run_in with RunOpts. This shim exists so the sixteen existing call \
            sites do not all churn at once; delete it when the sweep namespace goes in \
            Phase 4."
)]
pub fn run(repo: &Path, args: &[&str]) -> Result<GitCommandResult> {
    run_in(Some(repo), args, &RunOpts::none())
}

/// Run git in `cwd` with `opts`.
///
/// **`cwd` is `Option` on purpose.** It is not defensive optionality: `clone`
/// calls this with `None` because git has to run *outside* the destination
/// repository, which does not exist yet. An earlier revision of the plan
/// proposed `run_in(repo: &Path, ...)` — which deletes the only way to spawn
/// git without a working directory and breaks `clone`. If this parameter is
/// ever "cleaned up" into a plain `&Path`, `clone` stops working and the
/// failure looks like a clone bug rather than a signature bug.
pub fn run_in(cwd: Option<&Path>, args: &[&str], opts: &RunOpts<'_>) -> Result<GitCommandResult> {
    let mut cmd = Command::new("git");
    cmd.arg("--no-pager");
    cmd.args(args);
    if let Some(p) = cwd {
        cmd.current_dir(p);
    }
    // The caller's environment is applied first and the hardening block last,
    // so the hardening always wins.
    //
    // The other order is defensible on paper and wrong in practice: it means
    // any caller that sets `GIT_TERMINAL_PROMPT` to anything else can put an
    // interactive password prompt back on a git that ro runs unattended, and a
    // credential path is exactly where a caller is reaching for environment
    // variables. These four are the reason a fleet run does not stall on a
    // terminal nobody is watching, and they are not the caller's to relax.
    for (key, value) in opts.env {
        cmd.env(key, value);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .env("LC_ALL", "C")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat");

    let Output {
        status,
        stdout,
        stderr,
    } = match opts.timeout {
        None => cmd
            .output()
            .with_context(|| format!("spawning git {}", args.join(" ")))?,
        Some(limit) => spawn_with_timeout(&mut cmd, args, limit)?,
    };

    Ok(GitCommandResult {
        args: args.iter().map(|s| s.to_string()).collect(),
        status: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

/// Spawn with a deadline, killing the child if it overruns.
///
/// A timeout that returns without killing the child is worse than no timeout:
/// the process keeps holding the worktree lock and the git process the next
/// command needs, so the fleet stalls anyway and now also reports success.
///
/// Implemented by polling rather than by a thread, because `Child::wait_timeout`
/// is not in std and a background killer thread outliving its child is its own
/// class of bug. The poll interval is short relative to any useful timeout and
/// long relative to process-spawn cost.
fn spawn_with_timeout(cmd: &mut Command, args: &[&str], limit: Duration) -> Result<Output> {
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning git {}", args.join(" ")))?;

    let deadline = Instant::now() + limit;
    const POLL: Duration = Duration::from_millis(25);

    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                // `wait_with_output` after `try_wait` reaps the child, so the
                // pipes are closed and the read cannot block forever.
                return child
                    .wait_with_output()
                    .with_context(|| format!("collecting output of git {}", args.join(" ")));
            }
            Ok(None) => {}
            Err(e) => return Err(anyhow::Error::new(e).context("waiting on git")),
        }

        if Instant::now() >= deadline {
            let _ = child.kill();
            // Reap, so the killed child does not become a zombie.
            let _ = child.wait();
            bail!(
                "git {} did not finish within {:?} and was killed. A git that hangs \
                 here is usually waiting on a credential prompt or a network, and \
                 both are bounded by the run timeout for a reason.",
                args.join(" "),
                limit
            );
        }
        std::thread::sleep(POLL);
    }
}

/// Fetch refs from a remote.
pub fn fetch(repo: &Path, opts: &FetchOpts) -> Result<GitCommandResult> {
    let mut args: Vec<String> = vec!["fetch".to_string()];
    if opts.prune {
        args.push("--prune".to_string());
    }
    if opts.tags {
        args.push("--tags".to_string());
    }
    if let Some(d) = opts.depth {
        args.push(format!("--depth={d}"));
    }
    if let Some(remote) = &opts.remote {
        args.push(remote.clone());
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    run_in(Some(repo), &argv, &RunOpts::none())
}

/// Pull from a remote.
pub fn pull(repo: &Path, opts: &PullOpts) -> Result<PullOutcome> {
    let mut args: Vec<String> = vec!["pull".to_string()];
    match opts.strategy {
        PullStrategy::FastForwardOnly => args.push("--ff-only".to_string()),
        PullStrategy::Merge => args.push("--no-rebase".to_string()),
        PullStrategy::Rebase => args.push("--rebase".to_string()),
    }
    if let Some(remote) = &opts.remote {
        args.push(remote.clone());
    }
    if let Some(branch) = &opts.branch {
        args.push(branch.clone());
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = run_in(Some(repo), &argv, &RunOpts::none())?;
    let conflict = result.stderr.contains("conflict") || result.stdout.contains("CONFLICT");
    let already_up_to_date = result.stdout.contains("Already up to date");
    Ok(PullOutcome {
        result,
        conflict,
        already_up_to_date,
    })
}

/// Clone a repository to `dest`.
pub fn clone(url: &str, dest: &Path, opts: &CloneOpts) -> Result<CloneOutcome> {
    let mut args: Vec<String> = vec!["clone".to_string()];
    if let Some(d) = opts.depth {
        args.push(format!("--depth={d}"));
    }
    if let Some(branch) = &opts.branch {
        args.push("--branch".to_string());
        args.push(branch.clone());
    }
    if opts.recurse_submodules {
        args.push("--recurse-submodules".to_string());
    }
    args.push(url.to_string());
    let dest_str = dest
        .to_str()
        .context("clone destination path is not valid UTF-8")?
        .to_string();
    args.push(dest_str);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = run_in(None, &argv, &RunOpts::none())?;
    Ok(CloneOutcome {
        dest: dest.to_path_buf(),
        result,
    })
}

/// Stage paths and create a commit. Returns the new commit OID.
pub fn commit(repo: &Path, files: &[PathBuf], message: &str) -> Result<String> {
    if files.is_empty() {
        bail!("commit requires at least one file");
    }
    // Stage explicit files only; never `git add -A`.
    let mut add_args: Vec<String> = vec!["add".to_string(), "--".to_string()];
    for f in files {
        let s = f
            .to_str()
            .context("commit file path is not valid UTF-8")?
            .to_string();
        add_args.push(s);
    }
    let argv: Vec<&str> = add_args.iter().map(String::as_str).collect();
    let add = run_in(Some(repo), &argv, &RunOpts::none())?;
    if !add.ok() {
        bail!("git add failed: {}", add.stderr.trim());
    }
    let commit_args = vec!["commit", "--no-gpg-sign", "-m", message];
    let commit = run_in(Some(repo), &commit_args, &RunOpts::none())?;
    if !commit.ok() {
        bail!("git commit failed: {}", commit.stderr.trim());
    }
    let head = run_in(Some(repo), &["rev-parse", "HEAD"], &RunOpts::none())?;
    if !head.ok() {
        bail!("git rev-parse HEAD failed: {}", head.stderr.trim());
    }
    Ok(head.stdout.trim().to_string())
}

/// Push to a remote.
pub fn push(repo: &Path, opts: &PushOpts) -> Result<GitCommandResult> {
    let mut args: Vec<String> = vec!["push".to_string()];
    if opts.set_upstream {
        args.push("--set-upstream".to_string());
    }
    if opts.force_with_lease {
        args.push("--force-with-lease".to_string());
    }
    if opts.tags {
        args.push("--tags".to_string());
    }
    if let Some(remote) = &opts.remote {
        args.push(remote.clone());
    }
    if let Some(branch) = &opts.branch {
        args.push(branch.clone());
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    run_in(Some(repo), &argv, &RunOpts::none())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// The test the whole `RunOpts` exists for, and the reason it is written
    /// the way it is.
    ///
    /// Asserting that the `Command` was *built* with the variable proves
    /// nothing: the failure being designed against is a variable that is set on
    /// the builder and never seen by the child, which a `Command`-shaped
    /// assertion passes happily. So this runs a real `git` and reads the value
    /// back out of its own output.
    ///
    /// `git -c` with a config that echoes into the environment is not a thing,
    /// so the child is made to reveal it with the one git subcommand that
    /// prints a variable's value: `git var GIT_EDITOR` is not it either, so we
    /// use the generic escape hatch — `git --exec-path` is constant, and
    /// instead the value is observed through `git -c alias`, where the alias
    /// body is a shell command git will run. That is exactly the shape the real
    /// credential path uses too (an `http.extraheader` handed to one push), so
    /// the test exercises the same mechanism, not a simulation of it.
    #[test]
    fn an_injected_env_var_reaches_the_child_process() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let injected: Vec<(String, String)> = vec![(
            "RO_PROBE_TOKEN".to_string(),
            "value-that-must-arrive".to_string(),
        )];

        let result = run_in(
            Some(&repo),
            &["-c", "alias.probe=!echo $RO_PROBE_TOKEN", "probe"],
            &RunOpts::with_env(&injected),
        )
        .expect("git should run");

        assert!(
            result.ok(),
            "the probe alias should succeed, stderr: {}",
            result.stderr
        );
        assert!(
            result.stdout.contains("value-that-must-arrive"),
            "the injected variable did not reach the child. stdout was {:?}",
            result.stdout
        );
    }

    /// The inverse, and the reason the positive test above is not self-deceiving:
    /// without the injection the child sees nothing.
    #[test]
    fn without_the_injection_the_child_sees_nothing() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let result = run_in(
            Some(&repo),
            &["-c", "alias.probe=!echo [$RO_PROBE_TOKEN]", "probe"],
            &RunOpts::none(),
        )
        .expect("git should run");

        assert!(
            result.stdout.contains("[]") || result.stdout.trim().is_empty(),
            "an unset variable must print as empty, got {:?}",
            result.stdout
        );
        assert!(
            !result.stdout.contains("value-that-must-arrive"),
            "nothing should have injected a value"
        );
    }

    /// The hardening block wins over the caller's environment.
    ///
    /// This is not a style preference. `GIT_TERMINAL_PROMPT=0` is what stops a
    /// hung credential prompt from stalling a whole fleet, and a caller that
    /// could set it to anything else could put an interactive password prompt
    /// back on a git that ro runs unattended. The credential path is precisely
    /// where callers reach for environment variables, so the ordering has to
    /// protect that case.
    #[test]
    fn the_hardening_block_cannot_be_overridden_by_a_caller() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let overrides: Vec<(String, String)> = vec![
            ("GIT_TERMINAL_PROMPT".to_string(), "yes".to_string()),
            ("GCM_INTERACTIVE".to_string(), "Always".to_string()),
            ("LC_ALL".to_string(), "fr_FR.UTF-8".to_string()),
        ];

        let result = run_in(
            Some(&repo),
            &[
                "-c",
                "alias.probe=!echo \"$GIT_TERMINAL_PROMPT|$GCM_INTERACTIVE|$LC_ALL\"",
                "probe",
            ],
            &RunOpts::with_env(&overrides),
        )
        .expect("git should run");

        assert!(
            result.stdout.contains("0|Never|C"),
            "the hardening block must win over the caller's environment, got {:?}",
            result.stdout
        );
    }

    /// A caller *can* add variables — the credential path depends on it.
    ///
    /// The variable is read back through a shell identifier rather than the
    /// real `http.extraheader` key, because a dot is not legal in a shell
    /// variable name and `echo $http.extraheader` would expand to nothing. What
    /// is under test is that a caller-added value reaches the child at all,
    /// which the previous test already covers for the general case; this one
    /// pins that the hardening ordering did not become a blanket refusal.
    #[test]
    fn a_caller_can_still_add_variables() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let injected: Vec<(String, String)> =
            vec![("RO_EXTRA_HEADER".to_string(), "added-by-caller".to_string())];

        let result = run_in(
            Some(&repo),
            &["-c", "alias.probe=!echo [$RO_EXTRA_HEADER]", "probe"],
            &RunOpts::with_env(&injected),
        )
        .expect("git should run");

        assert!(
            result.stdout.contains("added-by-caller"),
            "a caller must be able to add an env var, got {:?}",
            result.stdout
        );
    }

    /// `Option<&Path>` is load-bearing: `clone` has to run git outside the
    /// destination, which does not exist yet. Collapsing it to `&Path` deletes
    /// the only way to spawn git with no working directory, and the failure
    /// looks like a clone bug rather than a signature bug.
    #[test]
    fn git_can_be_spawned_with_no_working_directory() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("cloned");
        let upstream = tmp.path().join("upstream.git");
        std::fs::create_dir_all(&tmp).unwrap();

        let r = run_in(
            None,
            &["init", "--bare", "-q", upstream.to_str().unwrap()],
            &RunOpts::none(),
        )
        .unwrap();
        assert!(
            r.ok(),
            "git init --bare outside a worktree failed: {}",
            r.stderr
        );

        // And the real consequence: clone still works, which is the caller
        // that depends on the None.
        let outcome = clone(upstream.to_str().unwrap(), &dest, &CloneOpts::default())
            .expect("clone should succeed");
        assert!(
            outcome.result.ok(),
            "clone failed: {}",
            outcome.result.stderr
        );
        assert!(dest.join(".git").exists(), "the clone should have landed");
    }

    fn temp_repo() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().to_path_buf();
        run_git(&path, &["init", "-q", "-b", "main"]);
        run_git(&path, &["config", "user.email", "test@example.com"]);
        run_git(&path, &["config", "user.name", "Test"]);
        run_git(&path, &["config", "commit.gpgSign", "false"]);
        (tmp, path)
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let r = run(dir, args).unwrap();
        assert!(
            r.ok(),
            "git {args:?} failed: stdout={:?} stderr={:?}",
            r.stdout,
            r.stderr
        );
    }

    #[test]
    fn run_returns_command_result() {
        let (_tmp, path) = temp_repo();
        let r = run(&path, &["status", "--porcelain"]).unwrap();
        assert!(r.ok());
        assert!(r.stdout.is_empty());
    }

    #[test]
    fn fetch_no_remote_fails_gracefully() {
        let (_tmp, path) = temp_repo();
        let r = fetch(&path, &FetchOpts::default()).unwrap();
        // The call must complete without panicking; status depends on git
        // version behaviour for an empty repo with no configured remote.
        // We just sanity-check that we got a structured result back.
        assert_eq!(r.args[0], "fetch");
    }

    #[test]
    fn commit_creates_oid() {
        let (_tmp, path) = temp_repo();
        std::fs::write(path.join("a.txt"), "hello").unwrap();
        let oid = commit(&path, &[PathBuf::from("a.txt")], "add a").unwrap();
        assert_eq!(oid.len(), 40);
    }

    #[test]
    fn commit_rejects_empty_files() {
        let (_tmp, path) = temp_repo();
        let err = commit(&path, &[], "msg").unwrap_err();
        assert!(err.to_string().contains("at least one file"));
    }

    #[test]
    fn clone_and_push_round_trip() {
        // upstream: bare repo
        let upstream_tmp = TempDir::new().unwrap();
        let upstream = upstream_tmp.path().join("origin.git");
        let r = run_in(
            None,
            &["init", "--bare", "-q", upstream.to_str().unwrap()],
            &RunOpts::none(),
        )
        .unwrap();
        assert!(r.ok(), "init bare failed: {}", r.stderr);

        // clone it
        let work_tmp = TempDir::new().unwrap();
        let work = work_tmp.path().join("work");
        let outcome = clone(upstream.to_str().unwrap(), &work, &CloneOpts::default()).unwrap();
        assert!(
            outcome.result.ok(),
            "clone failed: {}",
            outcome.result.stderr
        );
        run_git(&work, &["config", "user.email", "test@example.com"]);
        run_git(&work, &["config", "user.name", "Test"]);
        run_git(&work, &["config", "commit.gpgSign", "false"]);
        run_git(&work, &["checkout", "-q", "-b", "main"]);

        // commit something
        std::fs::write(work.join("a.txt"), "hello").unwrap();
        let _oid = commit(&work, &[PathBuf::from("a.txt")], "first").unwrap();

        // push it
        let push_opts = PushOpts {
            remote: Some("origin".into()),
            branch: Some("main".into()),
            set_upstream: true,
            ..Default::default()
        };
        let r = push(&work, &push_opts).unwrap();
        assert!(r.ok(), "push failed: {} / {}", r.stdout, r.stderr);
    }
}
