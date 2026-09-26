//! gix-based read queries.
//!
//! Pure read-only queries against a git repository using `gix`. No
//! mutations. All functions return typed results, never raw strings.
//!
//! For complex multi-commit graph operations (counting ahead/behind),
//! we currently shell out to `git rev-list` — this matches `git`'s exact
//! semantics and avoids re-implementing graph walking. Future work can
//! migrate hot paths to pure gix.
//!
//! See PLAN.md §12.5 for the API surface.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use crate::status::{AheadBehind, RepoStatus};

/// Discover the .git root for a working directory.
///
/// Searches upward from `dir` until it finds a `.git` directory.
pub fn discover(dir: &Path) -> Result<PathBuf> {
    let repo =
        gix::discover(dir).with_context(|| format!("discovering git repo at {}", dir.display()))?;
    Ok(repo.path().to_path_buf())
}

/// Resolve the current HEAD OID as a hex string, or `None` if the repo
/// has no commits yet (newly initialized).
pub fn head_oid(repo_path: &Path) -> Result<Option<String>> {
    let repo = open_repo(repo_path)?;
    match repo.head_id() {
        Ok(id) => Ok(Some(id.to_hex().to_string())),
        Err(_) => Ok(None),
    }
}

/// Return the name of the current branch, or `None` if in detached HEAD
/// or the repo has no commits.
pub fn current_branch(repo_path: &Path) -> Result<Option<String>> {
    let repo = open_repo(repo_path)?;
    let head = match repo.head() {
        Ok(h) => h,
        Err(_) => return Ok(None),
    };
    let name = head.referent_name();
    let Some(name) = name else {
        return Ok(None);
    };
    let s = name.shorten().to_string();
    Ok(Some(s))
}

