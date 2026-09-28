//! Sync engine implementation.
//!
//! Read repos from SQLite → create run record → for each repo:
//! acquire fs4 lock → clone if missing → fetch → pull per strategy
//! → record pre/post OID → release lock.

use anyhow::{Context, Result};
use clap::ValueEnum;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Instant;

use crate::manage::TrackedRepo;

/// Update strategy for syncing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[derive(ValueEnum)]
pub enum SyncStrategy {
    FfOnly,
    Rebase,
    Merge,
}

impl std::fmt::Display for SyncStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncStrategy::FfOnly => write!(f, "ff-only"),
            SyncStrategy::Rebase => write!(f, "rebase"),
            SyncStrategy::Merge => write!(f, "merge"),
        }
    }
}

/// Options for a sync operation.
#[derive(Debug, Clone)]
pub struct SyncOptions {
    pub strategy: SyncStrategy,
    pub autostash: bool,
    pub timeout_secs: u32,
    /// Preview only; do not clone/pull.
    pub dry_run: bool,
    /// Only clone missing repos; skip pulling existing.
    pub clone_only: bool,
    /// Only pull existing repos; skip cloning missing.
    pub pull_only: bool,
    /// Delete remote-tracking refs whose branch is gone upstream.
    ///
    /// `git remote prune`, not `git fetch --prune`. The narrower of the
    /// two on purpose: this removes local bookkeeping for branches that no
    /// longer exist and never touches the remote, so it is safe to run
    /// across a fleet without asking first.
    pub prune: bool,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            strategy: SyncStrategy::FfOnly,
            autostash: false,
            timeout_secs: 30,
            dry_run: false,
            clone_only: false,
            pull_only: false,
            prune: false,
        }
    }
}

/// Result of syncing a single repo.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncResult {
    pub repo_id: String,
    pub action: String,
    pub status: String,
    pub duration_ms: u64,
    pub error: Option<String>,
    pub pre_oid: Option<String>,
    pub post_oid: Option<String>,
}

/// The statuses `sync_repo` can put in [`SyncResult::status`], and what each
/// one means for the run.
///
/// Written down because the run-level verdict is a decision about *this*
/// list, and a decision that is not written down is a decision that gets
/// re-made wrong. Every status the module emits is here:
///
/// | status                | meaning                                    | fails the run |
/// |-----------------------|--------------------------------------------|---------------|
/// | `success`             | the repo is where the remote says it is    | no            |
/// | `dry_run`             | nothing was asked of the repo              | no            |
/// | `skipped`             | deliberately not touched, with a reason     | no            |
/// | `error`               | git refused, or ro could not run it        | **yes**       |
/// | `autostash_conflict`  | the pull landed, the pop did not           | **yes**       |
/// | `conflict`            | the pull itself stopped on a conflict      | **yes**       |
///
/// The three that fail are the three where the repo is **not** in the state
/// the user asked for and the user has to do something about it. The three
/// that do not are the three where the repo is exactly as it should be, or
/// was never touched.
///
/// `autostash_conflict` and `conflict` are the two rows that were missing,
/// and they are missing for the same reason: the aggregation counted
/// `status == "error"` and nothing else, so a run whose only bad repo was one
/// of these exited 0 and wrote `exit_code = 0` into `runs`. A script driving
/// ro saw success. That is the lie the whole `--autostash` work exists to
/// prevent, reintroduced one layer up.
///
/// `conflict` is a failure **for the run** and nothing more. `ro ship`
/// continues from a conflicted repo, but it does so by reading the
/// on-disk state — the merge/rebase markers and unmerged index entries, via
/// `ro_git::conflict::detect` — not by matching this string, so failing the
/// run does not stop a ship that is entitled to continue. The two answers
/// are about different questions: "did this run do what was asked?" and
/// "can the pipeline go on?".
///
/// The skips are deliberately **not** failures, and this is the part that
/// must not be "fixed" by making them so. `skipped_dirty` without
/// `--autostash` is the tool refusing to throw work away, which is the
/// correct answer; `skipped_unpushed` is a branch that has never been
/// pushed, which is the normal state of a new branch; `skipped_clone` under
/// `--pull-only` is the flag being honoured. A fleet of twenty healthy repos
/// where three are dirty must exit 0, or the exit code stops meaning
/// anything and scripts start ignoring it.
pub(crate) const STATUS_SUCCESS: &str = "success";
pub(crate) const STATUS_DRY_RUN: &str = "dry_run";
pub(crate) const STATUS_SKIPPED: &str = "skipped";
pub(crate) const STATUS_ERROR: &str = "error";
pub(crate) const STATUS_AUTOSTASH_CONFLICT: &str = "autostash_conflict";
pub(crate) const STATUS_CONFLICT: &str = "conflict";

/// Whether one repo's outcome makes the whole run fail.
///
/// Kept as a function rather than an inline comparison so the rule has one
/// home and one set of tests, and so a status added later cannot be
/// forgotten here — an unknown status is treated as a failure, because a
/// status nothing has classified is not a status anyone has vouched for.
pub(crate) fn status_fails_run(status: &str) -> bool {
    match status {
        STATUS_SUCCESS | STATUS_DRY_RUN | STATUS_SKIPPED => false,
        STATUS_ERROR | STATUS_AUTOSTASH_CONFLICT | STATUS_CONFLICT => true,
        other => {
            tracing::warn!(status = %other, "an unclassified sync status is failing the run");
            true
        }
    }
}

