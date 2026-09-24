//! Typed queries for ro state.
//!
//! Health scoring, inbox scoring, context queries.

use anyhow::Result;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Health scoring (rfo-31)
// ---------------------------------------------------------------------------

/// Health score classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthClass {
    Excellent, // 90-100
    Healthy,   // 75-89
    Attention, // 50-74
    Risky,     // 25-49
    Critical,  // 0-24
}

impl std::fmt::Display for HealthClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| format!("{self:?}"));
        f.write_str(&s)
    }
}

impl HealthClass {
    pub fn from_score(score: i64) -> Self {
        match score {
            s if s >= 90 => HealthClass::Excellent,
            s if s >= 75 => HealthClass::Healthy,
            s if s >= 50 => HealthClass::Attention,
            s if s >= 25 => HealthClass::Risky,
            _ => HealthClass::Critical,
        }
    }
}

/// Health snapshot for a repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthSnapshot {
    pub id: String,
    pub repo_id: String,
    pub ts: i64,
    pub score: i64,
    pub class: HealthClass,
    pub details_json: String,
}

/// Compute and store a health score for a repo.
pub fn score_repo_health(conn: &Connection, repo_id: &str) -> Result<HealthSnapshot> {
    let now = now_secs();
    let mut score: i64 = 100;

    // Penalties
    let archived: bool = conn
        .query_row(
            "SELECT archived FROM repos WHERE id = ?1",
            params![repo_id],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
        != 0;
    if archived {
        score -= 30;
    }

    let disabled: bool = conn
        .query_row(
            "SELECT disabled FROM repos WHERE id = ?1",
            params![repo_id],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
        != 0;
    if disabled {
        score -= 50;
    }

    // Failed runs penalty
    let failed_runs: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sync_results WHERE repo_id = ?1 AND status = 'error'",
            params![repo_id],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0);
    score -= (failed_runs * 5).min(30);

    // Recent failures penalty
    let recent_failures: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(count), 0) FROM failures WHERE last_seen_at > ?1",
            params![now - 86400],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0);
    score -= (recent_failures * 3).min(20);

    score = score.clamp(0, 100);
    let class = HealthClass::from_score(score);

    let id = Uuid::new_v4().to_string();
    let details = serde_json::json!({
        "archived": archived,
        "disabled": disabled,
        "failed_syncs": failed_runs,
        "recent_failures": recent_failures,
    })
    .to_string();

    conn.execute(
        "INSERT INTO repo_health_snapshots (id, repo_id, ts, score, class, details_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, repo_id, now, score, class.to_string(), details],
    )?;

    Ok(HealthSnapshot {
        id,
        repo_id: repo_id.to_string(),
        ts: now,
        score,
        class,
        details_json: details,
    })
}

/// Get the latest health snapshot for a repo.
pub fn latest_health(conn: &Connection, repo_id: &str) -> Result<Option<HealthSnapshot>> {
    Ok(conn
        .query_row(
            "SELECT id, repo_id, ts, score, class, details_json FROM repo_health_snapshots WHERE repo_id = ?1 ORDER BY ts DESC LIMIT 1",
            params![repo_id],
            row_to_health,
        )
        .ok())
}

