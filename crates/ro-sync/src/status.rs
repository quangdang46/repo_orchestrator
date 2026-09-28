//! Status display engine.
//!
//! Show repo status for all or single repo.
//! Output in text, JSON and NDJSON — the three variants of
//! `ro --format`, applied in `crates/ro` against this row.

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// Status of a tracked repository.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoStatus {
    pub repo_id: String,
    pub owner: String,
    pub name: String,
    pub branch: Option<String>,
    pub is_dirty: bool,
    /// `None` when the ahead/behind comparison could not be made at all.
    ///
    /// This was a bare `u32` defaulting to zero, which rendered a
    /// misconfigured upstream, a corrupt `.git` or a failing disk as
    /// `ahead=0 behind=0` — indistinguishable from a repo genuinely in
    /// sync. Across a fleet that is a green board over several
    /// unmeasured rows, and `None` is the only honest way to say so.
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    /// Why the comparison failed, when it did.
    ///
    /// Carried so the reason survives to the display rather than being
    /// flattened into a boolean — "unknown" alone sends the user hunting
    /// for which of twenty rows is unmeasured and why.
    pub unmeasurable_reason: Option<String>,
    /// Is the current branch one `ro` refuses to commit to directly?
    ///
    /// A fact, not a verdict. It belongs beside dirty and ahead/behind
    /// because every one of those is a thing that decides whether the next
    /// `ro ship` will work, and a user who has to run a command to learn
    /// that `main` is protected learns it one run too late.
    pub is_protected: bool,
    /// Is a merge or rebase in progress here?
    ///
    /// Also a fact, and also in the same list for the same reason: it is
    /// the one condition that makes every other field on the row — the
    /// ahead/behind comparison in particular — not mean what it appears to
    /// mean.
    ///
    /// It is `true` for an unmerged index with **no** operation in
    /// progress too, which is the state a conflicting `git pull
    /// --autostash` leaves behind: the pull fast-forwards, the pop of
    /// the stashed work conflicts, git exits 0, and none of the four
    /// operation markers is written. A tree full of conflict markers
    /// that this field calls `false` is the one answer `ro status` must
    /// never give.
    pub in_conflict: bool,
    /// What the conflict is, when there is one.
    ///
    /// `None` when [`Self::in_conflict`] is `false`, so a consumer can
    /// branch on the boolean and read this only when it matters. The
    /// value is the `ConflictOp` git reported — `merge`, `rebase`,
    /// `cherry-pick`, `revert`, or `stash pop` for the unmerged-index
    /// case above, where no operation marker names the cause.
    ///
    /// It is a `String` rather than the enum so this row stays a plain
    /// data record: it is serialised to JSON and NDJSON as-is, and a
    /// consumer reading `ro status --format ndjson` gets the same word
    /// the text renderer prints.
    pub conflict_kind: Option<String>,
    pub last_synced_at: Option<i64>,
}

impl RepoStatus {
    /// Can this repo be pushed? `false` when the comparison failed is
    /// *not* a claim that it is up to date — it is a refusal to answer.
    pub fn is_in_sync(&self) -> Option<bool> {
        match (self.ahead, self.behind) {
            (Some(0), Some(0)) => Some(true),
            (Some(_), Some(_)) => Some(false),
            _ => None,
        }
    }
}