/// Sync a single repo.
pub fn sync_repo(
    conn: &Connection,
    repo: &TrackedRepo,
    opts: &SyncOptions,
    run_id: &str,
) -> Result<SyncResult> {
    let start = Instant::now();
    let local = Path::new(&repo.local_path);

    let pre_oid = if local.join(".git").exists() {
        ro_git::read::head_oid(local).ok().flatten()
    } else {
        None
    };

    // `git remote prune` removes local remote-tracking refs for branches
    // the remote no longer has. It touches only local bookkeeping, so it is
    // run before the pull rather than after: the pull refetches what is
    // still there, and anything the prune deleted was already gone upstream.
    if opts.prune && !opts.dry_run && local.join(".git").exists() {
        // The remote name is a **required** argument. `git remote prune`
        // with no name is a usage error — exit 129, "usage: git remote
        // prune [<options>] <name>" — and `run_in` returns `Ok` for a
        // non-zero exit, so the old `let _ =` discarded a command that had
        // never run. The flag existed in `--help` with a full paragraph
        // describing exactly what it did, and changed nothing.
        //
        // `origin` is the name the pull below uses, so it is the name that
        // is pruned: a repo whose remote is called something else has no
        // remote-tracking refs under `origin` to prune, and pruning a
        // different name would be pruning a remote this sync never read.
        let pruned = ro_git::mutation::run_in(
            Some(local),
            &["remote", "prune", "origin"],
            &ro_git::mutation::RunOpts::none(),
        );
        if let Err(e) = &pruned {
            // A prune that cannot run is not a reason to fail a sync — the
            // pull is the work, and the prune is housekeeping the user asked
            // for once, not on every run. It is reported, though: silent
            // is how this flag became a no-op nobody noticed.
            tracing::warn!("git remote prune origin failed in {}: {e}", local.display());
        }
    }

    if opts.dry_run {
        let action = if local.join(".git").exists() {
            "would_pull"
        } else {
            "would_clone"
        };
        let duration = start.elapsed().as_millis() as u64;
        return Ok(SyncResult {
            repo_id: repo.id.clone(),
            action: action.into(),
            status: "dry_run".into(),
            duration_ms: duration,
            error: None,
            pre_oid: pre_oid.clone(),
            post_oid: pre_oid,
        });
    }

    if !local.join(".git").exists() {
        if opts.pull_only {
            let duration = start.elapsed().as_millis() as u64;
            return Ok(SyncResult {
                repo_id: repo.id.clone(),
                action: "skipped_clone".into(),
                status: "skipped".into(),
                duration_ms: duration,
                error: None,
                pre_oid: pre_oid.clone(),
                post_oid: None,
            });
        }
        // Clone
        let clone_opts = ro_git::mutation::CloneOpts {
            // V4 dropped the cached default_branch; the tracked branch is the
            // only hint left for a clone target.
            branch: repo.branch.clone(),
            ..Default::default()
        };
        match ro_git::mutation::clone(&repo.clone_url, local, &clone_opts) {
            Ok(outcome) => {
                if !outcome.result.ok() {
                    let duration = start.elapsed().as_millis() as u64;
                    let err_msg = outcome.result.stderr.trim().to_string();
                    record_result(
                        conn,
                        run_id,
                        &repo.id,
                        "clone",
                        "error",
                        duration,
                        Some(&err_msg),
                        &pre_oid,
                        &None,
                    )?;
                    return Ok(SyncResult {
                        repo_id: repo.id.clone(),
                        action: "clone".into(),
                        status: "error".into(),
                        duration_ms: duration,
                        error: Some(err_msg),
                        pre_oid,
                        post_oid: None,
                    });
                }
                let post_oid = ro_git::read::head_oid(local).ok().flatten();
                let duration = start.elapsed().as_millis() as u64;
                record_result(
                    conn, run_id, &repo.id, "clone", "success", duration, None, &pre_oid, &post_oid,
                )?;
                Ok(SyncResult {
                    repo_id: repo.id.clone(),
                    action: "clone".into(),
                    status: "success".into(),
                    duration_ms: duration,
                    error: None,
                    pre_oid,
                    post_oid,
                })
            }
            Err(e) => {
                let duration = start.elapsed().as_millis() as u64;
                let err_msg = format!("{e:#}");
                record_result(
                    conn,
                    run_id,
                    &repo.id,
                    "clone",
                    "error",
                    duration,
                    Some(&err_msg),
                    &pre_oid,
                    &None,
                )?;
                Ok(SyncResult {
                    repo_id: repo.id.clone(),
                    action: "clone".into(),
                    status: "error".into(),
                    duration_ms: duration,
                    error: Some(err_msg),
                    pre_oid: pre_oid.clone(),
                    post_oid: None,
                })
            }
        }
    } else {
        if opts.clone_only {
            let duration = start.elapsed().as_millis() as u64;
            return Ok(SyncResult {
                repo_id: repo.id.clone(),
                action: "skipped_pull".into(),
                status: "skipped".into(),
                duration_ms: duration,
                error: None,
                pre_oid: pre_oid.clone(),
                post_oid: pre_oid,
            });
        }
        // Fetch + pull
        let fetch_opts = ro_git::mutation::FetchOpts::default();
        if let Err(e) = ro_git::mutation::fetch(local, &fetch_opts) {
            let duration = start.elapsed().as_millis() as u64;
            let err_msg = format!("{e:#}");
            record_result(
                conn,
                run_id,
                &repo.id,
                "fetch",
                "error",
                duration,
                Some(&err_msg),
                &pre_oid,
                &pre_oid,
            )?;
            return Ok(SyncResult {
                repo_id: repo.id.clone(),
                action: "fetch".into(),
                status: "error".into(),
                duration_ms: duration,
                error: Some(err_msg),
                pre_oid: pre_oid.clone(),
                post_oid: pre_oid,
            });
        }

        let branch = ro_git::read::current_branch(local)
            .ok()
            .flatten()
            .or(repo.branch.clone())
            .unwrap_or_else(|| "main".into());

        let pull_strategy = match opts.strategy {
            SyncStrategy::FfOnly => ro_git::mutation::PullStrategy::FastForwardOnly,
            SyncStrategy::Rebase => ro_git::mutation::PullStrategy::Rebase,
            SyncStrategy::Merge => ro_git::mutation::PullStrategy::Merge,
        };

        let pull_opts = ro_git::mutation::PullOpts {
            branch: Some(branch.clone()),
            strategy: pull_strategy,
            autostash: opts.autostash,
            ..Default::default()
        };

        // A dirty worktree is skipped, not clobbered.
        //
        // There is deliberately no `--force` on this command. A fleet tool
        // that discards uncommitted work because someone typed a flag
        // while rushing is a tool that gets uninstalled after the first
        // accident. `--autostash` is the way to say "yes, I mean it", and
        // the skip names the count so the user can decide.
        //
        // The flag is now passed all the way down to git (it used to stop
        // here, at bypassing this skip, so git refused the merge with
        // "Your local changes ... would be overwritten" and the sync
        // reported an error for a tree the user had explicitly asked to
        // sync). Passing it is only half the fix: a pop that conflicts
        // still exits 0, which the `autostash_hold` arm below refuses to
        // report as success.
        let dirty_count = dirty_file_count(local)?;
        if dirty_count > 0 && !opts.autostash && !opts.dry_run {
            let duration = start.elapsed().as_millis() as u64;
            let detail = format!("{dirty_count} uncommitted change(s) (use --autostash)");
            record_result(
                conn,
                run_id,
                &repo.id,
                "skipped_dirty",
                "skipped",
                duration,
                Some(&detail),
                &pre_oid,
                &pre_oid,
            )?;
            return Ok(SyncResult {
                repo_id: repo.id.clone(),
                action: "skipped_dirty".into(),
                status: "skipped".into(),
                duration_ms: duration,
                error: None,
                pre_oid: pre_oid.clone(),
                post_oid: pre_oid,
            });
        }

        let pull_result = ro_git::mutation::pull(local, &pull_opts);
        let post_oid = ro_git::read::head_oid(local).ok().flatten();

        let duration = start.elapsed().as_millis() as u64;

        match pull_result {
            // A pull that exited 0 while the autostash stayed stashed.
            //
            // **This arm has to be first.** It used to sit below
            // `Ok(outcome) if outcome.result.ok()`, which matches this case
            // too — a conflicting pop exits 0 — so the arm was unreachable
            // and every conflicting autostash was filed as a clean `updated`.
            // The comment that stood here claimed the opposite ordering was
            // the requirement, which is how it survived review.
            //
            // `git pull --autostash` returns success even when the pop
            // conflicts, so without this arm a green sync row means "your
            // uncommitted work is in `stash@{0}` and your tree has conflict
            // markers". That is worse than the old honest skip: the work is
            // recoverable, but only by someone who knows to look.
            Ok(outcome) if outcome.autostash_hold.is_some() => {
                let detail = outcome.autostash_hold.clone().unwrap_or_else(|| {
                    "the autostash did not pop; your uncommitted work is in the stash".to_string()
                });
                record_result(
                    conn,
                    run_id,
                    &repo.id,
                    "pull",
                    STATUS_AUTOSTASH_CONFLICT,
                    duration,
                    Some(&detail),
                    &pre_oid,
                    &post_oid,
                )?;
                Ok(SyncResult {
                    repo_id: repo.id.clone(),
                    action: "pull".into(),
                    status: STATUS_AUTOSTASH_CONFLICT.into(),
                    duration_ms: duration,
                    error: Some(detail),
                    pre_oid,
                    post_oid,
                })
            }
            // `pull` returns `Ok` when the command *ran*, not when it
            // succeeded — a non-zero git exit is reported in the outcome's
            // result. Matching only on `Ok` therefore recorded a failed
            // pull as `status = success`, which is the worst possible
            // failure: a rebase that hit a conflict, or a rejected
            // non-fast-forward under `--ff-only`, was indistinguishable
            // from a clean update. The clone branch above already checks
            // this; this one did not.
            Ok(outcome) if outcome.result.ok() => {
                let action = if outcome.already_up_to_date {
                    "already_up_to_date"
                } else {
                    "updated"
                };
                record_result(
                    conn, run_id, &repo.id, action, "success", duration, None, &pre_oid, &post_oid,
                )?;
                Ok(SyncResult {
                    repo_id: repo.id.clone(),
                    action: action.into(),
                    status: "success".into(),
                    duration_ms: duration,
                    error: None,
                    pre_oid,
                    post_oid,
                })
            }
            // The command ran and failed. Distinguish a conflict from a
            // plain refusal, because the two need different work: a
            // conflict is a stage `ro ship` knows how to continue from,
            // and a refusal is not.
            Ok(outcome) if outcome.conflict => {
                let stderr = outcome.result.stderr.trim();
                let detail = if stderr.is_empty() {
                    "git reported a conflict with no message"
                } else {
                    stderr
                };
                let err_msg = format!("conflicted: {detail}");
                record_result(
                    conn,
                    run_id,
                    &repo.id,
                    "pull",
                    STATUS_CONFLICT,
                    duration,
                    Some(&err_msg),
                    &pre_oid,
                    &post_oid,
                )?;
                Ok(SyncResult {
                    repo_id: repo.id.clone(),
                    action: "pull".into(),
                    status: STATUS_CONFLICT.into(),
                    duration_ms: duration,
                    error: Some(err_msg),
                    pre_oid,
                    post_oid,
                })
            }
            Ok(outcome) => {
                // A branch that has never been pushed has no remote-tracking
                // ref, and `git pull` answers "couldn't find remote ref".
                //
                // That is the *normal* state of a branch someone just made
                // with `git checkout -b`, so recording it as a sync failure
                // fills a fleet board with red for repos that are exactly
                // as they should be. It is a skip with a reason, not an
                // error — and there is nothing to fetch, which is the point.
                let stderr = outcome.result.stderr.trim().to_string();
                if stderr.contains("couldn't find remote ref")
                    || stderr.contains("does not appear to be a git repository")
                {
                    let detail = format!(
                        "{branch} has no remote branch yet — it has never been pushed. \
                         Nothing to fetch."
                    );
                    record_result(
                        conn,
                        run_id,
                        &repo.id,
                        "pull",
                        "skipped_unpushed",
                        duration,
                        Some(&detail),
                        &pre_oid,
                        &post_oid,
                    )?;
                    return Ok(SyncResult {
                        repo_id: repo.id.clone(),
                        action: "skipped_unpushed".into(),
                        status: "skipped".into(),
                        duration_ms: duration,
                        error: None,
                        pre_oid,
                        post_oid,
                    });
                }
                let err_msg = if stderr.is_empty() {
                    format!("git pull failed with status {}", outcome.result.status)
                } else {
                    stderr
                };
                record_result(
                    conn,
                    run_id,
                    &repo.id,
                    "pull",
                    "error",
                    duration,
                    Some(&err_msg),
                    &pre_oid,
                    &post_oid,
                )?;
                Ok(SyncResult {
                    repo_id: repo.id.clone(),
                    action: "pull".into(),
                    status: "error".into(),
                    duration_ms: duration,
                    error: Some(err_msg),
                    pre_oid,
                    post_oid,
                })
            }
            Err(e) => {
                let err_msg = format!("{e:#}");
                record_result(
                    conn,
                    run_id,
                    &repo.id,
                    "pull",
                    "error",
                    duration,
                    Some(&err_msg),
                    &pre_oid,
                    &post_oid,
                )?;
                Ok(SyncResult {
                    repo_id: repo.id.clone(),
                    action: "pull".into(),
                    status: "error".into(),
                    duration_ms: duration,
                    error: Some(err_msg),
                    pre_oid,
                    post_oid,
                })
            }
        }
    }
}

