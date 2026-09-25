//! Repo management: add, remove, list, init.
//!
//! `ro init`: initialize config + SQLite state directory.
//! `ro add`: parse spec, insert into state DB (offline-first; GitHub enrichment optional).
//! `ro remove`: delete repo from state DB.
//! `ro list`: enumerate tracked repos.

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use ro_config::paths::ConfigPaths;
use ro_core::repo_spec::RepoSpec;

/// A tracked repo as returned by list queries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrackedRepo {
    pub id: String,
    pub host: String,
    pub owner: String,
    pub name: String,
    pub branch: Option<String>,
    pub alias: Option<String>,
    pub clone_url: String,
    pub local_path: String,
    pub visibility: String,
    pub archived: bool,
    pub disabled: bool,
}

impl std::fmt::Display for TrackedRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)?;
        if let Some(ref a) = self.alias {
            write!(f, " as {a}")?;
        }
        Ok(())
    }
}

const REPO_COLUMNS: &str = "id, host, owner, name, branch, alias, clone_url, local_path, \
                             visibility, archived, disabled";

fn row_to_tracked(row: &rusqlite::Row<'_>) -> std::result::Result<TrackedRepo, rusqlite::Error> {
    Ok(TrackedRepo {
        id: row.get(0)?,
        host: row.get(1)?,
        owner: row.get(2)?,
        name: row.get(3)?,
        branch: row.get(4)?,
        alias: row.get(5)?,
        clone_url: row.get(6)?,
        local_path: row.get(7)?,
        visibility: row.get(8)?,
        archived: row.get::<_, i64>(9)? != 0,
        disabled: row.get::<_, i64>(10)? != 0,
    })
}

/// Initialize ro: create config file and state database if absent.
/// Returns `true` if anything was created, `false` if already initialized.
pub fn init(paths: &ConfigPaths) -> Result<bool> {
    paths.ensure_all()?;
    let mut created = false;

    let cfg_path = paths.config_toml();
    if !cfg_path.exists() {
        ro_config::loader::write_default(&cfg_path).context("writing default config")?;
        created = true;
    }

    let db_path = paths.state_db();
    if !db_path.exists() {
        let _conn = ro_state::open_db(&db_path)?;
        created = true;
    }

    Ok(created)
}

/// Add a repo to tracking. Parses the spec, resolves the local path, and
/// inserts into the `repos` table. Fails on duplicate (host, owner, name).
/// Owner and name are normalized to lowercase (GitHub is case-insensitive).
pub fn add(conn: &Connection, spec_str: &str, projects_dir: &Path) -> Result<TrackedRepo> {
    let mut spec =
        RepoSpec::parse(spec_str).map_err(|e| anyhow::anyhow!("invalid repo spec: {e}"))?;
    spec.owner = spec.owner.to_ascii_lowercase();
    spec.name = spec.name.to_ascii_lowercase();

    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM repos WHERE host = ?1 AND owner = ?2 AND name = ?3",
            params![spec.host, spec.owner, spec.name],
            |r| r.get(0),
        )
        .ok();
    if let Some(existing_id) = &existing {
        bail!(
            "repo {}/{} already tracked (id={})",
            spec.owner,
            spec.name,
            existing_id
        );
    }

    let id = Uuid::new_v4().to_string();
    let local_path = resolve_local_path(projects_dir, &spec);
    let now = now_secs();

    conn.execute(
        "INSERT INTO repos (id, host, owner, name, branch, alias, clone_url, local_path, \
                            visibility, archived, disabled, added_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'unknown', 0, 0, ?9, ?10)",
        params![
            id,
            spec.host,
            spec.owner,
            spec.name,
            spec.branch,
            spec.alias,
            spec.clone_url,
            local_path,
            now,
            now,
        ],
    )
    .context("inserting repo into state DB")?;

    Ok(TrackedRepo {
        id,
        host: spec.host,
        owner: spec.owner,
        name: spec.name,
        branch: spec.branch,
        alias: spec.alias,
        clone_url: spec.clone_url,
        local_path,
        visibility: "unknown".to_string(),
        archived: false,
        disabled: false,
    })
}

/// Remove a tracked repo by `owner/name` or alias. Returns the removed repo.
pub fn remove(conn: &Connection, key: &str) -> Result<TrackedRepo> {
    let repo = find_repo(conn, key).context("repo not found")?;
    delete_repo_cascade(conn, &repo.id).context("deleting repo from state DB")?;
    Ok(repo)
}

