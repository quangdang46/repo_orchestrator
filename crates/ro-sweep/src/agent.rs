//! Sweep agent implementation.
//!
//! Runs an AI-driven sweep with plan/apply cycle.
//! Quality gates + secret scan + denylist enforced before any apply.

use anyhow::Result;
use std::path::Path;

use crate::denylist::Denylist;
use crate::quality_gates;
use crate::secret_scan::{self};
use ro_config::CheckpointConfig;
use serde::{Deserialize, Serialize};

/// Summary of an agent sweep attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepSummary {
    pub repo_id: String,
    pub plan_created: bool,
    /// `None` when the gates were **not run** — which is the default.
    ///
    /// A `bool` here would have to say `true` for "skipped", and "skipped"
    /// reported as "passed" is the same unknown-versus-false confusion
    /// ro-dab.4 removed one layer up: a user reading this field would
    /// conclude the gates cleared a tree they never touched.
    pub gates_passed: Option<bool>,
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
pub fn sweep_repo(
    repo_path: &Path,
    repo_id: &str,
    checkpoint: &CheckpointConfig,
) -> Result<SweepSummary> {
    let mut summary = SweepSummary {
        repo_id: repo_id.to_string(),
        plan_created: false,
        gates_passed: None,
        secrets_clean: false,
        denylist_clean: false,
        applied: false,
        error: None,
    };

    // Quality gates, opt-in. `run_all` invokes `cargo test --workspace`
    // over the whole tree, so leaving it on by default means one
    // pre-existing failure in an untouched crate makes every repo look
    // unsweepable — across a fleet, that is not a gate, it is an outage.
    if checkpoint.quality_gates_enabled() {
        let gates = quality_gates::run_all(repo_path)?;
        summary.gates_passed = Some(!quality_gates::any_failed(&gates));
    }

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

    // `None` is a skip, not a failure: an unrun gate must not block, or
    // turning the gates off would block everything instead.
    let gates_ok = summary.gates_passed.unwrap_or(true);
    if gates_ok && summary.secrets_clean && summary.denylist_clean {
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

    /// Quality gates explicitly on. They invoke `cargo test --workspace`,
    /// so a test that wants them has to say so.
    fn gates_on() -> CheckpointConfig {
        CheckpointConfig {
            quality_gates: "on".to_string(),
            ..Default::default()
        }
    }

    /// The default is off, and that is a real answer rather than a pass.
    ///
    /// `gates_passed` is `Option<bool>` precisely so "not run" is
    /// distinguishable from "ran and passed". With the gates on by default
    /// this would have been `None` on a fresh repo and `Some(true)` on one
    /// whose tests happen to pass — and a reader could not tell those apart.
    #[test]
    fn the_gates_are_off_by_default_and_say_so() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());
        let summary = sweep_repo(tmp.path(), "test-repo", &CheckpointConfig::default()).unwrap();
        assert_eq!(
            summary.gates_passed, None,
            "an unrun gate must report None, never Some(true)"
        );
    }

    /// And a skipped gate must not block the sweep. If it did, turning the
    /// gates off would block every repo instead of running fewer checks.
    #[test]
    fn a_skipped_gate_does_not_block() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());
        let summary = sweep_repo(tmp.path(), "test-repo", &CheckpointConfig::default()).unwrap();
        assert!(
            summary.plan_created,
            "nothing should block when the gates are off"
        );
    }

    #[test]
    fn sweep_repo_empty_repo_passes_gates() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());
        let summary = sweep_repo(tmp.path(), "test-repo", &gates_on()).unwrap();
        assert_eq!(summary.gates_passed, Some(true));
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
        let summary = sweep_repo(tmp.path(), "test-repo", &CheckpointConfig::default()).unwrap();
        assert!(!summary.secrets_clean, "expected secrets_clean=false");
    }

    #[test]
    fn sweep_repo_with_denylisted_file_fails_denylist() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());
        std::fs::write(tmp.path().join(".env"), b"SECRET=foo\n").unwrap();
        let summary = sweep_repo(tmp.path(), "test-repo", &CheckpointConfig::default()).unwrap();
        assert!(!summary.denylist_clean, "expected denylist_clean=false");
    }
}
