//! Migration tests against a database the *previous* binary created.
//!
//! # Why this file exists and why it is the only one that can
//!
//! `ALTER TABLE repos DROP COLUMN default_branch` succeeds. The migration
//! succeeds. `cargo test` is **green** — because a freshly-built test
//! database never had the column in the first place, so a `SELECT` left
//! behind **compiles fine**. It is not a compile error; it is a wrong
//! answer at runtime, on every existing user's machine.
//!
//! So the normal test path cannot catch a stale column read at all. The
//! only way is to build a database *the old way* and open it with the
//! current binary, which is what every test here does.

use ro_state::migrate;
use rusqlite::{Connection, params};

/// A database left at `version`, as the binary of that day would have
/// left it.
///
/// The migrations are re-run from the real constants rather than
/// hand-written DDL: a hand-written "old schema" drifts from the real one
/// within a release, and a test that proves nothing is worse than no test.
fn database_at(version: i64) -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE IF NOT EXISTS _meta (key TEXT PRIMARY KEY, value TEXT);")
        .unwrap();
    conn.execute_batch(migrate::V1_INITIAL_SCHEMA).unwrap();
    conn.execute_batch(migrate::V2_REPO_TAGS).unwrap();
    conn.execute_batch(migrate::V3_DROP_INBOX).unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO _meta (key, value) VALUES ('version', ?1)",
        [version.to_string()],
    )
    .unwrap();
    conn
}

fn insert_repo(conn: &Connection, id: &str, owner: &str, name: &str) {
    conn.execute(
        "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
         VALUES (?1, 'github.com', ?2, ?3, ?4, ?5, 0, 0)",
        params![
            id,
            owner,
            name,
            format!("https://github.com/{owner}/{name}.git"),
            format!("/tmp/{owner}/{name}")
        ],
    )
    .unwrap();
}

/// The V4 path, against a database that actually has the things V4 drops.
///
/// A fresh test database never had a `plans` table or a
/// `repos.default_branch` column, so V4 is a no-op on it and the test
/// proves nothing. Here the V3 database is a real one.
#[test]
fn a_v3_database_upgrades_to_v4_and_loses_the_dropped_things() {
    let conn = database_at(3);

    // Preconditions: the things V4 exists to remove are present.
    let plans: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='plans'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(plans, 1, "a V3 database must have the plans table");
    let cols: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('repos') WHERE name='default_branch'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        cols, 1,
        "a V3 database must have repos.default_branch, or this test is \\
         proving nothing"
    );

    insert_repo(&conn, "r1", "acme", "api");

    migrate::run(&conn).unwrap();

    assert_eq!(migrate::current_version(&conn).unwrap(), 5);
    let plans_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='plans'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(plans_after, 0, "V4 must drop the plans table");
    let cols_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('repos') WHERE name='default_branch'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cols_after, 0, "V4 must drop repos.default_branch");

    // And the row survived both.
    let remaining: i64 = conn
        .query_row("SELECT COUNT(*) FROM repos WHERE id='r1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(remaining, 1, "V4 must not lose existing repos");
}

/// The V5 path, and the back-compat property that matters: every existing
/// row keeps the **global** credential, which is what a NULL means.
#[test]
fn a_v4_database_upgrades_to_v5_and_every_row_keeps_the_global_credential() {
    let conn = database_at(4);
    insert_repo(&conn, "r1", "acme", "api");
    insert_repo(&conn, "r2", "acme", "web");

    // Precondition: the columns V5 adds do not exist yet.
    let before: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('repos') WHERE name='credential_ref'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(before, 0, "a V4 database must not have credential_ref yet");

    migrate::run(&conn).unwrap();
    assert_eq!(migrate::current_version(&conn).unwrap(), 5);

    // Every row NULL means "inherit the global [auth] default", which is
    // the correct back-compat outcome — not merely the absence of a
    // migration. A row that gained an invented value would be worse than
    // one that gained nothing.
    let non_null: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM repos
             WHERE credential_ref IS NOT NULL OR author_ref IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        non_null, 0,
        "every pre-existing repo must keep the global credential, not gain \
         an invented per-repo one"
    );
}