/// Score health for all tracked repos.
pub fn score_all_health(conn: &Connection) -> Result<Vec<HealthSnapshot>> {
    let mut stmt = conn.prepare("SELECT id FROM repos")?;
    let ids: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();
    let mut snapshots = Vec::new();
    for id in ids {
        snapshots.push(score_repo_health(conn, &id)?);
    }
    Ok(snapshots)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Inbox item representing something needing attention.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxItem {
    pub repo_id: String,
    pub owner: String,
    pub name: String,
    pub priority: i64,
    pub reason: String,
}

/// Mark an inbox item as done (dismissed). Idempotent.
///
/// Future `compute_inbox` calls will exclude this repo's inbox entry.
pub fn mark_inbox_done(conn: &Connection, repo_id: &str) -> Result<()> {
    let now = now_secs();
    conn.execute(
        "INSERT OR REPLACE INTO inbox_dismissed (repo_id, dismissed_at) VALUES (?1, ?2)",
        params![repo_id, now],
    )?;
    Ok(())
}

/// Clear dismissals older than the given timestamp. Returns count cleared.
pub fn purge_inbox_dismissals(conn: &Connection, older_than: i64) -> Result<usize> {
    let n = conn.execute(
        "DELETE FROM inbox_dismissed WHERE dismissed_at < ?1",
        params![older_than],
    )?;
    Ok(n)
}

/// Compute inbox items for all repos.
pub fn compute_inbox(conn: &Connection) -> Result<Vec<InboxItem>> {
    let mut items = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT r.id, r.owner, r.name, r.archived, r.disabled \
         FROM repos r \
         WHERE r.id NOT IN (SELECT repo_id FROM inbox_dismissed) \
         ORDER BY r.owner, r.name",
    )?;
    let rows: Vec<(String, String, String, bool, bool)> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)? != 0,
                row.get::<_, i64>(4)? != 0,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    for (id, owner, name, archived, disabled) in rows {
        let mut priority = 0i64;
        let mut reasons = Vec::new();

        if disabled {
            priority += 10;
            reasons.push("disabled".into());
        }
        if archived {
            priority += 3;
            reasons.push("archived".into());
        }

        // Check for failed syncs
        let failed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sync_results WHERE repo_id = ?1 AND status = 'error'",
                params![id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0);
        if failed > 0 {
            priority += 5 * failed.min(3);
            reasons.push(format!("{failed} failed syncs"));
        }

        // Check for recent failures
        let recent: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM failures WHERE fingerprint LIKE '%' || ?1 || '%' AND last_seen_at > ?2",
                params![id, now_secs() - 86400],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0);
        if recent > 0 {
            priority += 3 * recent.min(3);
            reasons.push(format!("{recent} recent failures"));
        }

        if !reasons.is_empty() {
            items.push(InboxItem {
                repo_id: id,
                owner,
                name,
                priority,
                reason: reasons.join(", "),
            });
        }
    }

    items.sort_by_key(|item| std::cmp::Reverse(item.priority));
    Ok(items)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn row_to_health(row: &rusqlite::Row<'_>) -> std::result::Result<HealthSnapshot, rusqlite::Error> {
    let class_str: String = row.get(4)?;
    let class = match class_str.as_str() {
        "excellent" => HealthClass::Excellent,
        "healthy" => HealthClass::Healthy,
        "attention" => HealthClass::Attention,
        "risky" => HealthClass::Risky,
        _ => HealthClass::Critical,
    };
    Ok(HealthSnapshot {
        id: row.get(0)?,
        repo_id: row.get(1)?,
        ts: row.get(2)?,
        score: row.get(3)?,
        class,
        details_json: row.get(5)?,
    })
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn db() -> Connection {
        crate::open_memory().unwrap()
    }

    fn add_repo(conn: &Connection, id: &str, owner: &str, name: &str) {
        let now = now_secs();
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at) VALUES (?1, 'github.com', ?2, ?3, 'https://example/x.git', '/tmp/x', ?4, ?5)",
            params![id, owner, name, now, now],
        ).unwrap();
    }

    #[test]
    fn health_class_from_score() {
        assert_eq!(HealthClass::from_score(95), HealthClass::Excellent);
        assert_eq!(HealthClass::from_score(80), HealthClass::Healthy);
        assert_eq!(HealthClass::from_score(60), HealthClass::Attention);
        assert_eq!(HealthClass::from_score(30), HealthClass::Risky);
        assert_eq!(HealthClass::from_score(10), HealthClass::Critical);
    }

    #[test]
    fn score_repo_health_default_is_100() {
        let conn = db();
        add_repo(&conn, "r1", "alice", "proj1");
        let snap = score_repo_health(&conn, "r1").unwrap();
        assert_eq!(snap.score, 100);
        assert_eq!(snap.class, HealthClass::Excellent);
    }

    #[test]
    fn score_repo_health_archived_penalty() {
        let conn = db();
        add_repo(&conn, "r1", "alice", "proj1");
        conn.execute("UPDATE repos SET archived = 1 WHERE id = 'r1'", [])
            .unwrap();
        let snap = score_repo_health(&conn, "r1").unwrap();
        assert!(snap.score < 100);
    }

    #[test]
    fn score_repo_health_disabled_penalty() {
        let conn = db();
        add_repo(&conn, "r1", "alice", "proj1");
        conn.execute("UPDATE repos SET disabled = 1 WHERE id = 'r1'", [])
            .unwrap();
        let snap = score_repo_health(&conn, "r1").unwrap();
        assert!(snap.score < 80);
    }

    #[test]
    fn latest_health_returns_most_recent() {
        let conn = db();
        add_repo(&conn, "r1", "alice", "proj1");
        let s1 = score_repo_health(&conn, "r1").unwrap();
        let s2 = score_repo_health(&conn, "r1").unwrap();
        let latest = latest_health(&conn, "r1").unwrap().unwrap();
        assert_eq!(latest.id, s2.id);
        assert_ne!(latest.id, s1.id);
    }

    #[test]
    fn latest_health_missing_repo() {
        let conn = db();
        assert!(latest_health(&conn, "nonexistent").unwrap().is_none());
    }

    #[test]
    fn score_all_health_works() {
        let conn = db();
        add_repo(&conn, "r1", "alice", "proj1");
        add_repo(&conn, "r2", "bob", "proj2");
        let snaps = score_all_health(&conn).unwrap();
        assert_eq!(snaps.len(), 2);
    }
}
