//! Prune removed/missing/archived repos, and orphaned working copies on disk.
//!
//! Interactive confirmation by default, --force to skip.
//! Records audit event.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::manage::delete_repo_cascade;

/// Result of a prune operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PruneResult {
    pub repo_id: String,
    pub owner: String,
    pub name: String,
    pub removed: bool,
    pub reason: String,
}

/// Remove a single repo from tracking by ID.
pub fn prune_repo(conn: &Connection, repo_id: &str) -> Result<PruneResult> {
    let (owner, name) = conn
        .query_row(
            "SELECT owner, name FROM repos WHERE id = ?1",
            params![repo_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .with_context(|| format!("repo {repo_id} not found"))?;

    delete_repo_cascade(conn, repo_id)?;
    Ok(PruneResult {
        repo_id: repo_id.to_string(),
        owner,
        name,
        removed: true,
        reason: "manual prune".into(),
    })
}

/// Remove all archived repos from tracking.
pub fn prune_archived(conn: &Connection) -> Result<Vec<PruneResult>> {
    let mut results = Vec::new();
    let mut stmt = conn.prepare("SELECT id, owner, name FROM repos WHERE archived = 1")?;
    let rows: Vec<(String, String, String)> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    for (id, owner, name) in rows {
        delete_repo_cascade(conn, &id)?;
        results.push(PruneResult {
            repo_id: id,
            owner,
            name,
            removed: true,
            reason: "archived".into(),
        });
    }
    Ok(results)
}

/// Remove repos whose local_path no longer exists on disk.
pub fn prune_missing(conn: &Connection) -> Result<Vec<PruneResult>> {
    let mut results = Vec::new();
    let mut stmt = conn.prepare("SELECT id, owner, name, local_path FROM repos")?;
    let rows: Vec<(String, String, String, String)> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    for (id, owner, name, local_path) in rows {
        if !std::path::Path::new(&local_path).exists() {
            delete_repo_cascade(conn, &id)?;
            results.push(PruneResult {
                repo_id: id,
                owner,
                name,
                removed: true,
                reason: format!("local path missing: {local_path}"),
            });
        }
    }
    Ok(results)
}

/// What to do with an orphaned working copy found on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanAction {
    /// Report only.
    Report,
    /// Move into `<state_dir>/archived/<name>_<timestamp>`.
    Archive,
    /// Delete permanently.
    Delete,
}

/// A git working copy on disk that is not in the tracked inventory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Orphan {
    pub path: String,
    pub name: String,
}

/// Find git working copies under `root` that are not in the inventory.
///
/// Mirrors ru's `prune`: scan the projects directory for `.git` directories
/// and report any whose path the tracker does not know about. Search depth is
/// bounded to 4 levels, matching ru's `full` layout.
pub fn find_orphans(conn: &Connection, root: &Path) -> Result<Vec<Orphan>> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }

    let mut tracked: Vec<String> = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT local_path FROM repos")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for r in rows {
            if let Ok(p) = r {
                if !p.is_empty() {
                    tracked.push(p);
                }
            }
        }
    }

    let mut candidates = Vec::new();
    collect_git_dirs(root, 0, &mut candidates)?;

    let mut orphans = Vec::new();
    for path in candidates {
        let s = path.to_string_lossy().to_string();
        if tracked.iter().any(|t| t == &s) {
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| s.clone());
        orphans.push(Orphan { path: s, name });
    }
    orphans.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(orphans)
}

/// Walk up to `max_depth` levels collecting directories that contain `.git`.
fn collect_git_dirs(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) -> Result<()> {
    const MAX_DEPTH: usize = 4;
    if depth >= MAX_DEPTH {
        return Ok(());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Never descend into these; they are not managed working copies.
        if name == ".git" || name == "node_modules" || name == "target" {
            continue;
        }
        if path.join(".git").exists() {
            out.push(path.clone());
            // A working copy is a leaf: do not treat its subdirs as repos.
            continue;
        }
        collect_git_dirs(&path, depth + 1, out)?;
    }
    Ok(())
}

