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
    /// The ref `ahead`/`behind` were measured against, e.g. `origin/main`.
    ///
    /// Named on the row because the number is meaningless without it. The
    /// documented daily loop is `ro status`, *then* `ro sync`, so at the
    /// moment this row is rendered the remote-tracking ref is whatever the
    /// previous fetch left behind — and `behind=0` against a ref from this
    /// morning is not "in sync", it is "nobody has looked since this
    /// morning".
    pub measured_against: Option<String>,
    /// When that ref was last updated locally, in Unix seconds.
    ///
    /// `None` when it could not be read, which is a third answer and not a
    /// synonym for "just now": a repo with reflogs off has no timestamp, and
    /// guessing `now` there is how a stale board gets to look fresh.
    pub measured_against_updated_at: Option<i64>,
    /// Did *this* status call fetch first?
    ///
    /// The distinction the age alone cannot carry. A fetch that failed leaves
    /// the ref exactly as stale as it was, so "how old is the ref" and "is
    /// the number current" are different questions, and a row that answers
    /// only the first reads as fresh after a failed fetch.
    pub measured_after_fetch: bool,
    /// Why the optional fetch did not happen, when one was asked for and did
    /// not succeed.
    ///
    /// Present so a `--fetch` run that could not reach the remote says so
    /// instead of quietly printing the same numbers it would have printed
    /// without the flag.
    pub fetch_error: Option<String>,
    /// Does this repo have an `origin` remote at all?
    ///
    /// A fact, and a separate one from `ahead`/`behind`, because the two
    /// answers a repo can give to "is it in sync" are not the same answer.
    ///
    /// A repo whose `origin` was deleted reports `ahead=0 behind=0` — the
    /// arithmetic is right, there is genuinely nothing to be behind — and
    /// that rendered as **in sync**, indistinguishable from a repo that is
    /// genuinely in sync with a remote. `ro sync` called the same repo out
    /// correctly and `ro status` rendered it clean: two verbs, one repo, two
    /// answers, and the direction that hides a problem is the one that
    /// matters. A user who deleted a remote to stop a repo being pushed and
    /// then ran `ro status` was shown a green row.
    ///
    /// `false` is therefore carried on the row rather than folded into the
    /// counts, and it is `false` for a repo with no branch too — a repo with
    /// no branch has no upstream either, and the two facts are the same fact
    /// seen from two sides.
    ///
    /// It is deliberately **not** a reason to call the repo broken. A
    /// local-only repo is a legitimate thing to have, and this field exists
    /// so it can be *told apart* from a synced one, not so it can be flagged.
    pub has_upstream: bool,
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

    /// Are these ahead/behind numbers known to reflect the remote *now*?
    ///
    /// `ro status` does not fetch — that is a deliberate decision, not an
    /// oversight; see [`status_repo_with`] — so the default row's numbers are
    /// as old as the last fetch, and `behind=0` against a ref from this
    /// morning is not "in sync", it is "nobody has looked since this
    /// morning". `ro status --behind` is the flag that exists to find a repo
    /// the remote has moved past, and over a never-fetched repo it printed
    /// nothing at all, because the number it filtered on was the wrong one.
    ///
    /// Three fields answer this and the renderer needs one boolean, so the
    /// answer is computed here rather than at each call site — the
    /// combination that matters is easy to get wrong by hand, because
    /// `measured_after_fetch` is `true` for a fetch that **failed**
    /// (asserted, with reason, at `a_failed_fetch_is_named_on_the_row_not_
    /// hidden_behind_a_zero`), so "the row says it fetched" is not the same
    /// statement as "the numbers are current".
    ///
    /// A row with no measurement cannot be stale about one, so this is
    /// `false` for a repo with no `origin` and for a row that is not cloned:
    /// there is no remote to be out of date with.
    pub fn is_measurement_stale(&self) -> bool {
        self.measured_against.is_some()
            && !(self.measured_after_fetch && self.fetch_error.is_none())
    }
}

/// Get status for a single tracked repo by ID.
///
/// Queries the state DB for repo metadata, then uses `ro_git::read`
/// functions to inspect the actual repository on disk.
///
/// A **local** measurement: nothing here talks to the network, and the row
/// says so. See [`status_repo_with`] for the opt-in that does fetch, and
/// for why it is not the default.
pub fn status_repo(conn: &Connection, repo_id: &str) -> Result<RepoStatus> {
    status_repo_with(conn, repo_id, false)
}

