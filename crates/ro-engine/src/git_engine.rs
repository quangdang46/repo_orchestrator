//! `GitEngine` — the raw backend.
//!
//! It commits whatever is in the worktree under a single message. It does
//! not read the diff, cannot split one change into several commits, and
//! cannot resolve a conflict. That is not a limitation to work around —
//! it is the reason the agent engines exist, and why this one is never the
//! default.
//!
//! It is still the right choice in exactly one situation: a repo where the
//! changes are mechanical, or where no agent is installed and the user
//! asked for a commit rather than a stall.

use std::path::Path;
use std::time::Duration;

use ro_core::FailureClass;

use crate::{Availability, CommitRecord, Engine, EngineContext, EngineKind, EngineOutcome};

/// The raw backend.
pub struct GitEngine {
    bin: String,
    default_args: Vec<String>,
}

impl GitEngine {
    pub fn new() -> Self {
        Self {
            bin: "git".to_string(),
            default_args: Vec::new(),
        }
    }
}

impl Default for GitEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine for GitEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::Git
    }

    fn bin(&self) -> &str {
        &self.bin
    }

    fn default_args(&self) -> &[String] {
        &self.default_args
    }

    /// `git` is the one binary assumed present. The probe is cheap and the
    /// answer is used to produce a *setup* error rather than a per-repo
    /// one, which is the difference between "git is not installed" and
    /// "this repository failed".
    fn availability(&self) -> Availability {
        match which(&self.bin) {
            Some(p) => Availability::Present(p),
            None => Availability::Missing,
        }
    }

    fn checkpoint(&self, ctx: &EngineContext<'_>) -> EngineOutcome {
        let root = ctx.repo_root;
        if !root.join(".git").exists() {
            return EngineOutcome::Failed {
                error: format!("{} is not a git repository", root.display()),
                class: FailureClass::MissingGit,
            };
        }

        let before = ro_git::read::head_oid(root).unwrap_or(None);

        // A clean tree is a fact, not a failure. Distinct from an engine
        // that tried and could not: a status board that merges those two
        // makes "nothing to do" look like "something broke".
        if let Err(why) = has_changes(root) {
            return EngineOutcome::Failed {
                error: why,
                class: FailureClass::MissingGit,
            };
        }
        if !ro_git::read::is_dirty(root).unwrap_or(false) {
            return EngineOutcome::NothingToCommit;
        }

        // One message for the whole worktree. Splitting is the agent's job;
        // doing it here would mean re-deriving the bucket rules that this
        // crate is removing.
        let message = match ctx.message_override {
            Some(m) if !m.trim().is_empty() => m.to_string(),
            _ => default_message(root),
        };

        if let Err(e) = ro_git::primitives::stage_all(root) {
            return EngineOutcome::Failed {
                error: format!("staging failed: {e:#}"),
                class: FailureClass::DirtyWorktree,
            };
        }
        // NB: staging failed is a dirty-tree problem the caller can clear.

        let oid = match ro_git::primitives::commit_all(root, &message) {
            Ok(oid) => oid,
            Err(e) => {
                let text = format!("{e:#}");
                return EngineOutcome::Failed {
                    error: text.clone(),
                    class: classify(&text),
                };
            }
        };

        let files = changed_files(root, before.as_deref());
        EngineOutcome::Committed {
            commits: vec![CommitRecord {
                message,
                oid,
                files,
            }],
        }
    }
}

/// One message for everything, because this engine has no basis for a
/// better one.
///
/// A timestamp is not a commit message, and a generated conventional-commit
/// subject is the thing Phase 4 removes: those rules produced plausible
/// subjects for changes that did not fit them, and a user who trusted one
/// had no reason to read the diff.
fn default_message(root: &Path) -> String {
    let branch = ro_git::read::current_branch(root)
        .ok()
        .flatten()
        .unwrap_or_else(|| "work".to_string());
    format!("wip on {branch}")
}

/// Is the worktree dirty, or could not be read?
///
/// Separate from the answer so "cannot tell" never becomes "clean".
fn has_changes(root: &Path) -> Result<(), String> {
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| format!("running git status: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git status failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Files the commit touched, relative to the repo root.
fn changed_files(root: &Path, before: Option<&str>) -> Vec<String> {
    let spec = match before {
        Some(oid) => format!("{oid}..HEAD"),
        None => "HEAD".to_string(),
    };
    let out = std::process::Command::new("git")
        .args(["diff", "--name-only", &spec])
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
        // A commit that landed with an unreadable file list is still a
        // commit. Returning empty is honest; failing here would report a
        // successful write as a failure.
        _ => Vec::new(),
    }
}

fn classify(stderr: &str) -> FailureClass {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("please tell me who you are") || lower.contains("empty ident") {
        return FailureClass::AuthError;
    }
    if lower.contains("conflict") || lower.contains("merge conflict") {
        return FailureClass::MergeConflict;
    }
    if lower.contains("could not resolve host")
        || lower.contains("connection timed out")
        || lower.contains("operation timed out")
    {
        return FailureClass::NetworkTimeout;
    }
    FailureClass::DirtyWorktree
}

/// The first `name` on `PATH`, if it is there.
fn which(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        // Executable, not merely present: a directory called `git` on PATH
        // would otherwise read as available and fail per-repo instead of
        // as a setup problem.
        if is_executable(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        for ext in [".exe", ".cmd", ".bat"] {
            let with_ext = dir.join(format!("{name}{ext}"));
            if with_ext.is_file() {
                return Some(with_ext);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(windows)]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// The deadline `EngineContext` guarantees, for documentation parity with
/// the agent engines.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