/// Get status for a single tracked repo by ID.
///
/// Queries the state DB for repo metadata, then uses `ro_git::read`
/// functions to inspect the actual repository on disk.
pub fn status_repo(conn: &Connection, repo_id: &str) -> Result<RepoStatus> {
    let mut stmt =
        conn.prepare("SELECT owner, name, branch, local_path FROM repos WHERE id = ?1")?;
    let row = stmt
        .query_row([repo_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .with_context(|| format!("repo with id={repo_id} not found"))?;

    let (owner, name, tracked_branch, local_path) = row;
    let path = std::path::PathBuf::from(&local_path);

    let (branch, is_dirty, ahead, behind, unmeasurable_reason) = if path.join(".git").exists() {
        let branch = ro_git::read::current_branch(&path)?;
        let is_dirty = ro_git::read::is_dirty(&path)?;
        // The cached default_branch is gone as of V4. The branch git
        // reports is authoritative; the tracked branch is only a
        // fallback for a detached HEAD.
        let upstream_branch = branch.as_ref().or(tracked_branch.as_ref());
        match upstream_branch {
            // The measurement is kept even when it fails. `?` here
            // would abort the *whole* `ro status` listing because one
            // repo has a typo'd upstream — which is the same bug from
            // the other end: one unmeasurable row takes out the other
            // nineteen, so the user learns about nothing at all.
            Some(b) => {
                // A repo with no `origin` has no upstream to compare
                // against, and that is a *fact about the repository*,
                // not a failed measurement — so it is asked first, and
                // answered from `has_remote` rather than by letting a
                // guaranteed `rev-list` failure stand in for the answer.
                // Without this check a perfectly healthy local repo
                // reported "unknown" forever, which teaches users to
                // ignore the unknown marker and so hides the rows that
                // are genuinely broken.
                match ro_git::read::has_remote(&path, "origin") {
                    Ok(false) => (branch, is_dirty, Some(0), Some(0), None),
                    // Could not even find out whether there is a remote —
                    // a broken checkout. Not a clean zero.
                    Err(e) => (
                        branch,
                        is_dirty,
                        None,
                        None,
                        Some(format!("cannot determine remotes: {e:#}")),
                    ),
                    Ok(true) => {
                        let upstream = format!("origin/{b}");
                        match ro_git::read::ahead_behind(&path, &upstream) {
                            Ok(ab) => (branch, is_dirty, Some(ab.ahead), Some(ab.behind), None),
                            // The remote exists but `origin/main` does not
                            // — a wrong branch name, a fetch that has not
                            // run, or a typo in the tracked branch. Named
                            // precisely so the fix is obvious.
                            Err(e) => (
                                branch,
                                is_dirty,
                                None,
                                None,
                                Some(format!("no upstream ref {upstream}: {e:#}")),
                            ),
                        }
                    }
                }
            }
            // No branch means there is no upstream to compare against.
            // That is a fact, not a failure: the repo has nothing to be
            // behind. `None` on ahead/behind would render as "unknown",
            // which would be a *less* honest answer than saying so.
            None => (branch, is_dirty, Some(0), Some(0), None),
        }
    } else {
        // Not cloned. The row exists but the worktree does not, so
        // there is genuinely nothing to measure — reported as "not
        // cloned" rather than as a clean zero.
        (None, false, None, None, Some("not cloned".to_string()))
    };

    let last_synced_at: Option<i64> = conn
        .query_row(
            "SELECT MAX(r.started_at) FROM sync_results sr
             JOIN runs r ON sr.run_id = r.id
             WHERE sr.repo_id = ?1 AND sr.status = 'success'",
            [repo_id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .ok()
        .flatten();

    // Resolved before the struct literal, because two of the fields want
    // to *look* at the branch and one wants to own it.
    let effective = branch.clone().or(tracked_branch.clone());
    let is_protected = effective
        .as_deref()
        .is_some_and(ro_git::primitives::is_protected_branch);

    // The conflict read is kept apart from the `?`-using block above on
    // purpose. A repo whose index is unmerged is a repo to *report*, not
    // a reason to fail the row: `ro status` exists to answer "is
    // anything wrong with my repos right now", and this is the answer
    // for one of them. The error is swallowed only after the state has
    // been read, and only because a repo that cannot be read at all is
    // already covered by the `not cloned` branch above.
    let conflict = ro_git::conflict::detect(&path).ok().flatten();
    let in_conflict = conflict.is_some();
    let conflict_kind = conflict.as_ref().map(|c| c.op.to_string());

    Ok(RepoStatus {
        repo_id: repo_id.to_string(),
        owner,
        name,
        branch: effective,
        is_dirty,
        ahead,
        behind,
        unmeasurable_reason,
        // Both are read from the working copy rather than from the row,
        // because both are properties of *this* checkout right now: a repo
        // can be tracked against `main` while sitting on a feature branch,
        // and it is the branch it is on that the next `ro ship` will use.
        is_protected,
        in_conflict,
        conflict_kind,
        last_synced_at,
    })
}

/// Get status for all tracked repos.
pub fn status_all(conn: &Connection) -> Result<Vec<RepoStatus>> {
    let mut stmt = conn.prepare("SELECT id FROM repos ORDER BY owner, name")?;
    let ids: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();

    let mut statuses = Vec::new();
    for id in ids {
        statuses.push(status_repo(conn, &id)?);
    }
    Ok(statuses)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    fn setup() -> (TempDir, Connection) {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        (tmp, conn)
    }

    fn projects_dir(tmp: &TempDir) -> PathBuf {
        tmp.path().join("projects")
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

    fn init_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        run_git(dir, &["init", "-q", "-b", "main"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
        run_git(dir, &["config", "user.name", "Test"]);
    }

    fn commit(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-q", "-m", &format!("add {name}")]);
    }

    #[test]
    fn status_repo_missing_repo() {
        let (tmp, conn) = setup();
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp)).unwrap();

        let status = status_repo(&conn, &repo.id).unwrap();
        assert_eq!(status.repo_id, repo.id);
        assert_eq!(status.owner, "alice");
        assert_eq!(status.name, "proj1");
        assert_eq!(status.branch, None);
        assert!(!status.is_dirty);
        // The row exists but nothing was cloned, so there is genuinely
        // nothing to measure. This is the case the bead is about: it used
        // to read as a clean `ahead=0 behind=0`, which is a green row over
        // a repo that does not exist.
        assert_eq!(status.ahead, None);
        assert_eq!(status.behind, None);
        assert_eq!(status.unmeasurable_reason.as_deref(), Some("not cloned"));
        assert_eq!(status.is_in_sync(), None);
        assert_eq!(status.last_synced_at, None);
    }

    #[test]
    fn status_repo_clean_repo() {
        let (tmp, conn) = setup();
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp)).unwrap();

        let local_path = projects_dir(&tmp).join("alice").join("proj1");
        init_repo(&local_path);
        commit(&local_path, "a.txt", "hello");

        let status = status_repo(&conn, &repo.id).unwrap();
        assert_eq!(status.repo_id, repo.id);
        assert_eq!(status.owner, "alice");
        assert_eq!(status.name, "proj1");
        assert_eq!(status.branch, Some("main".to_string()));
        assert!(!status.is_dirty);
        assert_eq!(status.ahead, Some(0));
        assert_eq!(status.behind, Some(0));
        assert_eq!(status.unmeasurable_reason, None);
        assert_eq!(status.last_synced_at, None);
    }

    #[test]
    fn status_repo_dirty_repo() {
        let (tmp, conn) = setup();
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp)).unwrap();

        let local_path = projects_dir(&tmp).join("alice").join("proj1");
        init_repo(&local_path);
        commit(&local_path, "a.txt", "hello");
        std::fs::write(local_path.join("a.txt"), "changed").unwrap();

        let status = status_repo(&conn, &repo.id).unwrap();
        assert_eq!(status.branch, Some("main".to_string()));
        assert!(status.is_dirty);
    }

    #[test]
    fn status_all_returns_all_repos() {
        let (tmp, conn) = setup();
        let repo1 = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp)).unwrap();
        let repo2 = crate::manage::add(&conn, "bob/proj2", &projects_dir(&tmp)).unwrap();

        let statuses = status_all(&conn).unwrap();
        assert_eq!(statuses.len(), 2);
        let ids: Vec<String> = statuses.into_iter().map(|s| s.repo_id).collect();
        assert!(ids.contains(&repo1.id));
        assert!(ids.contains(&repo2.id));
    }

    /// The bead's own bug, as a test.
    ///
    /// A repo whose tracked branch does not exist on the remote used to
    /// render `ahead=0 behind=0` — a green row for a repo nobody measured.
    /// It must now be explicitly unknown, and must carry the reason so the
    /// user knows *which* of twenty rows is unmeasured and why.
    #[test]
    fn an_unmeasurable_repo_is_unknown_not_in_sync() {
        let (tmp, conn) = setup();
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp)).unwrap();

        let local_path = projects_dir(&tmp).join("alice").join("proj1");
        init_repo(&local_path);
        commit(&local_path, "a.txt", "hello");
        run_git(&local_path, &["remote", "add", "origin", "../upstream.git"]);

        // A branch the remote does not have.
        run_git(
            &local_path,
            &["checkout", "-q", "-b", "does-not-exist-upstream"],
        );
        commit(&local_path, "b.txt", "more");

        let status = status_repo(&conn, &repo.id).unwrap();

        assert_eq!(status.ahead, None, "an unmeasured row must not claim 0");
        assert_eq!(status.behind, None, "an unmeasured row must not claim 0");
        assert_eq!(
            status.is_in_sync(),
            None,
            "is_in_sync must refuse to answer, not answer yes"
        );
        let reason = status
            .unmeasurable_reason
            .as_deref()
            .expect("an unmeasurable repo must say why");
        assert!(
            reason.contains("does-not-exist-upstream"),
            "the reason must name the ref that could not be resolved: {reason}"
        );
    }

    /// The inverse, and the reason `has_remote` is consulted: a local repo
    /// with no remote at all is **in sync with nothing**, which is a fact,
    /// not a failure. Marking it unknown would be a different lie.
    #[test]
    fn a_local_repo_with_no_remote_is_in_sync_not_unknown() {
        let (tmp, conn) = setup();
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp)).unwrap();

        let local_path = projects_dir(&tmp).join("alice").join("proj1");
        init_repo(&local_path);
        commit(&local_path, "a.txt", "hello");

        let status = status_repo(&conn, &repo.id).unwrap();
        assert_eq!(status.ahead, Some(0));
        assert_eq!(status.behind, Some(0));
        assert_eq!(status.unmeasurable_reason, None);
        assert_eq!(status.is_in_sync(), Some(true));
    }

    /// One broken row must not take out the other nineteen.
    ///
    /// The other way to be wrong: propagate the error with `?` and abort
    /// the whole listing. Then a single misconfigured repo means the user
    /// sees *no* statuses at all, which is no better than a green board.
    #[test]
    fn one_broken_repo_does_not_hide_the_others() {
        let (tmp, conn) = setup();
        let healthy = crate::manage::add(&conn, "alice/healthy", &projects_dir(&tmp)).unwrap();
        let broken = crate::manage::add(&conn, "alice/broken", &projects_dir(&tmp)).unwrap();

        let healthy_path = projects_dir(&tmp).join("alice").join("healthy");
        init_repo(&healthy_path);
        commit(&healthy_path, "a.txt", "hello");

        // Tracked but never cloned: the row is real, the worktree is not.
        let broken_path = projects_dir(&tmp).join("alice").join("broken");
        std::fs::create_dir_all(&broken_path).unwrap();

        let statuses = status_all(&conn).unwrap();
        assert_eq!(statuses.len(), 2, "both rows must be reported");

        let healthy_status = statuses.iter().find(|s| s.repo_id == healthy.id).unwrap();
        let broken_status = statuses.iter().find(|s| s.repo_id == broken.id).unwrap();

        assert_eq!(healthy_status.is_in_sync(), Some(true));
        assert_eq!(broken_status.is_in_sync(), None);
        assert!(broken_status.unmeasurable_reason.is_some());
    }

    /// The downstream consequence of the ro-rne.10 FK bug, asserted so it
    /// cannot come back silently.
    ///
    /// `last_synced_at` is a `JOIN` across `sync_results` and `runs`. While
    /// `sync_results` was permanently empty — every insert failed its
    /// foreign key and the error was discarded — this field was
    /// permanently `None` for every repo, forever, and nothing said so.
    /// A status that cannot report when it last synced is a status whose
    /// `None` a user learns to ignore.
    #[test]
    fn a_real_sync_makes_last_synced_at_appear() {
        use ro_testkit::Worktree;

        let (tmp, conn) = setup();
        let remote = ro_testkit::BareRemote::ephemeral();
        let seed = Worktree::empty();
        seed.add_remote("origin", remote.path());
        seed.write("a.txt", "hello\n");
        seed.commit("initial");
        ro_testkit::worktree::run(seed.path(), &["push", "origin", "main"]);

        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp)).unwrap();
        conn.execute(
            "UPDATE repos SET clone_url = ?1 WHERE id = ?2",
            rusqlite::params![remote.path().to_string_lossy(), repo.id],
        )
        .unwrap();

        // Before any sync, there is nothing to report. `None` is correct
        // here and must not be confused with the broken state below.
        assert_eq!(status_repo(&conn, &repo.id).unwrap().last_synced_at, None);

        crate::sync::sync_all(&conn, &crate::sync::SyncOptions::default(), &[]).unwrap();

        let after = status_repo(&conn, &repo.id).unwrap().last_synced_at;
        assert!(
            after.is_some(),
            "after a real sync, last_synced_at must be populated. It stayed \
             None forever when sync_results could not be written to."
        );
    }

    #[test]
    fn status_repo_not_found() {
        let (_, conn) = setup();
        let err = status_repo(&conn, "nonexistent-id").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    // ── The unmerged index ──

    /// A repo whose tree is full of conflict markers is a repo in
    /// conflict, however it got there.
    ///
    /// `git pull --autostash` is the way in: the pull fast-forwards, the
    /// pop of the stashed work conflicts, **git exits 0**, and it writes
    /// none of `MERGE_HEAD`, `REBASE_HEAD`, `CHERRY_PICK_HEAD` or
    /// `REVERT_HEAD`. `ro status` used to answer `in_conflict = false`
    /// for exactly that tree, and said nothing about the markers — the
    /// user learns their repo is fine hours before something tries to
    /// build on it.
    #[test]
    fn a_conflicting_autostash_pop_reports_in_conflict() {
        let (tmp, conn) = setup();
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp)).unwrap();
        let local_path = projects_dir(&tmp).join("alice").join("proj1");
        leave_conflicting_autostash_pop(&local_path);

        // The precondition, asserted: this is a tree with no operation
        // marker in it at all. The old check looked for those four files
        // and nothing else, so this is the whole reason it said "fine".
        for marker in ["MERGE_HEAD", "REBASE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD"] {
            assert!(
                !local_path.join(".git").join(marker).exists(),
                "{marker} must be absent, or this is not the case under test"
            );
        }
        assert_eq!(
            porcelain(&local_path),
            "UU shared.txt",
            "the index is unmerged, and it is the only evidence there is"
        );

        let status = status_repo(&conn, &repo.id).unwrap();
        assert!(
            status.in_conflict,
            "a tree with conflict markers in it is not a healthy repo"
        );
        assert_eq!(
            status.conflict_kind.as_deref(),
            Some("stash pop"),
            "the row must say *why*, so a user is not sent hunting for a \
             merge that is not running"
        );
    }

    /// The negative control. Without it the test above passes on a
    /// `detect` that answers `true` unconditionally, which is the same
    /// shape of defect one layer down.
    #[test]
    fn a_clean_repo_reports_no_conflict() {
        let (tmp, conn) = setup();
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp)).unwrap();
        let local_path = projects_dir(&tmp).join("alice").join("proj1");
        init_repo(&local_path);
        commit(&local_path, "a.txt", "hello");

        let status = status_repo(&conn, &repo.id).unwrap();
        assert!(!status.in_conflict, "a clean repo is not in conflict");
        assert_eq!(status.conflict_kind, None);
    }

    /// A dirty repo is not a conflicted repo. The two are different
    /// facts and a user acting on one of them does the wrong thing to
    /// the other — `git add .` fixes a dirty tree, and does nothing for
    /// a conflict.
    #[test]
    fn a_dirty_repo_reports_no_conflict() {
        let (tmp, conn) = setup();
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp)).unwrap();
        let local_path = projects_dir(&tmp).join("alice").join("proj1");
        init_repo(&local_path);
        commit(&local_path, "a.txt", "hello");
        std::fs::write(local_path.join("a.txt"), "changed").unwrap();

        let status = status_repo(&conn, &repo.id).unwrap();
        assert!(status.is_dirty);
        assert!(!status.in_conflict, "dirty is not conflict");
        assert_eq!(status.conflict_kind, None);
    }

    /// The signal is about the index, so it is not specific to the one
    /// command that produces it most often. A conflicting `git merge
    /// --squash` writes no `MERGE_HEAD` and lands the same way.
    #[test]
    fn unmerged_entries_from_another_source_are_also_reported() {
        let (tmp, conn) = setup();
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp)).unwrap();
        let local_path = projects_dir(&tmp).join("alice").join("proj1");
        init_repo(&local_path);
        commit(&local_path, "f.txt", "line1\nbase\nline3\n");
        run_git(&local_path, &["checkout", "-q", "-b", "feat"]);
        commit(&local_path, "f.txt", "line1\ntheirs\nline3\n");
        run_git(&local_path, &["checkout", "-q", "main"]);
        commit(&local_path, "f.txt", "line1\nours\nline3\n");
        let out = git_out(&local_path, &["merge", "--squash", "feat"]);
        assert!(
            !out.success,
            "the squash merge must have conflicted, or there is nothing to report"
        );
        assert_eq!(porcelain(&local_path), "UU f.txt");

        let status = status_repo(&conn, &repo.id).unwrap();
        assert!(status.in_conflict, "the source of the conflict is irrelevant");
    }

    /// A repo that is not a git repository at all has no index to
    /// unmerge, and must not be reported as conflicted just because a
    /// `detect` on it failed.
    #[test]
    fn a_directory_that_is_not_a_repo_reports_no_conflict() {
        let (tmp, conn) = setup();
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp)).unwrap();
        let local_path = projects_dir(&tmp).join("alice").join("proj1");
        std::fs::create_dir_all(&local_path).unwrap();

        let status = status_repo(&conn, &repo.id).unwrap();
        assert!(!status.in_conflict);
        assert_eq!(status.conflict_kind, None);
    }

    /// One bad row must not take out the others.
    ///
    /// A conflicted repo is the newest way to make `ro status` abort, and
    /// an abort over a fleet teaches the user the same thing a green
    /// board does: that this command cannot be trusted.
    #[test]
    fn a_conflicted_repo_does_not_hide_the_rest_of_the_fleet() {
        let (tmp, conn) = setup();
        let conflicted = crate::manage::add(&conn, "alice/conflicted", &projects_dir(&tmp)).unwrap();
        let clean = crate::manage::add(&conn, "bob/clean", &projects_dir(&tmp)).unwrap();
        let missing = crate::manage::add(&conn, "carol/missing", &projects_dir(&tmp)).unwrap();

        leave_conflicting_autostash_pop(&projects_dir(&tmp).join("alice").join("conflicted"));

        let clean_path = projects_dir(&tmp).join("bob").join("clean");
        init_repo(&clean_path);
        commit(&clean_path, "a.txt", "hello");

        // Tracked but never cloned — the row is real, the worktree is not.
        std::fs::create_dir_all(projects_dir(&tmp).join("carol").join("missing")).unwrap();

        let statuses = status_all(&conn).unwrap();
        assert_eq!(statuses.len(), 3, "every row must be reported");

        let by_id = |id: &str| {
            statuses
                .iter()
                .find(|s| s.repo_id == id)
                .unwrap_or_else(|| panic!("no row for {id}"))
                .clone()
        };

        let conflicted_status = by_id(&conflicted.id);
        assert!(conflicted_status.in_conflict);

        let clean_status = by_id(&clean.id);
        assert!(
            !clean_status.in_conflict,
            "one conflicted repo must not colour the others"
        );
        assert_eq!(clean_status.is_in_sync(), Some(true));

        let missing_status = by_id(&missing.id);
        assert_eq!(
            missing_status.unmeasurable_reason.as_deref(),
            Some("not cloned")
        );
        assert!(!missing_status.in_conflict);
    }

    /// The signal has to survive every machine format.
    ///
    /// `ro status --format json` and `--format ndjson` both serialise
    /// this struct whole, so a field that is not on it is a field no
    /// script ever sees — and a status that only appears in one format
    /// is a status most scripts never see.
    #[test]
    fn the_conflict_signal_reaches_the_machine_formats() {
        let (tmp, conn) = setup();
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp)).unwrap();
        let local_path = projects_dir(&tmp).join("alice").join("proj1");
        leave_conflicting_autostash_pop(&local_path);

        let status = status_repo(&conn, &repo.id).unwrap();

        // What both `json` and `ndjson` emit for this row.
        let row: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&status).unwrap()).unwrap();
        assert_eq!(row["in_conflict"], serde_json::Value::Bool(true));
        assert_eq!(row["conflict_kind"], "stash pop");
    }

    // ── fixtures ──

    /// A bare repo whose `HEAD` points at `main`.
    ///
    /// The `symbolic-ref` is load-bearing, not decoration: a fresh
    /// `git init --bare` leaves `HEAD` at `refs/heads/master`, so a
    /// clone of it checks out an empty `master`, the first commit lands
    /// on a branch the remote has no ref for, and the push fails with
    /// "src refspec main does not match any" — a fixture that never
    /// reaches the state it exists to describe.
    fn bare_remote(p: &Path) {
        std::fs::create_dir_all(p).unwrap();
        run_git(p, &["init", "--bare", "-q", "."]);
        run_git(p, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    }

    /// A repo whose index is unmerged and which has **no** operation
    /// marker: the state a conflicting `git pull --autostash` leaves.
    ///
    /// Real git against a real bare remote, because the thing asserted
    /// is exactly what git leaves on disk. A fixture that staged an
    /// unmerged entry by hand would only prove that the reader reads
    /// what the writer wrote — and `git update-index --index-info` is
    /// the one way to build this state without the pull that produces
    /// it, which is why it is not used here.
    fn leave_conflicting_autostash_pop(local_path: &Path) {
        let tmp = tempfile::TempDir::new().unwrap();
        let remote = tmp.path().join("remote.git");
        bare_remote(&remote);

        init_repo(local_path);
        commit(local_path, "shared.txt", "line1\nline2\nline3\n");
        run_git(
            local_path,
            &["remote", "add", "origin", &remote.display().to_string()],
        );
        run_git(local_path, &["push", "-q", "origin", "main"]);

        // A second checkout rewrites the same line and pushes it.
        let other = tmp.path().join("other");
        run_git(tmp.path(), &["clone", "-q", &remote.display().to_string(), "other"]);
        run_git(&other, &["config", "user.email", "test@example.com"]);
        run_git(&other, &["config", "user.name", "Test"]);
        commit(&other, "shared.txt", "line1\nREMOTE-VERSION\nline3\n");
        run_git(&other, &["push", "-q", "origin", "main"]);

        // The local checkout rewrites the same line and does not commit,
        // so the pull has to stash it and the pop is what conflicts.
        std::fs::write(
            local_path.join("shared.txt"),
            "line1\nLOCAL-VERSION\nline3\n",
        )
        .unwrap();
        // Without tracking, `git pull` refuses to guess a branch and the
        // fixture never reaches the state under test.
        run_git(
            local_path,
            &["branch", "--set-upstream-to=origin/main", "main"],
        );
        let out = git_out(local_path, &["pull", "--autostash"]);
        assert!(
            out.success,
            "git calls a conflicting autostash pop a success — that is the \
             entire reason the exit code cannot be the signal. stderr: {}",
            out.stderr
        );
    }

    /// `git status --porcelain`, the index's own answer.
    fn porcelain(dir: &Path) -> String {
        git_out(dir, &["status", "--porcelain"]).stdout.trim().to_string()
    }

    /// A git call whose *output* is wanted, and whose failure is not
    /// fatal — several steps below are expected to fail, and the ones
    /// that are not have their exit code asserted by name.
    struct GitOut {
        success: bool,
        stdout: String,
        stderr: String,
    }

    fn git_out(dir: &Path, args: &[&str]) -> GitOut {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .output()
            .unwrap_or_else(|e| panic!("git {args:?} in {} could not run: {e}", dir.display()));
        GitOut {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }
}
