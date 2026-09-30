//! Conflict detection, explanation, and abort for git operations.
//!
//! Detects active merge/rebase/cherry-pick/revert by checking for
//! MERGE_HEAD, REBASE_HEAD, CHERRY_PICK_HEAD, REVERT_HEAD in .git/,
//! **and** the case those four markers cannot see: an index that still
//! holds unmerged entries with no operation in progress — what a
//! conflicting `git pull --autostash` leaves behind.
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
    /// The index holds unmerged entries and **no operation marker names
    /// the cause** — [`ConflictOp::Merge`] through [`ConflictOp::Revert`]
    /// are all absent, so the only evidence is the index.
    ///
    /// The motivating case is `git pull --autostash`: the pull
    /// fast-forwards, the pop of the stashed work conflicts, git exits
    /// **0**, and it leaves none of the four markers behind. `git stash
    /// apply` and `git merge --squash` conflict the same way. There was
    /// no decision here to call it the *cause* — only a state to record.
    ///
    /// It is a variant rather than a bare `bool` on [`ConflictState`]
    /// because `ro status` renders the op, and a status that can only
    /// say "conflict" sends the user to look for a merge that is not
    /// running. Verified against git 2.53: `MERGE_HEAD`,
    /// `REBASE_HEAD`, `CHERRY_PICK_HEAD` and `REVERT_HEAD` are all
    /// absent, `git diff --name-only --diff-filter=U` names the file,
    /// and the stash is still held.
    StashPop,
}

impl std::fmt::Display for ConflictOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Merge => "merge",
            Self::Rebase => "rebase",
            Self::CherryPick => "cherry-pick",
            Self::Revert => "revert",
            // Not a real `git` subcommand, so it is never handed to one
            // as a verb — `Display` is a noun here, not a command.
            Self::StashPop => "stash pop",
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
    /// Conflicted files, with the worktree conflict markers noted.
    ///
    /// Empty is meaningful and is not the same as "not in conflict": a
    /// delete/delete conflict leaves an unmerged index entry and no
    /// file on disk for it to hold markers in. See [`Self::unmerged`].
    pub files: Vec<ConflictedFile>,
    /// Every unmerged index entry, whatever the conflict was.
    ///
    /// A superset of [`Self::files`]: `files` is what the *worktree*
    /// shows, this is what the *index* holds. They differ whenever
    /// there is nothing left on disk to hold markers — `DD`, and a
    /// `UU` the user resolved in place without staging. Both are
    /// still a repo a human has to look at, so both are reported.
    pub unmerged: Vec<String>,
}

impl ConflictState {
    /// True if there are no conflicted files (conflict may be resolved).
    ///
    /// Both halves, not just [`Self::files`]. The ship pipeline uses
    /// this to recognise a *stale* marker — a `REBASE_HEAD` left behind
    /// by a successful `--continue` — and treats an empty state as
    /// "resolved". A conflict whose file was resolved in place but not
    /// staged is not resolved; the index still says the file has two
    /// versions and nothing may be built on it until someone decides.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.unmerged.is_empty()
    }

    /// Number of conflicted files.
    pub fn len(&self) -> usize {
        self.files.len()
    }
}

