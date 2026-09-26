//! Git primitives the orchestrator needs and ro-git's typed API did not have.
//!
//! The typed API's rule — `commit` refuses an empty file list and forbids
//! `add -A` — is right in general and wrong for this tool. "Commit everything
//! I have" is the flagship use case, so WIP-style staging is a first-class
//! operation here rather than something callers hand-roll.
//!
//! It was hand-rolled: `ro-sweep/src/commit.rs` grew its own `git add -A` and
//! its own porcelain reading, in violation of ro-git's own stated rule, and the
//! two copies drifted. This module is the one copy. `parse_porcelain`,
//! `is_protected_branch` and the `-uall` decision are carried over **verbatim**
//! from `ro-sweep` rather than re-derived, because a re-derivation that is
//! almost-right is how a nested repository silently becomes a broken gitlink.
//!
//! Every command goes through `run_in`, so the hardening block and any
//! per-invocation credential apply here exactly as they do to `push`.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::mutation::{RunOpts, run_in};

/// Branches ro refuses to commit to or push.
const PROTECTED_BRANCHES: &[&str] = &["main", "master"];
const PROTECTED_PREFIX: &str = "release/";

/// True when the branch must not be committed to or pushed by a sweep.
///
/// `release/*` counts because a release branch is the one place a mistaken
/// push is expensive in a way that is not obvious at the time.
pub fn is_protected_branch(branch: &str) -> bool {
    PROTECTED_BRANCHES.contains(&branch) || branch.starts_with(PROTECTED_PREFIX)
}

/// One entry of `git status --porcelain=v1 -z`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    /// The two-character XY code, with spaces removed (`M`, `??`, `A`, …).
    pub code: String,
    /// The path, relative to the repository root.
    pub path: String,
}

impl StatusEntry {
    /// Is this entry a file that exists in the worktree (as opposed to a
    /// deletion)?
    pub fn is_present(&self) -> bool {
        !self.is_deleted() && self.code != "??"
    }

    pub fn is_deleted(&self) -> bool {
        self.code.contains('D')
    }

    pub fn is_untracked(&self) -> bool {
        self.code == "??"
    }

    /// A nested repository or an untracked directory, which git reports as a
    /// single `dir/` entry and which must not be swept into a commit.
    pub fn is_nested_or_dir(&self) -> bool {
        self.path.ends_with('/')
    }
}

/// Parse NUL-delimited porcelain output.
///
/// Renames and copies are two NUL-terminated fields: `XY <old>\0<new>\0`. The
/// destination is the meaningful path, so it wins. Carried over verbatim from
/// the same NUL-safe loop in ru's `cs_process_repo`.
///
/// The `-z` form is not a stylistic choice. Without it git quotes paths
/// containing spaces, quotes, newlines, or non-ASCII bytes, and the quoting is
/// not always reversible — so a commit message built from a quoted path can
/// name the wrong file, or split a filename in two.
pub fn parse_porcelain(raw: &str) -> Vec<StatusEntry> {
    let mut fields = raw.split('\0').filter(|f| !f.is_empty());
    let mut out = Vec::new();

    while let Some(entry) = fields.next() {
        // Every entry is "XY<space>path" — three leading bytes are ASCII, so
        // slicing is safe even when the path itself is not.
        if entry.len() < 3 || !entry.is_char_boundary(2) || !entry.is_char_boundary(3) {
            continue;
        }
        let code = entry[..2].replace(' ', "");
        let mut path = entry[3..].to_string();

        if code.starts_with('R') || code.starts_with('C') {
            if let Some(dest) = fields.next() {
                path = dest.to_string();
            }
        }
        out.push(StatusEntry { code, path });
    }
    out
}