/// Act on orphaned working copies.
///
/// `Report` changes nothing. `Archive` moves each to `<state_dir>/archived`
/// with a timestamp suffix. `Delete` removes it permanently — destructive.
pub fn handle_orphans(
    orphans: &[Orphan],
    action: OrphanAction,
    state_dir: &Path,
) -> Result<Vec<PathBuf>> {
    let mut done = Vec::new();
    if action == OrphanAction::Report {
        return Ok(done);
    }

    if action == OrphanAction::Archive {
        let archive_dir = state_dir.join("archived");
        std::fs::create_dir_all(&archive_dir)
            .with_context(|| format!("creating archive dir {}", archive_dir.display()))?;
        let stamp = timestamp();
        for o in orphans {
            let src = PathBuf::from(&o.path);
            let dest = archive_dir.join(format!("{}_{stamp}", o.name));
            if std::fs::rename(&src, &dest).is_ok() {
                done.push(dest);
            } else {
                tracing::warn!(path = %o.path, "failed to archive orphan");
            }
        }
        return Ok(done);
    }

    for o in orphans {
        let src = PathBuf::from(&o.path);
        if let Err(e) = std::fs::remove_dir_all(&src) {
            tracing::warn!(path = %o.path, error = %e, "failed to delete orphan");
        } else {
            done.push(src);
        }
    }
    Ok(done)
}

/// `YYYYmmdd_HHMMSS` in UTC.
fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Civil-time conversion from Unix seconds (days since 1970-01-01).
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}{m:02}{d:02}_{h:02}{mi:02}{s:02}")
}

