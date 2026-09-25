//! Database migrations for ro.
//!
//! Migrations are linear, additive, and idempotent. Each migration is a
//! single SQL string executed in order. The current applied version is
//! recorded in `_meta` (key/value table) so we never re-run a migration.

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Run all pending migrations.
pub fn run(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS _meta (key TEXT PRIMARY KEY, value TEXT);")
        .context("ensuring _meta table exists")?;

    let current: i64 = conn
        .query_row(
            "SELECT COALESCE(CAST(value AS INTEGER), 0) FROM _meta WHERE key='version'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    apply(conn, current)
}

fn apply(conn: &Connection, from: i64) -> Result<()> {
    let migrations: &[&str] = &[
        // v1: full initial schema (PLAN.md §13)
        V1_INITIAL_SCHEMA,
        // v2: repo_tags (ADDITION.md A2)
        V2_REPO_TAGS,
        // v3: DROP inbox_dismissed (removed inbox feature)
        V3_DROP_INBOX,
        // v4: DROP plans (review lifecycle) + the cached default_branch
        V4_DROP_PLANS,
    ];

    for (i, sql) in migrations.iter().enumerate() {
        let version = (i + 1) as i64;
        if version > from {
            tracing::info!(version, "applying migration");
            conn.execute_batch(sql)
                .with_context(|| format!("applying migration v{version}"))?;
            conn.execute(
                "INSERT OR REPLACE INTO _meta (key, value) VALUES ('version', ?1)",
                [version.to_string()],
            )?;
        }
    }
    Ok(())
}

/// Get the currently applied schema version.
pub fn current_version(conn: &Connection) -> Result<i64> {
    let v: i64 = conn
        .query_row(
            "SELECT COALESCE(CAST(value AS INTEGER), 0) FROM _meta WHERE key='version'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    Ok(v)
}

const V1_INITIAL_SCHEMA: &str = r#"
-- v1: initial schema (PLAN.md §13)

CREATE TABLE IF NOT EXISTS repos (
    id              TEXT PRIMARY KEY,
    host            TEXT NOT NULL DEFAULT 'github.com',
    owner           TEXT NOT NULL,
    name            TEXT NOT NULL,
    branch          TEXT,
    alias           TEXT,
    clone_url       TEXT NOT NULL,
    local_path      TEXT NOT NULL,
    visibility      TEXT NOT NULL DEFAULT 'unknown',
    default_branch  TEXT,
    archived        INTEGER NOT NULL DEFAULT 0,
    disabled        INTEGER NOT NULL DEFAULT 0,
    added_at        INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    UNIQUE(host, owner, name)
);

CREATE INDEX IF NOT EXISTS idx_repos_owner_name ON repos(owner, name);

CREATE TABLE IF NOT EXISTS runs (
    id              TEXT PRIMARY KEY,
    command         TEXT NOT NULL,
    started_at      INTEGER NOT NULL,
    ended_at        INTEGER,
    exit_code       INTEGER,
    args_json       TEXT NOT NULL,
    user            TEXT,
    host            TEXT
);

CREATE INDEX IF NOT EXISTS idx_runs_started_at ON runs(started_at);

CREATE TABLE IF NOT EXISTS run_events (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id          TEXT NOT NULL REFERENCES runs(id),
    ts              INTEGER NOT NULL,
    level           TEXT NOT NULL,
    message         TEXT NOT NULL,
    data_json       TEXT
);

CREATE INDEX IF NOT EXISTS idx_run_events_run_ts ON run_events(run_id, ts);

CREATE TABLE IF NOT EXISTS sync_results (
    run_id          TEXT NOT NULL REFERENCES runs(id),
    repo_id         TEXT NOT NULL REFERENCES repos(id),
    action          TEXT NOT NULL,
    status          TEXT NOT NULL,
    duration_ms     INTEGER NOT NULL,
    error           TEXT,
    pre_oid         TEXT,
    post_oid        TEXT,
    PRIMARY KEY (run_id, repo_id)
);

CREATE TABLE IF NOT EXISTS jobs (
    id              TEXT PRIMARY KEY,
    kind            TEXT NOT NULL,
    status          TEXT NOT NULL,
    repo_id         TEXT REFERENCES repos(id),
    payload_json    TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    started_at      INTEGER,
    ended_at        INTEGER,
    attempts        INTEGER NOT NULL DEFAULT 0,
    max_attempts    INTEGER NOT NULL DEFAULT 3,
    error           TEXT,
    created_by      TEXT NOT NULL DEFAULT 'cli'
);

CREATE INDEX IF NOT EXISTS idx_jobs_status_created ON jobs(status, created_at);

CREATE TABLE IF NOT EXISTS job_events (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id          TEXT NOT NULL REFERENCES jobs(id),
    ts              INTEGER NOT NULL,
    level           TEXT NOT NULL,
    message         TEXT NOT NULL,
    data_json       TEXT
);

CREATE INDEX IF NOT EXISTS idx_job_events_job_ts ON job_events(job_id, ts);

CREATE TABLE IF NOT EXISTS plans (
    id                  TEXT PRIMARY KEY,
    kind                TEXT NOT NULL,
    repo_id             TEXT REFERENCES repos(id),
    status              TEXT NOT NULL,
    created_at          INTEGER NOT NULL,
    applied_at          INTEGER,
    risk_class          TEXT,
    risk_reasons_json   TEXT,
    plan_json           TEXT NOT NULL,
    rollback_json       TEXT
);

CREATE TABLE IF NOT EXISTS failures (
    id              TEXT PRIMARY KEY,
    fingerprint     TEXT NOT NULL UNIQUE,
    class           TEXT NOT NULL,
    first_seen_at   INTEGER NOT NULL,
    last_seen_at    INTEGER NOT NULL,
    count           INTEGER NOT NULL,
    suggested_fix   TEXT
);

CREATE INDEX IF NOT EXISTS idx_failures_class ON failures(class);

CREATE TABLE IF NOT EXISTS repo_health_snapshots (
    id              TEXT PRIMARY KEY,
    repo_id         TEXT NOT NULL REFERENCES repos(id),
    ts              INTEGER NOT NULL,
    score           INTEGER NOT NULL,
    class           TEXT NOT NULL,
    details_json    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_health_repo_ts ON repo_health_snapshots(repo_id, ts);

CREATE TABLE IF NOT EXISTS context_cache (
    id              TEXT PRIMARY KEY,
    repo_id         TEXT NOT NULL REFERENCES repos(id),
    kind            TEXT NOT NULL,
    cache_key       TEXT NOT NULL,
    generated_at    INTEGER NOT NULL,
    expires_at      INTEGER,
    content_json    TEXT NOT NULL,
    UNIQUE(repo_id, kind, cache_key)
);

CREATE TABLE IF NOT EXISTS audit_log (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    ts              INTEGER NOT NULL,
    actor           TEXT NOT NULL,
    action          TEXT NOT NULL,
    target          TEXT,
    details_json    TEXT
);

CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit_log(ts);
"#;

const V2_REPO_TAGS: &str = r#"
-- v2: repo tags (ADDITION.md A2)

CREATE TABLE IF NOT EXISTS repo_tags (
    repo_id         TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    tag             TEXT NOT NULL,
    PRIMARY KEY (repo_id, tag)
);

CREATE INDEX IF NOT EXISTS idx_repo_tags_tag ON repo_tags(tag);
"#;

const V3_DROP_INBOX: &str = r#"
-- v3: drop inbox_dismissed (removed inbox feature ro-refactor)
DROP TABLE IF EXISTS inbox_dismissed;
"#;

// v4 drops two things, and the asymmetry is the point:
//
//   plans                 gone, with the review lifecycle. Its table is named
//                         by manage::NULLABLE_FK_TABLES, which was updated
//                         in the same commit — that list is a raw string
//                         array, and a stale entry there is invisible to
//                         every static check and only breaks at runtime.
//
//   repos.default_branch  gone. It was a cache of a value git answers more
//                         freshly: the base for a rebase comes from
//                         `git symbolic-ref refs/remotes/origin/HEAD`, and
//                         keeping a copy only lets the two disagree on a
//                         rename.
//
// repo_health_snapshots is deliberately NOT dropped: the scorer survives and
// --filter health:<N> still reads it.
//
// DROP COLUMN needs SQLite 3.35+ (2021). The workspace pins rusqlite with
// `bundled`, so the version that matters is the one cargo resolves, not the
// system sqlite3 — worth confirming rather than assuming.
const V4_DROP_PLANS: &str = r#"
-- v4: drop the review lifecycle and the cached default branch
DROP TABLE IF EXISTS plans;
ALTER TABLE repos DROP COLUMN default_branch;
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn fresh() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        run(&conn).unwrap();
        conn
    }

    #[test]
    fn migration_records_version() {
        let conn = fresh();
        let v = current_version(&conn).unwrap();
        assert_eq!(v, 4);
    }

    #[test]
    fn migration_is_idempotent() {
        let conn = fresh();
        run(&conn).unwrap();
        run(&conn).unwrap();
        let v = current_version(&conn).unwrap();
        assert_eq!(v, 4);
    }

    /// The upgrade path, which no other test in the workspace exercises.
    ///
    /// A fresh database applies V1 through V4 in one go, so it cannot catch a
    /// migration that only works on an empty schema. A database that already
    /// carries V1+V2+V3 — with a real repo row, a real `plans` row, and the
    /// `default_branch` column populated — is the only thing that proves the
    /// migration does what it claims to an existing user.
    #[test]
    fn v3_database_upgrades_to_v4() {
        let conn = Connection::open_in_memory().unwrap();

        // Build a v3 database: everything up to and including V3, nothing after.
        // `_meta` is created by `run()` itself, not by any migration, so the
        // fixture has to make it the way the binary would.
        conn.execute_batch("CREATE TABLE IF NOT EXISTS _meta (key TEXT PRIMARY KEY, value TEXT);")
            .unwrap();
        conn.execute_batch(V1_INITIAL_SCHEMA).unwrap();
        conn.execute_batch(V2_REPO_TAGS).unwrap();
        conn.execute_batch(V3_DROP_INBOX).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO _meta (key, value) VALUES ('version', '3')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, branch, alias, clone_url, \
             local_path, visibility, default_branch, archived, disabled, added_at, updated_at) \
             VALUES ('r1', 'github.com', 'acme', 'api', 'main', NULL, \
             'https://github.com/acme/api.git', '/tmp/api', 'public', 'main', 0, 0, 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO plans (id, repo_id, status, created_at) \
             VALUES ('p1', 'r1', 'draft', 0)",
            [],
        )
        .unwrap();

        // Preconditions, so a typo in the fixture fails here and not later.
        assert_eq!(current_version(&conn).unwrap(), 3);
        assert!(table_exists(&conn, "plans"), "fixture should have plans");
        assert!(
            column_exists(&conn, "repos", "default_branch"),
            "fixture should have default_branch"
        );

        run(&conn).unwrap();

        assert_eq!(current_version(&conn).unwrap(), 4);
        assert!(
            !table_exists(&conn, "plans"),
            "V4 must drop the plans table"
        );
        assert!(
            !column_exists(&conn, "repos", "default_branch"),
            "V4 must drop repos.default_branch"
        );
        // The row it was attached to must survive: the migration drops a
        // column, not a repository.
        let name: String = conn
            .query_row("SELECT name FROM repos WHERE id = 'r1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "api", "the repo row must survive the migration");

        // Re-running must not fail on the already-dropped column.
        run(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), 4);
    }

    fn table_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?1",
            [name],
            |_| Ok(()),
        )
        .is_ok()
    }

    fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
        // A missing column is what we want to detect, so a failed PRAGMA is
        // the answer rather than a panic.
        conn.prepare(&format!("SELECT {column} FROM {table} LIMIT 1"))
            .is_ok()
    }

    #[test]
    fn all_tables_exist() {
        let conn = fresh();
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        let tables: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        for expected in [
            "_meta",
            "audit_log",
            "context_cache",
            "failures",
            "job_events",
            "jobs",
            "repo_health_snapshots",
            "repo_tags",
            "repos",
            "run_events",
            "runs",
            "sync_results",
        ] {
            assert!(
                tables.iter().any(|t| t == expected),
                "expected table {expected} not found in {tables:?}",
            );
        }
    }

    #[test]
    fn all_indexes_exist() {
        let conn = fresh();
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'idx_%' ORDER BY name")
            .unwrap();
        let indexes: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        for expected in [
            "idx_audit_ts",
            "idx_failures_class",
            "idx_health_repo_ts",
            "idx_job_events_job_ts",
            "idx_jobs_status_created",
            "idx_repos_owner_name",
            "idx_run_events_run_ts",
            "idx_runs_started_at",
        ] {
            assert!(
                indexes.iter().any(|i| i == expected),
                "expected index {expected} not found in {indexes:?}",
            );
        }
    }

    #[test]
    fn foreign_keys_enforced() {
        let conn = fresh();
        // Need foreign_keys enabled (open_db sets it; for tests we set it manually).
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        // Inserting run_events with non-existent run_id should fail.
        let res = conn.execute(
            "INSERT INTO run_events (run_id, ts, level, message) VALUES ('missing', 0, 'info', 'hi')",
            [],
        );
        assert!(res.is_err(), "FK violation should fail");
    }

    #[test]
    fn repos_unique_constraint() {
        let conn = fresh();
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES ('id1', 'github.com', 'rust-lang', 'rust', 'https://example/x.git', '/tmp/x', 0, 0)",
            [],
        )
        .unwrap();
        let dup = conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES ('id2', 'github.com', 'rust-lang', 'rust', 'https://example/y.git', '/tmp/y', 0, 0)",
            [],
        );
        assert!(
            dup.is_err(),
            "(host,owner,name) UNIQUE must reject duplicates"
        );
    }
}
