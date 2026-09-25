//! Conflict detection, explanation, and abort for git operations.
//!
//! Detects active merge/rebase/cherry-pick/revert by checking for
//! MERGE_HEAD, REBASE_HEAD, CHERRY_PICK_HEAD, REVERT_HEAD in .git/.
//! Lists conflicted files from the index and explains safe options.
//! Abort safely resets the repo to the pre-operation state.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The type of in-progress operation that caused conflicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConflictOp {
    Merge,
    Rebase,
    CherryPick,
    Revert,
}

impl std::fmt::Display for ConflictOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Merge => "merge",
            Self::Rebase => "rebase",
            Self::CherryPick => "cherry-pick",
            Self::Revert => "revert",
        })
    }
}

/// A single conflicted file with its index stage info.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConflictedFile {
    /// Relative path from repo root.
    pub path: String,
    /// True if the file contains conflict markers (<<<<<<< ======= >>>>>>>).
    pub has_markers: bool,
    /// Index stages present (1=base, 2=ours, 3=theirs). Empty if deleted.
    pub stages: Vec<u8>,
}

/// Full conflict state for a repository.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConflictState {
    /// The operation in progress.
    pub op: ConflictOp,
    /// The reference that was being merged/rebased onto (if known).
    pub head_ref: Option<String>,
    /// All conflicted files.
    pub files: Vec<ConflictedFile>,
}

impl ConflictState {
    /// True if there are no conflicted files (conflict may be resolved).
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Number of conflicted files.
    pub fn len(&self) -> usize {
        self.files.len()
    }
}

/// Detect whether `repo_path` has an in-progress operation with conflicts.
///
/// Returns `Ok(Some(state))` if a conflict is active, `Ok(None)` if the
/// repo is clean (no MERGE_HEAD etc.), or `Err` if the path is not a git repo.
pub fn detect(repo_path: &Path) -> Result<Option<ConflictState>> {
    let git_dir = find_git_dir(repo_path)?;
    let op = detect_op(&git_dir)?;
    let Some(op) = op else {
        return Ok(None);
    };
    let head_ref = read_head_ref(repo_path, &git_dir, op);
    let files = list_conflicted_files(repo_path)?;
    Ok(Some(ConflictState {
        op,
        head_ref,
        files,
    }))
}

/// List all repos (from a given list) that are currently in conflict.
///
/// For each repo path in `repos`, runs `detect` and returns the subset
/// that have active conflicts.
pub fn list_conflicts(repos: &[PathBuf]) -> Vec<(PathBuf, ConflictState)> {
    repos
        .iter()
        .filter_map(|p| match detect(p) {
            Ok(Some(state)) => Some((p.clone(), state)),
            _ => None,
        })
        .collect()
}

/// Explain the conflict for a single repo.
///
/// Returns a human-readable summary string.
pub fn explain(state: &ConflictState) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Conflict from in-progress {} ({} file(s))\n",
        state.op,
        state.files.len()
    ));
    if let Some(ref r) = state.head_ref {
        out.push_str(&format!("  HEAD: {r}\n"));
    }
    out.push_str("  Conflicted files:\n");
    for f in &state.files {
        let marker_hint = if f.has_markers {
            " [has conflict markers]"
        } else {
            ""
        };
        out.push_str(&format!("    - {}{marker_hint}\n", f.path));
    }
    out.push_str("  Safe options:\n");
    out.push_str(&format!(
        "    1. Resolve manually, then `git add` + `git {} --continue`\n",
        state.op
    ));
    out.push_str(&format!(
        "    2. Abort: `ro conflict abort <repo>` (runs `git {} --abort`)\n",
        state.op
    ));
    out
}