/// Sync all tracked repos.
///
/// Opens a real `runs` row first. It used to mint a `run_id` with
/// `Uuid::new_v4()` and never insert it, so every `sync_results` write
/// violated a foreign key and — because `record_result` discarded the
/// error — every result vanished. The run row is what makes those writes
/// legal, so it is created here rather than assumed.
///
/// The run is finalised on **every** path including the failing ones. A
/// run left open means "still going", and a fleet that crashed halfway
/// would leave a run that never ends and a status that never updates.
/// Sync every repo, or just `selected` when the caller named some.
///
/// `selected` is a list of repo ids, empty meaning "all". The caller
/// resolves names to ids before calling, because a name that matches
/// nothing is a **usage** error and only the caller knows whether the user
/// named a repo: `ro sync` on an empty registry is a legitimate question
/// with the answer "nothing to do", and `ro sync nonexistent` is a typo
/// that deserves to be reported rather than absorbed.
pub fn sync_all(
    conn: &Connection,
    opts: &SyncOptions,
    selected: &[String],
) -> Result<Vec<SyncResult>> {
    // Archived and disabled rows are excluded here rather than at each
    // call site. `list(conn, None)` returns every row, so a sync that did
    // not filter would reach into repos the user had explicitly retired —
    // and a sync that touches an archived repo is a sync that does
    // something you asked it not to do.
    let all = crate::manage::list(conn, None)?;
    let repos: Vec<_> = all
        .into_iter()
        .filter(|r| !r.archived && !r.disabled)
        .filter(|r| selected.is_empty() || selected.contains(&r.id))
        .collect();
    let run = ro_jobs::open_run(conn, "sync", &[]).context("opening the sync run record")?;
    let run_id = run.id.clone();

    let mut results = Vec::new();
    for repo in &repos {
        // A failed *write* aborts the run rather than being folded into a
        // per-repo result: the audit trail is missing, so every result in
        // this run is untrustworthy, including the ones already recorded.
        results.push(sync_repo(conn, repo, opts, &run_id)?);
    }

    // Exit code reflects the worst outcome, so a run that recorded a failure
    // is not filed as a clean one. `sync_results.status` already carries the
    // per-repo detail; this is the run-level verdict, and it is the same
    // verdict a script gets from the process exit status.
    //
    // It used to count `status == "error"` and nothing else, which meant
    // `autostash_conflict` — a real per-repo failure, with the user's work
    // in a stash — left the run at exit 0. See `status_fails_run` for the
    // rule and, more importantly, for why the skips stay out of it.
    let failed = results
        .iter()
        .filter(|r| status_fails_run(&r.status))
        .count();
    // Counted separately from `failed` because the two need different work:
    // a pull conflict is a stage `ro ship` knows how to continue from, and
    // an autostash conflict is a stash the user has to resolve by hand. A
    // single "conflicted" number would hide which.
    let conflicted = results
        .iter()
        .filter(|r| r.status == STATUS_AUTOSTASH_CONFLICT || r.status == STATUS_CONFLICT)
        .count();
    let exit_code = run_exit_code(&results);
    ro_jobs::finalize_run(conn, &run_id, exit_code).context("finalising the sync run record")?;

    tracing::info!(
        run = %run_id,
        repos = results.len(),
        failed,
        conflicted,
        "sync run complete"
    );
    Ok(results)
}

/// The process exit code for a finished sync run: 0 when every repo is in a
/// state the user asked for or was deliberately not touched, 1 otherwise.
///
/// Public because the CLI has to print this same number, and a caller that
/// had to re-derive the rule would be a second place for it to go stale.
pub fn run_exit_code(results: &[SyncResult]) -> i32 {
    if results.iter().any(|r| status_fails_run(&r.status)) {
        1
    } else {
        0
    }
}

