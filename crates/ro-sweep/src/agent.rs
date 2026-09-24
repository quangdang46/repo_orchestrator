//! Sweep agent implementation.
//!
//! Runs an AI-driven sweep with plan/apply cycle.
//! Quality gates + secret scan + denylist enforced before any apply.

use anyhow::Result;
use std::path::Path;

use crate::denylist::Denylist;
use crate::quality_gates;
use crate::secret_scan::{self};
use serde::{Deserialize, Serialize};

/// Summary of an agent sweep attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepSummary {
    pub repo_id: String,
    pub plan_created: bool,
    pub gates_passed: bool,
    pub secrets_clean: bool,
    pub denylist_clean: bool,
    pub applied: bool,
    pub error: Option<String>,
}

/// Run a sweep agent on a single repo.
///
/// 1. Detect quality gates and run them.
/// 2. Scan all files in the repo for secrets.
/// 3. Check denylist on all files.
/// 4. If all pass, mark as sweepable.
pub fn sweep_repo(repo_path: &Path, repo_id: &str) -> Result<SweepSummary> {
    let mut summary = SweepSummary {
        repo_id: repo_id.to_string(),
        plan_created: false,
        gates_passed: false,
        secrets_clean: false,
        denylist_clean: false,
        applied: false,
        error: None,
    };

    // Run quality gates
    let gates = quality_gates::run_all(repo_path)?;
    summary.gates_passed = !quality_gates::any_failed(&gates);

    // Collect all files (tracked + untracked)
    let all_files = collect_all_files(repo_path)?;

    // Secret scan on all files
    let paths_refs: Vec<&Path> = all_files.iter().map(|p| p.as_path()).collect();
    let findings = secret_scan::scan_files(&paths_refs)?;
    summary.secrets_clean = findings.is_empty();

    // Denylist check
    let denylist = Denylist::new_default()?;
    let violations: Vec<String> = all_files
        .iter()
        .filter_map(|p| {
            if let Ok(rel) = p.strip_prefix(repo_path) {
                if denylist.is_denied(rel.to_string_lossy().as_ref()) {
                    return Some(rel.to_string_lossy().into_owned());
                }
            }
            None
        })
        .collect();
    summary.denylist_clean = violations.is_empty();

    if summary.gates_passed && summary.secrets_clean && summary.denylist_clean {
        summary.plan_created = true;
    }

    Ok(summary)
}

fn collect_all_files(repo_path: &Path) -> Result<Vec<std::path::PathBuf>> {
    use std::process::Command;
    // Try git ls-files first for tracked files
    let out = Command::new("git")
        .args(["-C", &repo_path.to_string_lossy(), "ls-files"])
        .output()?;
    let mut files = Vec::new();
    if out.status.success() {
        let tracked: Vec<std::path::PathBuf> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| repo_path.join(l))
            .collect();
        files.extend(tracked);
    }
    // Also include untracked files that exist on disk
    let out = Command::new("git")
        .args([
            "-C",
            &repo_path.to_string_lossy(),
            "ls-files",
            "--others",
            "--exclude-standard",
        ])
        .output()?;
    if out.status.success() {
        let untracked: Vec<std::path::PathBuf> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| repo_path.join(l))
            .collect();
        files.extend(untracked);
    }
    // Fallback: if git failed or no files, scan the directory directly
    if files.is_empty() {
        for entry in std::fs::read_dir(repo_path)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                files.push(path);
            }
        }
    }
    Ok(files)
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
            .expect("git config email");
        std::process::Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(dir)
            .output()
            .expect("git config name");
    }

    #[test]
    fn sweep_repo_empty_repo_passes_gates() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());
        let summary = sweep_repo(tmp.path(), "test-repo").unwrap();
        assert!(summary.gates_passed);
        assert!(summary.secrets_clean);
        assert!(summary.denylist_clean);
        assert!(summary.plan_created);
    }

    #[test]
    fn sweep_repo_with_secret_fails_secret_check() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());
        // Write a file with a pattern the secret scanner should catch
        std::fs::write(
            tmp.path().join("config.py"),
            b"api_key = 'ghp_1234567890abcdef1234567890abcdef1234'\n",
        )
        .unwrap();
        let summary = sweep_repo(tmp.path(), "test-repo").unwrap();
        assert!(!summary.secrets_clean, "expected secrets_clean=false");
    }

    #[test]
    fn sweep_repo_with_denylisted_file_fails_denylist() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());
        std::fs::write(tmp.path().join(".env"), b"SECRET=foo\n").unwrap();
        let summary = sweep_repo(tmp.path(), "test-repo").unwrap();
        assert!(!summary.denylist_clean, "expected denylist_clean=false");
    }
}