/// Read the worktree status for a repo.
///
/// `-uall` expands untracked directories into individual files. Without it git
/// collapses an untracked directory into a single `dir/` entry, which would
/// commit the whole tree in one opaque step — and would also hide the nested
/// repository that the caller needs to skip, since both look like `dir/`.
pub fn read_status(repo: &Path) -> Result<Vec<StatusEntry>> {
    let out = run_in(
        Some(repo),
        &["status", "--porcelain=v1", "-z", "-uall"],
        &RunOpts::none(),
    )?;
    if !out.ok() {
        bail!("git status failed: {}", out.stderr.trim());
    }
    Ok(parse_porcelain(&out.stdout))
}

/// The nested repositories and untracked directories in a status listing.
///
/// Split out because the caller has to *do* something with them — skip them
/// and say so — and a value that separates the decision from the report is how
/// the skip gets forgotten.
pub fn nested_or_dirs(entries: &[StatusEntry]) -> Vec<String> {
    entries
        .iter()
        .filter(|e| e.is_nested_or_dir())
        .map(|e| e.path.trim_end_matches('/').to_string())
        .collect()
}

/// `git add -A` — stage everything, including brand-new files.
///
/// This is the operation `mutation::commit` deliberately refuses, and it is
/// here deliberately. The typed rule exists so a caller cannot commit something
/// it did not name; this tool's whole job is committing what the user did not
/// name, one bucket at a time.
///
/// Nested repositories are **not** excluded here. `git add -A` already refuses
/// to descend into them, and the porcelain guard exists so the *caller* can
/// report them rather than silently leaving them out of the summary.
pub fn stage_all(repo: &Path) -> Result<()> {
    let out = run_in(Some(repo), &["add", "-A"], &RunOpts::none())?;
    if !out.ok() {
        bail!("git add -A failed: {}", out.stderr.trim());
    }
    Ok(())
}

/// Stage specific paths.
///
/// `--` separates the paths from the options, so a file named `-f` or
/// `--all` cannot become a flag. The typed API's rule is preserved here: this
/// function names exactly what it stages.
pub fn stage_paths(repo: &Path, paths: &[std::path::PathBuf]) -> Result<()> {
    if paths.is_empty() {
        bail!("stage_paths requires at least one path");
    }
    let mut args: Vec<String> = vec!["add".to_string(), "--".to_string()];
    for p in paths {
        args.push(p.to_string_lossy().into_owned());
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = run_in(Some(repo), &argv, &RunOpts::none())?;
    if !out.ok() {
        bail!("git add failed: {}", out.stderr.trim());
    }
    Ok(())
}

/// Commit whatever is staged, with a subject. No file list — that is
/// `mutation::commit`'s job.
pub fn commit_all(repo: &Path, message: &str) -> Result<String> {
    let out = run_in(
        Some(repo),
        &["commit", "--no-gpg-sign", "-m", message],
        &RunOpts::none(),
    )?;
    if !out.ok() {
        bail!("git commit failed: {}", out.stderr.trim());
    }
    head_short(repo)
}

/// The short OID of HEAD.
pub fn head_short(repo: &Path) -> Result<String> {
    let out = run_in(
        Some(repo),
        &["rev-parse", "--short", "HEAD"],
        &RunOpts::none(),
    )?;
    if !out.ok() {
        bail!("git rev-parse failed: {}", out.stderr.trim());
    }
    Ok(out.stdout.trim().to_string())
}

/// Create a branch at the current HEAD without checking it out.
pub fn create_branch(repo: &Path, name: &str) -> Result<()> {
    let out = run_in(Some(repo), &["branch", "--", name], &RunOpts::none())?;
    if !out.ok() {
        bail!("creating branch {name:?} failed: {}", out.stderr.trim());
    }
    Ok(())
}

/// Check out a branch or a commit-ish.
///
/// The `--` goes **after** the target, not before. `git checkout -- foo` means
/// "restore the file foo from the index", which is a different command that
/// happens to share a binary — putting the separator first silently checks out
/// a path instead of a branch, and fails with "pathspec did not match".
/// Trailing `--` is how git is told to read the argument as a revision.
pub fn checkout(repo: &Path, target: &str) -> Result<()> {
    let mut args: Vec<String> = vec!["checkout".to_string(), target.to_string()];
    args.push("--".to_string());
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = run_in(Some(repo), &argv, &RunOpts::none())?;
    if !out.ok() {
        bail!("checking out {target:?} failed: {}", out.stderr.trim());
    }
    Ok(())
}

/// The current branch name, or `None` on a detached HEAD.
pub fn current_branch(repo: &Path) -> Option<String> {
    let out = run_in(
        Some(repo),
        &["rev-parse", "--abbrev-ref", "HEAD"],
        &RunOpts::none(),
    )
    .ok()?;
    if !out.ok() {
        return None;
    }
    let name = out.stdout.trim().to_string();
    if name == "HEAD" { None } else { Some(name) }
}

pub fn branch_exists(repo: &Path, name: &str) -> bool {
    run_in(
        Some(repo),
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ],
        &RunOpts::none(),
    )
    .map(|o| o.ok())
    .unwrap_or(false)
}

