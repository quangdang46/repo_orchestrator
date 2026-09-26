//! Sweep commit implementation.
//!
//! Stages changes, runs secret scan + denylist check, quality gates, then commits.

use anyhow::Result;
use std::path::Path;

use crate::denylist::Denylist;
use crate::quality_gates;
use crate::secret_scan::{self, SecretScanMode};
use ro_config::CheckpointConfig;
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
/// 3. Run quality gates — only when `[checkpoint] quality_gates = "on"`.
/// 4. Secret scan on dirty files, at the configured mode.
/// 5. Stage all and commit.
///
/// The settings come from `[checkpoint]` rather than being baked in. The
/// preflight used to call `should_block(SecretScanMode::Warn, …)`, which
/// is **always false** — `should_block` only returns true for `Block` — so
/// `BlockedBySecrets` was unreachable and a file holding a live-looking
/// credential was committed anyway. The code that would have stopped it
/// existed; nothing could reach it.
pub fn sweep_commit(
    repo_path: &Path,
    message: &str,
    checkpoint: &CheckpointConfig,
) -> Result<CommitOutcome> {
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

    // 3. Quality gates, opt-in.
    //
    // Off by default because `run_all` invokes `cargo test --workspace`
    // over the whole tree: a pre-existing failure in an untouched crate
    // would block a one-file commit, and across a fleet that is the
    // dominant cost of the run. A gate that fires on things the user did
    // not touch trains people to override it, and the override disables
    // the gates that do matter.
    if checkpoint.quality_gates_enabled() {
        let gates = quality_gates::run_all(repo_path)?;
        let failed: Vec<String> = gates
            .iter()
            .filter(|g| matches!(g.status, quality_gates::GateStatus::Failed))
            .map(|g| g.name.clone())
            .collect();
        if !failed.is_empty() {
            return Ok(CommitOutcome::BlockedByGates { failures: failed });
        }
    }

    // 4. Secret scan on dirty files, at the configured mode.
    //
    // `off` skips the scan entirely; `warn` reports without blocking.
    // Reporting matters as much as blocking: a silently skipped `.env`
    // teaches the user that ro committed everything, which is the belief
    // that gets a real secret committed next.
    let mode = checkpoint.secret_scan_mode();
    if mode != "off" {
        let full_paths: Vec<std::path::PathBuf> = dirty_files.clone();
        let path_refs: Vec<&Path> = full_paths.iter().map(|p| p.as_path()).collect();
        let findings = secret_scan::scan_files(&path_refs)?;
        let scan_mode = match mode {
            "warn" => SecretScanMode::Warn,
            _ => SecretScanMode::Block,
        };
        if secret_scan::should_block(scan_mode, &findings) {
            let files: Vec<String> = findings
                .iter()
                .map(|f| {
                    // Redacted: the finding matched on the *shape* of a
                    // secret, so echoing it verbatim would repeat the
                    // leak in the very message reporting it.
                    format!("{} ({})", f.path, f.rule)
                })
                .collect();
            return Ok(CommitOutcome::BlockedBySecrets { files });
        }
        if mode == "warn" && !findings.is_empty() {
            for f in &findings {
                eprintln!("warning: possible secret in {} ({})", f.path, f.rule);
            }
        }
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
        let result =
            sweep_commit(tmp.path(), "test message", &CheckpointConfig::default()).unwrap();
        assert!(matches!(result, CommitOutcome::NothingToCommit));
    }

    #[test]
    fn commit_succeeds_with_clean_files() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());
        std::fs::write(tmp.path().join("hello.txt"), b"world\n").unwrap();
        let result = sweep_commit(tmp.path(), "add hello", &CheckpointConfig::default()).unwrap();
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
        let result = sweep_commit(tmp.path(), "add env", &CheckpointConfig::default()).unwrap();
        assert!(
            matches!(result, CommitOutcome::BlockedByDenylist { .. }),
            "expected BlockedByDenylist, got {:?}",
            result
        );
    }

    /// The bug the bead is named for.
    ///
    /// The preflight called `should_block(SecretScanMode::Warn, …)`, and
    /// `should_block` returns true only for `Block` — so it was
    /// **permanently false**, `BlockedBySecrets` was unreachable, and a
    /// repo holding a live-looking credential was committed. The code
    /// that would have stopped it existed; nothing could reach it.
    #[test]
    fn a_pat_shaped_file_blocks_the_commit_by_default() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());
        std::fs::write(
            tmp.path().join("config.py"),
            b"token = 'ghp_1234567890abcdef1234567890abcdef1234'\n",
        )
        .unwrap();

        let result = sweep_commit(tmp.path(), "add config", &CheckpointConfig::default()).unwrap();
        assert!(
            matches!(result, CommitOutcome::BlockedBySecrets { .. }),
            "a PAT-shaped file must block by default, got {:?}",
            result
        );
    }

    /// The negative control. Without it, the test above would also pass on
    /// a scanner that finds nothing at all — which is the other way this
    /// bug could have hidden.
    #[test]
    fn the_secret_scan_actually_finds_the_pat() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("config.py"),
            b"token = 'ghp_1234567890abcdef1234567890abcdef1234'\n",
        )
        .unwrap();
        let findings = secret_scan::scan_files(&[tmp.path().join("config.py").as_path()]).unwrap();
        assert!(
            !findings.is_empty(),
            "the scanner must see a PAT; a scan that finds nothing would make              the blocking test above pass for the wrong reason"
        );
    }

    /// `warn` reports without blocking, and says which file.
    ///
    /// Reporting is the half that matters for trust: a silently skipped
    /// `.env` teaches the user that ro committed everything, which is the
    /// belief that gets a real secret committed next.
    #[test]
    fn warn_mode_reports_but_does_not_block() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());
        std::fs::write(
            tmp.path().join("config.py"),
            b"token = 'ghp_1234567890abcdef1234567890abcdef1234'\n",
        )
        .unwrap();

        let warn = CheckpointConfig {
            secret_scan: "warn".to_string(),
            ..Default::default()
        };
        let result = sweep_commit(tmp.path(), "add config", &warn).unwrap();
        assert!(
            matches!(result, CommitOutcome::Committed { .. }),
            "warn mode must not block, got {:?}",
            result
        );
    }

    /// `off` skips the scan entirely — a different thing from "found
    /// nothing".
    #[test]
    fn off_mode_does_not_scan_at_all() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());
        std::fs::write(
            tmp.path().join("config.py"),
            b"token = 'ghp_1234567890abcdef1234567890abcdef1234'\n",
        )
        .unwrap();

        let off = CheckpointConfig {
            secret_scan: "off".to_string(),
            ..Default::default()
        };
        let result = sweep_commit(tmp.path(), "add config", &off).unwrap();
        assert!(
            matches!(result, CommitOutcome::Committed { .. }),
            "off mode must skip the scan, got {:?}",
            result
        );
    }

    /// A blocked secret must not be echoed in the message.
    ///
    /// The finding matched on the *shape* of a secret, so printing the
    /// matched text repeats the leak in the very message reporting it.
    #[test]
    fn a_blocked_secret_is_reported_without_its_value() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());
        let token = "ghp_1234567890abcdef1234567890abcdef1234";
        std::fs::write(tmp.path().join("config.py"), format!("token = '{token}'\n")).unwrap();

        let result = sweep_commit(tmp.path(), "add config", &CheckpointConfig::default()).unwrap();
        let CommitOutcome::BlockedBySecrets { files } = result else {
            panic!("expected a block");
        };
        let rendered = files.join(" ");
        assert!(
            !rendered.contains(token),
            "the message must not echo the secret it found: {rendered}"
        );
        assert!(
            rendered.contains("config.py"),
            "it must still name the file: {rendered}"
        );
    }

    /// Quality gates are off by default, and a repo that would fail them
    /// still commits.
    ///
    /// `run_all` invokes `cargo test --workspace` over the whole tree, so
    /// a pre-existing failure in an untouched crate would block a
    /// one-file commit — and across a fleet that is the dominant cost of
    /// the run.
    #[test]
    fn quality_gates_are_off_by_default() {
        let tmp = TempDir::new().unwrap();
        git_init(tmp.path());
        std::fs::write(tmp.path().join("hello.txt"), b"world\n").unwrap();
        assert_eq!(
            CheckpointConfig::default().quality_gates,
            "off",
            "the default must be off, and validated as off|on"
        );
        // And the commit proceeds rather than being blocked by a gate that
        // was never meant to run.
        let result = sweep_commit(tmp.path(), "add hello", &CheckpointConfig::default()).unwrap();
        assert!(matches!(result, CommitOutcome::Committed { .. }));
    }
}
