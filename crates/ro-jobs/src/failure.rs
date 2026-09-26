//! Failure classification - persistence for it.
//!
//! The `FailureClass` taxonomy itself lives in `ro-core` and is re-exported
//! here, so `ro_jobs::FailureClass` and `ro_core::FailureClass` are the same
//! type rather than two that happen to have the same name.
//!
//! What stays here is the genuinely jobs-shaped part: the SQLite `failures`
//! table, the fingerprinting that lets one cause be counted across runs, and
//! the exit-code-aware entry point.

// `pub use` rather than a plain `use`: this both brings the type into scope for
// the SQLite half below and re-exports it, so `ro_jobs::FailureClass` and
// `ro_core::FailureClass` are the same type rather than two that happen to
// share a name and a shape.
pub use ro_core::FailureClass;
use serde::{Deserialize, Serialize};

/// Classify a process exit code plus its stderr.
///
/// The exit code is consulted **first** for the one thing only it can say: 126
/// and 127 are "could not execute", which is [`FailureClass::MissingGit`]
/// when the child was the VCS binary. Everything else defers to the text,
/// because the tools overlap exit codes for causes that need opposite
/// responses - 1 is both "auth failed" and "network flaked", and only the text
/// separates them.
pub fn classify_exit(exit_code: i32, stderr: &str) -> FailureClass {
    if exit_code == 126 || exit_code == 127 {
        return FailureClass::MissingGit;
    }
    if !stderr.trim().is_empty() {
        return FailureClass::classify(stderr);
    }
    // No output at all. The code is all there is.
    match exit_code {
        2 => FailureClass::NetworkTimeout,
        _ => FailureClass::AuthError,
    }
}

/// A row from the `failures` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureRecord {
    pub id: String,
    pub fingerprint: String,
    pub class: FailureClass,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    pub count: i64,
    pub suggested_fix: Option<String>,
}

/// Ensure the `failures` table exists.
fn ensure_table(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS failures (
            id TEXT PRIMARY KEY,
            fingerprint TEXT NOT NULL UNIQUE,
            class TEXT NOT NULL,
            first_seen_at INTEGER NOT NULL,
            last_seen_at INTEGER NOT NULL,
            count INTEGER NOT NULL,
            suggested_fix TEXT
        )",
        [],
    )?;
    Ok(())
}

/// Upsert a failure record. Increments `count` on fingerprint conflict.
pub fn record_failure(
    conn: &rusqlite::Connection,
    fingerprint: &str,
    class: FailureClass,
    suggested_fix: Option<&str>,
) -> rusqlite::Result<()> {
    ensure_table(conn)?;
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let class_str = class.to_string();

    // Try update first (existing fingerprint)
    let updated = conn.execute(
        "UPDATE failures SET count = count + 1, last_seen_at = ?1 WHERE fingerprint = ?2",
        rusqlite::params![now, fingerprint],
    )?;

    if updated == 0 {
        // Insert new record
        let id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO failures (id, fingerprint, class, first_seen_at, last_seen_at, count, suggested_fix)
             VALUES (?1, ?2, ?3, ?4, ?4, 1, ?5)",
            rusqlite::params![id, fingerprint, class_str, now, suggested_fix],
        )?;
    }
    Ok(())
}

/// List all failures, optionally filtered by class.
pub fn list_failures(
    conn: &rusqlite::Connection,
    class_filter: Option<FailureClass>,
) -> rusqlite::Result<Vec<FailureRecord>> {
    ensure_table(conn)?;
    let mut query = String::from(
        "SELECT id, fingerprint, class, first_seen_at, last_seen_at, count, suggested_fix FROM failures",
    );
    // params are handled inline with class_filter

    if class_filter.is_some() {
        query.push_str(" WHERE class = ?1");
    }
    query.push_str(" ORDER BY last_seen_at DESC");

    let mut stmt = conn.prepare(&query)?;

    if let Some(class) = class_filter {
        let class_str = class.to_string();
        let rows = stmt.query_map([class_str], |row| {
            let class_str: String = row.get(2)?;
            Ok(FailureRecord {
                id: row.get(0)?,
                fingerprint: row.get(1)?,
                class: serde_json::from_value(serde_json::Value::String(class_str.clone()))
                    .unwrap_or(FailureClass::AuthError),
                first_seen_at: row.get(3)?,
                last_seen_at: row.get(4)?,
                count: row.get(5)?,
                suggested_fix: row.get(6)?,
            })
        })?;
        return rows.collect();
    }

    let rows = stmt.query_map([], |row| {
        let class_str: String = row.get(2)?;
        Ok(FailureRecord {
            id: row.get(0)?,
            fingerprint: row.get(1)?,
            class: serde_json::from_value(serde_json::Value::String(class_str.clone()))
                .unwrap_or(FailureClass::AuthError),
            first_seen_at: row.get(3)?,
            last_seen_at: row.get(4)?,
            count: row.get(5)?,
            suggested_fix: row.get(6)?,
        })
    })?;
    rows.collect()
}

/// Get a single failure by fingerprint.
pub fn get_failure(
    conn: &rusqlite::Connection,
    fingerprint: &str,
) -> rusqlite::Result<Option<FailureRecord>> {
    ensure_table(conn)?;
    let mut stmt = conn.prepare(
        "SELECT id, fingerprint, class, first_seen_at, last_seen_at, count, suggested_fix
         FROM failures WHERE fingerprint = ?1",
    )?;
    let mut rows = stmt.query_map([fingerprint], |row| {
        let class_str: String = row.get(2)?;
        Ok(FailureRecord {
            id: row.get(0)?,
            fingerprint: row.get(1)?,
            class: serde_json::from_value(serde_json::Value::String(class_str.clone()))
                .unwrap_or(FailureClass::AuthError),
            first_seen_at: row.get(3)?,
            last_seen_at: row.get(4)?,
            count: row.get(5)?,
            suggested_fix: row.get(6)?,
        })
    })?;
    rows.next().transpose()
}