/// Delete a repo and all rows in dependent tables that reference it.
///
/// SQLite enforces `PRAGMA foreign_keys=ON` (see `ro_state::open_db`), so a
/// plain `DELETE FROM repos` fails once `sync_results`, `repo_health_snapshots`,
/// `context_cache`, `plans`, or `jobs` reference the repo. The schema does not
/// declare `ON DELETE CASCADE`, so we emulate it here in a single transaction
/// to keep `remove` / `prune` atomic.
pub fn delete_repo_cascade(conn: &Connection, repo_id: &str) -> rusqlite::Result<()> {
    // Tables with a NOT NULL FK to repos(id) — these would block the parent
    // delete outright and must be cleared first.
    const CHILD_TABLES: &[&str] = &["sync_results", "repo_health_snapshots", "context_cache"];
    // Tables with a nullable FK to repos(id). We null them out so historical
    // run/job records survive a prune (audit-friendly) but no longer
    // hold a reference to a row that's about to disappear.
    //
    // "plans" was here and is now gone with the V4 migration that drops its
    // table. Leaving the name in would make `ro remove` issue an UPDATE
    // against a table that no longer exists — green in every static check,
    // broken at runtime.
    const NULLABLE_FK_TABLES: &[&str] = &["jobs"];

    let tx = conn.unchecked_transaction()?;
    for table in CHILD_TABLES {
        tx.execute(
            &format!("DELETE FROM {table} WHERE repo_id = ?1"),
            params![repo_id],
        )?;
    }
    for table in NULLABLE_FK_TABLES {
        tx.execute(
            &format!("UPDATE {table} SET repo_id = NULL WHERE repo_id = ?1"),
            params![repo_id],
        )?;
    }
    tx.execute("DELETE FROM repos WHERE id = ?1", params![repo_id])?;
    tx.commit()
}

/// List all tracked repos, optionally filtered by owner prefix.
pub fn list(conn: &Connection, owner_filter: Option<&str>) -> Result<Vec<TrackedRepo>> {
    let mut repos = Vec::new();

    match owner_filter {
        Some(owner) => {
            let pattern = format!("{owner}%");
            let mut stmt = conn.prepare(&format!(
                "SELECT {REPO_COLUMNS} FROM repos WHERE owner LIKE ?1 ORDER BY owner, name"
            ))?;
            let mut rows = stmt.query(params![pattern])?;
            while let Some(row) = rows.next()? {
                repos.push(row_to_tracked(row)?);
            }
        }
        None => {
            let mut stmt = conn.prepare(&format!(
                "SELECT {REPO_COLUMNS} FROM repos ORDER BY owner, name"
            ))?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                repos.push(row_to_tracked(row)?);
            }
        }
    }

    Ok(repos)
}

/// Find a repo by `owner/name`, alias, or raw id.
/// Owner/name lookups are case-insensitive (GitHub convention).
pub fn find_repo(conn: &Connection, key: &str) -> Result<TrackedRepo> {
    // Try owner/name
    let parts: Vec<&str> = key.splitn(2, '/').collect();
    if parts.len() == 2 {
        if let Ok(r) = conn.query_row(
            &format!("SELECT {REPO_COLUMNS} FROM repos WHERE LOWER(owner)=LOWER(?1) AND LOWER(name)=LOWER(?2)"),
            params![parts[0], parts[1]],
            row_to_tracked,
        ) {
            return Ok(r);
        }
    }
    // Try alias
    if let Ok(r) = conn.query_row(
        &format!("SELECT {REPO_COLUMNS} FROM repos WHERE alias=?1"),
        params![key],
        row_to_tracked,
    ) {
        return Ok(r);
    }
    // Try id
    if let Ok(r) = conn.query_row(
        &format!("SELECT {REPO_COLUMNS} FROM repos WHERE id=?1"),
        params![key],
        row_to_tracked,
    ) {
        return Ok(r);
    }
    bail!("repo '{key}' not found")
}