/// Detect whether `repo_path` has an in-progress operation with conflicts.
///
/// Returns `Ok(Some(state))` if a conflict is active, `Ok(None)` if the
/// repo is clean, or `Err` if the path is not a git repo.
///
/// # Why the index is consulted, not only the markers
///
/// The four operation markers are a correct list for the four
/// operations that write them. They are not a correct list for
/// "conflicted", because a conflict does not always come from an
/// operation that is still running:
///
/// ```text
/// $ git pull --autostash          # fast-forwards, pops the stash
/// Applying autostash resulted in conflicts.
/// $ echo $?                       # 0  — git calls this a success
/// $ git status --porcelain
/// UU shared.txt
/// ```
///
/// The pull was a fast-forward, so there is no merge to finish and
/// nothing wrote `MERGE_HEAD`. The evidence is the index, and the
/// index is what this reads. `git stash apply` and `git merge
/// --squash` conflict the same way, so the signal is the state rather
/// than the one command that produces it most often.
///
/// # What this changes for callers
///
/// Every caller asks the same question — *is this worktree in a state
/// ro must not touch?* — and an unmerged index is that state whatever
/// produced it. So this widens deliberately rather than adding a
/// second predicate that callers have to remember to also ask: a
/// narrower `detect` plus a separate query is two answers to one
/// question, and a caller that consults only one of them is the bug
/// this was written to remove.
///
/// One caller needs to know the difference, and it can: [`ConflictState::op`]
/// is [`ConflictOp::StashPop`] here and one of the four real operations
/// otherwise. The ship pipeline's stale-`REBASE_HEAD` exemption, for
/// instance, is keyed on `op == Rebase` and is untouched by this,
/// because a stale marker has no unmerged entries and a `StashPop`
/// always does.
///
/// The cost is that this now runs one index read on every clean repo as
/// well. It is a `git diff` over the index, not the worktree, and it
/// replaces a `stat` of four files — the previous implementation ran
/// the same command as soon as it found a marker.
pub fn detect(repo_path: &Path) -> Result<Option<ConflictState>> {
    let git_dir = find_git_dir(repo_path)?;
    let op = detect_op(&git_dir)?;
    let unmerged = unmerged_paths(repo_path)?;
    let files = list_conflicted_files(repo_path, &unmerged);

    let op = match op {
        Some(op) => op,
        // No operation marker, but the index is unmerged. That is the
        // autostash/stash-apply/squash-merge case, and it is still a
        // conflict — see the type-level note on [`ConflictOp::StashPop`].
        None if !unmerged.is_empty() => ConflictOp::StashPop,
        // Nothing in progress and nothing unmerged. Clean.
        None => return Ok(None),
    };

    Ok(Some(ConflictState {
        op,
        head_ref: read_head_ref(repo_path, &git_dir, op),
        files,
        unmerged,
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
    let listed = state.unmerged.len();
    match state.op {
        ConflictOp::StashPop => out.push_str(&format!(
            "Conflict from an unmerged index, with no operation in progress ({listed} file(s))\n"
        )),
        op => out.push_str(&format!(
            "Conflict from in-progress {op} ({listed} file(s))\n"
        )),
    }
    if let Some(ref r) = state.head_ref {
        out.push_str(&format!("  HEAD: {r}\n"));
    }
    out.push_str("  Conflicted files:\n");
    for path in &state.unmerged {
        // Reuse the worktree reading `detect` already did where it can,
        // rather than reading every file a second time.
        let has_markers = state
            .files
            .iter()
            .find(|f| &f.path == path)
            .is_some_and(|f| f.has_markers);
        let marker_hint = if has_markers {
            " [has conflict markers]"
        } else {
            ""
        };
        out.push_str(&format!("    - {path}{marker_hint}\n"));
    }
    out.push_str("  Safe options:\n");
    if state.op == ConflictOp::StashPop {
        // Deliberately not `git stash pop --abort` and not a lie about
        // which operation ran. There is no marker, so there is no
        // operation to abort: what happened was an apply of stashed
        // work onto a tree that had moved, and only a person can say
        // which version of each file they meant.
        out.push_str("    1. Resolve each file, then `git add <file>`\n");
        out.push_str(
            "    2. If the conflict came from `git pull --autostash`, your stashed\n\
             \x20      work is still held: resolve the files, then `git stash pop`\n",
        );
        out.push_str(
            "    3. To throw the unmerged state away, `git checkout --theirs <file>`\n\
             \x20      per file and stage it — there is no operation to abort here\n",
        );
        return out;
    }
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
    let op = detect_op(&git_dir)?.context(
        "no operation is in progress, so there is nothing to abort. \
         An unmerged index left by a stash pop has no `--abort`: resolve \
         the files, `git add` them, and `git stash pop` if a stash is held.",
    )?;
    let flag = match op {
        ConflictOp::Merge | ConflictOp::Rebase | ConflictOp::CherryPick | ConflictOp::Revert => {
            "--abort"
        }
        // Unreachable: `detect_op` only ever returns the four marker
        // operations, and none of them is `StashPop`. Listed rather
        // than absorbed by a catch-all so that adding a marker-backed
        // operation later cannot silently skip its abort flag.
        ConflictOp::StashPop => {
            anyhow::bail!("a stash pop has no operation to abort")
        }
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
    let unmerged = unmerged_paths(repo_path).map_err(MarkResolvedError::IndexQueryFailed)?;
    if !unmerged.is_empty() {
        return Err(MarkResolvedError::UnmergedEntries(unmerged));
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
        // There is no marker to read, and no OID to name. `None` is the
        // honest answer: the cause is the index, not a ref.
        ConflictOp::StashPop => None,
    }
}

/// Every path git reports as unmerged, in git's own order.
///
/// `git diff --name-only --diff-filter=U` rather than `git status
/// --porcelain`: the porcelain output is the same information in a
/// format that has to be parsed, and this is the one call whose whole
/// job is the question being asked.
///
/// A failure is **not** an empty answer. An empty answer here would
/// report a clean repo on a tree that could not be read, which is the
/// exact lie `ro status` exists to stop telling — so the error
/// propagates and the caller decides what to do with it.
fn unmerged_paths(repo_path: &Path) -> Result<Vec<String>> {
    let output = std::process::Command::new("git")
        .args(["diff", "--name-only", "--diff-filter=U"])
        .current_dir(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .context("running git diff --name-only --diff-filter=U")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git diff --diff-filter=U failed: {stderr}");
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// The conflicted files, with the worktree's conflict markers noted.
///
/// `unmerged` is passed in rather than re-queried: `detect` has
/// already read the index, and reading it twice would make the two
/// halves of one state able to disagree about what it is.
fn list_conflicted_files(repo_path: &Path, unmerged: &[String]) -> Vec<ConflictedFile> {
    unmerged
        .iter()
        .map(|name| {
            let has_markers = file_has_conflict_markers(&repo_path.join(name));
            ConflictedFile {
                path: name.clone(),
                has_markers,
                stages: vec![],
            }
        })
        .collect()
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
        init_repo_at(&p);
        p
    }

    /// The same, at a path of the caller's choosing.
    ///
    /// A clone into `$root` lands in `$root/<name>`, and a test that
    /// writes to `$root` instead of `$root/<name>` passes for the wrong
    /// reason — the file it edited was never in the repository.
    fn init_repo_at(p: &Path) {
        std::fs::create_dir_all(p).unwrap();
        run_git(p, &["init", "-q", "-b", "main"]);
        run_git(p, &["config", "user.email", "test@example.com"]);
        run_git(p, &["config", "user.name", "Test"]);
    }

    /// A bare repo at `p`, with `HEAD` pointing at `main`.
    ///
    /// The `symbolic-ref` is not decoration. A fresh `git init --bare`
    /// leaves `HEAD` at `refs/heads/master`, so a clone of it checks out
    /// an *empty* `master` and the first commit in the clone lands on a
    /// branch the remote has no ref for — the push then fails with "src
    /// refspec main does not match any" and the fixture never reaches
    /// the state it exists to describe.
    fn init_bare(p: &Path) {
        std::fs::create_dir_all(p).unwrap();
        run_git(p, &["init", "--bare", "-q", "."]);
        run_git(p, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap_or_else(|e| {
                // A bare `unwrap()` here reports "No such file or
                // directory" and nothing about *which* directory, which
                // is how a fixture that ran git in a path it never
                // created reads as a git failure.
                panic!("git {args:?} in {} could not even run: {e}", dir.display());
            });
        if !out.status.success() {
            panic!(
                "git {args:?} in {} failed:\nstdout: {}\nstderr: {}",
                dir.display(),
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
            unmerged: vec!["src/main.rs".into(), "Cargo.toml".into()],
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
        assert_eq!(ConflictOp::StashPop.to_string(), "stash pop");
    }

    // ── The unmerged index, with no operation in progress ──

    /// A conflicting `git pull --autostash` is the case the four markers
    /// cannot see.
    ///
    /// The pull fast-forwards, the pop of the stashed work conflicts, git
    /// exits **0**, and it leaves none of `MERGE_HEAD`, `REBASE_HEAD`,
    /// `CHERRY_PICK_HEAD` or `REVERT_HEAD` behind. Before this, `detect`
    /// returned `None` for exactly that tree — `ro status` said a repo
    /// full of conflict markers was fine.
    #[test]
    fn detect_conflicting_autostash_pop() {
        let tmp = TempDir::new().unwrap();
        let remote = tmp.path().join("remote.git");
        init_bare(&remote);

        let a = tmp.path().join("a");
        init_repo_at(&a);
        commit(&a, "shared.txt", "line1\nline2\nline3\n");
        run_git(
            &a,
            &["remote", "add", "origin", &remote.display().to_string()],
        );
        run_git(&a, &["push", "-q", "origin", "main"]);

        // A clone of `remote` into `tmp` lands in `tmp/b`.
        let b = tmp.path().join("b");
        run_git(
            tmp.path(),
            &["clone", "-q", &remote.display().to_string(), "b"],
        );
        run_git(&b, &["config", "user.email", "test@example.com"]);
        run_git(&b, &["config", "user.name", "Test"]);
        commit(&b, "shared.txt", "line1\nREMOTE-VERSION\nline3\n");
        run_git(&b, &["push", "-q", "origin", "main"]);

        // The local side rewrites the same line the remote did.
        std::fs::write(a.join("shared.txt"), "line1\nLOCAL-VERSION\nline3\n").unwrap();
        // Without tracking, `git pull` refuses to guess a branch and the
        // test never reaches the state it exists to describe.
        run_git(&a, &["branch", "--set-upstream-to=origin/main", "main"]);

        let out = std::process::Command::new("git")
            .args(["pull", "--autostash"])
            .current_dir(&a)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git calls a conflicting autostash pop a success — that is the \
             whole reason the exit code cannot be the signal: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        // The precondition, asserted rather than assumed: this is the
        // state the four markers do not cover.
        let git_dir = find_git_dir(&a).unwrap();
        assert_eq!(
            detect_op(&git_dir).unwrap(),
            None,
            "the autostash pop must leave no operation marker, or this test is \
             not testing the case it exists for"
        );

        let state = detect(&a)
            .unwrap()
            .expect("a conflicting autostash pop is a conflict, not a clean repo");
        assert_eq!(state.op, ConflictOp::StashPop);
        assert_eq!(state.unmerged, vec!["shared.txt".to_string()]);
        assert!(state.files.iter().any(|f| f.path == "shared.txt"));
        assert!(
            state.files.iter().any(|f| f.has_markers),
            "the worktree holds conflict markers and the state must say so"
        );
        assert!(!state.is_empty());
    }

    /// The negative control for the test above.
    ///
    /// Without it, `detect_conflicting_autostash_pop` passes on a
    /// `detect` that returns `Some` unconditionally — which is the same
    /// shape of bug as the one being fixed, one layer down.
    #[test]
    fn detect_clean_repo_after_autostash_setup_is_none() {
        let tmp = TempDir::new().unwrap();
        let p = init_repo(&tmp);
        commit(&p, "a.txt", "hello");
        assert!(
            detect(&p).unwrap().is_none(),
            "a clean repo must not be reported as conflicted"
        );
    }

    /// A conflicting `git stash apply` leaves the same unmerged index and
    /// the same absence of markers. The signal is the state, not the one
    /// command that produces it most often.
    #[test]
    fn detect_conflicting_stash_apply() {
        let tmp = TempDir::new().unwrap();
        let p = init_repo(&tmp);
        commit(&p, "f.txt", "line1\nbase\nline3\n");
        std::fs::write(p.join("f.txt"), "line1\nlocal\nline3\n").unwrap();
        run_git(&p, &["stash", "push", "-q", "-m", "mine"]);
        commit(&p, "f.txt", "line1\nremote\nline3\n");

        let out = std::process::Command::new("git")
            .args(["stash", "apply"])
            .current_dir(&p)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(!out.status.success(), "the apply must conflict");

        let git_dir = find_git_dir(&p).unwrap();
        assert_eq!(detect_op(&git_dir).unwrap(), None);

        let state = detect(&p)
            .unwrap()
            .expect("a conflicting stash apply is a conflict");
        assert_eq!(state.op, ConflictOp::StashPop);
        assert_eq!(state.unmerged, vec!["f.txt".to_string()]);
    }

    /// `git merge --squash` conflicts without writing `MERGE_HEAD` — it
    /// stages the result and leaves the index unmerged.
    #[test]
    fn detect_conflicting_squash_merge() {
        let tmp = TempDir::new().unwrap();
        let p = init_repo(&tmp);
        commit(&p, "f.txt", "line1\nbase\nline3\n");
        run_git(&p, &["checkout", "-q", "-b", "feat"]);
        commit(&p, "f.txt", "line1\ntheirs\nline3\n");
        run_git(&p, &["checkout", "-q", "main"]);
        commit(&p, "f.txt", "line1\nours\nline3\n");

        let out = std::process::Command::new("git")
            .args(["merge", "--squash", "feat"])
            .current_dir(&p)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(!out.status.success(), "the squash merge must conflict");

        let git_dir = find_git_dir(&p).unwrap();
        assert_eq!(detect_op(&git_dir).unwrap(), None);

        let state = detect(&p)
            .unwrap()
            .expect("a conflicting squash merge is a conflict");
        assert_eq!(state.op, ConflictOp::StashPop);
        assert_eq!(state.unmerged, vec!["f.txt".to_string()]);
    }

    /// Resolved in place but not staged: the markers are gone, the index
    /// still holds two versions, and nothing may be built on it.
    ///
    /// This is the case that makes `is_empty` look at the index as well
    /// as the worktree — a `files`-only reading would call this resolved.
    #[test]
    fn detect_resolved_but_unstaged_is_still_a_conflict() {
        let tmp = TempDir::new().unwrap();
        let p = init_repo(&tmp);
        commit(&p, "f.txt", "line1\nbase\nline3\n");
        run_git(&p, &["checkout", "-q", "-b", "feat"]);
        commit(&p, "f.txt", "line1\ntheirs\nline3\n");
        run_git(&p, &["checkout", "-q", "main"]);
        commit(&p, "f.txt", "line1\nours\nline3\n");
        let _ = std::process::Command::new("git")
            .args(["merge", "feat"])
            .current_dir(&p)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();

        // Resolve the content, leave the index alone.
        std::fs::write(p.join("f.txt"), "line1\nresolved\nline3\n").unwrap();
        assert!(
            !file_has_conflict_markers(&p.join("f.txt")),
            "the markers must be gone, or this is not the case under test"
        );

        let state = detect(&p)
            .unwrap()
            .expect("an unmerged index is a conflict even with no markers left");
        assert_eq!(state.unmerged, vec!["f.txt".to_string()]);
        // The file is still listed, and now reads as marker-free: it is
        // the resolved content, sitting in the index with two versions
        // of it still recorded. That is why the *op* is the other half
        // of the row — "not in a merge" does not mean "not conflicted".
        assert!(
            state
                .files
                .iter()
                .any(|f| f.path == "f.txt" && !f.has_markers),
            "the resolved file is listed and marked marker-free: {state:?}"
        );
        assert!(
            !state.is_empty(),
            "resolved-but-unstaged is not resolved: the index still holds two versions"
        );
    }

    /// A modify/delete conflict leaves an unmerged entry and **no file
    /// on disk to hold markers in** — git keeps the HEAD version in the
    /// tree and stages nothing.
    ///
    /// This is the case that makes `is_empty` look at the index as well
    /// as the worktree: `files` is empty here, and a `files`-only
    /// reading would call the repo resolved.
    #[test]
    fn detect_modify_delete_conflict() {
        let tmp = TempDir::new().unwrap();
        let p = init_repo(&tmp);
        commit(&p, "f.txt", "base");
        run_git(&p, &["checkout", "-q", "-b", "feat"]);
        run_git(&p, &["rm", "-q", "f.txt"]);
        run_git(&p, &["commit", "-q", "-m", "delete on feat"]);
        run_git(&p, &["checkout", "-q", "main"]);
        commit(&p, "f.txt", "main version");

        let _ = std::process::Command::new("git")
            .args(["merge", "feat"])
            .current_dir(&p)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();

        let state = detect(&p)
            .unwrap()
            .expect("a modify/delete conflict is a conflict");
        assert_eq!(state.op, ConflictOp::Merge);
        assert_eq!(state.unmerged, vec!["f.txt".to_string()]);
        // The file is still listed — git keeps the HEAD version in the
        // tree — but it holds no markers, because nothing was merged
        // into it. The index is the only witness that it is unmerged.
        assert!(
            state
                .files
                .iter()
                .any(|f| f.path == "f.txt" && !f.has_markers),
            "the unmerged file is listed and reads as marker-free: {state:?}"
        );
        assert!(
            !state.is_empty(),
            "an unmerged index is a conflict even with nothing to mark"
        );
    }

    /// Staged-only and untracked-only are not conflicts. Both are the
    /// ordinary state of a working repo, and reporting them would teach
    /// the user to ignore the marker.
    #[test]
    fn detect_ignores_staged_and_untracked_only() {
        let tmp = TempDir::new().unwrap();
        let p = init_repo(&tmp);
        commit(&p, "a.txt", "hello");

        std::fs::write(p.join("a.txt"), "changed").unwrap();
        run_git(&p, &["add", "a.txt"]);
        assert!(
            detect(&p).unwrap().is_none(),
            "a staged change is not a conflict"
        );

        run_git(&p, &["reset", "-q", "HEAD"]);
        std::fs::write(p.join("untracked.txt"), "new").unwrap();
        assert!(
            detect(&p).unwrap().is_none(),
            "an untracked file is not a conflict"
        );
    }

    /// The stale-marker exemption the ship pipeline relies on must
    /// survive the widening.
    ///
    /// A successful `git rebase --continue` leaves `REBASE_HEAD` behind
    /// on git 2.53 — the `rebase-merge` directory is gone, the branch
    /// has moved, the tree is clean — and the pipeline treats a marker
    /// with no unmerged entries as "resolved, carry on". That exemption
    /// is keyed on `op == Rebase`, and a `StashPop` always has unmerged
    /// entries, so the two cannot collide. Asserted rather than assumed,
    /// because the collision would be silent: the pipeline would skip a
    /// repo it had just resolved, forever.
    #[test]
    fn detect_stale_rebase_marker_is_none() {
        let tmp = TempDir::new().unwrap();
        let p = init_repo(&tmp);
        commit(&p, "a.txt", "base");
        run_git(&p, &["checkout", "-q", "-b", "feat"]);
        commit(&p, "a.txt", "theirs");
        run_git(&p, &["checkout", "-q", "main"]);
        commit(&p, "a.txt", "ours");
        let _ = std::process::Command::new("git")
            .args(["rebase", "feat"])
            .current_dir(&p)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(
            detect(&p).unwrap().is_some(),
            "the rebase must have conflicted"
        );

        // Resolve and continue. git 2.53 keeps REBASE_HEAD afterwards.
        run_git(&p, &["add", "."]);
        run_git(&p, &["-c", "core.editor=true", "rebase", "--continue"]);

        let git_dir = find_git_dir(&p).unwrap();
        let op = detect_op(&git_dir).unwrap();
        let unmerged = unmerged_paths(&p).unwrap();
        assert!(
            op.is_some() && unmerged.is_empty(),
            "the stale-marker case is a marker over a clean index — got \
             op={op:?}, unmerged={unmerged:?}, or this is not the case under test"
        );
        // The premise the ship pipeline's exemption rests on: the marker
        // is there, the index is clean, and nothing is waiting for a
        // person. `detect` still reports the marker — it is the
        // *emptiness* that exempts it, which is the whole reason
        // `is_empty` reads the index and not just the file list.
        let state = detect(&p)
            .unwrap()
            .expect("detect still reports the marker");
        assert!(state.is_empty());
        assert_eq!(
            state.op,
            ConflictOp::Rebase,
            "and the exemption is keyed on exactly this op"
        );
        assert!(
            state.unmerged.is_empty(),
            "and it is unmerged entries — not the file list — that make the \
             difference: a StashPop always has some"
        );
    }

    /// `list_conflicts` inherits the widened `detect`, so a fleet query
    /// sees the same conflict a single-repo query does.
    #[test]
    fn list_conflicts_sees_an_autostash_pop() {
        let tmp = TempDir::new().unwrap();
        let remote = tmp.path().join("remote.git");
        init_bare(&remote);

        let a = tmp.path().join("a");
        init_repo_at(&a);
        commit(&a, "shared.txt", "line1\nline2\nline3\n");
        run_git(
            &a,
            &["remote", "add", "origin", &remote.display().to_string()],
        );
        run_git(&a, &["push", "-q", "origin", "main"]);

        // A clone of `remote` into `tmp` lands in `tmp/b`.
        let b = tmp.path().join("b");
        run_git(
            tmp.path(),
            &["clone", "-q", &remote.display().to_string(), "b"],
        );
        run_git(&b, &["config", "user.email", "test@example.com"]);
        run_git(&b, &["config", "user.name", "Test"]);
        commit(&b, "shared.txt", "line1\nREMOTE-VERSION\nline3\n");
        run_git(&b, &["push", "-q", "origin", "main"]);

        std::fs::write(a.join("shared.txt"), "line1\nLOCAL-VERSION\nline3\n").unwrap();
        run_git(&a, &["branch", "--set-upstream-to=origin/main", "main"]);
        let _ = std::process::Command::new("git")
            .args(["pull", "--autostash"])
            .current_dir(&a)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();

        let clean = tmp.path().join("clean");
        init_repo_at(&clean);
        commit(&clean, "a.txt", "hello");

        let conflicts = list_conflicts(&[a.clone(), clean]);
        assert_eq!(conflicts.len(), 1, "only the conflicted repo is listed");
        assert_eq!(conflicts[0].0, a);
        assert_eq!(conflicts[0].1.op, ConflictOp::StashPop);
    }

    /// `explain` must not tell the user to run a command that cannot work.
    ///
    /// There is no `git stash pop --abort`, and `git merge --abort` does
    /// not cover it either — verified against git 2.53, which answers
    /// `merge --abort` with "There is no merge to abort (MERGE_HEAD
    /// missing)" and leaves the index unmerged. The advice has to name
    /// the two commands that do work, and not the one that does not.
    #[test]
    fn explain_for_a_stash_pop_does_not_offer_a_broken_abort() {
        let state = ConflictState {
            op: ConflictOp::StashPop,
            head_ref: None,
            files: vec![ConflictedFile {
                path: "shared.txt".into(),
                has_markers: true,
                stages: vec![],
            }],
            unmerged: vec!["shared.txt".into()],
        };
        let text = explain(&state);
        assert!(text.contains("stash pop"), "it names what happened: {text}");
        assert!(
            !text.contains("git stash pop --abort"),
            "that subcommand does not exist: {text}"
        );
        assert!(
            !text.contains("git merge --abort"),
            "no merge is running, so this does nothing: {text}"
        );
        assert!(
            text.contains("git add"),
            "the one thing that does work is staging the resolution: {text}"
        );
        assert!(
            text.contains("git stash pop"),
            "the stashed work is the user's only route back to it: {text}"
        );
    }

    /// The four real operations keep their abort advice.
    #[test]
    fn explain_for_a_real_operation_still_offers_abort() {
        let state = ConflictState {
            op: ConflictOp::Merge,
            head_ref: Some("abc123".into()),
            files: vec![ConflictedFile {
                path: "a.txt".into(),
                has_markers: true,
                stages: vec![],
            }],
            unmerged: vec!["a.txt".into()],
        };
        let text = explain(&state);
        assert!(
            text.contains("merge --abort"),
            "a merge can be aborted: {text}"
        );
    }

    /// `abort` refuses rather than running a command that cannot work.
    #[test]
    fn abort_refuses_a_stash_pop() {
        let tmp = TempDir::new().unwrap();
        let p = init_repo(&tmp);
        commit(&p, "f.txt", "line1\nbase\nline3\n");
        std::fs::write(p.join("f.txt"), "line1\nlocal\nline3\n").unwrap();
        run_git(&p, &["stash", "push", "-q", "-m", "mine"]);
        commit(&p, "f.txt", "line1\nremote\nline3\n");
        let _ = std::process::Command::new("git")
            .args(["stash", "apply"])
            .current_dir(&p)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(detect(&p).unwrap().is_some());

        let err = abort(&p).expect_err("there is no operation to abort");
        let msg = err.to_string();
        assert!(
            msg.contains("nothing to abort") || msg.contains("no operation"),
            "the refusal must say why: {msg}"
        );
    }

    /// `verify_resolved` reads the same index, so it agrees with `detect`
    /// about what "resolved" means.
    #[test]
    fn verify_resolved_rejects_an_unmerged_index_with_no_markers() {
        let tmp = TempDir::new().unwrap();
        let p = init_repo(&tmp);
        commit(&p, "f.txt", "line1\nbase\nline3\n");
        run_git(&p, &["checkout", "-q", "-b", "feat"]);
        commit(&p, "f.txt", "line1\ntheirs\nline3\n");
        run_git(&p, &["checkout", "-q", "main"]);
        commit(&p, "f.txt", "line1\nours\nline3\n");
        let _ = std::process::Command::new("git")
            .args(["merge", "feat"])
            .current_dir(&p)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        std::fs::write(p.join("f.txt"), "line1\nresolved\nline3\n").unwrap();

        let err = verify_resolved(&p).expect_err("an unmerged index is not resolved");
        assert!(
            matches!(err, MarkResolvedError::UnmergedEntries(_)),
            "the index is what is still wrong: {err}"
        );
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
