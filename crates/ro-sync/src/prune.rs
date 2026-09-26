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
/// Whether two path strings refer to the same location.
///
/// Exists because a stored `local_path` and a freshly-walked directory can
/// spell the same path differently: `/` vs `\` on Windows, a trailing
/// separator, a `.` segment. Comparing them as raw strings reported every
/// tracked repo as an orphan on Windows.
///
/// Two tiers, because neither alone is sufficient:
///
/// 1. **Canonicalize both** and compare. This is what makes a stored
///    forward-slash row (written by the pre-fix `resolve_local_path`, still
///    sitting in existing `state.db` files) match a backslash candidate.
///    It only works when the directory still exists.
/// 2. **Fall back to a separator-insensitive comparison** when either side
///    cannot be canonicalized — a tracked repo whose directory was deleted,
///    which is the case a strict `canonicalize` would silently miss.
fn paths_equivalent(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (pa, pb) = (Path::new(a), Path::new(b));
    if let (Ok(ca), Ok(cb)) = (pa.canonicalize(), pb.canonicalize()) {
        if ca == cb {
            return true;
        }
    }
    normalize_separators(a) == normalize_separators(b)
}

/// Lowercase-free, separator-normalized, trailing-separator-free form.
///
/// Deliberately does not resolve `..` or symlinks: this is a last-resort
/// equality check, not a path algebra, and pretending otherwise would
/// reintroduce a filesystem dependency into what should stay a string
/// comparison.
fn normalize_separators(p: &str) -> String {
    let unified = p.replace('\\', "/");
    let trimmed = unified.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn find_orphans(conn: &Connection, root: &Path) -> Result<Vec<Orphan>> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }

    let mut tracked: Vec<String> = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT local_path FROM repos")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for p in rows.flatten() {
            if !p.is_empty() {
                tracked.push(p);
            }
        }
    }

    let mut candidates = Vec::new();
    collect_git_dirs(root, 0, &mut candidates)?;

    let mut orphans = Vec::new();
    for path in candidates {
        let s = path.to_string_lossy().to_string();
        if tracked.iter().any(|t| paths_equivalent(t, &s)) {
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

    /// The half of the fix that protects existing databases.
    ///
    /// `resolve_local_path` now writes platform-native separators, which
    /// fixes rows created from here on. But every `state.db` already on disk
    /// holds rows written by the old `format!("{}/{}/{}", …)`, which on
    /// Windows produced forward slashes while `collect_git_dirs` produces
    /// backslashes. Without the canonicalizing comparison those rows are
    /// still reported as orphans — the fix would convert a wrong list into
    /// *every* existing user's repos becoming orphans.
    ///
    /// Gated to Windows on purpose. On POSIX `.replace('\\', "/")` is a
    /// no-op, so the stored path would equal the walked path, the very first
    /// `a == b` branch would match, and the test would pass without ever
    /// reaching the canonicalize tier — proving nothing, and staying green if
    /// the fix were reverted. Better an honest Windows-only test than a
    /// cross-platform one that quietly tests the wrong thing.
    #[cfg(windows)]
    #[test]
    fn forward_slash_stored_path_is_not_reported_as_orphan() {
        let (tmp, conn) = setup();
        let root = projects_dir(&tmp);
        std::fs::create_dir_all(&root).unwrap();
        let p = root.join("alice").join("legacy");
        make_repo_at(&p);
        crate::manage::add(&conn, "alice/legacy", &root).unwrap();

        // Rewrite the stored path into the pre-fix spelling.
        let legacy = p.to_string_lossy().replace('\\', "/");
        let n = conn
            .execute(
                "UPDATE repos SET local_path = ?1 WHERE owner = 'alice' AND name = 'legacy'",
                [&legacy],
            )
            .unwrap();
        assert_eq!(n, 1, "row must exist to rewrite it");
        assert_ne!(
            legacy,
            p.to_string_lossy(),
            "on Windows the two spellings must differ or this test proves nothing"
        );

        assert!(
            find_orphans(&conn, &root).unwrap().is_empty(),
            "a forward-slash stored path must still match its directory, \
             otherwise every pre-fix database reports all its repos as orphans"
        );
    }

    /// The fallback tier, on every platform.
    ///
    /// A tracked repo whose directory has been deleted cannot be canonicalized
    /// on either side, so the string comparison is the only thing that can
    /// match them. The two spellings are made to differ by a *trailing
    /// separator* rather than by a separator swap, because a `\`→`/` replace
    /// is a no-op on POSIX — building the mismatch that way would make this
    /// test vacuous on Linux and macOS while appearing to pass.
    #[test]
    fn separator_mismatch_still_matches_when_path_is_gone() {
        let (tmp, conn) = setup();
        let root = projects_dir(&tmp);
        std::fs::create_dir_all(&root).unwrap();
        let p = root.join("alice").join("vanished");
        make_repo_at(&p);
        crate::manage::add(&conn, "alice/vanished", &root).unwrap();

        let stored = p.to_string_lossy();
        let with_trailing = format!("{stored}/");
        conn.execute(
            "UPDATE repos SET local_path = ?1 WHERE owner = 'alice' AND name = 'vanished'",
            [&with_trailing],
        )
        .unwrap();
        std::fs::remove_dir_all(&p).unwrap();

        assert_ne!(
            with_trailing, stored,
            "the raw spellings must differ, otherwise this test proves nothing"
        );
        assert!(
            paths_equivalent(&with_trailing, &stored),
            "with the directory gone neither side can be canonicalized, so the \
             string fallback is the only thing that can match them — that is \
             exactly the case the fallback exists for"
        );
        assert!(
            !paths_equivalent(
                &with_trailing,
                &root.join("bob").join("other").to_string_lossy()
            ),
            "the fallback must still distinguish genuinely different paths"
        );
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