/// Build the on-disk path a repo will occupy.
///
/// Uses `PathBuf::join` rather than `format!("{}/{}/{}")` so the stored
/// string carries the platform's own separator. The old `format!` wrote
/// forward slashes on Windows while `collect_git_dirs` produced
/// backslashes, and the two were compared as raw strings — so on Windows
/// *every* tracked repo was reported as an orphan.
///
/// Note this only fixes rows written from now on. `find_orphans`
/// canonicalizes before comparing, which is what rescues the forward-slash
/// rows already sitting in existing `state.db` files. Both halves are
/// required; either alone leaves users with a wrong orphan list.
fn resolve_local_path(projects_dir: &Path, spec: &RepoSpec) -> String {
    let joined = projects_dir.join(&spec.owner).join(&spec.name);
    ro_config::paths::expand_tilde(&joined.to_string_lossy())
        .to_string_lossy()
        .into_owned()
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
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
    fn init_creates_config_and_db() {
        let tmp = TempDir::new().unwrap();
        let paths = ConfigPaths {
            config_dir: tmp.path().join("cfg"),
            state_dir: tmp.path().join("state"),
            cache_dir: tmp.path().join("cache"),
        };
        assert!(init(&paths).unwrap());
        assert!(paths.config_toml().exists());
        assert!(paths.state_db().exists());
    }

    #[test]
    fn init_idempotent() {
        let tmp = TempDir::new().unwrap();
        let paths = ConfigPaths {
            config_dir: tmp.path().join("cfg"),
            state_dir: tmp.path().join("state"),
            cache_dir: tmp.path().join("cache"),
        };
        assert!(init(&paths).unwrap());
        assert!(!init(&paths).unwrap());
    }

    #[test]
    fn add_basic() {
        let (tmp, conn) = setup();
        let repo = add(&conn, "quangdang46/repo_orchestrator", &projects_dir(&tmp)).unwrap();
        assert_eq!(repo.owner, "quangdang46");
        assert_eq!(repo.name, "repo_orchestrator");
        assert_eq!(repo.host, "github.com");
        assert!(repo.clone_url.contains("repo_orchestrator"));
    }

    #[test]
    fn add_with_branch_and_alias() {
        let (tmp, conn) = setup();
        let repo = add(
            &conn,
            "quangdang46/repo_orchestrator#develop as ro",
            &projects_dir(&tmp),
        )
        .unwrap();
        assert_eq!(repo.branch.as_deref(), Some("develop"));
        assert_eq!(repo.alias.as_deref(), Some("ro"));
    }

    #[test]
    fn add_rejects_duplicate() {
        let (tmp, conn) = setup();
        add(&conn, "quangdang46/repo_orchestrator", &projects_dir(&tmp)).unwrap();
        let err = add(&conn, "quangdang46/repo_orchestrator", &projects_dir(&tmp)).unwrap_err();
        assert!(err.to_string().contains("already tracked"));
    }

    #[test]
    fn add_rejects_invalid_spec() {
        let (tmp, conn) = setup();
        let err = add(&conn, "notaslash", &projects_dir(&tmp)).unwrap_err();
        assert!(err.to_string().contains("invalid repo spec"));
    }

    #[test]
    fn remove_by_owner_name() {
        let (tmp, conn) = setup();
        add(&conn, "quangdang46/repo_orchestrator", &projects_dir(&tmp)).unwrap();
        let removed = remove(&conn, "quangdang46/repo_orchestrator").unwrap();
        assert_eq!(removed.name, "repo_orchestrator");
        assert!(list(&conn, None).unwrap().is_empty());
    }

    #[test]
    fn remove_by_alias() {
        let (tmp, conn) = setup();
        add(&conn, "quangdang46/repo_orchestrator as ro", &projects_dir(&tmp)).unwrap();
        let removed = remove(&conn, "ro").unwrap();
        assert_eq!(removed.name, "repo_orchestrator");
    }

    #[test]
    fn remove_missing_fails() {
        let (_, conn) = setup();
        let err = remove(&conn, "nonexistent/repo").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    /// Regression: a repo with sync_results / health_snapshots referencing
    /// it must still be removable. Pre-fix `DELETE FROM repos` tripped
    /// SQLite's FK enforcement.
    #[test]
    fn remove_clears_dependent_rows() {
        let (tmp, conn) = setup();
        let repo = add(&conn, "alice/proj1", &projects_dir(&tmp)).unwrap();

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
        ro_state::queries::score_repo_health(&conn, &repo.id).unwrap();

        remove(&conn, &repo.id).unwrap();
        assert!(list(&conn, None).unwrap().is_empty());

        // Child rows are gone; the run record itself survives as audit history.
        let remaining_sync: i64 = conn
            .query_row("SELECT COUNT(*) FROM sync_results", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining_sync, 0);
        let remaining_health: i64 = conn
            .query_row("SELECT COUNT(*) FROM repo_health_snapshots", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(remaining_health, 0);
    }

    #[test]
    fn list_returns_added_repos() {
        let (tmp, conn) = setup();
        add(&conn, "alice/proj1", &projects_dir(&tmp)).unwrap();
        add(&conn, "bob/proj2", &projects_dir(&tmp)).unwrap();
        let repos = list(&conn, None).unwrap();
        assert_eq!(repos.len(), 2);
    }

    #[test]
    fn list_with_owner_filter() {
        let (tmp, conn) = setup();
        add(&conn, "alice/proj1", &projects_dir(&tmp)).unwrap();
        add(&conn, "bob/proj2", &projects_dir(&tmp)).unwrap();
        let repos = list(&conn, Some("alice")).unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].owner, "alice");
    }

    #[test]
    fn tracked_repo_display() {
        let repo = TrackedRepo {
            id: "x".into(),
            host: "github.com".into(),
            owner: "quangdang46".into(),
            name: "repo_orchestrator".into(),
            branch: None,
            alias: Some("ro".into()),
            clone_url: String::new(),
            local_path: String::new(),
            visibility: "unknown".into(),
            archived: false,
            disabled: false,
        };
        assert_eq!(format!("{repo}"), "quangdang46/repo_orchestrator as ro");
    }

    #[test]
    fn find_repo_by_id() {
        let (tmp, conn) = setup();
        let added = add(&conn, "alice/proj1", &projects_dir(&tmp)).unwrap();
        let found = find_repo(&conn, &added.id).unwrap();
        assert_eq!(found.name, "proj1");
    }
}