/// Write one row to `sync_results`.
///
/// The error is **returned, not discarded**. This used to be
/// `let _ = conn.execute(...)`, and that one character was the whole bug:
/// `sync_results.run_id` is `NOT NULL REFERENCES runs(id)` and
/// `ro_state::open_db` turns `PRAGMA foreign_keys = ON`, so an insert with
/// a `run_id` that has no `runs` row fails every time — and the failure
/// was thrown away. The table was therefore permanently empty while every
/// caller believed it had recorded a result, and `ro status`'s
/// `last_synced_at` was permanently NULL.
///
/// Discarding an error from a write is only safe when the write is
/// genuinely optional. This one is the audit trail.
#[allow(clippy::too_many_arguments)]
fn record_result(
    conn: &Connection,
    run_id: &str,
    repo_id: &str,
    action: &str,
    status: &str,
    duration_ms: u64,
    error: Option<&str>,
    pre_oid: &Option<String>,
    post_oid: &Option<String>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO sync_results (run_id, repo_id, action, status, duration_ms, error, pre_oid, post_oid)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![run_id, repo_id, action, status, duration_ms as i64, error, pre_oid, post_oid],
    )
    .with_context(|| {
        format!(
            "recording the {action}/{status} result for repo {repo_id} \
             under run {run_id} — is the run row missing?"
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    pub(crate) fn setup() -> (TempDir, Connection) {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        (tmp, conn)
    }

    /// Run git without depending on the machine's own configuration.
    ///
    /// A developer has `user.email` in their `~/.gitconfig`; a CI
    /// runner does not, so a fixture that commits is green on one and
    /// "Author identity unknown" on the other. The identity comes from
    /// the environment git documents for exactly this, and the config
    /// files are neutralised — `NUL` on Windows, `/dev/null` elsewhere.
    pub(crate) fn run_git(dir: &Path, args: &[&str]) -> std::process::Output {
        let mut cmd = std::process::Command::new("git");
        cmd.args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com");
        #[cfg(windows)]
        cmd.env("GIT_CONFIG_GLOBAL", "NUL")
            .env("GIT_CONFIG_SYSTEM", "NUL");
        #[cfg(not(windows))]
        cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        cmd.output().expect("git runs")
    }

    pub(crate) fn init_bare_remote(dir: &Path) -> PathBuf {
        let remote = dir.join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        run_git(&remote, &["init", "--bare", "-b", "main"]);
        remote
    }

    pub(crate) fn commit_to_remote(dir: &Path, remote: &Path, name: &str, content: &str) {
        let clone_dir = dir.join("tmp-clone");
        std::fs::create_dir_all(&clone_dir).unwrap();
        run_git(&clone_dir, &["clone", &remote.to_string_lossy(), "."]);
        run_git(&clone_dir, &["config", "user.email", "test@example.com"]);
        run_git(&clone_dir, &["config", "user.name", "Test"]);
        std::fs::write(clone_dir.join(name), content).unwrap();
        run_git(&clone_dir, &["add", "."]);
        run_git(&clone_dir, &["commit", "-m", &format!("add {name}")]);
        run_git(&clone_dir, &["push", "origin", "main"]);
        std::fs::remove_dir_all(&clone_dir).unwrap();
    }

    /// The bug this bead is named for, as a test.
    ///
    /// `sync_results.run_id` is `NOT NULL REFERENCES runs(id)`, foreign
    /// keys are on, and `sync_all` used to mint a `run_id` without ever
    /// inserting a `runs` row — so every insert failed and `let _ =`
    /// discarded the error. `sync_results` was therefore permanently
    /// empty, `ro status`'s `last_synced_at` permanently NULL, and the
    /// health scorer's failed-sync penalty permanently zero.
    ///
    /// The negative control below is what makes this assertion real.
    #[test]
    fn a_sync_run_actually_records_rows() {
        let (tmp, conn) = setup();
        let remote = init_bare_remote(tmp.path());
        commit_to_remote(tmp.path(), &remote, "a.txt", "hello");

        let local_path = tmp.path().join("local").join("proj1");
        let repo_id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES (?1, 'github.com', 'alice', 'proj1', ?2, ?3, ?4, ?5)",
            params![
                repo_id,
                remote.to_string_lossy().to_string(),
                local_path.to_string_lossy().to_string(),
                now,
                now
            ],
        )
        .unwrap();

        let results = sync_all(&conn, &SyncOptions::default(), &[]).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, "success", "the sync itself should work");

        // The whole point: the row is in the database, not just returned.
        let recorded: i64 = conn
            .query_row("SELECT COUNT(*) FROM sync_results", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            recorded, 1,
            "sync_results must not be empty; the insert was failing on a \
             foreign key and the error was being discarded"
        );

        // And the run it hangs off must exist, which is what made the
        // insert legal in the first place.
        let runs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE command = 'sync'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(runs, 1, "a sync run must create exactly one runs row");

        // A run left open means "still going". A finished run is recorded
        // as finished, or `ro status` can never show a last-synced time.
        let open: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE ended_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(open, 0, "a finished sync run must be finalised");
    }

    /// `--prune` must actually prune.
    ///
    /// `git remote prune` with **no remote name is a usage error** — exit
    /// 129, "usage: git remote prune [<options>] <name>" — and `run_in`
    /// returns `Ok` for a non-zero exit, so the old `let _ =` discarded a
    /// command that had never run. The flag was in `--help` with a full
    /// paragraph describing what it did, and changed nothing.
    #[test]
    fn prune_removes_stale_remote_tracking_refs() {
        let (tmp, conn) = setup();
        let remote = init_bare_remote(tmp.path());
        commit_to_remote(tmp.path(), &remote, "a.txt", "hello");

        let local_path = tmp.path().join("local").join("proj1");
        let repo_id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES (?1, 'github.com', 'alice', 'proj1', ?2, ?3, ?4, ?5)",
            params![
                repo_id,
                remote.to_string_lossy().to_string(),
                local_path.to_string_lossy().to_string(),
                now,
                now
            ],
        )
        .unwrap();

        // Sync once so the checkout exists and has a remote-tracking ref.
        let results = sync_all(&conn, &SyncOptions::default(), &[]).unwrap();
        assert_eq!(results[0].status, "success");

        // A stale remote-tracking ref: a branch the remote no longer has.
        // `update-ref` rather than a real push, so the fixture does not
        // depend on the remote's ref layout.
        let head = run_git(&local_path, &["rev-parse", "HEAD"]);
        let head = String::from_utf8_lossy(&head.stdout).trim().to_string();
        run_git(
            &local_path,
            &["update-ref", "refs/remotes/origin/gone", &head],
        );
        let before = run_git(&local_path, &["for-each-ref", "refs/remotes/origin/gone"]);
        assert!(
            before.status.success(),
            "the stale ref must exist before the prune"
        );

        let opts = SyncOptions {
            prune: true,
            ..Default::default()
        };
        let results = sync_all(&conn, &opts, &[]).unwrap();
        assert_eq!(results[0].status, "success");

        let after = run_git(&local_path, &["for-each-ref", "refs/remotes/origin/gone"]);
        assert!(
            after.stdout.is_empty(),
            "--prune must remove the stale remote-tracking ref; it is still there"
        );
    }

    /// `--autostash` is the flag that makes a dirty worktree syncable, and
    /// the case that needs it is the one where the remote moved **on the file
    /// the tree is dirty in**. Anything less is a false pass: if the remote
    /// only touched a different file, `git pull --ff-only` fast-forwards
    /// straight past the dirty file, leaves it dirty, and reports success —
    /// so the test passes on a build that never stashed anything and never
    /// popped anything. `git stash list` is empty in that case too, because
    /// no stash was ever created. Asserting on the pull's *exit status*
    /// alone therefore proves nothing about `--autostash` at all.
    ///
    /// Here the remote rewrites the same line the tree has uncommitted, so a
    /// pull without `--autostash` is refused by git itself:
    ///
    ///     Your local changes to the following files would be overwritten
    ///     by merge
    ///
    /// That refusal is what the flag is supposed to prevent. The run must
    /// stash, pull, and pop.
    ///
    /// Two facts about git that shape the fix, both measured here rather
    /// than assumed:
    ///
    ///  * `git pull --autostash` **exits 0 even when the pop conflicts.** On
    ///    a real tree with the remote and the worktree both rewriting the
    ///    same line, the pull fast-forwards, the pop conflicts, and git
    ///    prints `Applying autostash resulted in conflicts` and returns
    ///    success. So `status == "success"` alone does not mean the work
    ///    came back; a fix has to read the tree or the stream, not the code.
    ///  * A failed pop **keeps** the stash (`git stash list` still shows
    ///    `stash@{0}: autostash`), and the work is then reachable only
    ///    through that stash. The plan's rule — a failed pop is a per-repo
    ///    failure naming `git stash list`, never a proceed on a half-popped
    ///    tree — is therefore a real requirement, not a nicety: a silent
    ///    success here is a user's uncommitted work that nothing mentions.
    ///
    /// **What this test asserts was corrected, and why.** It used to require
    /// `status == "success"` and an empty stash — which is exactly the lie
    /// the two facts above describe. Its own scenario (both sides rewriting
    /// the same file) makes the pop conflict, so those assertions could only
    /// ever pass on a build that reported a false success. It now asserts
    /// what actually happens, and the **stash existing at all** is what
    /// proves the flag reached git: without `--autostash` on the command
    /// line git refuses the merge before it stashes anything, and there is
    /// nothing in the stash list to find.
    #[test]
    fn autostash_pulls_a_tree_that_is_dirty_in_a_file_the_remote_moved() {
        let (tmp, conn) = setup();
        let remote = init_bare_remote(tmp.path());
        commit_to_remote(tmp.path(), &remote, "a.txt", "base");

        // A real clone, so there is a real `origin` and a real worktree.
        let local_path = tmp.path().join("local").join("proj1");
        std::fs::create_dir_all(&local_path).unwrap();
        run_git(&local_path, &["clone", &remote.to_string_lossy(), "."]);
        run_git(&local_path, &["config", "user.email", "test@example.com"]);
        run_git(&local_path, &["config", "user.name", "Test"]);

        // The tree goes dirty, in the file the remote is about to change.
        std::fs::write(local_path.join("a.txt"), "LOCAL UNCOMMITTED WORK").unwrap();
        let dirty = run_git(&local_path, &["status", "--porcelain"]);
        assert!(
            dirty.status.success() && !dirty.stdout.is_empty(),
            "the worktree must actually be dirty before the sync, or this test \
             proves nothing: {}",
            String::from_utf8_lossy(&dirty.stdout)
        );

        // Now the remote moves, rewriting that exact file.
        commit_to_remote(tmp.path(), &remote, "a.txt", "REMOTE MOVED AHEAD");

        let repo_id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES (?1, 'github.com', 'alice', 'proj1', ?2, ?3, ?4, ?5)",
            params![
                repo_id,
                remote.to_string_lossy().to_string(),
                local_path.to_string_lossy().to_string(),
                now,
                now
            ],
        )
        .unwrap();

        let results = sync_all(
            &conn,
            &SyncOptions {
                autostash: true,
                ..SyncOptions::default()
            },
            &[],
        )
        .unwrap();

        // The flag reached git, so git stashed and pulled rather than
        // refusing the merge. The pop then conflicted — both sides rewrote
        // the same file — and that must be reported, not filed as success.
        assert_eq!(
            results[0].status, "autostash_conflict",
            "--autostash must let a dirty tree pull, and a pop that conflicted \
             must be reported as such rather than as a clean sync. error: {:?}",
            results[0].error
        );
        assert!(
            results[0]
                .error
                .as_deref()
                .is_some_and(|e| e.contains("git stash list")),
            "the failure must name `git stash list`, where the work now lives: {:?}",
            results[0].error
        );

        // The pull actually landed: the remote's version is in the tree,
        // underneath the conflict markers the pop left behind.
        let after = std::fs::read_to_string(local_path.join("a.txt")).unwrap();
        assert!(
            after.contains("REMOTE MOVED AHEAD"),
            "the remote's version must have landed, got: {after}"
        );
        assert!(
            after.contains("LOCAL UNCOMMITTED WORK"),
            "the local work must still be present, in the conflict: {after}"
        );

        // And the stash holds the work. This is the assertion that proves
        // the flag reached git at all: a build that never passed
        // `--autostash` was refused before stashing anything, so there
        // would be nothing here to find.
        let stashes = run_git(&local_path, &["stash", "list"]);
        assert!(
            stashes.status.success()
                && String::from_utf8_lossy(&stashes.stdout).contains("autostash"),
            "--autostash must stash what it could not pop, so the work is \
             recoverable: {}",
            String::from_utf8_lossy(&stashes.stdout)
        );
    }

    /// The flag has to reach git, not merely bypass ro's own dirty-skip.
    ///
    /// The test above asserts the run succeeded, which is the outcome the
    /// flag exists for — but it cannot distinguish "git was told to
    /// autostash" from "the pull happened to be clean". This one can,
    /// because it reads the tree git was actually handed.
    ///
    /// The remote and the worktree rewrite the **same line of the same
    /// file**, so the pop cannot apply cleanly. A build that never passes
    /// `--autostash` to git is refused before it stashes anything: the
    /// status is an error, the stash list is empty, and the local edit is
    /// still sitting uncommitted in the tree. A build that passes the flag
    /// and then reads the exit code reports success over a tree of conflict
    /// markers. Only a build that passes the flag **and** reads the tree
    /// reports the third thing, which is the truth.
    #[test]
    fn autostash_conflict_is_reported_as_a_failure_naming_the_stash() {
        let (tmp, conn) = setup();
        let remote = init_bare_remote(tmp.path());
        commit_to_remote(tmp.path(), &remote, "a.txt", "base");

        let local_path = tmp.path().join("local").join("proj1");
        std::fs::create_dir_all(&local_path).unwrap();
        run_git(&local_path, &["clone", &remote.to_string_lossy(), "."]);
        run_git(&local_path, &["config", "user.email", "test@example.com"]);
        run_git(&local_path, &["config", "user.name", "Test"]);

        // Both sides rewrite the same line, so the pop must conflict.
        std::fs::write(local_path.join("a.txt"), "LOCAL UNCOMMITTED WORK").unwrap();
        commit_to_remote(tmp.path(), &remote, "a.txt", "REMOTE MOVED AHEAD");

        let repo_id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES (?1, 'github.com', 'alice', 'proj1', ?2, ?3, ?4, ?5)",
            params![
                repo_id,
                remote.to_string_lossy().to_string(),
                local_path.to_string_lossy().to_string(),
                now,
                now
            ],
        )
        .unwrap();

        let results = sync_all(
            &conn,
            &SyncOptions {
                autostash: true,
                ..SyncOptions::default()
            },
            &[],
        )
        .unwrap();

        // Not success. `git pull --autostash` exits 0 here, so a run that
        // read the exit code would file this as a clean sync over a tree
        // holding conflict markers.
        assert_ne!(
            results[0].status, "success",
            "a conflicting autostash pop must not be recorded as a success, \
             got {:?}",
            results[0]
        );
        assert_eq!(
            results[0].status, "autostash_conflict",
            "the failure must be named for what it is, got {:?}",
            results[0]
        );

        // The message names the recovery, because the work is only reachable
        // through the stash and nothing else mentions it.
        let error = results[0]
            .error
            .as_deref()
            .expect("a failure must carry the reason");
        assert!(
            error.contains("git stash list"),
            "the message must name `git stash list`, where the work now lives: {error}"
        );
        assert!(
            error.contains("stash pop"),
            "the message must say how to get the work back: {error}"
        );

        // And the stash really does still hold the work — asserted on git's
        // own list, not on the message.
        let stashes = run_git(&local_path, &["stash", "list"]);
        assert!(
            stashes.status.success()
                && String::from_utf8_lossy(&stashes.stdout).contains("autostash"),
            "the failed pop must keep the stash, so the work is recoverable: {}",
            String::from_utf8_lossy(&stashes.stdout)
        );

        // The tree really does hold conflict markers, which is the state a
        // green sync row would have hidden.
        let after = std::fs::read_to_string(local_path.join("a.txt")).unwrap();
        assert!(
            after.contains("<<<<<<<") && after.contains(">>>>>>>"),
            "the worktree must show the conflict, got: {after}"
        );

        // The audit trail agrees with the verdict, so `ro status` does not
        // report a clean run either.
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sync_results WHERE repo_id = ?1 AND status = 'success'",
                params![repo_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            rows, 0,
            "a conflicting pop must not leave a success row in the audit trail"
        );
    }

    /// Without this, the test above would also pass on a database where
    /// nothing can be inserted at all — the assertion would be true for a
    /// reason that has nothing to do with the run row.
    #[test]
    fn the_recording_test_would_have_caught_the_old_bug() {
        let (tmp, conn) = setup();
        let remote = init_bare_remote(tmp.path());
        commit_to_remote(tmp.path(), &remote, "a.txt", "hello");
        let local_path = tmp.path().join("local").join("proj1");
        let repo_id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES (?1, 'github.com', 'alice', 'proj1', ?2, ?3, ?4, ?5)",
            params![
                repo_id,
                remote.to_string_lossy().to_string(),
                local_path.to_string_lossy().to_string(),
                now,
                now
            ],
        )
        .unwrap();

        sync_all(&conn, &SyncOptions::default(), &[]).unwrap();

        // What the old code did: a bare UUID with no `runs` row behind it.
        let orphan: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sync_results
                 WHERE run_id NOT IN (SELECT id FROM runs)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            orphan, 0,
            "no result may reference a run that does not exist — that is \
             the foreign key the old code violated on every write"
        );
    }

    /// A pull that fails is not a success.
    ///
    /// The two failure shapes are separated deliberately: a conflict is a
    /// stage `ro ship` continues from, and a plain refusal is not. Both
    /// used to land in the `success` row, which is how a fleet could look
    /// healthy while nothing was syncing.
    #[test]
    fn a_failed_pull_is_not_recorded_as_success() {
        let (tmp, conn) = setup();
        let remote = init_bare_remote(tmp.path());
        commit_to_remote(tmp.path(), &remote, "a.txt", "hello");

        // The remote moves on after the local clone diverges.
        let local_path = tmp.path().join("local").join("proj1");
        let repo_id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES (?1, 'github.com', 'alice', 'proj1', ?2, ?3, ?4, ?5)",
            params![
                repo_id,
                remote.to_string_lossy().to_string(),
                local_path.to_string_lossy().to_string(),
                now,
                now
            ],
        )
        .unwrap();

        // First sync clones.
        sync_all(&conn, &SyncOptions::default(), &[]).unwrap();

        // Both sides move, so the branch genuinely diverges and a
        // fast-forward pull has no answer.
        //
        // This used to move only the *local* side and assert the pull was
        // refused. It was refused — because `git pull` was being handed the
        // branch name where the remote goes, so every pull errored with "does
        // not appear to be a git repository". The test was reading a bug as
        // the behaviour it wanted, and passed for the wrong reason.
        //
        // Local ahead on its own is *not* a failed pull: `git pull --ff-only`
        // with a local commit and a remote that has not moved says "Already
        // up to date" and exits 0, correctly. There is nothing to fetch, and
        // an unpushed local commit is not an error.
        commit_to_remote(tmp.path(), &remote, "c.txt", "remote moved on");
        std::fs::write(local_path.join("b.txt"), "local only\n").unwrap();
        run_git(&local_path, &["add", "."]);
        run_git(&local_path, &["commit", "-m", "local commit"]);

        let results = sync_all(
            &conn,
            &SyncOptions {
                strategy: SyncStrategy::FfOnly,
                ..Default::default()
            },
            &[],
        )
        .unwrap();

        assert_ne!(
            results[0].status, "success",
            "a refused fast-forward must not be recorded as a success, got \
             {:?}",
            results[0]
        );
        assert!(
            results[0].error.is_some(),
            "a failure must carry the reason git gave"
        );

        // And nothing in the audit trail says success either.
        let success_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sync_results WHERE status = 'success' AND action = 'updated'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            success_rows, 0,
            "a refused pull must not leave an 'updated/success' row behind"
        );
    }

    #[test]
    fn sync_repo_clones_missing_repo() {
        let (tmp, conn) = setup();
        let remote = init_bare_remote(tmp.path());
        commit_to_remote(tmp.path(), &remote, "a.txt", "hello");

        let local_path = tmp.path().join("local").join("proj1");
        let repo_id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES (?1, 'github.com', 'alice', 'proj1', ?2, ?3, ?4, ?5)",
            params![
                repo_id,
                remote.to_string_lossy().to_string(),
                local_path.to_string_lossy().to_string(),
                now,
                now
            ],
        )
        .unwrap();

        let repo = TrackedRepo {
            id: repo_id,
            host: "github.com".into(),
            owner: "alice".into(),
            name: "proj1".into(),
            branch: None,
            alias: None,
            clone_url: remote.to_string_lossy().to_string(),
            local_path: local_path.to_string_lossy().to_string(),
            visibility: "unknown".into(),
            archived: false,
            disabled: false,
            credential_ref: None,
            author_ref: None,
            engine: None,
            engine_args: None,
        };

        let opts = SyncOptions::default();
        // A run row has to exist first: the write this test exercises is
        // foreign-keyed against `runs`, which is the entire bug ro-rne.10
        // is about. Minting a bare id here would fail on the constraint
        // rather than on anything the test means to check.
        let run = ro_jobs::open_run(&conn, "sync", &[]).unwrap();
        let result = sync_repo(&conn, &repo, &opts, &run.id).unwrap();
        assert_eq!(result.action, "clone");
        assert_eq!(result.status, "success");
        assert!(result.post_oid.is_some());
        assert!(local_path.join(".git").exists());
    }

    #[test]
    fn sync_all_with_empty_list() {
        let (_, conn) = setup();
        let opts = SyncOptions::default();
        let results = sync_all(&conn, &opts, &[]).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn sync_strategy_display() {
        assert_eq!(SyncStrategy::FfOnly.to_string(), "ff-only");
        assert_eq!(SyncStrategy::Rebase.to_string(), "rebase");
        assert_eq!(SyncStrategy::Merge.to_string(), "merge");
    }
}

