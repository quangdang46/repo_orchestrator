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

/// Classification of a failed git command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitErrorKind {
    /// Authentication failure (bad creds, missing token, denied push).
    Auth,
    /// Network failure (DNS, TLS, refused connection).
    Network,
    /// Merge conflict.
    Conflict,
    /// Worktree is dirty (uncommitted changes block the operation).
    Dirty,
    /// Non-fast-forward push or pull rejected.
    NonFastForward,
    /// `git` binary not found on PATH.
    GitMissing,
    /// Anything we couldn't classify.
    Other,
}

impl GitErrorKind {
    pub fn classify(stderr: &str) -> Self {
        let s = stderr.to_ascii_lowercase();
        if s.contains("could not read username")
            || s.contains("authentication failed")
            || s.contains("permission denied")
            || s.contains("403 forbidden")
            || s.contains("401 unauthorized")
        {
            Self::Auth
        } else if s.contains("could not resolve host")
            || s.contains("connection refused")
            || s.contains("operation timed out")
            || s.contains("ssl certificate problem")
        {
            Self::Network
        } else if s.contains("conflict") || s.contains("merge conflict") {
            Self::Conflict
        } else if s.contains("uncommitted changes")
            || s.contains("would be overwritten")
            || s.contains("local changes")
        {
            Self::Dirty
        } else if s.contains("non-fast-forward") || s.contains("rejected") {
            Self::NonFastForward
        } else {
            Self::Other
        }
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

/// Run an arbitrary git subcommand inside `repo`. Use one of the typed
/// helpers when possible; this is escape-hatch for callers that need
/// special flags.
pub fn run(repo: &Path, args: &[&str]) -> Result<GitCommandResult> {
    run_in(Some(repo), args)
}

fn run_in(cwd: Option<&Path>, args: &[&str]) -> Result<GitCommandResult> {
    let mut cmd = Command::new("git");
    cmd.arg("--no-pager");
    cmd.args(args);
    if let Some(p) = cwd {
        cmd.current_dir(p);
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
    } = cmd
        .output()
        .with_context(|| format!("spawning git {}", args.join(" ")))?;

    Ok(GitCommandResult {
        args: args.iter().map(|s| s.to_string()).collect(),
        status: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
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
    run(repo, &argv)
}

/// Fetch from a specific remote by name. Convenience wrapper around `fetch`.
pub fn fetch_remote(repo: &Path, remote: &str) -> Result<()> {
    let opts = FetchOpts {
        remote: Some(remote.to_string()),
        ..Default::default()
    };
    fetch(repo, &opts)?;
    Ok(())
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
    let result = run(repo, &argv)?;
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
    let result = run_in(None, &argv)?;
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
    let add = run(repo, &argv)?;
    if !add.ok() {
        bail!("git add failed: {}", add.stderr.trim());
    }
    let commit_args = vec!["commit", "--no-gpg-sign", "-m", message];
    let commit = run(repo, &commit_args)?;
    if !commit.ok() {
        bail!("git commit failed: {}", commit.stderr.trim());
    }
    let head = run(repo, &["rev-parse", "HEAD"])?;
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
    run(repo, &argv)
}

/// Hard-reset the working tree to a specific OID. **Destructive.**
pub fn reset_hard(repo: &Path, oid: &str) -> Result<GitCommandResult> {
    if oid.is_empty() {
        bail!("reset_hard requires an oid");
    }
    run(repo, &["reset", "--hard", oid])
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
    fn classify_auth_errors() {
        assert_eq!(
            GitErrorKind::classify("fatal: Authentication failed"),
            GitErrorKind::Auth
        );
        assert_eq!(
            GitErrorKind::classify("ERROR: Permission denied"),
            GitErrorKind::Auth
        );
    }

    #[test]
    fn classify_network_errors() {
        assert_eq!(
            GitErrorKind::classify("fatal: unable to access ... Could not resolve host"),
            GitErrorKind::Network
        );
        assert_eq!(
            GitErrorKind::classify("fatal: SSL certificate problem"),
            GitErrorKind::Network
        );
    }

    #[test]
    fn classify_conflict() {
        assert_eq!(
            GitErrorKind::classify("CONFLICT (content): Merge conflict in foo"),
            GitErrorKind::Conflict
        );
    }

    #[test]
    fn classify_other() {
        assert_eq!(
            GitErrorKind::classify("some random error"),
            GitErrorKind::Other
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
    fn reset_hard_rejects_empty_oid() {
        let (_tmp, path) = temp_repo();
        let err = reset_hard(&path, "").unwrap_err();
        assert!(err.to_string().contains("requires an oid"));
    }

    #[test]
    fn reset_hard_to_previous_commit() {
        let (_tmp, path) = temp_repo();
        std::fs::write(path.join("a.txt"), "v1").unwrap();
        let oid1 = commit(&path, &[PathBuf::from("a.txt")], "v1").unwrap();
        std::fs::write(path.join("a.txt"), "v2").unwrap();
        let _oid2 = commit(&path, &[PathBuf::from("a.txt")], "v2").unwrap();
        let r = reset_hard(&path, &oid1).unwrap();
        assert!(r.ok());
        let content = std::fs::read_to_string(path.join("a.txt")).unwrap();
        assert_eq!(content, "v1");
    }

    #[test]
    fn clone_and_push_round_trip() {
        // upstream: bare repo
        let upstream_tmp = TempDir::new().unwrap();
        let upstream = upstream_tmp.path().join("origin.git");
        let r = run_in(None, &["init", "--bare", "-q", upstream.to_str().unwrap()]).unwrap();
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
