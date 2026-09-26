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

/// One forward-only schema step.
///
/// `Sql` covers everything expressible as a single idempotent script — which
/// is most of it, because `CREATE TABLE IF NOT EXISTS` is naturally
/// re-runnable. `Guarded` exists because SQLite has **no** `ADD COLUMN IF NOT
/// EXISTS`: a migration that adds a column cannot be written as plain SQL and
/// be safe to run twice, and "safe to run twice" is not optional here. If the
/// process dies between the ALTER and the `_meta` version write, the next run
/// sees the old version and replays the step — which for a bare `ADD COLUMN`
/// means `duplicate column name` and ro fails to open its own database.
enum Migration {
    Sql(&'static str),
    Guarded(fn(&Connection) -> Result<()>),
}

fn apply(conn: &Connection, from: i64) -> Result<()> {
    let migrations: &[Migration] = &[
        // v1: full initial schema (PLAN.md §13)
        Migration::Sql(V1_INITIAL_SCHEMA),
        // v2: repo_tags (ADDITION.md A2)
        Migration::Sql(V2_REPO_TAGS),
        // v3: DROP inbox_dismissed (removed inbox feature)
        Migration::Sql(V3_DROP_INBOX),
        // v4: DROP plans (review lifecycle) + the cached default_branch
        Migration::Sql(V4_DROP_PLANS),
        // v5: per-repo config columns
        Migration::Guarded(v5_add_repo_config),
    ];

    for (i, migration) in migrations.iter().enumerate() {
        let version = (i + 1) as i64;
        if version > from {
            tracing::info!(version, "applying migration");
            match migration {
                Migration::Sql(sql) => conn
                    .execute_batch(sql)
                    .with_context(|| format!("applying migration v{version}"))?,
                Migration::Guarded(step) => {
                    step(conn).with_context(|| format!("applying migration v{version}"))?
                }
            }
            conn.execute(
                "INSERT OR REPLACE INTO _meta (key, value) VALUES ('version', ?1)",
                [version.to_string()],
            )?;
        }
    }
    Ok(())
}

/// Does `table` have a column named `column`?
///
/// Backed by `PRAGMA table_info`, which is the only statement that answers
/// this — there is no `information_schema` and no `ALTER TABLE ... IF NOT
/// EXISTS`. The table name is interpolated because `PRAGMA` does not accept a
/// bound parameter; every caller passes a literal.
pub fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    let sql = format!("PRAGMA table_info({table})");
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return false;
    };
    let Ok(mut rows) = stmt.query([]) else {
        return false;
    };
    while let Ok(Some(row)) = rows.next() {
        // table_info columns: cid, name, type, notnull, dflt_value, pk
        let name: String = match row.get(1) {
            Ok(n) => n,
            Err(_) => return false,
        };
        if name == column {
            return true;
        }
    }
    false
}

/// v5: the four per-repo config columns. This **is** the entire per-repo
/// config layer — there is no `.ro/config.local.toml`.
///
/// Each is NULL by default, and NULL is meaningful throughout: no
/// `credential_ref` means the global `[auth]` default, no `author_ref` means
/// `[identity].default`, no `engine` means `[agent].engine`. An existing row
/// that predates this migration therefore stays on the global configuration,
/// which is what a user upgrading should get.
fn v5_add_repo_config(conn: &Connection) -> Result<()> {
    for column in ["credential_ref", "author_ref", "engine", "engine_args"] {
        if column_exists(conn, "repos", column) {
            tracing::debug!(column, "column already present, skipping");
            continue;
        }
        conn.execute_batch(&format!("ALTER TABLE repos ADD COLUMN {column} TEXT;"))
            .with_context(|| format!("adding repos.{column}"))?;
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
        assert_eq!(v, 5);
    }

    #[test]
    fn migration_is_idempotent() {
        let conn = fresh();
        run(&conn).unwrap();
        run(&conn).unwrap();
        let v = current_version(&conn).unwrap();
        assert_eq!(v, 5);
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
            "INSERT INTO plans (id, repo_id, kind, status, created_at, plan_json) \
             VALUES ('p1', 'r1', 'review', 'draft', 0, '{}')",
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

        // `run` applies every pending migration, so it carries this fixture
        // to the current version, not just to 4. The V4-specific assertions
        // below are what this test is about.
        assert_eq!(current_version(&conn).unwrap(), 5);
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
        assert_eq!(current_version(&conn).unwrap(), 5);
    }

    fn table_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?1",
            [name],
            |_| Ok(()),
        )
        .is_ok()
    }

    #[test]
    fn v5_adds_the_four_config_columns() {
        let conn = fresh();
        run(&conn).unwrap();
        for column in ["credential_ref", "author_ref", "engine", "engine_args"] {
            assert!(
                column_exists(&conn, "repos", column),
                "v5 must add repos.{column}"
            );
        }
    }

    /// The trap the `Guarded` variant exists for. `ALTER TABLE ... ADD COLUMN`
    /// has no `IF NOT EXISTS` in SQLite, so a replayed V5 dies with
    /// `duplicate column name` — and if the version write is what failed, the
    /// replay is exactly what the next run attempts. Re-running the step
    /// against an already-migrated database must be a no-op, not an error.
    #[test]
    fn v5_is_idempotent_when_replayed_at_the_same_version() {
        let conn = fresh();
        run(&conn).unwrap();
        // The version gate would skip it; call the step directly to prove the
        // step itself is re-runnable, which is what the gate does not protect.
        v5_add_repo_config(&conn).unwrap();
        v5_add_repo_config(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), 5);
    }

    /// An existing row must land on the global configuration, not on a value
    /// invented for it. Every one of the four columns is a NULL-means-inherit
    /// column, so a row that predates V5 has to stay NULL.
    #[test]
    fn v4_database_upgrades_to_v5_leaving_existing_rows_on_the_global_default() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE IF NOT EXISTS _meta (key TEXT PRIMARY KEY, value TEXT);")
            .unwrap();
        conn.execute_batch(V1_INITIAL_SCHEMA).unwrap();
        conn.execute_batch(V2_REPO_TAGS).unwrap();
        conn.execute_batch(V3_DROP_INBOX).unwrap();
        conn.execute_batch(V4_DROP_PLANS).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO _meta (key, value) VALUES ('version', '4')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, branch, alias, clone_url, \
             local_path, visibility, archived, disabled, added_at, updated_at) \
             VALUES ('r1', 'github.com', 'acme', 'api', 'main', NULL, \
             'https://github.com/acme/api.git', '/tmp/api', 'public', 0, 0, 0, 0)",
            [],
        )
        .unwrap();

        // Precondition: V5 has not run yet.
        assert!(!column_exists(&conn, "repos", "credential_ref"));

        run(&conn).unwrap();

        assert_eq!(current_version(&conn).unwrap(), 5);
        let row: (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT credential_ref, author_ref, engine, engine_args FROM repos WHERE id = 'r1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (None, None, None, None),
            "an upgraded row must inherit the global config, not gain a value"
        );
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