/// Return true if the worktree has uncommitted changes.
///
/// Currently shells out to `git status --porcelain` since gix's status
/// API requires substantial setup. This will be migrated to pure gix in
/// a follow-up bead.
pub fn is_dirty(repo_path: &Path) -> Result<bool> {
    let output = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .with_context(|| format!("running git status in {}", repo_path.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(!output.stdout.is_empty())
}

/// Get a coarse status of the repository.
///
/// Returns one of: `Missing` (path doesn't exist or isn't a repo), `Dirty`
/// (worktree has changes), `Current` (clean and on a branch), or
/// `Unknown` (we couldn't determine).
///
/// For ahead/behind comparisons, callers should use `ahead_behind`
/// explicitly with an upstream reference.
pub fn status(repo_path: &Path) -> Result<RepoStatus> {
    if !repo_path.exists() {
        return Ok(RepoStatus::Missing);
    }
    if open_repo(repo_path).is_err() {
        return Ok(RepoStatus::Missing);
    }
    if is_dirty(repo_path)? {
        return Ok(RepoStatus::Dirty);
    }
    Ok(RepoStatus::Current)
}

/// Get ahead/behind counts of HEAD relative to a ref name like
/// `origin/main`. Shells out to `git rev-list --left-right --count`.
///
/// **An error is an error, not zero.** This used to return
/// `AheadBehind::ZERO` on any git failure, which conflated *"I do not know"*
/// with *"you are in sync"* — a typo'd upstream displayed as up to date, and a
/// repo whose `.git` is corrupt displayed as clean. Across a twenty-repo fleet
/// that is a green board over two unmeasured rows, which is worse than no board.
///
/// A number that is always zero when it cannot be computed is not data.
pub fn ahead_behind(repo_path: &Path, upstream: &str) -> Result<AheadBehind> {
    let output = std::process::Command::new("git")
        .args([
            "rev-list",
            "--left-right",
            "--count",
            &format!("{upstream}...HEAD"),
        ])
        .current_dir(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .with_context(|| format!("running git rev-list in {}", repo_path.display()))?;
    if !output.status.success() {
        // The reason matters and is not "unknown revision" in every case: it
        // can equally be a corrupt object store or an unreadable directory.
        bail!(
            "cannot measure {upstream} against HEAD in {}: {}",
            repo_path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let s = String::from_utf8_lossy(&output.stdout);
    let mut iter = s.split_whitespace();
    let behind: u32 = iter
        .next()
        .and_then(|s| s.parse().ok())
        .with_context(|| format!("unreadable rev-list output for {upstream}: {s:?}"))?;
    let ahead: u32 = iter
        .next()
        .and_then(|s| s.parse().ok())
        .with_context(|| format!("unreadable rev-list output for {upstream}: {s:?}"))?;
    Ok(AheadBehind { ahead, behind })
}

/// The URL of a named remote.
///
/// Separate from `has_remote` because "does it have one" and "what is it"
/// are different questions, and the second is needed to adopt a checkout: a
/// row needs a `clone_url`.
///
/// `Ok(None)` means the remote genuinely has no URL. `Err` means the query
/// could not be run — a path that is not a repo, or `git` failing.
///
/// The distinction is not theoretical. `ro add <local-path>` stores this
/// value as the row's `clone_url`, and on `None` the caller falls back to
/// synthesising `https://github.com/<parent>/<name>.git` from the directory
/// layout. A `None` produced by a *failed query* therefore becomes a stored
/// URL that has never been verified against the actual remote, and the
/// failure surfaces much later as a clone error against a nonsense
/// repository. `Ok(None)` is a fact worth handling; `Err` is a broken
/// measurement and must not be answered with a guess.
pub fn remote_url(repo_path: &Path, remote: &str) -> Result<Option<String>> {
    let out = std::process::Command::new("git")
        .args(["remote", "get-url", remote])
        .current_dir(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .with_context(|| {
            format!(
                "reading remote {remote:?} in {} — the path may not be a git repository",
                repo_path.display()
            )
        })?;
    if !out.status.success() {
        // git exits non-zero here for two quite different reasons: the
        // remote does not exist (exit 2, "No such remote"), and something
        // went wrong. Collapsing both into `None` is the bug, so the
        // stderr decides.
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("No such remote") {
            return Ok(None);
        }
        bail!(
            "cannot read remote {remote:?} in {}: {}",
            repo_path.display(),
            stderr.trim()
        );
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if url.is_empty() {
        // An empty URL is git reporting a remote that exists but is
        // unset. That is not the same as a missing remote, and the
        // caller synthesises differently for each.
        bail!(
            "remote {remote:?} in {} has an empty URL",
            repo_path.display()
        );
    }
    Ok(Some(url))
}

/// Return whether the repository has a remote with the given name.
///
/// `Ok(false)` is a real answer: the remote is genuinely not configured.
/// `Err` is *"I could not find out"* — the path is not a repo, or `git`
/// failed.
///
/// Those were the same value before, and the difference is the whole
/// point of the bead. `ro sync` uses this for the *"no GitHub remote →
/// local commit only"* rule (PLAN §1, the `has_remote` exception), so a
/// `false` obtained from a failed query tells the orchestrator to skip
/// the push **because there is nowhere to push** — when in fact the push
/// was never attempted and the remote may well exist. A wrong `false` is
/// silent: the run reports success having done less than it claimed.
pub fn has_remote(repo_path: &Path, remote: &str) -> Result<bool> {
    let out = std::process::Command::new("git")
        .args(["remote"])
        .current_dir(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .with_context(|| {
            format!(
                "listing remotes in {} — the path may not be a git repository",
                repo_path.display()
            )
        })?;
    if !out.status.success() {
        bail!(
            "cannot list remotes in {}: {}",
            repo_path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    // `git remote` prints one bare name per line — confirmed against real
    // output rather than assumed, because the `-v` form is
    // `name<TAB>url (fetch)` and comparing a whole line to a name would
    // then never match.
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|line| line.trim() == remote))
}

fn open_repo(path: &Path) -> Result<gix::Repository> {
    gix::open(path).with_context(|| format!("opening git repo at {}", path.display()))
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
        (tmp, path)
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        if !out.status.success() {
            panic!(
                "git {args:?} failed:\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    fn commit(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-q", "-m", &format!("add {name}")]);
    }

    #[test]
    fn discover_finds_git_dir() {
        let (_tmp, path) = temp_repo();
        let root = discover(&path).unwrap();
        assert!(root.ends_with(".git"));
    }

    #[test]
    fn head_oid_empty_repo() {
        let (_tmp, path) = temp_repo();
        assert_eq!(head_oid(&path).unwrap(), None);
    }

    #[test]
    fn head_oid_after_commit() {
        let (_tmp, path) = temp_repo();
        commit(&path, "a.txt", "hello");
        let oid = head_oid(&path).unwrap().expect("HEAD should exist");
        assert_eq!(oid.len(), 40);
    }

    #[test]
    fn current_branch_after_commit() {
        let (_tmp, path) = temp_repo();
        commit(&path, "a.txt", "hello");
        let branch = current_branch(&path).unwrap();
        assert_eq!(branch, Some("main".to_string()));
    }

    #[test]
    fn is_dirty_clean_repo() {
        let (_tmp, path) = temp_repo();
        commit(&path, "a.txt", "hello");
        assert!(!is_dirty(&path).unwrap());
    }

    #[test]
    fn is_dirty_with_changes() {
        let (_tmp, path) = temp_repo();
        commit(&path, "a.txt", "hello");
        std::fs::write(path.join("a.txt"), "changed").unwrap();
        assert!(is_dirty(&path).unwrap());
    }

    #[test]
    fn status_clean_repo() {
        let (_tmp, path) = temp_repo();
        commit(&path, "a.txt", "hello");
        assert_eq!(status(&path).unwrap(), RepoStatus::Current);
    }

    #[test]
    fn status_dirty_repo() {
        let (_tmp, path) = temp_repo();
        commit(&path, "a.txt", "hello");
        std::fs::write(path.join("a.txt"), "changed").unwrap();
        assert_eq!(status(&path).unwrap(), RepoStatus::Dirty);
    }

    #[test]
    fn status_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("does-not-exist");
        assert_eq!(status(&path).unwrap(), RepoStatus::Missing);
    }
}