/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
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

    fn projects_dir(tmp: &TempDir) -> PathBuf {
        tmp.path().join("projects")
    }

    #[test]
    fn prune_repo_removes_tracked_repo() {
        let (tmp, conn) = setup();
        let repo = crate::manage::add(&conn, "alice/proj1", &projects_dir(&tmp)).unwrap();
        let result = prune_repo(&conn, &repo.id).unwrap();
        assert!(result.removed);
        assert_eq!(result.owner, "alice");
        assert_eq!(result.name, "proj1");
        assert!(crate::manage::list(&conn, None).unwrap().is_empty());
    }

    #[test]
    fn prune_repo_not_found() {
        let (_, conn) = setup();
        let err = prune_repo(&conn, "nonexistent").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn prune_archived_removes_archived() {
        let (tmp, conn) = setup();
        let repo = crate::manage::add(&conn, "alice/archived-repo", &projects_dir(&tmp)).unwrap();
        // Mark as archived
        conn.execute(
            "UPDATE repos SET archived = 1 WHERE id = ?1",
            params![repo.id],
        )
        .unwrap();

        let results = prune_archived(&conn).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "archived-repo");
        assert!(results[0].removed);
        assert!(crate::manage::list(&conn, None).unwrap().is_empty());
    }

    #[test]
    fn prune_archived_skips_non_archived() {
        let (tmp, conn) = setup();
        crate::manage::add(&conn, "alice/active", &projects_dir(&tmp)).unwrap();
        let results = prune_archived(&conn).unwrap();
        assert!(results.is_empty());
        assert_eq!(crate::manage::list(&conn, None).unwrap().len(), 1);
    }

    #[test]
    fn prune_missing_removes_nonexistent_paths() {
        let (tmp, conn) = setup();
        let _repo = crate::manage::add(&conn, "alice/gone", &projects_dir(&tmp)).unwrap();
        // Don't create the directory
        let results = prune_missing(&conn).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "gone");
        assert!(results[0].removed);
    }

    #[test]
    fn prune_missing_keeps_existing_paths() {
        let (tmp, conn) = setup();
        let _repo = crate::manage::add(&conn, "alice/present", &projects_dir(&tmp)).unwrap();
        // Create the local path
        let local = projects_dir(&tmp).join("alice").join("present");
        std::fs::create_dir_all(&local).unwrap();

        let results = prune_missing(&conn).unwrap();
        assert!(results.is_empty());
    }

    /// Regression: a repo with rows in dependent tables (sync_results,
    /// repo_health_snapshots, etc.) must still be prunable. Pre-fix this
    /// hit a SQLite `FOREIGN KEY constraint failed` (code 787).
    #[test]
    fn prune_repo_with_sync_history_succeeds() {
        let (tmp, conn) = setup();
        let repo = crate::manage::add(&conn, "alice/with-history", &projects_dir(&tmp)).unwrap();

        // Insert a run + sync_result so FK enforcement bites.
        conn.execute(
            "INSERT INTO runs (id, command, started_at, args_json) VALUES (?1, ?2, ?3, ?4)",
            params!["run-1", "sync", 0i64, "[]"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sync_results (run_id, repo_id, action, status, duration_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params!["run-1", repo.id, "clone", "success", 0i64],
        )
        .unwrap();

        // Health snapshot, plan, and job all reference the same repo.
        ro_state::queries::score_repo_health(&conn, &repo.id).unwrap();

        let result = prune_repo(&conn, &repo.id).unwrap();
        assert!(result.removed);
        assert!(crate::manage::list(&conn, None).unwrap().is_empty());
    }

    /// Regression: archived prune must also tolerate FK history.
    #[test]
    fn prune_archived_with_sync_history_succeeds() {
        let (tmp, conn) = setup();
        let repo = crate::manage::add(&conn, "alice/old", &projects_dir(&tmp)).unwrap();
        ro_state::queries::score_repo_health(&conn, &repo.id).unwrap();
        conn.execute(
            "UPDATE repos SET archived = 1 WHERE id = ?1",
            params![repo.id],
        )
        .unwrap();

        let results = prune_archived(&conn).unwrap();
        assert_eq!(results.len(), 1);
        assert!(crate::manage::list(&conn, None).unwrap().is_empty());
    }

    // ── Orphan working copies ──

    fn make_repo_at(path: &std::path::Path) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::create_dir_all(path.join(".git")).unwrap();
    }

    #[test]
    fn find_orphans_reports_untracked_working_copies() {
        let (tmp, conn) = setup();
        let root = projects_dir(&tmp);
        std::fs::create_dir_all(&root).unwrap();

        let tracked_path = root.join("alice").join("tracked");
        let tracked = crate::manage::add(&conn, "alice/tracked", &root).unwrap();
        make_repo_at(&tracked_path);
        make_repo_at(&root.join("alice").join("stray"));
        make_repo_at(&root.join("nobody").join("also-stray"));

        let orphans = find_orphans(&conn, &root).unwrap();
        let mut names: Vec<&str> = orphans.iter().map(|o| o.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["also-stray", "stray"]);
        assert!(orphans.iter().all(|o| !o.path.contains("tracked")));
        assert_eq!(tracked.name, "tracked");
    }

    #[test]
    fn find_orphans_returns_empty_when_all_tracked() {
        let (tmp, conn) = setup();
        let root = projects_dir(&tmp);
        std::fs::create_dir_all(&root).unwrap();
        let p = root.join("alice").join("only");
        crate::manage::add(&conn, "alice/only", &root).unwrap();
        make_repo_at(&p);

        assert!(find_orphans(&conn, &root).unwrap().is_empty());
    }

    #[test]
    fn find_orphans_missing_root_is_empty_not_error() {
        let (tmp, conn) = setup();
        let missing = tmp.path().join("nope");
        assert!(find_orphans(&conn, &missing).unwrap().is_empty());
    }

    #[test]
    fn report_action_changes_nothing() {
        let (tmp, conn) = setup();
        let root = projects_dir(&tmp);
        std::fs::create_dir_all(&root).unwrap();
        let stray = root.join("stray");
        make_repo_at(&stray);

        let orphans = find_orphans(&conn, &root).unwrap();
        let done = handle_orphans(&orphans, OrphanAction::Report, tmp.path()).unwrap();
        assert!(done.is_empty());
        assert!(stray.join(".git").exists(), "report must not move anything");
    }

    #[test]
    fn archive_action_moves_orphan_into_archived_dir() {
        let (tmp, conn) = setup();
        let root = projects_dir(&tmp);
        std::fs::create_dir_all(&root).unwrap();
        let stray = root.join("stray");
        make_repo_at(&stray);

        let orphans = find_orphans(&conn, &root).unwrap();
        let state = tmp.path().join("state");
        let done = handle_orphans(&orphans, OrphanAction::Archive, &state).unwrap();

        assert_eq!(done.len(), 1);
        assert!(!stray.exists(), "source should be gone");
        assert!(done[0].starts_with(state.join("archived")));
        assert!(done[0].join(".git").exists(), "contents must move with it");
        assert!(
            done[0]
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("stray_"),
            "archive name carries a timestamp suffix"
        );
    }

    #[test]
    fn delete_action_removes_orphan() {
        let (tmp, conn) = setup();
        let root = projects_dir(&tmp);
        std::fs::create_dir_all(&root).unwrap();
        let stray = root.join("stray");
        make_repo_at(&stray);

        let orphans = find_orphans(&conn, &root).unwrap();
        let done = handle_orphans(&orphans, OrphanAction::Delete, tmp.path()).unwrap();

        assert_eq!(done.len(), 1);
        assert!(!stray.exists());
    }

    #[test]
    fn timestamp_is_well_formed() {
        let t = timestamp();
        assert_eq!(t.len(), 15, "YYYYmmdd_HHMMSS");
        assert_eq!(&t[8..9], "_");
        assert!(t.chars().all(|c| c.is_ascii_digit() || c == '_'));
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }
}
