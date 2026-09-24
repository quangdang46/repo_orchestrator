//! Status display engine.
//!
//! Show repo status for all or single repo.
//! Output in text/JSON/NDJSON/TOON.

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
    pub ahead: u32,
    pub behind: u32,
    pub last_synced_at: Option<i64>,
}

/// Get status for a single tracked repo by ID.
///
/// Queries the state DB for repo metadata, then uses `ro_git::read`
/// functions to inspect the actual repository on disk.
pub fn status_repo(conn: &Connection, repo_id: &str) -> Result<RepoStatus> {
    let mut stmt = conn.prepare(
        "SELECT owner, name, branch, local_path, default_branch FROM repos WHERE id = ?1",
    )?;
    let row = stmt
        .query_row([repo_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })
        .with_context(|| format!("repo with id={repo_id} not found"))?;

    let (owner, name, tracked_branch, local_path, default_branch) = row;
    let path = std::path::PathBuf::from(&local_path);

    let (branch, is_dirty, ahead, behind) = if path.join(".git").exists() {
        let branch = ro_git::read::current_branch(&path)?;
        let is_dirty = ro_git::read::is_dirty(&path)?;
        let upstream_branch = branch
            .as_ref()
            .or(tracked_branch.as_ref())
            .or(default_branch.as_ref());
        let (ahead, behind) = if let Some(ref b) = upstream_branch {
            let upstream = format!("origin/{b}");
            let ab = ro_git::read::ahead_behind(&path, &upstream)?;
            (ab.ahead, ab.behind)
        } else {
            (0, 0)
        };
        (branch, is_dirty, ahead, behind)
    } else {
        (None, false, 0, 0)
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

    Ok(RepoStatus {
        repo_id: repo_id.to_string(),
        owner,
        name,
        branch: branch.or(tracked_branch),
        is_dirty,
        ahead,
        behind,
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
        assert_eq!(status.ahead, 0);
        assert_eq!(status.behind, 0);
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
        assert_eq!(status.ahead, 0);
        assert_eq!(status.behind, 0);
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

    #[test]
    fn status_repo_not_found() {
        let (_, conn) = setup();
        let err = status_repo(&conn, "nonexistent-id").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }
}