/// How many uncommitted changes a worktree has.
///
/// `-uall` so an untracked directory is counted as its files: a directory
/// holding three new files is three changes the user could lose, and
/// "skipped: 1 uncommitted change" understates it.
fn dirty_file_count(repo: &Path) -> Result<usize> {
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain", "-z", "-uall"])
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| anyhow::anyhow!("running git status: {e}"))?;
    if !out.status.success() {
        return Ok(0);
    }
    Ok(ro_git::primitives::parse_porcelain(&String::from_utf8_lossy(&out.stdout)).len())
}

#[cfg(test)]
mod filter_and_skip_tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, Connection) {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        (tmp, conn)
    }

    fn track(conn: &Connection, owner: &str, name: &str, archived: bool, disabled: bool) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at,
                                 archived, disabled)
             VALUES (?1, 'github.com', ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8)",
            rusqlite::params![
                format!("id-{owner}-{name}"),
                owner,
                name,
                format!("https://example.com/{owner}/{name}.git"),
                format!("/nonexistent/{owner}/{name}"),
                now,
                archived as i64,
                disabled as i64
            ],
        )
        .unwrap();
    }

    /// Archived and disabled rows are **not** synced.
    ///
    /// `list(conn, None)` returns every row, so without the filter
    /// `ro sync` reached into repos the user had explicitly retired — and
    /// a sync that touches an archived repo is a sync that does something
    /// you asked it not to do.
    #[test]
    fn archived_and_disabled_repos_are_excluded() {
        let (_tmp, conn) = setup();
        track(&conn, "acme", "live", false, false);
        track(&conn, "acme", "archived", true, false);
        track(&conn, "acme", "disabled", false, true);

        // `--pull-only` means "do nothing but touch existing checkouts",
        // which is the cheapest way to observe the selection.
        let opts = SyncOptions {
            pull_only: true,
            ..Default::default()
        };
        let results = sync_all(&conn, &opts, &[]).unwrap();

        assert_eq!(
            results.len(),
            1,
            "only the live repo may be synced, got {:?}",
            results.iter().map(|r| &r.action).collect::<Vec<_>>()
        );
    }

    /// A dirty worktree is **skipped**, with the count named.
    ///
    /// There is no `--force` on this command. A fleet tool that discards
    /// uncommitted work because someone typed a flag while rushing is a
    /// tool that gets uninstalled after the first accident.
    #[test]
    fn a_dirty_worktree_is_skipped_and_says_how_many() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();

        // A real repo on a real remote, so the pull has somewhere to be a
        // no-op rather than a failure.
        let remote = tmp.path().join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        run_git(&remote, &["init", "--bare", "-q", "--initial-branch=main"]);

        let work = tmp.path().join("work");
        run_git(
            tmp.path(),
            &["clone", "-q", &remote.to_string_lossy(), "work"],
        );
        run_git(&work, &["config", "user.email", "t@e.com"]);
        run_git(&work, &["config", "user.name", "T"]);
        run_git(&work, &["commit", "-q", "--allow-empty", "-m", "init"]);
        run_git(&work, &["push", "-q", "-u", "origin", "main"]);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES ('r1', 'github.com', 'acme', 'api', ?1, ?2, ?3, ?3)",
            rusqlite::params![remote.to_string_lossy(), work.to_string_lossy(), now],
        )
        .unwrap();

        // Two uncommitted files.
        std::fs::write(work.join("a.txt"), "one\n").unwrap();
        std::fs::write(work.join("b.txt"), "two\n").unwrap();

        let results = sync_all(&conn, &SyncOptions::default(), &[]).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].action, "skipped_dirty",
            "a dirty worktree must be skipped, got {:?}",
            results[0]
        );
        assert_eq!(results[0].status, "skipped");

        // And nothing was written.
        let porcelain = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&work)
                .output()
                .unwrap()
                .stdout,
        )
        .into_owned();
        assert!(
            porcelain.contains("a.txt") && porcelain.contains("b.txt"),
            "the uncommitted work must still be there, got: {porcelain}"
        );
    }

    /// A clean worktree is not skipped — the guard must not become a
    /// blanket "never pull".
    #[test]
    fn a_clean_worktree_is_not_skipped() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let remote = tmp.path().join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        run_git(&remote, &["init", "--bare", "-q", "--initial-branch=main"]);
        let work = tmp.path().join("work");
        run_git(
            tmp.path(),
            &["clone", "-q", &remote.to_string_lossy(), "work"],
        );
        run_git(&work, &["config", "user.email", "t@e.com"]);
        run_git(&work, &["config", "user.name", "T"]);
        run_git(&work, &["commit", "-q", "--allow-empty", "-m", "init"]);
        run_git(&work, &["push", "-q", "-u", "origin", "main"]);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES ('r1', 'github.com', 'acme', 'api', ?1, ?2, ?3, ?3)",
            rusqlite::params![remote.to_string_lossy(), work.to_string_lossy(), now],
        )
        .unwrap();

        let results = sync_all(&conn, &SyncOptions::default(), &[]).unwrap();
        assert_ne!(
            results[0].action, "skipped_dirty",
            "a clean worktree must not be skipped, got {:?}",
            results[0]
        );
    }

    /// Run git, with an identity and no signing forced on the child.
    ///
    /// Both are set here rather than left to the machine. A developer's
    /// global `~/.gitconfig` supplies them locally, so a test that commits
    /// passes on a workstation and fails on a CI runner with no identity
    /// configured — "Author identity unknown" on one machine and green on
    /// another, for the same commit.
    /// Run git without depending on the machine's own configuration.
    ///
    /// A developer has `user.email` in their `~/.gitconfig`; a CI
    /// runner does not, so a fixture that commits is green on one and
    /// "Author identity unknown" on the other. The identity comes from
    /// the environment git documents for exactly this, and the config
    /// files are neutralised — `NUL` on Windows, `/dev/null` elsewhere.
    fn run_git(dir: &Path, args: &[&str]) -> std::process::Output {
        let mut cmd = std::process::Command::new("git");
        cmd.args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com");
        #[cfg(windows)]
        cmd.env("GIT_CONFIG_GLOBAL", "NUL")
            .env("GIT_CONFIG_SYSTEM", "NUL");
        #[cfg(not(windows))]
        cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        cmd.output().expect("git runs")
    }
}

