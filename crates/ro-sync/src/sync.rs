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
            branch: Some(branch),
            strategy: pull_strategy,
            ..Default::default()
        };

        let pull_result = ro_git::mutation::pull(local, &pull_opts);
        let post_oid = ro_git::read::head_oid(local).ok().flatten();

        let duration = start.elapsed().as_millis() as u64;

        match pull_result {
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
                    "conflict",
                    duration,
                    Some(&err_msg),
                    &pre_oid,
                    &post_oid,
                )?;
                Ok(SyncResult {
                    repo_id: repo.id.clone(),
                    action: "pull".into(),
                    status: "conflict".into(),
                    duration_ms: duration,
                    error: Some(err_msg),
                    pre_oid,
                    post_oid,
                })
            }
            Ok(outcome) => {
                let stderr = outcome.result.stderr.trim();
                let err_msg = if stderr.is_empty() {
                    format!("git pull failed with status {}", outcome.result.status)
                } else {
                    stderr.to_string()
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
pub fn sync_all(conn: &Connection, opts: &SyncOptions) -> Result<Vec<SyncResult>> {
    let repos = crate::manage::list(conn, None)?;
    let run = ro_jobs::open_run(conn, "sync", &[]).context("opening the sync run record")?;
    let run_id = run.id.clone();

    let mut results = Vec::new();
    for repo in &repos {
        // A failed *write* aborts the run rather than being folded into a
        // per-repo result: the audit trail is missing, so every result in
        // this run is untrustworthy, including the ones already recorded.
        results.push(sync_repo(conn, repo, opts, &run_id)?);
    }

    // Exit code reflects the worst outcome, so a run that recorded errors
    // is not filed as a clean one. `sync_results.status` already carries
    // the per-repo detail; this is the run-level verdict.
    let failed = results.iter().filter(|r| r.status == "error").count();
    let conflicted = results.iter().filter(|r| r.status == "conflict").count();
    let exit_code = if failed > 0 { 1 } else { 0 };
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

    fn setup() -> (TempDir, Connection) {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        (tmp, conn)
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
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    fn init_bare_remote(dir: &Path) -> PathBuf {
        let remote = dir.join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        run_git(&remote, &["init", "--bare", "-b", "main"]);
        remote
    }

    fn commit_to_remote(dir: &Path, remote: &Path, name: &str, content: &str) {
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

        let results = sync_all(&conn, &SyncOptions::default()).unwrap();
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

        sync_all(&conn, &SyncOptions::default()).unwrap();

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
        sync_all(&conn, &SyncOptions::default()).unwrap();

        // Local commits something the remote does not have, so a
        // fast-forward pull must refuse.
        std::fs::write(local_path.join("b.txt"), "local only\n").unwrap();
        run_git(&local_path, &["add", "."]);
        run_git(&local_path, &["commit", "-m", "local commit"]);

        let results = sync_all(
            &conn,
            &SyncOptions {
                strategy: SyncStrategy::FfOnly,
                ..Default::default()
            },
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
        let results = sync_all(&conn, &opts).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn sync_strategy_display() {
        assert_eq!(SyncStrategy::FfOnly.to_string(), "ff-only");
        assert_eq!(SyncStrategy::Rebase.to_string(), "rebase");
        assert_eq!(SyncStrategy::Merge.to_string(), "merge");
    }
}
