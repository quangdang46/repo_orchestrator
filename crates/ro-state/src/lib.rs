//! SQLite state management for ro.
//!
//! SQLite is the single source of truth for all ro state. This crate
//! handles connection management, schema migrations, and typed queries.
//!
//! See PLAN.md §13 for the complete schema.

pub mod migrate;
pub mod queries;

// Re-export rusqlite so downstream crates (ro CLI, integration tests) can
// name `ro_state::Connection` without taking a direct rusqlite dependency
// and risking a version mismatch with the one we link.
pub use rusqlite;
pub use rusqlite::Connection;

use anyhow::{Context, Result};
use std::path::Path;

/// Open (or create) the ro state database with PRAGMAs and migrations applied.
///
/// Sets:
/// - `journal_mode=WAL` — concurrent reads while writing
/// - `foreign_keys=ON` — enforce referential integrity
/// - `synchronous=NORMAL` — durable but not paranoid
/// - `busy_timeout=5000` — wait up to 5s on lock contention
pub fn open_db(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating state directory {}", parent.display()))?;
    }
    let conn = Connection::open(path)
        .with_context(|| format!("opening state database at {}", path.display()))?;
    apply_pragmas(&conn)?;
    migrate::run(&conn)?;
    Ok(conn)
}

/// Open an in-memory database. Useful for tests.
pub fn open_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory().context("opening in-memory state database")?;
    apply_pragmas(&conn)?;
    migrate::run(&conn)?;
    Ok(conn)
}

fn apply_pragmas(conn: &Connection) -> Result<()> {
    // journal_mode is queried (not executed) because it returns a row.
    let mode: String = conn
        .query_row("PRAGMA journal_mode=WAL;", [], |r| r.get(0))
        .context("setting journal_mode=WAL")?;
    tracing::debug!(journal_mode = %mode, "sqlite journal mode");
    conn.execute_batch(
        "PRAGMA foreign_keys=ON; \
         PRAGMA synchronous=NORMAL; \
         PRAGMA busy_timeout=5000;",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_memory_succeeds() {
        let conn = open_memory().expect("open in-memory db");
        let version: i64 = conn
            .query_row("SELECT user_version FROM pragma_user_version", [], |r| {
                r.get(0)
            })
            .unwrap();
        let _ = version; // not used; just sanity check the connection
    }

    #[test]
    fn pragmas_are_set() {
        let conn = open_memory().expect("open in-memory db");
        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1, "foreign_keys must be ON");

        let busy: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert!(busy >= 5000, "busy_timeout must be >=5000ms");
    }

    #[test]
    fn open_creates_parent_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("nested/dir/state.db");
        let conn = open_db(&nested).expect("open db with nested parent");
        drop(conn);
        assert!(nested.exists());
    }
}