/// Safely abort the in-progress operation.
///
/// Runs `git <op> --abort` and verifies the state is clean afterward.
pub fn abort(repo_path: &Path) -> Result<()> {
    let git_dir = find_git_dir(repo_path)?;
    let op = detect_op(&git_dir)?.context("no in-progress operation to abort")?;
    let flag = match op {
        ConflictOp::Merge => "--abort",
        ConflictOp::Rebase => "--abort",
        ConflictOp::CherryPick => "--abort",
        ConflictOp::Revert => "--abort",
    };
    let output = std::process::Command::new("git")
        .arg(op.to_string())
        .arg(flag)
        .current_dir(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .with_context(|| format!("running git {op} --abort in {}", repo_path.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git {op} --abort failed: {stderr}");
    }
    // Verify cleanup
    if detect(repo_path)?.is_some() {
        tracing::warn!(
            "git {} --abort completed but conflict state still detected in {}",
            op,
            repo_path.display()
        );
    }
    Ok(())
}

/// Finish (commit) an in-progress merge/rebase after conflict resolution.
///
/// Runs `git <op> --continue` with a no-op editor to complete the operation.
pub fn finish(repo_path: &Path) -> Result<()> {
    let git_dir = find_git_dir(repo_path)?;
    let op = detect_op(&git_dir)?.context("no in-progress operation to finish")?;
    let output = std::process::Command::new("git")
        .arg(op.to_string())
        .arg("--continue")
        .current_dir(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .env("GIT_EDITOR", "true")
        .output()
        .with_context(|| format!("running git {} --continue in {}", op, repo_path.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git {} --continue failed: {stderr}", op);
    }
    // Verify cleanup
    if detect(repo_path)?.is_some() {
        tracing::warn!(
            "git {} --continue completed but conflict state still detected in {}",
            op,
            repo_path.display()
        );
    }
    Ok(())
}

/// Verifies that a previously-conflicted repo is now resolved.
///
/// Returns `Ok(())` if:
///
/// - no `MERGE_HEAD` / `REBASE_HEAD` / `CHERRY_PICK_HEAD` / `REVERT_HEAD` exists
///   (i.e. the in-progress operation finished), AND
/// - there are no unmerged index entries (`git diff --name-only --diff-filter=U`
///   is empty), AND
/// - no tracked file contains conflict markers (`<<<<<<<`).
///
/// Otherwise returns a `MarkResolvedError` describing what's still wrong, so
/// `ro conflict mark-resolved <repo>` can surface a precise message instead
/// of silently claiming success.
pub fn verify_resolved(repo_path: &Path) -> Result<(), MarkResolvedError> {
    let git_dir = find_git_dir(repo_path).map_err(MarkResolvedError::NotARepo)?;

    // Check conflict markers FIRST (user hasn't resolved yet)
    let with_markers =
        files_with_conflict_markers(repo_path).map_err(MarkResolvedError::IndexQueryFailed)?;
    if !with_markers.is_empty() {
        return Err(MarkResolvedError::ConflictMarkersRemain(with_markers));
    }

    // Check unmerged entries SECOND (user hasn't resolved or staged yet)
    let unmerged = list_conflicted_files(repo_path).map_err(MarkResolvedError::IndexQueryFailed)?;
    if !unmerged.is_empty() {
        return Err(MarkResolvedError::UnmergedEntries(
            unmerged.into_iter().map(|f| f.path).collect(),
        ));
    }

    // Operation still in progress LAST — files are clean, only need to --continue
    if let Some(op) = detect_op(&git_dir).map_err(MarkResolvedError::NotARepo)? {
        return Err(MarkResolvedError::OperationStillInProgress(op));
    }

    Ok(())
}

/// Why `verify_resolved` rejected a repo.
#[derive(Debug)]
pub enum MarkResolvedError {
    NotARepo(anyhow::Error),
    IndexQueryFailed(anyhow::Error),
    OperationStillInProgress(ConflictOp),
    UnmergedEntries(Vec<String>),
    ConflictMarkersRemain(Vec<String>),
}

impl std::fmt::Display for MarkResolvedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotARepo(e) => write!(f, "{e}"),
            Self::IndexQueryFailed(e) => write!(f, "querying git index failed: {e}"),
            Self::OperationStillInProgress(op) => write!(
                f,
                "{op} still in progress — run `git {op} --continue` or `ro conflict abort <repo>`"
            ),
            Self::UnmergedEntries(paths) => write!(
                f,
                "{} unmerged index entr{} remaining: {}",
                paths.len(),
                if paths.len() == 1 { "y" } else { "ies" },
                paths.join(", ")
            ),
            Self::ConflictMarkersRemain(paths) => write!(
                f,
                "conflict markers (`<<<<<<<`) still present in: {}",
                paths.join(", ")
            ),
        }
    }
}

impl std::error::Error for MarkResolvedError {}

/// List tracked files in the repo that still contain `<<<<<<<` conflict markers.
fn files_with_conflict_markers(repo_path: &Path) -> Result<Vec<String>> {
    let output = std::process::Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .context("running git ls-files")?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for raw in output.stdout.split(|&b| b == 0) {
        if raw.is_empty() {
            continue;
        }
        let name = String::from_utf8_lossy(raw).into_owned();
        let full = repo_path.join(&name);
        if file_has_conflict_markers(&full) {
            out.push(name);
        }
    }
    Ok(out)
}

// --- internals ---

fn find_git_dir(repo_path: &Path) -> Result<PathBuf> {
    let repo = gix::discover(repo_path)
        .with_context(|| format!("not a git repo: {}", repo_path.display()))?;
    Ok(repo.path().to_path_buf())
}

fn detect_op(git_dir: &Path) -> Result<Option<ConflictOp>> {
    if git_dir.join("MERGE_HEAD").exists() {
        return Ok(Some(ConflictOp::Merge));
    }
    if git_dir.join("REBASE_HEAD").exists() {
        return Ok(Some(ConflictOp::Rebase));
    }
    if git_dir.join("CHERRY_PICK_HEAD").exists() {
        return Ok(Some(ConflictOp::CherryPick));
    }
    if git_dir.join("REVERT_HEAD").exists() {
        return Ok(Some(ConflictOp::Revert));
    }
    Ok(None)
}

fn read_head_ref(_repo_path: &Path, git_dir: &Path, op: ConflictOp) -> Option<String> {
    match op {
        ConflictOp::Merge => {
            // MERGE_HEAD contains the OID being merged
            std::fs::read_to_string(git_dir.join("MERGE_HEAD"))
                .ok()
                .map(|s| s.trim().to_string())
        }
        ConflictOp::Rebase => {
            // REBASE_HEAD contains the OID being rebased
            std::fs::read_to_string(git_dir.join("REBASE_HEAD"))
                .ok()
                .map(|s| s.trim().to_string())
        }
        ConflictOp::CherryPick => std::fs::read_to_string(git_dir.join("CHERRY_PICK_HEAD"))
            .ok()
            .map(|s| s.trim().to_string()),
        ConflictOp::Revert => std::fs::read_to_string(git_dir.join("REVERT_HEAD"))
            .ok()
            .map(|s| s.trim().to_string()),
    }
}

fn list_conflicted_files(repo_path: &Path) -> Result<Vec<ConflictedFile>> {
    let output = std::process::Command::new("git")
        .args(["diff", "--name-only", "--diff-filter=U"])
        .current_dir(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .context("running git diff --name-only --diff-filter=U")?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let names = String::from_utf8_lossy(&output.stdout);
    let mut files = Vec::new();
    for name in names.lines() {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let full = repo_path.join(name);
        let has_markers = file_has_conflict_markers(&full);
        files.push(ConflictedFile {
            path: name.to_string(),
            has_markers,
            stages: vec![],
        });
    }
    Ok(files)
}

fn file_has_conflict_markers(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    // Quick binary check
    if bytes.iter().take(512).any(|&b| b == 0) {
        return false;
    }
    let text = String::from_utf8_lossy(&bytes);
    // Require the full marker triple at line starts. This avoids flagging
    // documentation, test fixtures, or source code that happens to mention
    // `<<<<<<<` as a string literal — only real, unresolved git conflicts
    // produce all three markers anchored at column 0 in the same file.
    let mut saw_start = false;
    let mut saw_sep_after_start = false;
    for line in text.lines() {
        if line.starts_with("<<<<<<<") || line == "<<<<<<<" {
            saw_start = true;
            saw_sep_after_start = false;
        } else if saw_start && (line.starts_with("=======") || line == "=======") {
            saw_sep_after_start = true;
        } else if saw_sep_after_start && (line.starts_with(">>>>>>>") || line == ">>>>>>>") {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn init_repo(tmp: &TempDir) -> PathBuf {
        let p = tmp.path().to_path_buf();
        run_git(&p, &["init", "-q", "-b", "main"]);
        run_git(&p, &["config", "user.email", "test@example.com"]);
        run_git(&p, &["config", "user.name", "Test"]);
        p
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
    fn detect_clean_repo_returns_none() {
        let tmp = TempDir::new().unwrap();
        let p = init_repo(&tmp);
        assert!(detect(&p).unwrap().is_none());
    }

    #[test]
    fn detect_merge_conflict() {
        let tmp = TempDir::new().unwrap();
        let p = init_repo(&tmp);
        commit(&p, "a.txt", "base");
        run_git(&p, &["checkout", "-q", "-b", "feature"]);
        commit(&p, "a.txt", "feature change");
        run_git(&p, &["checkout", "-q", "main"]);
        commit(&p, "a.txt", "main change");
        // This will conflict
        let result = std::process::Command::new("git")
            .args(["merge", "feature"])
            .current_dir(&p)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(!result.status.success()); // merge should fail with conflict

        let state = detect(&p).unwrap().expect("should detect merge conflict");
        assert_eq!(state.op, ConflictOp::Merge);
        assert!(!state.files.is_empty());
        assert!(state.files.iter().any(|f| f.path == "a.txt"));
    }

    #[test]
    fn explain_formats_nicely() {
        let state = ConflictState {
            op: ConflictOp::Merge,
            head_ref: Some("abc123".into()),
            files: vec![
                ConflictedFile {
                    path: "src/main.rs".into(),
                    has_markers: true,
                    stages: vec![],
                },
                ConflictedFile {
                    path: "Cargo.toml".into(),
                    has_markers: false,
                    stages: vec![],
                },
            ],
        };
        let text = explain(&state);
        assert!(text.contains("merge"));
        assert!(text.contains("src/main.rs"));
        assert!(text.contains("Cargo.toml"));
        assert!(text.contains("conflict markers"));
        assert!(text.contains("abort"));
    }

    #[test]
    fn abort_merge() {
        let tmp = TempDir::new().unwrap();
        let p = init_repo(&tmp);
        commit(&p, "a.txt", "base");
        run_git(&p, &["checkout", "-q", "-b", "feature"]);
        commit(&p, "a.txt", "feature change");
        run_git(&p, &["checkout", "-q", "main"]);
        commit(&p, "a.txt", "main change");
        let _ = std::process::Command::new("git")
            .args(["merge", "feature"])
            .current_dir(&p)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(detect(&p).unwrap().is_some());
        abort(&p).unwrap();
        assert!(detect(&p).unwrap().is_none());
    }

    #[test]
    fn list_conflicts_filters_clean_repos() {
        let tmp1 = TempDir::new().unwrap();
        let p1 = init_repo(&tmp1);
        commit(&p1, "a.txt", "hello");

        let tmp2 = TempDir::new().unwrap();
        let p2 = init_repo(&tmp2);
        commit(&p2, "a.txt", "base");
        run_git(&p2, &["checkout", "-q", "-b", "feature"]);
        commit(&p2, "a.txt", "feature change");
        run_git(&p2, &["checkout", "-q", "main"]);
        commit(&p2, "a.txt", "main change");
        let _ = std::process::Command::new("git")
            .args(["merge", "feature"])
            .current_dir(&p2)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();

        let conflicts = list_conflicts(&[p1.clone(), p2.clone()]);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].0, p2);
    }

    #[test]
    fn conflict_op_display() {
        assert_eq!(ConflictOp::Merge.to_string(), "merge");
        assert_eq!(ConflictOp::Rebase.to_string(), "rebase");
        assert_eq!(ConflictOp::CherryPick.to_string(), "cherry-pick");
        assert_eq!(ConflictOp::Revert.to_string(), "revert");
    }

    #[test]
    fn verify_resolved_accepts_clean_repo() {
        let tmp = TempDir::new().unwrap();
        let p = init_repo(&tmp);
        commit(&p, "a.txt", "hello");
        verify_resolved(&p).expect("clean repo should verify");
    }

    #[test]
    fn verify_resolved_rejects_repo_with_active_merge() {
        let tmp = TempDir::new().unwrap();
        let p = init_repo(&tmp);
        commit(&p, "a.txt", "base");
        run_git(&p, &["checkout", "-q", "-b", "feature"]);
        commit(&p, "a.txt", "feature change");
        run_git(&p, &["checkout", "-q", "main"]);
        commit(&p, "a.txt", "main change");
        let _ = std::process::Command::new("git")
            .args(["merge", "feature"])
            .current_dir(&p)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        let err = verify_resolved(&p).expect_err("should reject");
        let msg = err.to_string();
        // Either the operation flag is still around, or the index has unmerged
        // entries — both are valid pre-resolution states.
        // With the new check order (markers → unmerged → op), conflict markers
        // are detected first — any of these is a valid pre-resolution state.
        assert!(
            msg.contains("merge") || msg.contains("unmerged") || msg.contains("conflict markers"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn verify_resolved_rejects_lingering_conflict_markers() {
        let tmp = TempDir::new().unwrap();
        let p = init_repo(&tmp);
        commit(
            &p,
            "a.txt",
            "ok\n<<<<<<< HEAD\nmine\n=======\ntheirs\n>>>>>>> feature\n",
        );
        let err = verify_resolved(&p).expect_err("should reject");
        assert!(
            err.to_string().contains("conflict markers"),
            "unexpected error: {err}"
        );
    }
}
