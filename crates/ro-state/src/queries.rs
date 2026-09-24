//! Typed queries for ro state.
//!
//! Health scoring, context queries.

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
