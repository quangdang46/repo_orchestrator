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

use anyhow::{Context, Result};
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
        return Ok(AheadBehind::ZERO);
    }
    let s = String::from_utf8_lossy(&output.stdout);
    let mut iter = s.split_whitespace();
    let behind: u32 = iter.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let ahead: u32 = iter.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    Ok(AheadBehind { ahead, behind })
}

/// Return true if the repository has a remote with the given name.
///
/// Returns false if the path is not a git repo or the remote query fails.
pub fn has_remote(repo_path: &Path, remote: &str) -> bool {
    let Ok(out) = std::process::Command::new("git")
        .args(["remote"])
        .current_dir(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|line| line.trim() == remote)
}

/// List local branches that have been merged into HEAD.
///
/// Excludes the current branch. Returns an empty list on any error.
pub fn merged_branches(repo_path: &Path) -> Vec<String> {
    let Ok(out) = std::process::Command::new("git")
        .args(["branch", "--merged", "--format=%(refname:short)"])
        .current_dir(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let current = current_branch(repo_path).ok().flatten();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .filter(|s| Some(s) != current.as_ref())
        .filter(|s| s != "main" && s != "master")
        .collect()
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