/// A branch that was never pushed is a skip, not a failure.
///
/// `git checkout -b` is the normal way a branch starts, so "no remote
/// branch yet" is the state of most repos an hour after they were created.
/// Recording it as a sync error fills a fleet board with red for repos that
/// are exactly as they should be, and the message a user sees is git's
/// ("couldn't find remote ref") rather than anything they can act on.
#[cfg(test)]
mod unpushed_branch_tests {
    use super::*;

    #[test]
    fn an_unpushed_branch_is_skipped_rather_than_recorded_as_an_error() {
        let (tmp, conn) = super::tests::setup();
        let remote = super::tests::init_bare_remote(tmp.path());
        super::tests::commit_to_remote(tmp.path(), &remote, "a.txt", "hello");
        let local_path = tmp.path().join("local").join("proj1");
        let repo_id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES (?1, 'github.com', 'alice', 'proj1', ?2, ?3, ?4, ?5)",
            params![
                repo_id,
                remote.to_string_lossy().to_string(),
                local_path.to_string_lossy().to_string(),
                now,
                now
            ],
        )
        .unwrap();
        sync_all(&conn, &SyncOptions::default(), &[]).unwrap();

        // A branch made locally and never pushed — the state right after
        // `git checkout -b`.
        super::tests::run_git(&local_path, &["checkout", "-b", "fresh"]);