/// Get status for a single tracked repo, optionally fetching first.
///
/// `fetch_first` is the `--fetch` opt-in. It is **not** the default, and that
/// is a decision rather than an oversight: `ro status` is the first half of
/// the documented daily loop, it runs over a whole fleet, and turning it
/// into a network operation changes what it costs and what it means to run
/// it in a script — a status board that blocks for the length of a fetch is
/// not a board. The default instead says what the number was measured
/// against and when that ref last moved, so a stale base is visible on the
/// row rather than hidden inside a zero, and the flag is there for the user
/// who wants the current answer and is willing to pay for it.
///
/// The flag is spelled as a separate function rather than as a parameter on
/// [`status_repo`] so that every existing caller — and the CLI in
/// `crates/ro` — keeps compiling unchanged, and wiring `--fetch` is one
/// call site rather than a workspace-wide signature change.
pub fn status_repo_with(conn: &Connection, repo_id: &str, fetch_first: bool) -> Result<RepoStatus> {
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
    // The URL the credential is scoped to. Read here rather than inside the
    // `if fetch_first` block so the row shape above stays the single place
    // this query is written.
    let mut clone_url = String::new();

    // The optional fetch, taken **before** the branch is read, because the
    // branch the row is on determines which remote-tracking ref is compared
    // against, and fetching first means that comparison is against the
    // freshest ref the remote has to offer.
    //
    // The failure is swallowed into a field rather than propagated. `ro
    // status` over twenty repos where one remote is unreachable is still
    // nineteen good rows, and the abort-the-listing answer is the same
    // trade-off as every other read on this row: one broken repo must not
    // cost the user the other nineteen. The unreachable remote is named in
    // `fetch_error` so it is not mistaken for a successful measurement.
    let mut fetch_error = None;
    if fetch_first {
        // The row's credential, scoped to the row's own host.
        //
        // `ro status --fetch` fetched **anonymously**: `fetch()` runs with
        // `RunOpts::none()`, `FetchOpts::default()` has `host: None`, and
        // nothing on this path resolved the row's `credential_ref`. So the
        // flag — whose whole purpose is to measure against the freshest ref
        // the remote has — measured against a stale one for every private
        // repo, and reported numbers it had not earned. `ro sync` and `ro
        // ship` both carry the token; `ro status` was the third verb that
        // talks to the remote and the one that did not.
        //
        // The reference is read from the row and the per-repo file with the
        // same precedence `ro sync` uses, so the two verbs cannot disagree
        // about which token a repo is using.
        let credential_ref = {
            let mut reference: Option<String> = None;
            let mut stmt = conn
                .prepare("SELECT credential_ref, clone_url FROM repos WHERE id = ?1")
                .with_context(|| format!("repo with id={repo_id} not found"))?;
            let mut rows = stmt.query([repo_id])?;
            if let Some(row) = rows.next()? {
                reference = row.get::<_, Option<String>>(0)?;
                clone_url = row.get::<_, String>(1)?;
            }
            if let Some(l) = ro_config::local::RepoLocalConfig::load(&path)
                .ok()
                .flatten()
            {
                let mut discard_a = None;
                let mut discard_b = None;
                let mut discard_c = None;
                l.apply_to(
                    &mut discard_a,
                    &mut reference,
                    &mut discard_b,
                    &mut discard_c,
                );
            }
            reference
        };
        // A reference that cannot be resolved is reported in `fetch_error`
        // rather than raised: `ro status` over twenty repos is still nineteen
        // good rows, and this is the row's problem, not the listing's.
        let credential_env =
            match crate::manage::credential_env(&clone_url, credential_ref.as_deref()) {
                Ok(c) => c.env,
                Err(e) => {
                    fetch_error = Some(format!("{e}; the fetch went out anonymously"));
                    Vec::new()
                }
            };
        let fetch_run = ro_git::mutation::RunOpts {
            env: &credential_env,
            ..ro_git::mutation::RunOpts::none()
        };
        match ro_git::mutation::fetch_in(&path, &ro_git::mutation::FetchOpts::default(), &fetch_run)
        {
            // `fetch` returns `Ok` for a git that *ran and failed*:
            // `run_in` reports the exit status in the outcome rather than as
            // an error, and only a spawn failure is an `Err`. Checking
            // `ok()` is what turns "the fetch did not happen" into a row that
            // says so — without it, `--fetch` over an unreachable remote
            // prints exactly the numbers it would have printed without the
            // flag, and the flag becomes a claim rather than a measurement.
            Ok(result) if !result.ok() => {
                fetch_error = Some(format!("fetch did not succeed: {}", result.stderr.trim()));
            }
            Err(e) => fetch_error = Some(format!("fetch could not run: {e:#}")),
            Ok(_) => {}
        }
    }

    let (branch, is_dirty, ahead, behind, unmeasurable_reason, has_upstream) =
        if path.join(".git").exists() {
            // Both reads are kept fallible on purpose. `?` here would abort
            // the *whole* `ro status` listing because one repo has a corrupt
            // `.git` — which is the same bug from the other end: one
            // unmeasurable row takes out the other nineteen, so the user
            // learns about nothing at all. The reasoning was applied to the
            // ahead/behind measurement below and not to these two, which run
            // unconditionally on every row before any of it. A row that
            // cannot be read is a row to *report*, not a reason to fail the
            // board: `ro status` exists to answer "is anything wrong with
            // my repos right now", and this is the answer for one of them.
            let branch_read = ro_git::read::current_branch(&path);
            let branch = branch_read.as_ref().ok().and_then(|o| o.as_ref()).cloned();
            let dirty_read = ro_git::read::is_dirty(&path);
            let is_dirty = dirty_read.as_ref().ok().copied().unwrap_or(false);
            // A `.git` that exists but which git cannot read at all — a
            // `HEAD` that is not a ref, a config that will not parse — is
            // the one state where every measurement below is guaranteed to
            // fail. It is reported as a row rather than propagated, for the
            // same reason the ahead/behind measurement is: one unmeasurable
            // row must not take out the other nineteen. The reason names the
            // checkout rather than guessing a number, so the user can go and
            // look at it.
            if branch_read.is_err() && dirty_read.is_err() {
                (
                    None,
                    false,
                    None,
                    None,
                    Some("checkout is unreadable by git".to_string()),
                    false,
                )
            } else {
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
                            Ok(false) => (
                                branch,
                                is_dirty,
                                Some(0),
                                Some(0),
                                None,
                                // The counts are honest — there is nothing to be
                                // behind — and the row still has to be able to
                                // say so. `false` here is what keeps a repo
                                // whose `origin` was deleted from rendering as
                                // in sync with a remote it no longer has.
                                false,
                            ),
                            // Could not even find out whether there is a remote —
                            // a broken checkout. Not a clean zero.
                            Err(e) => (
                                branch,
                                is_dirty,
                                None,
                                None,
                                Some(format!("cannot determine remotes: {e:#}")),
                                false,
                            ),
                            Ok(true) => {
                                let upstream = format!("origin/{b}");
                                match ro_git::read::ahead_behind(&path, &upstream) {
                                    Ok(ab) => (
                                        branch,
                                        is_dirty,
                                        Some(ab.ahead),
                                        Some(ab.behind),
                                        None,
                                        true,
                                    ),
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
                                        true,
                                    ),
                                }
                            }
                        }
                    }
                    // No branch means there is no upstream to compare against.
                    // That is a fact, not a failure: the repo has nothing to be
                    // behind. `None` on ahead/behind would render as "unknown",
                    // which would be a *less* honest answer than saying so.
                    None => (branch, is_dirty, Some(0), Some(0), None, false),
                }
            }
        } else {
            // Not cloned. The row exists but the worktree does not, so
            // there is genuinely nothing to measure — reported as "not
            // cloned" rather than as a clean zero.
            (
                None,
                false,
                None,
                None,
                Some("not cloned".to_string()),
                false,
            )
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

    // What the comparison above was measured against, and when that ref last
    // moved. Read here, off the branch the row actually resolved, because
    // the whole point is that a reader can tell "in sync" apart from "in
    // sync with a ref from six hours ago" without running anything.
    //
    // The *name* of the ref and the *age* of the ref are separate answers.
    // A repo whose reflogs are off has a perfectly good `origin/main` and no
    // timestamp for it, and collapsing that to "unknown" would throw away the
    // half of the answer that is still true.
    //
    // Gated on `has_upstream` as well as on the counts being present, because
    // the counts alone were not enough: a repo with no `origin` measures
    // `0/0` perfectly well, so the guard below it used to name `origin/main`
    // as the base for a repository that has no origin at all. The ref is not
    // the base of a measurement that had no base, and a JSON consumer reading
    // `measured_against: "origin/main"` was told the counts meant something
    // they do not.
    let (measured_against, measured_against_updated_at) =
        if has_upstream && ahead.is_some() && behind.is_some() {
            match branch.as_deref().or(tracked_branch.as_deref()) {
                Some(b) => (
                    Some(format!("origin/{b}")),
                    ro_git::status::remote_ref_updated_at(&path, "origin", b)
                        .ok()
                        .flatten(),
                ),
                None => (None, None),
            }
        } else {
            (None, None)
        };

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
        measured_against,
        measured_against_updated_at,
        measured_after_fetch: fetch_first,
        fetch_error,
        has_upstream,
    })
}

