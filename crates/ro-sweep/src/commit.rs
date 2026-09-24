//! Sweep commit implementation.
//!
//! Stages changes, runs secret scan + denylist check, quality gates, then commits.

use anyhow::Result;
use std::path::Path;

use crate::denylist::Denylist;
use crate::quality_gates;
use crate::secret_scan::{self, SecretScanMode};
use serde::{Deserialize, Serialize};

/// Outcome of a sweep commit attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommitOutcome {
    Committed { message: String, oid: String },
    NothingToCommit,
    BlockedByGates { failures: Vec<String> },
    BlockedBySecrets { files: Vec<String> },
    BlockedByDenylist { files: Vec<String> },
}

/// Run sweep commit: scan, gate, commit.
///
/// Order of operations:
/// 1. Detect all modified/untracked files.
/// 2. Check denylist on those files BEFORE staging.
/// 3. Run quality gates.
/// 4. Secret scan on dirty files.
/// 5. Stage all and commit.
pub fn sweep_commit(repo_path: &Path, message: &str) -> Result<CommitOutcome> {
    // 1. Check for changes to commit
    let dirty_files = dirty_files(repo_path)?;
    if dirty_files.is_empty() {
        return Ok(CommitOutcome::NothingToCommit);
    }

    // 2. Denylist check on dirty files BEFORE staging
    let denylist = Denylist::new_default()?;
    let blocked: Vec<String> = dirty_files
        .iter()
        .filter_map(|f| {
            if let Ok(rel) = f.strip_prefix(repo_path) {
                // Normalize to forward slashes so glob patterns match on Windows.
                let s = rel.to_string_lossy().replace('\\', "/");
                if denylist.is_denied(&s) {
                    return Some(s);
                }
            }
            None
        })
        .collect();
    if !blocked.is_empty() {
        return Ok(CommitOutcome::BlockedByDenylist { files: blocked });
    }

    // 3. Quality gates
    let gates = quality_gates::run_all(repo_path)?;
    let failed: Vec<String> = gates
        .iter()
        .filter(|g| matches!(g.status, quality_gates::GateStatus::Failed))
        .map(|g| g.name.clone())
        .collect();
    if !failed.is_empty() {
        return Ok(CommitOutcome::BlockedByGates { failures: failed });
    }

    // 4. Secret scan on dirty files
    let full_paths: Vec<std::path::PathBuf> = dirty_files.clone();
    let path_refs: Vec<&Path> = full_paths.iter().map(|p| p.as_path()).collect();
    let findings = secret_scan::scan_files(&path_refs)?;
    if secret_scan::should_block(SecretScanMode::Warn, &findings) {
        let files: Vec<String> = findings.iter().map(|f| f.path.clone()).collect();
        return Ok(CommitOutcome::BlockedBySecrets { files });
    }

    // 5. Stage and commit
    stage_all(repo_path)?;
    let oid = commit(repo_path, message)?;

    Ok(CommitOutcome::Committed {
        message: message.to_string(),
        oid,
    })
}

/// Return all dirty files (modified, new, untracked) in the repo.
fn dirty_files(repo_path: &Path) -> Result<Vec<std::path::PathBuf>> {
    use std::process::Command;
    let out = Command::new("git")
        .args(["-C", &repo_path.to_string_lossy(), "status", "--porcelain"])
        .output()?;
    if !out.status.success() {
        anyhow::bail!("git status failed");
    }
    let mut files = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if line.len() < 3 {
            continue;
        }
        // " M path/to/file" or "?? path/to/file"
        let path_str = &line[3..];
        let path = repo_path.join(path_str);
        if path.exists() && path.is_file() {
            files.push(path);
        }
    }
    Ok(files)
}

fn stage_all(repo_path: &Path) -> Result<()> {
    use std::process::Command;
    let out = Command::new("git")
        .args(["-C", &repo_path.to_string_lossy(), "add", "-A"])
        .output()?;
    if !out.status.success() {
        anyhow::bail!("git add failed");
    }
    Ok(())
}

fn commit(repo_path: &Path, message: &str) -> Result<String> {
    use std::process::Command;
    let out = Command::new("git")
        .args(["-C", &repo_path.to_string_lossy(), "commit", "-m", message])
        .output()?;
    if !out.status.success() {
        anyhow::bail!("git commit failed");
    }
    let out = Command::new("git")
        .args([
            "-C",
            &repo_path.to_string_lossy(),
            "rev-parse",
            "--short",
            "HEAD",
        ])
        .output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn git_init(dir: &std::path::Path) {
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir)
            .output()
            .expect("git init");
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(dir)
            .output()
            .expect("git config");
        std::process::Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(dir)
            .output()
            .expect("git config");
    }

    #[test]
    fn nothing_to_commit_on_clean_repo() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());
        let result = sweep_commit(tmp.path(), "test message").unwrap();
        assert!(matches!(result, CommitOutcome::NothingToCommit));
    }

    #[test]
    fn commit_succeeds_with_clean_files() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());
        std::fs::write(tmp.path().join("hello.txt"), b"world\n").unwrap();
        let result = sweep_commit(tmp.path(), "add hello").unwrap();
        assert!(
            matches!(result, CommitOutcome::Committed { .. }),
            "expected Committed, got {:?}",
            result
        );
    }

    #[test]
    fn blocked_by_denylisted_file() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());
        std::fs::write(tmp.path().join(".env"), b"SECRET=foo\n").unwrap();
        let result = sweep_commit(tmp.path(), "add env").unwrap();
        assert!(
            matches!(result, CommitOutcome::BlockedByDenylist { .. }),
            "expected BlockedByDenylist, got {:?}",
            result
        );
    }
}
