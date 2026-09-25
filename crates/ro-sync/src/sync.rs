//! Sync engine implementation.
//!
//! Read repos from SQLite → create run record → for each repo:
//! acquire fs4 lock → clone if missing → fetch → pull per strategy
//! → record pre/post OID → release lock.

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
) -> SyncResult {
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
        return SyncResult {
            repo_id: repo.id.clone(),
            action: action.into(),
            status: "dry_run".into(),
            duration_ms: duration,
            error: None,
            pre_oid: pre_oid.clone(),
            post_oid: pre_oid,
        };
    }

    if !local.join(".git").exists() {
        if opts.pull_only {
            let duration = start.elapsed().as_millis() as u64;
            return SyncResult {
                repo_id: repo.id.clone(),
                action: "skipped_clone".into(),
                status: "skipped".into(),
                duration_ms: duration,
                error: None,
                pre_oid,
                post_oid: None,
            };
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
                    );
                    return SyncResult {
                        repo_id: repo.id.clone(),
                        action: "clone".into(),
                        status: "error".into(),
                        duration_ms: duration,
                        error: Some(err_msg),
                        pre_oid,
                        post_oid: None,
                    };
                }
                let post_oid = ro_git::read::head_oid(local).ok().flatten();
                let duration = start.elapsed().as_millis() as u64;
                record_result(
                    conn, run_id, &repo.id, "clone", "success", duration, None, &pre_oid, &post_oid,
                );
                SyncResult {
                    repo_id: repo.id.clone(),
                    action: "clone".into(),
                    status: "success".into(),
                    duration_ms: duration,
                    error: None,
                    pre_oid,
                    post_oid,
                }
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
                );
                SyncResult {
                    repo_id: repo.id.clone(),
                    action: "clone".into(),
                    status: "error".into(),
                    duration_ms: duration,
                    error: Some(err_msg),
                    pre_oid,
                    post_oid: None,
                }
            }
        }
    } else {
        if opts.clone_only {
            let duration = start.elapsed().as_millis() as u64;
            return SyncResult {
                repo_id: repo.id.clone(),
                action: "skipped_pull".into(),
                status: "skipped".into(),
                duration_ms: duration,
                error: None,
                pre_oid: pre_oid.clone(),
                post_oid: pre_oid,
            };
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
            );
            return SyncResult {
                repo_id: repo.id.clone(),
                action: "fetch".into(),
                status: "error".into(),
                duration_ms: duration,
                error: Some(err_msg),
                pre_oid: pre_oid.clone(),
                post_oid: pre_oid,
            };
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
            Ok(outcome) => {
                let action = if outcome.already_up_to_date {
                    "already_up_to_date"
                } else {
                    "updated"
                };
                record_result(
                    conn, run_id, &repo.id, action, "success", duration, None, &pre_oid, &post_oid,
                );
                SyncResult {
                    repo_id: repo.id.clone(),
                    action: action.into(),
                    status: "success".into(),
                    duration_ms: duration,
                    error: None,
                    pre_oid,
                    post_oid,
                }
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
                );
                SyncResult {
                    repo_id: repo.id.clone(),
                    action: "pull".into(),
                    status: "error".into(),
                    duration_ms: duration,
                    error: Some(err_msg),
                    pre_oid,
                    post_oid,
                }
            }
        }
    }
}

/// Sync all tracked repos.
pub fn sync_all(conn: &Connection, opts: &SyncOptions) -> anyhow::Result<Vec<SyncResult>> {
    let repos = crate::manage::list(conn, None)?;
    let run_id = uuid::Uuid::new_v4().to_string();
    let mut results = Vec::new();
    for repo in &repos {
        results.push(sync_repo(conn, repo, opts, &run_id));
    }
    Ok(results)
}

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
) {
    let _ = conn.execute(
        "INSERT INTO sync_results (run_id, repo_id, action, status, duration_ms, error, pre_oid, post_oid)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![run_id, repo_id, action, status, duration_ms as i64, error, pre_oid, post_oid],
    );
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
        };

        let opts = SyncOptions::default();
        let result = sync_repo(&conn, &repo, &opts, "test-run");
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