/// Delete all failure records.
pub fn clear_failures(conn: &rusqlite::Connection) -> rusqlite::Result<usize> {
    ensure_table(conn)?;
    conn.execute("DELETE FROM failures", [])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_auth_error() {
        assert_eq!(
            classify_exit(1, "Authentication failed for repository"),
            FailureClass::AuthError
        );
    }

    #[test]
    fn classify_github_permission_denied() {
        assert_eq!(
            classify_exit(1, "github: Permission denied to api.github.com"),
            FailureClass::GithubPermissionDenied
        );
    }

    #[test]
    fn classify_rate_limited() {
        assert_eq!(
            classify_exit(1, "rate limit exceeded, retry after 60s"),
            FailureClass::RateLimited
        );
    }

    #[test]
    fn classify_merge_conflict() {
        assert_eq!(
            classify_exit(1, "CONFLICT (content): Merge conflict in src/main.rs"),
            FailureClass::MergeConflict
        );
    }

    #[test]
    fn classify_dirty_worktree() {
        assert_eq!(
            classify_exit(1, "your local changes would be overwritten"),
            FailureClass::DirtyWorktree
        );
    }

    #[test]
    fn classify_network_timeout() {
        assert_eq!(
            classify_exit(1, "fatal: connection timed out"),
            FailureClass::NetworkTimeout
        );
    }

    #[test]
    fn classify_missing_git() {
        assert_eq!(
            classify_exit(127, "git: not found"),
            FailureClass::MissingGit
        );
    }

    #[test]
    fn classify_quality_gate() {
        assert_eq!(
            classify_exit(1, "quality gate failed: clippy reported errors"),
            FailureClass::QualityGateFailed
        );
    }

    #[test]
    fn classify_secret_scan() {
        assert_eq!(
            classify_exit(1, "blocked by secret scan: AWS key found"),
            FailureClass::SecretScanBlocked
        );
    }

    #[test]
    fn classify_missing_provider() {
        assert_eq!(
            classify_exit(1, "provider not found: claude"),
            FailureClass::MissingProvider
        );
    }

    #[test]
    fn retryable_classes() {
        assert!(FailureClass::RateLimited.is_retryable());
        assert!(FailureClass::NetworkTimeout.is_retryable());
        assert!(FailureClass::MergeConflict.is_retryable());
        assert!(!FailureClass::AuthError.is_retryable());
        assert!(!FailureClass::SecretScanBlocked.is_retryable());
    }

    #[test]
    fn display_roundtrip() {
        for variant in [
            FailureClass::AuthError,
            FailureClass::RateLimited,
            FailureClass::MergeConflict,
            FailureClass::SecretScanBlocked,
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            let back: FailureClass = serde_json::from_str(&json).unwrap();
            assert_eq!(variant, back);
        }
    }

    use ro_state::open_memory;

    #[test]
    fn record_and_get_failure() {
        let conn = open_memory().unwrap();
        record_failure(&conn, "fp-1", FailureClass::AuthError, Some("re-auth")).unwrap();
        let rec = get_failure(&conn, "fp-1").unwrap().unwrap();
        assert_eq!(rec.fingerprint, "fp-1");
        assert_eq!(rec.class, FailureClass::AuthError);
        assert_eq!(rec.count, 1);
        assert_eq!(rec.suggested_fix.as_deref(), Some("re-auth"));
    }

    #[test]
    fn record_failure_increments_count() {
        let conn = open_memory().unwrap();
        record_failure(&conn, "fp-inc", FailureClass::RateLimited, None).unwrap();
        record_failure(&conn, "fp-inc", FailureClass::RateLimited, None).unwrap();
        record_failure(&conn, "fp-inc", FailureClass::RateLimited, None).unwrap();
        let rec = get_failure(&conn, "fp-inc").unwrap().unwrap();
        assert_eq!(rec.count, 3);
    }

    #[test]
    fn list_failures_all() {
        let conn = open_memory().unwrap();
        record_failure(&conn, "fp-a", FailureClass::AuthError, None).unwrap();
        record_failure(&conn, "fp-b", FailureClass::RateLimited, None).unwrap();
        let all = list_failures(&conn, None).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn list_failures_filtered() {
        let conn = open_memory().unwrap();
        record_failure(&conn, "fp-auth", FailureClass::AuthError, None).unwrap();
        record_failure(&conn, "fp-net", FailureClass::NetworkTimeout, None).unwrap();
        let filtered = list_failures(&conn, Some(FailureClass::AuthError)).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].fingerprint, "fp-auth");
    }

    #[test]
    fn get_failure_missing() {
        let conn = open_memory().unwrap();
        assert!(get_failure(&conn, "nonexistent").unwrap().is_none());
    }

    #[test]
    fn clear_failures_works() {
        let conn = open_memory().unwrap();
        record_failure(&conn, "fp-x", FailureClass::AuthError, None).unwrap();
        record_failure(&conn, "fp-y", FailureClass::RateLimited, None).unwrap();
        let deleted = clear_failures(&conn).unwrap();
        assert_eq!(deleted, 2);
        let all = list_failures(&conn, None).unwrap();
        assert!(all.is_empty());
    }
}