        let results = sync_all(&conn, &SyncOptions::default(), &[]).unwrap();
        let r = &results[0];
        assert_eq!(
            r.status, "skipped",
            "a branch with no remote yet must not be an error, got {r:?}"
        );
        assert_eq!(r.action, "skipped_unpushed");
        assert!(r.error.is_none(), "a skip carries no error: {r:?}");

        // And it must not be counted as a failed sync in the table either.
        let failed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sync_results WHERE repo_id = ?1 AND status = 'error'",
                params![repo_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(failed, 0, "a never-pushed branch is not a sync failure");
    }
}

/// The run-level verdict, and the audit trail it writes.
///
/// `sync_all` used to count `status == "error"` and nothing else, so a run
/// whose only bad repo was an `autostash_conflict` exited 0 — and wrote
/// `exit_code = 0` into `runs` alongside a `sync_results` row saying the
/// user's work was in a stash. A script driving ro saw success. That is the
/// lie the whole `--autostash` work exists to prevent, reintroduced one
/// layer up.
#[cfg(test)]
mod run_exit_code_tests {
    use super::*;

    fn result(status: &str) -> SyncResult {
        SyncResult {
            repo_id: "r1".into(),
            action: "pull".into(),
            status: status.into(),
            duration_ms: 1,
            error: None,
            pre_oid: None,
            post_oid: None,
        }
    }

    /// The bug, as a unit test: the conflict is a failure, and it is not the
    /// string `error`.
    #[test]
    fn an_autostash_conflict_fails_the_run() {
        assert_eq!(
            run_exit_code(&[result(STATUS_AUTOSTASH_CONFLICT)]),
            1,
            "a run whose only bad repo is an autostash conflict must not exit 0"
        );
        assert_eq!(run_exit_code(&[result(STATUS_ERROR)]), 1, "an error fails the run");
    }

    /// The negative control. This is the assertion that stops the fix above
    /// from being "solved" by making every non-success a failure: a fleet of
    /// healthy repos where some are dirty, some unpushed, some merely
    /// simulated, is a **good** run, and it must exit 0.
    #[test]
    fn a_fleet_of_healthy_skips_exits_zero() {
        let results = vec![
            result(STATUS_SUCCESS),
            result(STATUS_SUCCESS),
            result(STATUS_SKIPPED),          // skipped_dirty
            result(STATUS_SKIPPED),          // skipped_unpushed
            result(STATUS_DRY_RUN),          // --dry-run
            result(STATUS_SUCCESS),
        ];
        assert_eq!(
            run_exit_code(&results),
            0,
            "skips are not failures: a fleet of twenty healthy repos must not \
             exit 1, or the exit code stops meaning anything"
        );
        assert!(
            !results.iter().any(|r| status_fails_run(&r.status)),
            "no skip may be classified as a failure"
        );
    }

    /// An empty run is a success. `ro sync` on an empty registry is a
    /// legitimate question with the answer "nothing to do", not a failure.
    #[test]
    fn an_empty_run_exits_zero() {
        assert_eq!(run_exit_code(&[]), 0, "nothing went wrong because nothing ran");
    }

    /// The third failure status, and the reason it is not the string `error`.
    ///
    /// A pull that stops on a conflict is filed as `conflict`, not `error`,
    /// because the two need different work: a conflict is a stage `ro ship`
    /// knows how to continue from, and an error is not. That distinction is
    /// worth having in the audit trail — and it is exactly why the run-level
    /// aggregation, which counted only `status == "error"`, let a run whose
    /// only bad repo was a conflicted pull exit 0.
    ///
    /// Asserted at the rule rather than through a real conflicted rebase,
    /// because whether `git pull --rebase` stops on a same-line divergence is
    /// a property of the git version and of how the histories diverged, not
    /// of this code: on this machine (git 2.53) a genuinely diverged
    /// same-line rebase reports "Successfully rebased" and drops the local
    /// commit, so a fixture built on that premise would pass or fail for
    /// reasons that have nothing to do with the exit code. The rule is the
    /// thing under test, and it is the thing that was wrong.
    #[test]
    fn a_conflicted_pull_fails_the_run_even_though_it_is_not_the_string_error() {
        assert!(
            status_fails_run(STATUS_CONFLICT),
            "a conflicted pull is a failure, and it is not the string `error`"
        );
        assert_eq!(
            run_exit_code(&[result(STATUS_CONFLICT)]),
            1,
            "a run whose only bad repo is a conflicted pull must not exit 0"
        );
        // And it is still distinguishable from an autostash conflict, which
        // is the whole reason it is not filed as `error`.
        assert_ne!(
            STATUS_CONFLICT, STATUS_AUTOSTASH_CONFLICT,
            "the two conflicts must stay separate in a query"
        );
    }

    /// A status nobody has classified fails the run, loudly, rather than
    /// being waved through. A status nothing knows about is not a status
    /// anyone has vouched for, and the safe default for an unvouched status
    /// is to say so.
    #[test]
    fn an_unclassified_status_fails_the_run_rather_than_passing() {
        assert_eq!(
            run_exit_code(&[result("some_future_status")]),
            1,
            "an unknown status must not be treated as success by default"
        );
    }

    /// The end-to-end version: the conflict reaches the `runs` row, not just
    /// the return value.
    ///
    /// `finalize_run` is what writes `exit_code`, so a run recorded with 0
    /// while its results hold a conflict is an audit-trail lie — `ro status`
    /// reads `runs`, and it would report a clean run over a repo whose work
    /// is in a stash.
    #[test]
    fn a_conflicting_run_records_a_non_zero_exit_code_in_the_runs_table() {
        let (tmp, conn) = super::tests::setup();
        let remote = super::tests::init_bare_remote(tmp.path());
        super::tests::commit_to_remote(tmp.path(), &remote, "a.txt", "base");

        let local_path = tmp.path().join("local").join("proj1");
        std::fs::create_dir_all(&local_path).unwrap();
        super::tests::run_git(&local_path, &["clone", &remote.to_string_lossy(), "."]);
        super::tests::run_git(&local_path, &["config", "user.email", "test@example.com"]);
        super::tests::run_git(&local_path, &["config", "user.name", "Test"]);

        // Both sides rewrite the same line, so the pop must conflict.
        std::fs::write(local_path.join("a.txt"), "LOCAL UNCOMMITTED WORK").unwrap();
        super::tests::commit_to_remote(tmp.path(), &remote, "a.txt", "REMOTE MOVED AHEAD");

        let repo_id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES (?1, 'github.com', 'alice', 'proj1', ?2, ?3, ?4, ?5)",
            params![
                repo_id,
                remote.to_string_lossy().to_string(),
                local_path.to_string_lossy().to_string(),
                now,
                now
            ],
        )
        .unwrap();

        let results = sync_all(
            &conn,
            &SyncOptions {
                autostash: true,
                ..SyncOptions::default()
            },
            &[],
        )
        .unwrap();
        assert_eq!(
            results[0].status, "autostash_conflict",
            "the fixture must actually conflict, or this proves nothing: {:?}",
            results[0]
        );