/// **V5 is not idempotent in SQLite.** `ALTER TABLE … ADD COLUMN` has no
/// `IF NOT EXISTS` and errors with `duplicate column name` on a second
/// run.
///
/// This is the most likely way a migration like this ships broken: the
/// *second* run of ro on an upgraded machine fails at open, and the first
/// run looked fine.
#[test]
fn v5_survives_being_run_twice() {
    let conn = database_at(4);
    insert_repo(&conn, "r1", "acme", "api");

    migrate::run(&conn).unwrap();
    assert_eq!(migrate::current_version(&conn).unwrap(), 5);

    // Directly re-running the step, bypassing the version gate, is what
    // catches an unguarded `ADD COLUMN`. Skipping this leaves the guard
    // untested and the bug ships.
    migrate::run(&conn).unwrap();
    migrate::v5_add_repo_config(&conn).unwrap();
    migrate::v5_add_repo_config(&conn).unwrap();

    assert_eq!(migrate::current_version(&conn).unwrap(), 5);
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM repos WHERE id='r1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 1, "a repeated run must not lose rows");
}

/// A `SELECT` that enumerated `repos` columns before V5 must not silently
/// return nulls for the new ones.
///
/// `manage::list` selects a `REPO_COLUMNS` constant. If that constant
/// drifts from the migration, the query still succeeds — it just asks for
/// fewer columns — and a repo the user configured reads back as
/// unconfigured. Nothing errors, anywhere.
#[test]
fn a_stale_column_enumeration_does_not_return_nulls() {
    let conn = database_at(4);
    insert_repo(&conn, "r1", "acme", "api");
    migrate::run(&conn).unwrap();

    // Set a per-repo value through the real writer.
    ro_sync::manage::set_repo_config(&conn, "acme/api", "credential_ref", "env:WORK_TOKEN")
        .unwrap();

    // Then read it back through the real reader. If `REPO_COLUMNS` were
    // stale the write would succeed and the read would return None.
    let listed = ro_sync::manage::list(&conn, None).unwrap();
    let row = listed.iter().find(|r| r.id == "r1").unwrap();
    assert_eq!(
        row.credential_ref.as_deref(),
        Some("env:WORK_TOKEN"),
        "a value written through the registry must be readable back; if \
         this is None, REPO_COLUMNS has drifted from the migration"
    );

    // And a row with nothing set is None, not an empty string — the two
    // mean "inherit" and "set to nothing" and must not collapse.
    insert_repo(&conn, "r2", "acme", "web");
    let listed = ro_sync::manage::list(&conn, None).unwrap();
    let unset = listed.iter().find(|r| r.id == "r2").unwrap();
    assert_eq!(
        unset.credential_ref, None,
        "an unset value is None (inherit), not an empty string"
    );
}

/// The whole point in one test: open a pre-V4 database, migrate it, and
/// use it through the real API.
#[test]
fn a_v3_database_is_usable_through_the_real_api_after_upgrading() {
    let conn = database_at(3);
    insert_repo(&conn, "r1", "acme", "api");
    insert_repo(&conn, "r2", "acme", "web");

    migrate::run(&conn).unwrap();

    // `list` — the query with the constant column list.
    let listed = ro_sync::manage::list(&conn, None).unwrap();
    assert_eq!(listed.len(), 2, "both repos must survive the upgrade");

    // `find_repo` — a lookup by owner/name.
    let found = ro_sync::manage::find_repo(&conn, "acme/api").unwrap();
    assert_eq!(found.id, "r1");

    // `status` — reads git from disk, so the row must at least resolve
    // without a SQL error. A missing column surfaces here.
    let status = ro_sync::status::status_repo(&conn, "r1").unwrap();
    assert_eq!(status.owner, "acme");
    assert_eq!(
        status.unmeasurable_reason.as_deref(),
        Some("not cloned"),
        "a row whose worktree was never cloned is unmeasurable, not clean"
    );
}