/// Get status for all tracked repos.
///
/// Local, like [`status_repo`]. See [`status_all_with`] for the fetch opt-in.
pub fn status_all(conn: &Connection) -> Result<Vec<RepoStatus>> {
    status_all_with(conn, false)
}

/// Get status for all tracked repos, optionally fetching each one first.
///
/// `fetch_first` is the same opt-in as [`status_repo_with`], threaded
/// through so `ro status --fetch` is one flag on one command rather than a
/// per-row decision.
pub fn status_all_with(conn: &Connection, fetch_first: bool) -> Result<Vec<RepoStatus>> {
    let mut stmt = conn.prepare("SELECT id FROM repos ORDER BY owner, name")?;
    let ids: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();

    let mut statuses = Vec::new();
    for id in ids {
        statuses.push(status_repo_with(conn, &id, fetch_first)?);
    }
    Ok(statuses)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ro_testkit::{BareRemote, Worktree};
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
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();

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
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();

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
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();

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
        let repo1 =
            crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();
        let repo2 = crate::manage::add(&conn, "bob/proj2", &projects_dir(&tmp), "nested").unwrap();

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
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();

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
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();

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
        let healthy =
            crate::manage::add(&conn, "alice/healthy", &projects_dir(&tmp), "nested").unwrap();
        let broken =
            crate::manage::add(&conn, "alice/broken", &projects_dir(&tmp), "nested").unwrap();

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

        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();
        conn.execute(
            "UPDATE repos SET clone_url = ?1, local_path = ?2 WHERE id = ?3",
            rusqlite::params![
                remote.path().to_string_lossy(),
                seed.path().to_string_lossy(),
                repo.id
            ],
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
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();
        let local_path = projects_dir(&tmp).join("alice").join("proj1");
        leave_conflicting_autostash_pop(&local_path);

        // The precondition, asserted: this is a tree with no operation
        // marker in it at all. The old check looked for those four files
        // and nothing else, so this is the whole reason it said "fine".
        for marker in [
            "MERGE_HEAD",
            "REBASE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
        ] {
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
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();
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
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();
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
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();
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
        assert!(
            status.in_conflict,
            "the source of the conflict is irrelevant"
        );
    }

    /// A repo that is not a git repository at all has no index to
    /// unmerge, and must not be reported as conflicted just because a
    /// `detect` on it failed.
    #[test]
    fn a_directory_that_is_not_a_repo_reports_no_conflict() {
        let (tmp, conn) = setup();
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();
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
        let conflicted =
            crate::manage::add(&conn, "alice/conflicted", &projects_dir(&tmp), "nested").unwrap();
        let clean = crate::manage::add(&conn, "bob/clean", &projects_dir(&tmp), "nested").unwrap();
        let missing =
            crate::manage::add(&conn, "carol/missing", &projects_dir(&tmp), "nested").unwrap();

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
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();
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
        run_git(
            tmp.path(),
            &["clone", "-q", &remote.display().to_string(), "other"],
        );
        run_git(&other, &["config", "user.email", "test@example.com"]);
        run_git(&other, &["config", "user.name", "Test"]);
        commit(&other, "shared.txt", "line1\nREMOTE-VERSION\nline3\n");
        // `HEAD:main` rather than `origin main`: a bare repo created by
        // `git init --bare` leaves HEAD at `master`, so the clone lands on
        // `master` and a hard-coded `main` refspec matches nothing.
        run_git(&other, &["push", "-q", "origin", "HEAD:main"]);

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
        git_out(dir, &["status", "--porcelain"])
            .stdout
            .trim()
            .to_string()
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

    // ── What the row was measured against ──

    /// A status that never fetches measures against whatever the last fetch
    /// left behind, and the row has to say so.
    ///
    /// This is the bug the bead is about. `ro status` is the first half of
    /// the documented daily loop — status, *then* sync — so at the moment
    /// the row is rendered the remote-tracking ref is hours old, and
    /// `behind=0` against a ref from this morning is not "in sync", it is
    /// "nobody has looked since this morning". The old row printed the same
    /// `behind=0` for both, which is how a stale board reads as a green one.
    #[test]
    fn a_status_that_did_not_fetch_says_what_it_measured_against() {
        let (tmp, conn) = setup();
        let remote = ro_testkit::BareRemote::ephemeral();
        let seed = Worktree::empty();
        seed.add_remote("origin", remote.path());
        seed.write("a.txt", "hello\n");
        seed.commit("initial");
        ro_testkit::worktree::run(seed.path(), &["push", "origin", "main"]);

        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();
        conn.execute(
            "UPDATE repos SET clone_url = ?1, local_path = ?2 WHERE id = ?3",
            rusqlite::params![
                remote.path().to_string_lossy(),
                seed.path().to_string_lossy(),
                repo.id
            ],
        )
        .unwrap();

        // The remote moves on. The checkout does not fetch — which is the
        // whole state `ro status` is in during the daily loop.
        let other = tmp.path().join("other");
        // `-b main` so the checkout is on the branch the remote actually has.
        // A bare repo made by `git init --bare` leaves HEAD at `master`,
        // so a plain clone checks out nothing and the push that follows is
        // a root commit — rejected as a non-fast-forward.
        run_git(
            tmp.path(),
            &[
                "clone",
                "-q",
                "-b",
                "main",
                &remote.path().to_string_lossy(),
                "other",
            ],
        );
        run_git(&other, &["config", "user.email", "test@example.com"]);
        run_git(&other, &["config", "user.name", "Test"]);
        std::fs::write(other.join("b.txt"), "remote moved\n").unwrap();
        run_git(&other, &["add", "."]);
        run_git(&other, &["commit", "-q", "-m", "remote moved"]);
        // `HEAD:main` rather than `origin main`: a bare repo created by
        // `git init --bare` leaves HEAD at `master`, so the clone lands on
        // `master` and a hard-coded `main` refspec matches nothing.
        run_git(&other, &["push", "-q", "origin", "HEAD:main"]);

        let status = status_repo(&conn, &repo.id).unwrap();

        // The number is measured against the ref that is there, and the row
        // names it — so a reader can tell "in sync" from "in sync with a
        // ref from before the remote moved".
        assert_eq!(
            status.measured_against.as_deref(),
            Some("origin/main"),
            "the row must name the ref the counts were measured against: {status:?}"
        );
        assert_eq!(
            status.behind,
            Some(0),
            "the local ref is genuinely behind nothing it can see"
        );
        assert!(
            status.measured_against_updated_at.is_some(),
            "a ref that was pushed to has a reflog, and the row must carry \
             when it last moved: {status:?}"
        );
        assert!(
            !status.measured_after_fetch,
            "the default `ro status` does not fetch, and the row must not \
             claim it did"
        );
        assert_eq!(status.fetch_error, None);
    }

    /// The age is the part that stops `behind=0` reading as "in sync".
    ///
    /// A ref that was updated an hour ago and a ref that was updated a minute
    /// ago are both "not behind", and only one of them is a statement about
    /// the remote as it is now.
    #[test]
    fn a_stale_remote_tracking_ref_is_visible_on_the_row() {
        let (tmp, conn) = setup();
        let remote = ro_testkit::BareRemote::ephemeral();
        let seed = Worktree::empty();
        seed.add_remote("origin", remote.path());
        seed.write("a.txt", "hello\n");
        seed.commit("initial");
        ro_testkit::worktree::run(seed.path(), &["push", "origin", "main"]);

        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();
        conn.execute(
            "UPDATE repos SET clone_url = ?1, local_path = ?2 WHERE id = ?3",
            rusqlite::params![
                remote.path().to_string_lossy(),
                seed.path().to_string_lossy(),
                repo.id
            ],
        )
        .unwrap();

        // The remote-tracking ref is rewound to an hour ago, which is the
        // state a checkout is in when the last fetch was an hour ago.
        let reflog = seed
            .path()
            .join(".git")
            .join("logs")
            .join("refs")
            .join("remotes")
            .join("origin")
            .join("main");
        let contents = std::fs::read_to_string(&reflog).unwrap();
        // The head of the line is rewritten and the message after the tab is
        // left alone. Splitting the whole line on whitespace and rejoining
        // would collapse the message's own spaces, and the reader counts
        // fields from the right of the *head* — so a message that had been
        // squashed to one word would shift every field it is counting.
        let (head, message) = contents
            .split_once('\t')
            .map(|(h, m)| (h, Some(m)))
            .unwrap_or((contents.trim_end(), None));
        let mut fields: Vec<&str> = head.split_whitespace().collect();
        let an_hour_ago = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            - 3600;
        // Declared before `fields` so it outlives the borrow taken below.
        let replacement = an_hour_ago.to_string();
        // The timestamp is the second field from the right of the head: the
        // timezone is last, and the committer identity before it may contain
        // spaces. Counted from the left it would land on the identity.
        let idx = fields.len().saturating_sub(2);
        fields[idx] = &replacement;
        let mut rewritten = fields.join(" ");
        if let Some(m) = message {
            rewritten.push('\t');
            rewritten.push_str(m);
        }
        std::fs::write(&reflog, format!("{rewritten}\n")).unwrap();

        let status = status_repo(&conn, &repo.id).unwrap();
        let updated = status
            .measured_against_updated_at
            .expect("the reflog was rewritten, so there is a timestamp on it");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(
            (now - updated).abs() < 3700 && (now - updated) > 3500,
            "the row must carry the age of the ref, not a fresh timestamp: \
             updated={updated} now={now}"
        );
    }

    /// `--fetch` is the opt-in, and it is opt-in on purpose.
    ///
    /// Fetching turns `ro status` from a fast local check into a network
    /// operation over the whole fleet, which is a real cost and a behaviour
    /// change — so the default reports the staleness instead, and the flag is
    /// the way to say "I want the current answer and I am paying for it".
    #[test]
    fn fetch_first_measures_against_the_current_remote() {
        let (tmp, conn) = setup();
        let remote = ro_testkit::BareRemote::ephemeral();
        let seed = Worktree::empty();
        seed.add_remote("origin", remote.path());
        seed.write("a.txt", "hello\n");
        seed.commit("initial");
        ro_testkit::worktree::run(seed.path(), &["push", "origin", "main"]);

        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();
        conn.execute(
            "UPDATE repos SET clone_url = ?1, local_path = ?2 WHERE id = ?3",
            rusqlite::params![
                remote.path().to_string_lossy(),
                seed.path().to_string_lossy(),
                repo.id
            ],
        )
        .unwrap();

        // The remote moves on, and the checkout has not seen it.
        let other = tmp.path().join("other");
        // `-b main` so the checkout is on the branch the remote actually has.
        // A bare repo made by `git init --bare` leaves HEAD at `master`,
        // so a plain clone checks out nothing and the push that follows is
        // a root commit — rejected as a non-fast-forward.
        run_git(
            tmp.path(),
            &[
                "clone",
                "-q",
                "-b",
                "main",
                &remote.path().to_string_lossy(),
                "other",
            ],
        );
        run_git(&other, &["config", "user.email", "test@example.com"]);
        run_git(&other, &["config", "user.name", "Test"]);
        std::fs::write(other.join("b.txt"), "remote moved\n").unwrap();
        run_git(&other, &["add", "."]);
        run_git(&other, &["commit", "-q", "-m", "remote moved"]);
        // `HEAD:main` rather than `origin main`: a bare repo created by
        // `git init --bare` leaves HEAD at `master`, so the clone lands on
        // `master` and a hard-coded `main` refspec matches nothing.
        run_git(&other, &["push", "-q", "origin", "HEAD:main"]);

        // Without the flag: the local ref, and the row says it did not fetch.
        let without = status_repo(&conn, &repo.id).unwrap();
        assert!(!without.measured_after_fetch);
        assert_eq!(without.behind, Some(0));

        // With the flag: the fetch happens, the ref moves, and the row says
        // the number is current. This is the assertion the flag exists for —
        // a `--fetch` that left `behind=0` over a repo that is actually
        // behind would be the same lie wearing a flag.
        let with = status_repo_with(&conn, &repo.id, true).unwrap();
        assert!(
            with.measured_after_fetch,
            "the row must say the measurement followed a fetch"
        );
        assert_eq!(
            with.behind,
            Some(1),
            "after the fetch the repo is one commit behind, and the row must \
             say so: {with:?}"
        );
        assert_eq!(with.fetch_error, None);
    }

    /// A fetch that fails leaves the ref exactly as stale as it was.
    ///
    /// The two questions — how old is the ref, and is the number current —
    /// are separate, and a row that answers only the first reads as fresh
    /// after a failed fetch. The failure is named on the row rather than
    /// propagated, because `ro status` over twenty repos where one remote is
    /// unreachable is still nineteen good rows.
    #[test]
    fn a_failed_fetch_is_named_on_the_row_not_hidden_behind_a_zero() {
        let (tmp, conn) = setup();
        let seed = Worktree::empty();
        // A remote that does not exist: the fetch cannot succeed.
        seed.add_remote("origin", &tmp.path().join("no-such-remote.git"));
        seed.write("a.txt", "hello\n");
        seed.commit("initial");

        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();
        conn.execute(
            "UPDATE repos SET clone_url = ?1, local_path = ?2 WHERE id = ?3",
            rusqlite::params![
                tmp.path().join("no-such-remote.git").to_string_lossy(),
                seed.path().to_string_lossy(),
                repo.id
            ],
        )
        .unwrap();

        let status = status_repo_with(&conn, &repo.id, true).unwrap();
        assert!(
            status.fetch_error.is_some(),
            "a fetch that could not reach the remote must be reported, not \
             absorbed into a clean row: {status:?}"
        );
        assert!(
            status.measured_after_fetch,
            "the flag was passed, so the row must say a fetch was attempted"
        );
    }

    /// The staleness fields reach the machine formats.
    ///
    /// `ro status --format json` and `--format ndjson` serialise this struct
    /// whole, so a field that is not on it is a field no script ever sees.
    #[test]
    fn the_staleness_fields_reach_the_machine_formats() {
        let (tmp, conn) = setup();
        let remote = ro_testkit::BareRemote::ephemeral();
        let seed = Worktree::empty();
        seed.add_remote("origin", remote.path());
        seed.write("a.txt", "hello\n");
        seed.commit("initial");
        ro_testkit::worktree::run(seed.path(), &["push", "origin", "main"]);

        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();
        conn.execute(
            "UPDATE repos SET clone_url = ?1, local_path = ?2 WHERE id = ?3",
            rusqlite::params![
                remote.path().to_string_lossy(),
                seed.path().to_string_lossy(),
                repo.id
            ],
        )
        .unwrap();

        let status = status_repo(&conn, &repo.id).unwrap();
        let row: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&status).unwrap()).unwrap();
        assert_eq!(row["measured_against"], "origin/main");
        assert!(
            row["measured_against_updated_at"].is_number(),
            "the age must reach json: {row}"
        );
        assert_eq!(row["measured_after_fetch"], serde_json::Value::Bool(false));
    }

    // ── A repo with no `origin` is not in sync with one ──

    /// A repo whose `origin` was deleted reports `ahead=0 behind=0`, which
    /// is the arithmetic being right and the answer being wrong.
    ///
    /// There is genuinely nothing to be behind, so the counts are honest —
    /// and that is exactly why they rendered as **in sync**, indistinguishable
    /// from a repo that is genuinely in sync with a remote. `ro sync` called
    /// the same repo out correctly and `ro status` rendered it clean: two
    /// verbs, one repo, two answers, and the direction that hides a problem
    /// is the one that matters. A user who deleted a remote to stop a repo
    /// being pushed, and then ran `ro status`, was shown a green row.
    ///
    /// The fix is a fact on the row, not a verdict: `has_upstream = false`
    /// says what the repo is, and leaves "is it broken" to the user.
    #[test]
    fn a_repo_with_no_origin_is_not_rendered_as_in_sync_with_a_remote() {
        let (tmp, conn) = setup();
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();

        let local_path = projects_dir(&tmp).join("alice").join("proj1");
        init_repo(&local_path);
        commit(&local_path, "a.txt", "hello");
        // No `remote add` at all: this is a local-only repo.

        let status = status_repo(&conn, &repo.id).unwrap();

        // The counts are honest — there is nothing to be behind — and the
        // row still has to be able to say so.
        assert_eq!(status.ahead, Some(0), "there is nothing to be ahead of");
        assert_eq!(status.behind, Some(0), "there is nothing to be behind");
        assert_eq!(status.unmeasurable_reason, None, "this is not a failure");
        assert_eq!(
            status.has_upstream, false,
            "the row must be able to say the repo has no origin"
        );
        assert_eq!(
            status.measured_against, None,
            "there is no ref to name as the base of a measurement that had \
             no base"
        );
        assert_eq!(
            status.is_in_sync(),
            Some(true),
            "the arithmetic is right and stays right: a local-only repo is in \
             sync with nothing. What changed is that the row can now say so."
        );
    }

    /// The same fact, in the two machine formats.
    ///
    /// `ro status --format json` and `--format ndjson` serialise this struct
    /// whole, so a field that is not on it is a field no script ever sees —
    /// and a status that only appears in one format is a status most scripts
    /// never see. A script that greps for `has_upstream: false` to find the
    /// repos whose remote was deleted is the whole reason the field exists.
    #[test]
    fn the_no_upstream_fact_reaches_the_machine_formats() {
        let (tmp, conn) = setup();
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();

        let local_path = projects_dir(&tmp).join("alice").join("proj1");
        init_repo(&local_path);
        commit(&local_path, "a.txt", "hello");

        let status = status_repo(&conn, &repo.id).unwrap();
        let row: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&status).unwrap()).unwrap();
        assert_eq!(
            row["has_upstream"],
            serde_json::Value::Bool(false),
            "the fact must reach json and ndjson: {row}"
        );
        assert_eq!(row["ahead"], 0);
        assert_eq!(row["behind"], 0);
    }

    /// The negative control, and the reason the field is a fact rather than a
    /// verdict.
    ///
    /// A repo with an `origin` is not broken for having one, and a repo with
    /// no `origin` is not broken for lacking one. Without this assertion the
    /// test above would also pass for a `has_upstream` that is always false,
    /// which is the same shape of defect one layer down.
    #[test]
    fn a_repo_with_an_origin_says_it_has_one() {
        let (tmp, conn) = setup();
        let remote = ro_testkit::BareRemote::ephemeral();
        let seed = Worktree::empty();
        seed.add_remote("origin", remote.path());
        seed.write("a.txt", "hello\n");
        seed.commit("initial");
        ro_testkit::worktree::run(seed.path(), &["push", "origin", "main"]);

        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();
        conn.execute(
            "UPDATE repos SET clone_url = ?1, local_path = ?2 WHERE id = ?3",
            rusqlite::params![
                remote.path().to_string_lossy(),
                seed.path().to_string_lossy(),
                repo.id
            ],
        )
        .unwrap();

        let status = status_repo(&conn, &repo.id).unwrap();
        assert_eq!(
            status.has_upstream, true,
            "a repo with an origin must not be told apart from one without"
        );
        assert_eq!(status.measured_against.as_deref(), Some("origin/main"));
    }

    /// A repo on a detached HEAD has no upstream either, and the two facts
    /// are the same fact seen from two sides.
    ///
    /// Built with a real `git checkout --detach`, because the state is
    /// "HEAD points at a commit and no branch names it" and the only honest
    /// way to reach it is for git to put it there — a repo that was merely
    /// `init`ed and never committed is *also* branchless, but for a
    /// different reason, and it does not exercise the branch the row would
    /// otherwise have used.
    #[test]
    fn a_repo_on_a_detached_head_says_it_has_no_upstream() {
        let (tmp, conn) = setup();
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();

        let local_path = projects_dir(&tmp).join("alice").join("proj1");
        init_repo(&local_path);
        commit(&local_path, "a.txt", "hello");
        run_git(&local_path, &["checkout", "-q", "--detach", "HEAD"]);

        let status = status_repo(&conn, &repo.id).unwrap();
        assert_eq!(status.branch, None, "a detached HEAD has no branch");
        assert_eq!(status.has_upstream, false);
        assert_eq!(status.ahead, Some(0));
        assert_eq!(status.behind, Some(0));
    }

    // ── `--behind` over a repo whose remote moved ──

    /// `ro status` does not fetch, so the default row's numbers are as old as
    /// the last fetch — and `ro status --behind` is the flag that exists to
    /// find a repo the remote has moved past. Over a never-fetched repo it
    /// printed nothing at all, because the number it filtered on was the
    /// wrong one: `behind=0` against a ref from this morning is not "in sync",
    /// it is "nobody has looked since this morning".
    ///
    /// The renderer is in `crates/ro` and is not this crate's to change, so
    /// what is asserted here is the **row**: that it carries everything the
    /// renderer needs to surface the staleness, and that the answer is
    /// computed in one place rather than at each call site.
    #[test]
    fn a_repo_whose_remote_moved_carries_the_staleness_on_the_row() {
        let (tmp, conn) = setup();
        let remote = ro_testkit::BareRemote::ephemeral();
        let seed = Worktree::empty();
        seed.add_remote("origin", remote.path());
        seed.write("a.txt", "hello\n");
        seed.commit("initial");
        ro_testkit::worktree::run(seed.path(), &["push", "origin", "main"]);

        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();
        conn.execute(
            "UPDATE repos SET clone_url = ?1, local_path = ?2 WHERE id = ?3",
            rusqlite::params![
                remote.path().to_string_lossy(),
                seed.path().to_string_lossy(),
                repo.id
            ],
        )
        .unwrap();

        // The remote moves on. The checkout does not fetch — which is the
        // whole state `ro status` is in during the daily loop.
        let other = tmp.path().join("other");
        run_git(
            tmp.path(),
            &[
                "clone",
                "-q",
                "-b",
                "main",
                &remote.path().to_string_lossy(),
                "other",
            ],
        );
        run_git(&other, &["config", "user.email", "test@example.com"]);
        run_git(&other, &["config", "user.name", "Test"]);
        for i in 1..=3 {
            std::fs::write(other.join("b.txt"), format!("remote moved {i}\n")).unwrap();
            run_git(&other, &["add", "."]);
            run_git(
                &other,
                &["commit", "-q", "-m", &format!("remote moved {i}")],
            );
        }
        run_git(&other, &["push", "-q", "origin", "HEAD:main"]);

        let status = status_repo(&conn, &repo.id).unwrap();

        // The number is the stale one, and the row says so.
        assert_eq!(
            status.behind,
            Some(0),
            "the local ref is genuinely behind nothing it can see — the \
             number is not the lie, the silence around it is"
        );
        assert_eq!(status.measured_against.as_deref(), Some("origin/main"));
        assert!(
            status.measured_against_updated_at.is_some(),
            "a ref that was pushed to has a reflog, and the row must carry \
             when it last moved: {status:?}"
        );
        assert!(
            !status.measured_after_fetch,
            "the default `ro status` does not fetch, and the row must not \
             claim it did"
        );
        assert_eq!(status.fetch_error, None);
        assert!(
            status.is_measurement_stale(),
            "the row must say the numbers are not current, so `--behind` can \
             surface this repo instead of hiding it: {status:?}"
        );
    }

    /// The other side of the same answer: a row that *did* fetch, and whose
    /// fetch succeeded, is not stale.
    ///
    /// Without this, `is_measurement_stale` returning `true` for everything
    /// would pass the test above — and a flag that surfaces every repo
    /// surfaces none of them.
    #[test]
    fn a_row_that_fetched_successfully_is_not_stale() {
        let (tmp, conn) = setup();
        let remote = ro_testkit::BareRemote::ephemeral();
        let seed = Worktree::empty();
        seed.add_remote("origin", remote.path());
        seed.write("a.txt", "hello\n");
        seed.commit("initial");
        ro_testkit::worktree::run(seed.path(), &["push", "origin", "main"]);

        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();
        conn.execute(
            "UPDATE repos SET clone_url = ?1, local_path = ?2 WHERE id = ?3",
            rusqlite::params![
                remote.path().to_string_lossy(),
                seed.path().to_string_lossy(),
                repo.id
            ],
        )
        .unwrap();

        let other = tmp.path().join("other");
        run_git(
            tmp.path(),
            &[
                "clone",
                "-q",
                "-b",
                "main",
                &remote.path().to_string_lossy(),
                "other",
            ],
        );
        run_git(&other, &["config", "user.email", "test@example.com"]);
        run_git(&other, &["config", "user.name", "Test"]);
        std::fs::write(other.join("b.txt"), "remote moved\n").unwrap();
        run_git(&other, &["add", "."]);
        run_git(&other, &["commit", "-q", "-m", "remote moved"]);
        run_git(&other, &["push", "-q", "origin", "HEAD:main"]);

        let status = status_repo_with(&conn, &repo.id, true).unwrap();
        assert!(status.measured_after_fetch);
        assert_eq!(status.fetch_error, None);
        assert_eq!(
            status.behind,
            Some(1),
            "after the fetch the repo is one commit behind, and the row must \
             say so: {status:?}"
        );
        assert!(
            !status.is_measurement_stale(),
            "a row that fetched and succeeded is current, and must not be \
             flagged as stale: {status:?}"
        );
    }

    /// A fetch that **failed** leaves the ref exactly as stale as it was.
    ///
    /// The trap in the combination: `measured_after_fetch` is `true` for a
    /// fetch that did not succeed, because the flag was passed and the
    /// attempt was made. "The row says it fetched" is therefore not the same
    /// statement as "the numbers are current", and a staleness check that
    /// looked only at the boolean would call a failed fetch fresh.
    #[test]
    fn a_failed_fetch_is_stale_even_though_the_row_says_it_fetched() {
        let (tmp, conn) = setup();
        // A real remote, really pushed to, so `origin/main` exists and the
        // ahead/behind comparison is genuinely made. The remote is then
        // replaced with a path that does not exist, so the *fetch* fails
        // while the row is still measurable — which is the case this is
        // about, and the case a fixture that only ever had a bogus remote
        // cannot reach (there, the comparison fails too and the row is
        // unmeasured rather than stale).
        let remote = ro_testkit::BareRemote::ephemeral();
        let seed = Worktree::empty();
        seed.add_remote("origin", remote.path());
        seed.write("a.txt", "hello\n");
        seed.commit("initial");
        ro_testkit::worktree::run(seed.path(), &["push", "origin", "main"]);

        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();
        conn.execute(
            "UPDATE repos SET clone_url = ?1, local_path = ?2 WHERE id = ?3",
            rusqlite::params![
                remote.path().to_string_lossy(),
                seed.path().to_string_lossy(),
                repo.id
            ],
        )
        .unwrap();
        run_git(
            seed.path(),
            &[
                "remote",
                "set-url",
                "origin",
                &tmp.path().join("no-such-remote.git").to_string_lossy(),
            ],
        );

        let status = status_repo_with(&conn, &repo.id, true).unwrap();
        assert_eq!(
            (status.ahead, status.behind),
            (Some(0), Some(0)),
            "the comparison is still made against the ref the last good \
             fetch left behind: {status:?}"
        );
        assert_eq!(status.measured_against.as_deref(), Some("origin/main"));
        assert!(
            status.measured_after_fetch,
            "the flag was passed, so the row must say a fetch was attempted"
        );
        assert!(
            status.fetch_error.is_some(),
            "a fetch that could not reach the remote must be reported"
        );
        assert!(
            status.is_measurement_stale(),
            "a failed fetch leaves the numbers exactly as stale as they were, \
             and the row must not call them current: {status:?}"
        );
    }

    /// A row with no measurement cannot be stale about one.
    ///
    /// A repo with no `origin` has no remote to be out of date with, and a
    /// row that is not cloned has no numbers at all. Both are `false`, and
    /// both are the honest answer — a staleness flag that fires on a
    /// local-only repo is a flag that fires on everything.
    #[test]
    fn a_row_with_no_measurement_is_not_stale() {
        let (tmp, conn) = setup();
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp), "nested").unwrap();

        let local_path = projects_dir(&tmp).join("alice").join("proj1");
        init_repo(&local_path);
        commit(&local_path, "a.txt", "hello");

        let status = status_repo(&conn, &repo.id).unwrap();
        assert_eq!(status.measured_against, None);
        assert!(
            !status.is_measurement_stale(),
            "there is no remote to be out of date with: {status:?}"
        );
    }
}