        // The run row is the audit trail, and it must agree with the verdict.
        let recorded: i64 = conn
            .query_row(
                "SELECT exit_code FROM runs WHERE command = 'sync' ORDER BY started_at DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            recorded, 1,
            "a run whose results hold an autostash conflict must not be \
             recorded with exit_code 0 — that is the lie, in the table"
        );

        // And the per-repo row says what happened, so the two are readable
        // together.
        let row_status: String = conn
            .query_row(
                "SELECT status FROM sync_results WHERE repo_id = ?1 ORDER BY rowid DESC LIMIT 1",
                params![repo_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            row_status, "autostash_conflict",
            "the per-repo row must name the conflict: {row_status}"
        );
    }

    /// The same run-level rule, from the other side: a fleet of skips writes
    /// `exit_code = 0`, so the audit trail does not contradict the process.
    #[test]
    fn a_run_of_only_skips_records_a_zero_exit_code() {
        let (tmp, conn) = super::tests::setup();
        let remote = super::tests::init_bare_remote(tmp.path());
        super::tests::commit_to_remote(tmp.path(), &remote, "a.txt", "base");

        let local_path = tmp.path().join("local").join("proj1");
        std::fs::create_dir_all(&local_path).unwrap();
        super::tests::run_git(&local_path, &["clone", &remote.to_string_lossy(), "."]);
        super::tests::run_git(&local_path, &["config", "user.email", "test@example.com"]);
        super::tests::run_git(&local_path, &["config", "user.name", "Test"]);
        super::tests::run_git(&local_path, &["commit", "-q", "--allow-empty", "-m", "init"]);
        super::tests::run_git(&local_path, &["push", "-q", "-u", "origin", "main"]);

        let repo_id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES (?1, 'github.com', 'alice', 'proj1', ?2, ?3, ?4, ?5)",
            params![
                repo_id,
                remote.to_string_lossy().to_string(),
                local_path.to_string_lossy().to_string(),
                now,
                now
            ],
        )
        .unwrap();

        // Dirty, with no --autostash: the tool refuses to throw the work
        // away, which is the correct answer and not a failure.
        std::fs::write(local_path.join("a.txt"), "uncommitted\n").unwrap();
        let results = sync_all(&conn, &SyncOptions::default(), &[]).unwrap();
        assert_eq!(results[0].status, "skipped", "the fixture must skip: {:?}", results[0]);

        let recorded: i64 = conn
            .query_row(
                "SELECT exit_code FROM runs WHERE command = 'sync' ORDER BY started_at DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            recorded, 0,
            "a run of skips is a good run, and the table must say so"
        );
    }
}

/// A conflict is reported **once**, and the sync after it is clean.
///
/// This is the wedge seen from `ro sync` rather than from git: the detector
/// asked whether any line of `git stash list` contained the word
/// `autostash`, so the stash the *first* sync left behind — correctly, it is
/// the user's only route back to their work — was reported as the *second*
/// sync's. From there on every `ro sync --autostash` on that repo was a red
/// `autostash_conflict` over a healthy worktree, with no way out short of
/// the user going and dropping a stash by hand.
///
/// The discriminator is age, not wording: a stash is this pull's only if it
/// was not there before the pull started.
#[cfg(test)]
mod autostash_wedge_tests {
    use super::*;

    /// Register a real clone of `remote` at `local_path`.
    fn track_clone(conn: &Connection, remote: &Path, local_path: &Path) {
        std::fs::create_dir_all(local_path).unwrap();
        super::tests::run_git(local_path, &["clone", &remote.to_string_lossy(), "."]);
        super::tests::run_git(local_path, &["config", "user.email", "test@example.com"]);
        super::tests::run_git(local_path, &["config", "user.name", "Test"]);
        let repo_id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES (?1, 'github.com', 'alice', 'proj1', ?2, ?3, ?4, ?5)",
            params![
                repo_id,
                remote.to_string_lossy().to_string(),
                local_path.to_string_lossy().to_string(),
                now,
                now
            ],
        )
        .unwrap();
    }

    fn sync_with_autostash(conn: &Connection) -> Vec<SyncResult> {
        sync_all(
            conn,
            &SyncOptions {
                autostash: true,
                ..SyncOptions::default()
            },
            &[],
        )
        .unwrap()
    }

    /// The defect, end to end, as the user meets it.
    #[test]
    fn a_conflict_is_reported_once_and_the_next_sync_is_clean() {
        let (tmp, conn) = super::tests::setup();
        let remote = super::tests::init_bare_remote(tmp.path());
        super::tests::commit_to_remote(tmp.path(), &remote, "a.txt", "base");

        let local_path = tmp.path().join("local").join("proj1");
        track_clone(&conn, &remote, &local_path);

        // Both sides rewrite the same line, so the pop must conflict.
        std::fs::write(local_path.join("a.txt"), "LOCAL UNCOMMITTED WORK").unwrap();
        super::tests::commit_to_remote(tmp.path(), &remote, "a.txt", "REMOTE MOVED AHEAD");

        // Sync #1: the conflict, reported once.
        let first = sync_with_autostash(&conn);
        assert_eq!(
            first[0].status, "autostash_conflict",
            "the pop conflicts here, so this sync must say so: {:?}",
            first[0]
        );
        let held = super::tests::run_git(&local_path, &["stash", "list", "--format=%H"]);
        assert_eq!(
            String::from_utf8_lossy(&held.stdout).lines().count(),
            1,
            "a failed pop keeps the user's work in a stash, and that is the \
             whole point: {}",
            String::from_utf8_lossy(&held.stdout)
        );

        // The user resolves the conflict markers and commits the resolution.
        // The stash is still there — it is their work, and nothing has any
        // business dropping it for them.
        super::tests::run_git(&local_path, &["checkout", "--theirs", "a.txt"]);
        super::tests::run_git(&local_path, &["add", "a.txt"]);
        super::tests::run_git(
            &local_path,
            &["commit", "-q", "-m", "resolved: keep the local work"],
        );
        let still_there = super::tests::run_git(&local_path, &["stash", "list", "--format=%H"]);
        assert_eq!(
            String::from_utf8_lossy(&still_there.stdout).lines().count(),
            1,
            "the stash must survive the resolution, or the user has lost work"
        );

        // Sync #2: the tree is clean and the pull is a no-op. It stashes
        // nothing and pops nothing, so the stash above is not its stash.
        let second = sync_with_autostash(&conn);
        assert_ne!(
            second[0].status, "autostash_conflict",
            "a leftover autostash from an EARLIER sync must not be reported \
             as this one's — that wedges the repo permanently, with no way \
             out short of dropping the stash by hand. Got {:?}",
            second[0]
        );
        assert_eq!(
            second[0].status, "success",
            "and a clean sync over a clean tree is a success: {:?}",
            second[0]
        );

        // The run agrees: a clean sync is a run that exits 0, in the table
        // as well as on the wire.
        assert_eq!(
            run_exit_code(&second),
            0,
            "the second sync is a good run and must not be reported as failed"
        );
    }

    /// The other half of the same defect, from `ro sync`.
    ///
    /// A stash the user made by hand is not this pull's stash, even when
    /// its message says `autostash` — which is a thing people write, because
    /// it is the word in the flag they just used. A substring match cannot
    /// tell the two apart, and invents a conflict on a sync that
    /// fast-forwarded perfectly.
    #[test]
    fn a_stash_the_user_made_is_not_reported_as_a_conflict() {
        let (tmp, conn) = super::tests::setup();
        let remote = super::tests::init_bare_remote(tmp.path());
        super::tests::commit_to_remote(tmp.path(), &remote, "a.txt", "base");

        let local_path = tmp.path().join("local").join("proj1");
        track_clone(&conn, &remote, &local_path);
        std::fs::write(local_path.join("a.txt"), "LOCAL UNCOMMITTED WORK").unwrap();

        // The user's own stash, made by hand, before the sync.
        super::tests::run_git(
            &local_path,
            &["stash", "push", "-m", "autostash of the report rewrite"],
        );

        // The remote moves, so the sync has real work to do.
        super::tests::commit_to_remote(tmp.path(), &remote, "a.txt", "REMOTE MOVED AHEAD");

        let results = sync_with_autostash(&conn);
        assert_eq!(
            results[0].status, "success",
            "this is an ordinary fast-forward over a clean tree; a stash the \
             user made is not this pull's stash, whatever it is called: {:?}",
            results[0]
        );
        assert_eq!(
            std::fs::read_to_string(local_path.join("a.txt")).unwrap(),
            "REMOTE MOVED AHEAD",
            "the remote's version must have landed"
        );
        assert_eq!(
            run_exit_code(&results),
            0,
            "and a clean sync over a clean tree is not a failure"
        );
    }
}