/// Every local branch, without the `*` marker or the detached-HEAD row.
pub fn list_branches(repo: &Path) -> Result<Vec<String>> {
    let out = run_in(
        Some(repo),
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
        &RunOpts::none(),
    )?;
    if !out.ok() {
        bail!("git for-each-ref failed: {}", out.stderr.trim());
    }
    Ok(out
        .stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// Unstaged changes in the worktree, one commit's worth.
pub fn diff_worktree(repo: &Path) -> Result<String> {
    let out = run_in(Some(repo), &["diff"], &RunOpts::none())?;
    if !out.ok() {
        bail!("git diff failed: {}", out.stderr.trim());
    }
    Ok(out.stdout)
}

/// Staged changes, one commit's worth.
///
/// Separate from [`diff_worktree`] because they are the two halves of a
/// commit and the engine prompt is built from both: an agent that only sees the
/// worktree diff has not seen what the user already staged.
pub fn diff_staged(repo: &Path) -> Result<String> {
    let out = run_in(Some(repo), &["diff", "--cached"], &RunOpts::none())?;
    if !out.ok() {
        bail!("git diff --cached failed: {}", out.stderr.trim());
    }
    Ok(out.stdout)
}

/// Stash the worktree, **including untracked files**.
///
/// `-u` is not a convenience. `git stash push` without it leaves new files
/// exactly where they are, so an autostash would report success, the
/// operation it was protecting would run against a dirty tree, and the new
/// files — the ones most likely to be what the user wanted stashed — would be
/// the ones still in the way.
///
/// Returns whether anything was actually stashed, because "nothing to stash" is
/// a normal outcome and the autostash path needs to know whether it has
/// something to put back.
pub fn stash_push(repo: &Path, message: &str) -> Result<bool> {
    let out = run_in(
        Some(repo),
        &["stash", "push", "-u", "-m", message],
        &RunOpts::none(),
    )?;
    if !out.ok() {
        bail!("git stash push failed: {}", out.stderr.trim());
    }
    Ok(!out.stdout.contains("No local changes to save"))
}

/// Pop the most recent stash.
///
/// Returns whether anything came back. A `stash pop` on an empty stash fails,
/// which for the autostash path means "there was nothing to restore" rather
/// than "restore it".
pub fn stash_pop(repo: &Path) -> Result<bool> {
    let out = run_in(Some(repo), &["stash", "pop"], &RunOpts::none())?;
    if !out.ok() {
        return Ok(false);
    }
    Ok(true)
}

/// The outcome of a merge or a rebase.
///
/// A value, not a `bool`. `PullOutcome::conflict` is computed and then
/// discarded today, so a conflicted pull is recorded as `updated` /
/// `success` / `error: None` — **a failed pull reported as a success**. The
/// standalone operations return this so the caller can branch on it, which is
/// what the `--resolve` stage in Phase 5 needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// Merged or rebased cleanly.
    Clean,
    /// The operation stopped with conflicts. The working tree is mid-operation
    /// and the caller owns resolving it.
    Conflicted,
    /// Nothing to do — already up to date, or already on the target.
    AlreadyDone,
    /// Refused by git for a reason that is not a conflict: a non-fast-forward,
    /// an untracked file in the way, an empty repository.
    Rejected { reason: String },
}

impl MergeOutcome {
    pub fn is_conflicted(&self) -> bool {
        matches!(self, MergeOutcome::Conflicted)
    }

    /// The working tree is mid-operation and needs a person or an engine.
    pub fn needs_resolution(&self) -> bool {
        matches!(self, MergeOutcome::Conflicted)
    }
}

/// Decide what a merge or rebase exit status and output actually mean.
///
/// `git merge` reports a conflict with status 1 *and* the same status for an
/// ordinary refusal, so the exit code alone cannot tell "someone must resolve
/// this" from "git said no". The text is the discriminator, which is why this
/// is one function used by both operations rather than two that drift.
fn classify_merge_output(out: &crate::mutation::GitCommandResult) -> MergeOutcome {
    let combined = format!("{}{}", out.stdout, out.stderr).to_ascii_lowercase();
    if combined.contains("conflict")
        || combined.contains("automatic merge failed")
        || combined.contains("could not apply")
    {
        return MergeOutcome::Conflicted;
    }
    if combined.contains("already up to date") || combined.contains("up to date") {
        return MergeOutcome::AlreadyDone;
    }
    if out.ok() {
        return MergeOutcome::Clean;
    }
    MergeOutcome::Rejected {
        reason: format!("{}{}", out.stderr, out.stdout).trim().to_string(),
    }
}

/// Rebase onto `upstream`, as a first-class operation.
///
/// Standalone because `pull` conflates fetch+pull and the rebase path needs
/// to be one step with a distinguishable outcome: the Phase 5 pipeline is
/// `fetch → rebase → conflict? → resolve → push`, and a `pull` that has already
/// done the fetch cannot answer "did the rebase conflict".
pub fn rebase(repo: &Path, upstream: &str) -> Result<MergeOutcome> {
    let out = run_in(Some(repo), &["rebase", upstream], &RunOpts::none())?;
    // On a conflict the rebase is left mid-sequence on purpose: aborting would
    // discard the user's commits, and the caller needs the conflicted state to
    // resolve rather than a clean tree.
    Ok(classify_merge_output(&out))
}

/// Abort an in-progress rebase, restoring the pre-rebase state.
pub fn rebase_abort(repo: &Path) -> Result<()> {
    let out = run_in(Some(repo), &["rebase", "--abort"], &RunOpts::none())?;
    if !out.ok() {
        bail!("git rebase --abort failed: {}", out.stderr.trim());
    }
    Ok(())
}

/// Continue an in-progress rebase after conflicts were resolved and staged.
pub fn rebase_continue(repo: &Path) -> Result<MergeOutcome> {
    let out = run_in(
        Some(repo),
        // An editor would block forever, which is the whole class of bug the
        // timeout exists for. The message is supplied rather than opened.
        &["-c", "core.editor=true", "rebase", "--continue"],
        &RunOpts::none(),
    )?;
    Ok(classify_merge_output(&out))
}

/// Merge `branch` into the current branch.
pub fn merge(repo: &Path, branch: &str) -> Result<MergeOutcome> {
    let out = run_in(
        Some(repo),
        &["merge", "--no-edit", branch],
        &RunOpts::none(),
    )?;
    Ok(classify_merge_output(&out))
}

/// The URL of a named remote.
///
/// `None` for a non-repo, a missing remote, or a query failure — the last
/// deliberately not distinguished, because every caller's correct response to
/// all three is the same.
pub fn remote_url(repo: &Path, remote: &str) -> Option<String> {
    let out = run_in(Some(repo), &["remote", "get-url", remote], &RunOpts::none()).ok()?;
    if !out.ok() {
        return None;
    }
    let url = out.stdout.trim().to_string();
    if url.is_empty() { None } else { Some(url) }
}

/// The configured user email for a repository or global config.
///
/// `None` when unset. An unset author email is a commit that fails at the worst
/// moment — after the work is staged and the message written — so this is read
/// *before* a commit rather than discovered by it.
pub fn git_config_user_email(repo: &Path) -> Option<String> {
    let out = run_in(
        Some(repo),
        &["config", "--get", "user.email"],
        &RunOpts::none(),
    )
    .ok()?;
    if !out.ok() {
        return None;
    }
    let email = out.stdout.trim().to_string();
    if email.is_empty() { None } else { Some(email) }
}

/// Is a rebase or merge currently in progress?
///
/// Checked before `rebase --continue`, because running it with conflicts still
/// present produces a second, more confusing failure — and the user learns to
/// avoid the flag rather than to use it.
pub fn is_rebase_in_progress(repo: &Path) -> bool {
    repo.join(".git").join("rebase-merge").exists()
        || repo.join(".git").join("rebase-apply").exists()
}

/// The files git reports as conflicted, as porcelain entries.
pub fn conflicted_files(repo: &Path) -> Result<Vec<StatusEntry>> {
    let out = run_in(
        Some(repo),
        &["diff", "--name-only", "--diff-filter=U"],
        &RunOpts::none(),
    )?;
    if !out.ok() {
        return Ok(Vec::new());
    }
    Ok(out
        .stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|path| StatusEntry {
            code: "UU".to_string(),
            path: path.to_string(),
        })
        .collect())
}

/// Open a repository, or say clearly that it is not one.
///
/// Used where a wrong path would otherwise produce a git error three calls
/// later, naming something that looks like a git problem.
pub fn ensure_repo(path: &Path) -> Result<()> {
    let out = run_in(Some(path), &["rev-parse", "--git-dir"], &RunOpts::none())
        .with_context(|| format!("running git in {}", path.display()))?;
    if !out.ok() {
        bail!("{} is not a git repository", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// A repository with one commit, on `main`.
    fn repo() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run_in(Some(&repo), &["init", "-q", "-b", "main"], &RunOpts::none()).unwrap();
        run_in(
            Some(&repo),
            &["config", "user.email", "t@example.com"],
            &RunOpts::none(),
        )
        .unwrap();
        run_in(
            Some(&repo),
            &["config", "user.name", "Test"],
            &RunOpts::none(),
        )
        .unwrap();
        std::fs::write(repo.join("README.md"), "# repo\n").unwrap();
        stage_all(&repo).unwrap();
        commit_all(&repo, "initial").unwrap();
        (tmp, repo)
    }

    /// A branch that edits `README.md` and returns to main, so the two can
    /// conflict.
    fn branch_edit(repo: &Path, branch: &str, text: &str, message: &str) {
        run_in(
            Some(repo),
            &["checkout", "-q", "-b", branch],
            &RunOpts::none(),
        )
        .unwrap();
        std::fs::write(repo.join("README.md"), text).unwrap();
        stage_all(repo).unwrap();
        commit_all(repo, message).unwrap();
        run_in(Some(repo), &["checkout", "-q", "main"], &RunOpts::none()).unwrap();
    }

    /// The test the bead names, and the reason `stage_all` exists.
    ///
    /// A brand-new untracked *directory* with files inside is where a naive
    /// implementation loses work. `git add -A` handles it; a hand-rolled "find
    /// the dirty files and add each one" commits an empty tree, because the
    /// directory is untracked and its contents are not in the listing the walk
    /// was built from.
    #[test]
    fn stage_all_commits_a_brand_new_untracked_directory() {
        let (_tmp, repo) = repo();

        // A whole new directory, with files at two depths, none tracked.
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        std::fs::write(repo.join("src/new.rs"), "fn main() {}\n").unwrap();
        std::fs::write(repo.join("src/deep/deeper.rs"), "// deep\n").unwrap();

        let entries = read_status(&repo).unwrap();
        assert!(
            entries.iter().any(|e| e.path == "src/new.rs"),
            "-uall must expand the untracked directory into files, got {:?}",
            entries.iter().map(|e| &e.path).collect::<Vec<_>>()
        );

        stage_all(&repo).unwrap();
        let oid = commit_all(&repo, "add new tree").unwrap();
        assert!(!oid.is_empty());

        // The files are in the commit, not merely absent from the worktree.
        let tree = run_in(
            Some(&repo),
            &["ls-tree", "-r", "--name-only", "HEAD"],
            &RunOpts::none(),
        )
        .unwrap();
        assert!(
            tree.stdout.contains("src/new.rs"),
            "the new file should be in the commit, tree was:\n{}",
            tree.stdout
        );
        assert!(
            tree.stdout.contains("src/deep/deeper.rs"),
            "the nested new file should be in the commit too, tree was:\n{}",
            tree.stdout
        );
    }

    /// The nested-repository guard, carried over from ro-sweep.
    ///
    /// git does not descend into a nested repository, so it reports the whole
    /// thing as a single `dir/` entry. Committing that adds a broken gitlink —
    /// a file that looks like a submodule and is not one.
    #[test]
    fn a_nested_repository_is_reported_rather_than_swept_in() {
        let (_tmp, repo) = repo();
        let nested = repo.join("vendor/inner");
        std::fs::create_dir_all(&nested).unwrap();
        run_in(
            Some(&nested),
            &["init", "-q", "-b", "main"],
            &RunOpts::none(),
        )
        .unwrap();

        let entries = read_status(&repo).unwrap();
        let reported = nested_or_dirs(&entries);
        assert!(
            reported.iter().any(|d| d == "vendor/inner"),
            "the nested repository should be reported, got {reported:?}"
        );
    }

    /// A plain untracked directory is not a nested repository, and the guard
    /// must not be so blunt that a new folder looks nested.
    #[test]
    fn a_plain_untracked_directory_is_not_reported_as_nested() {
        let (_tmp, repo) = repo();
        std::fs::create_dir_all(repo.join("docs")).unwrap();
        std::fs::write(repo.join("docs/a.md"), "a\n").unwrap();

        let entries = read_status(&repo).unwrap();
        assert!(
            nested_or_dirs(&entries).is_empty(),
            "a plain directory should expand to files, got {entries:?}"
        );
    }

    /// Renames are two NUL-terminated fields and the destination is the path
    /// that matters. Getting this wrong commits the old name.
    #[test]
    fn porcelain_renames_report_the_destination() {
        let raw = "R  old name.rs\0new name.rs\0 M other.rs\0";
        let entries = parse_porcelain(raw);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "new name.rs");
        assert_eq!(entries[0].code, "R");
        assert_eq!(entries[1].path, "other.rs");
    }

    /// `-z` exists so paths with spaces, quotes, and non-ASCII survive intact.
    /// A name containing a quote is exactly what a quoted format mangles.
    #[test]
    fn porcelain_keeps_awkward_paths_intact() {
        let raw = "?? a \"quoted\" name.rs\0?? 日本語.rs\0?? with\ttab.rs\0";
        let entries = parse_porcelain(raw);
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["a \"quoted\" name.rs", "日本語.rs", "with\ttab.rs"]
        );
    }

    /// A truncated or malformed field must not panic. `entry[..2]` on a
    /// multi-byte boundary would.
    #[test]
    fn porcelain_survives_malformed_input() {
        let raw = "?\0??\0? 日本語\0 M ok.rs\0";
        let entries = parse_porcelain(raw);
        assert!(
            entries.iter().any(|e| e.path == "ok.rs"),
            "the well-formed entries should still be read, got {entries:?}"
        );
    }

    /// The outcome must be a value, not a bool: a conflicted rebase and a
    /// refused one both exit non-zero, and the caller has to tell "resolve
    /// this" from "this will never work".
    #[test]
    fn a_rebase_onto_a_conflicting_branch_reports_a_conflict() {
        let (_tmp, repo) = repo();
        branch_edit(&repo, "feature", "# from feature\n", "feature edit");
        std::fs::write(repo.join("README.md"), "# from main\n").unwrap();
        stage_all(&repo).unwrap();
        commit_all(&repo, "main edit").unwrap();

        let outcome = rebase(&repo, "feature").unwrap();
        assert!(
            outcome.is_conflicted(),
            "a rebase that cannot apply must say so, got {outcome:?}"
        );
        assert!(outcome.needs_resolution());
        assert!(is_rebase_in_progress(&repo), "the rebase is mid-flight");

        // The conflicts are nameable, which is what the resolve stage edits.
        let files = conflicted_files(&repo).unwrap();
        assert!(
            files.iter().any(|f| f.path == "README.md"),
            "the conflicted file should be named, got {files:?}"
        );

        // Aborting restores the pre-rebase state rather than leaving the
        // repository mid-operation.
        rebase_abort(&repo).unwrap();
        assert!(!is_rebase_in_progress(&repo));
    }

    #[test]
    fn a_clean_rebase_reports_clean() {
        let (_tmp, repo) = repo();
        branch_edit(&repo, "feature", "# untouched\n", "feature edit");

        let outcome = rebase(&repo, "feature").unwrap();
        assert_eq!(outcome, MergeOutcome::Clean, "got {outcome:?}");
        assert!(!is_rebase_in_progress(&repo));
    }

    #[test]
    fn a_merge_of_a_conflicting_branch_reports_a_conflict() {
        let (_tmp, repo) = repo();
        branch_edit(&repo, "feature", "# feature\n", "feature");
        std::fs::write(repo.join("README.md"), "# main\n").unwrap();
        stage_all(&repo).unwrap();
        commit_all(&repo, "main").unwrap();

        let outcome = merge(&repo, "feature").unwrap();
        assert!(outcome.is_conflicted(), "got {outcome:?}");
    }

    /// A refusal that is not a conflict must be distinguishable, or every
    /// "cannot proceed" looks like "someone must resolve this".
    #[test]
    fn a_refusal_is_not_a_conflict() {
        let (_tmp, repo) = repo();
        let outcome = rebase(&repo, "no-such-branch").unwrap();
        assert!(
            matches!(outcome, MergeOutcome::Rejected { .. }),
            "rebasing onto a branch that does not exist is a refusal, got {outcome:?}"
        );
        assert!(!outcome.needs_resolution());
    }

    #[test]
    fn branches_can_be_created_listed_and_checked_out() {
        let (_tmp, repo) = repo();
        assert!(!branch_exists(&repo, "feature"));
        create_branch(&repo, "feature").unwrap();
        assert!(branch_exists(&repo, "feature"));

        let branches = list_branches(&repo).unwrap();
        assert!(
            branches.contains(&"feature".to_string()),
            "got {branches:?}"
        );
        assert!(branches.contains(&"main".to_string()), "got {branches:?}");
        // No `*` marker and no detached-HEAD row.
        assert!(
            branches.iter().all(|b| !b.starts_with('*')),
            "got {branches:?}"
        );

        checkout(&repo, "feature").unwrap();
        assert_eq!(current_branch(&repo).as_deref(), Some("feature"));
        assert!(!is_protected_branch("feature"));

        checkout(&repo, "main").unwrap();
        assert_eq!(current_branch(&repo).as_deref(), Some("main"));
        assert!(is_protected_branch("main"));
        assert!(is_protected_branch("master"));
        assert!(is_protected_branch("release/1.2"));
    }

    /// A branch name containing a slash is the case the `--` separator
    /// actually guards: without it git is free to read `feature/fix` as a path,
    /// and `git branch` and `git checkout` resolve it differently.
    #[test]
    fn a_branch_name_that_could_be_read_as_a_path_is_still_a_branch() {
        let (_tmp, repo) = repo();
        create_branch(&repo, "feature/fix").unwrap();
        assert!(branch_exists(&repo, "feature/fix"));
        assert!(
            list_branches(&repo)
                .unwrap()
                .contains(&"feature/fix".to_string()),
            "it should be a real branch, not a path"
        );
        checkout(&repo, "feature/fix").unwrap();
        assert_eq!(current_branch(&repo).as_deref(), Some("feature/fix"));
    }

    /// A name git itself refuses is still refused, and with git's own reason
    /// rather than a generic failure. `git branch -- --help` is not something
    /// ro should paper over.
    #[test]
    fn a_branch_name_git_rejects_is_reported_as_such() {
        let (_tmp, repo) = repo();
        let err = create_branch(&repo, "--help").unwrap_err();
        assert!(
            err.to_string().contains("not a valid branch name"),
            "git's own reason should reach the user, got: {err}"
        );
    }

    #[test]
    fn diff_worktree_and_diff_staged_are_the_two_halves() {
        let (_tmp, repo) = repo();

        // One file staged with one change, then modified again in the
        // worktree. The staged diff and the worktree diff are then different
        // halves of the same file, which is the case that matters: the engine
        // prompt is built from both, and an agent that only sees one of them
        // is reasoning about a state that does not exist.
        std::fs::write(repo.join("README.md"), "# staged\n").unwrap();
        stage_paths(&repo, &[repo.join("README.md")]).unwrap();
        std::fs::write(repo.join("README.md"), "# and then modified\n").unwrap();

        let staged = diff_staged(&repo).unwrap();
        let worktree = diff_worktree(&repo).unwrap();
        assert!(staged.contains("# staged"), "staged was:\n{staged}");
        assert!(
            !staged.contains("# and then modified"),
            "the worktree change must not appear in the staged diff"
        );
        assert!(
            worktree.contains("# and then modified"),
            "worktree was:\n{worktree}"
        );
    }

    /// The typed rule survives: naming your paths stages exactly those.
    #[test]
    fn stage_paths_refuses_an_empty_list() {
        let (_tmp, repo) = repo();
        assert!(stage_paths(&repo, &[]).is_err());
    }

    #[test]
    fn stash_push_reports_whether_there_was_anything_to_stash() {
        let (_tmp, repo) = repo();
        // Nothing dirty: not an error, and nothing stashed.
        assert!(!stash_push(&repo, "auto").unwrap());

        std::fs::write(repo.join("wip.txt"), "work in progress\n").unwrap();
        assert!(stash_push(&repo, "auto").unwrap());
        assert!(
            !repo.join("wip.txt").exists(),
            "a stashed change leaves the worktree"
        );
        assert!(stash_pop(&repo).unwrap());
        assert!(repo.join("wip.txt").exists(), "and popping brings it back");
    }

    #[test]
    fn remote_url_and_author_email_are_read_or_absent() {
        let (_tmp, repo) = repo();
        assert_eq!(
            git_config_user_email(&repo).as_deref(),
            Some("t@example.com")
        );
        assert_eq!(remote_url(&repo, "origin"), None);

        run_in(
            Some(&repo),
            &["remote", "add", "origin", "https://github.com/acme/api.git"],
            &RunOpts::none(),
        )
        .unwrap();
        assert_eq!(
            remote_url(&repo, "origin").as_deref(),
            Some("https://github.com/acme/api.git")
        );
        assert_eq!(remote_url(&repo, "upstream"), None);
    }

    #[test]
    fn a_path_that_is_not_a_repo_says_so() {
        let tmp = TempDir::new().unwrap();
        let not_a_repo = tmp.path().join("plain");
        std::fs::create_dir_all(&not_a_repo).unwrap();
        let err = ensure_repo(&not_a_repo).unwrap_err();
        assert!(
            err.to_string().contains("is not a git repository"),
            "the error should name the real problem, got: {err}"
        );
    }
}
