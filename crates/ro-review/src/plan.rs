//! Plan creation for risky operations.
//!
//! Every mutating operation creates a plan before executing. Plans are stored
//! in the `plans` table and go through risk classification + confirmation.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// Status of a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanStatus {
    Pending,
    Approved,
    Applied,
    Rejected,
    RolledBack,
}

impl std::fmt::Display for PlanStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| format!("{self:?}"));
        f.write_str(&s)
    }
}

/// Risk classification for a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanRisk {
    Low,
    Medium,
    High,
}

impl std::fmt::Display for PlanRisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanRisk::Low => write!(f, "LOW"),
            PlanRisk::Medium => write!(f, "MEDIUM"),
            PlanRisk::High => write!(f, "HIGH"),
        }
    }
}

/// A plan as stored in the `plans` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub kind: String,
    pub repo_id: Option<String>,
    pub status: PlanStatus,
    pub created_at: i64,
    pub applied_at: Option<i64>,
    pub risk_class: Option<PlanRisk>,
    pub risk_reasons_json: Option<String>,
    pub plan_json: String,
    pub rollback_json: Option<String>,
}

/// Input for creating a new plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanInput {
    pub kind: String,
    pub repo_id: Option<String>,
    pub plan_json: String,
    pub risk_class: Option<PlanRisk>,
    pub risk_reasons_json: Option<String>,
    pub rollback_json: Option<String>,
}

/// Create a new plan in the database. Returns the plan with generated id and timestamp.
pub fn create_plan(conn: &Connection, input: &PlanInput) -> Result<Plan> {
    let id = Uuid::new_v4().to_string();
    let now = now_secs();

    conn.execute(
        "INSERT INTO plans (id, kind, repo_id, status, created_at, applied_at,
                            risk_class, risk_reasons_json, plan_json, rollback_json)
         VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, ?8, ?9)",
        params![
            id,
            input.kind,
            input.repo_id,
            "pending",
            now,
            input.risk_class.map(|r| r.to_string()),
            input.risk_reasons_json,
            input.plan_json,
            input.rollback_json,
        ],
    )
    .context("inserting plan")?;

    Ok(Plan {
        id,
        kind: input.kind.clone(),
        repo_id: input.repo_id.clone(),
        status: PlanStatus::Pending,
        created_at: now,
        applied_at: None,
        risk_class: input.risk_class,
        risk_reasons_json: input.risk_reasons_json.clone(),
        plan_json: input.plan_json.clone(),
        rollback_json: input.rollback_json.clone(),
    })
}

/// Get a plan by id.
pub fn get_plan(conn: &Connection, plan_id: &str) -> Result<Option<Plan>> {
    Ok(conn
        .query_row(
            "SELECT id, kind, repo_id, status, created_at, applied_at,
                    risk_class, risk_reasons_json, plan_json, rollback_json
             FROM plans WHERE id = ?1",
            params![plan_id],
            row_to_plan,
        )
        .ok())
}

/// List plans, optionally filtered by status.
pub fn list_plans(conn: &Connection, status_filter: Option<PlanStatus>) -> Result<Vec<Plan>> {
    let mut plans = Vec::new();
    match status_filter {
        Some(status) => {
            let status_str = serde_json::to_value(status)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default();
            let mut stmt = conn.prepare(
                "SELECT id, kind, repo_id, status, created_at, applied_at,
                        risk_class, risk_reasons_json, plan_json, rollback_json
                 FROM plans WHERE status = ?1 ORDER BY created_at DESC",
            )?;
            let mut rows = stmt.query(params![status_str])?;
            while let Some(row) = rows.next()? {
                plans.push(row_to_plan(row)?);
            }
        }
        None => {
            let mut stmt = conn.prepare(
                "SELECT id, kind, repo_id, status, created_at, applied_at,
                        risk_class, risk_reasons_json, plan_json, rollback_json
                 FROM plans ORDER BY created_at DESC",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                plans.push(row_to_plan(row)?);
            }
        }
    }
    Ok(plans)
}

/// Approve a plan (set status to approved).
pub fn approve_plan(conn: &Connection, plan_id: &str) -> Result<()> {
    let n = conn
        .execute(
            "UPDATE plans SET status = 'approved' WHERE id = ?1 AND status = 'pending'",
            params![plan_id],
        )
        .context("approving plan")?;
    if n == 0 {
        anyhow::bail!("plan {plan_id} not found or not pending");
    }
    Ok(())
}

/// Reject a plan.
pub fn reject_plan(conn: &Connection, plan_id: &str) -> Result<()> {
    let n = conn
        .execute(
            "UPDATE plans SET status = 'rejected' WHERE id = ?1 AND status IN ('pending', 'approved')",
            params![plan_id],
        )
        .context("rejecting plan")?;
    if n == 0 {
        anyhow::bail!("plan {plan_id} not found or not in rejectable state");
    }
    Ok(())
}

fn row_to_plan(row: &rusqlite::Row<'_>) -> std::result::Result<Plan, rusqlite::Error> {
    let status_str: String = row.get(3)?;
    let status = match status_str.as_str() {
        "pending" => PlanStatus::Pending,
        "approved" => PlanStatus::Approved,
        "applied" => PlanStatus::Applied,
        "rejected" => PlanStatus::Rejected,
        "rolled_back" => PlanStatus::RolledBack,
        _ => PlanStatus::Pending,
    };

    let risk_str: Option<String> = row.get(6)?;
    let risk_class = risk_str.and_then(|s| match s.as_str() {
        "LOW" => Some(PlanRisk::Low),
        "MEDIUM" => Some(PlanRisk::Medium),
        "HIGH" => Some(PlanRisk::High),
        _ => None,
    });

    Ok(Plan {
        id: row.get(0)?,
        kind: row.get(1)?,
        repo_id: row.get(2)?,
        status,
        created_at: row.get(4)?,
        applied_at: row.get(5)?,
        risk_class,
        risk_reasons_json: row.get(7)?,
        plan_json: row.get(8)?,
        rollback_json: row.get(9)?,
    })
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

    fn setup() -> Connection {
        ro_state::open_memory().unwrap()
    }

    #[test]
    fn create_and_get_plan() {
        let conn = setup();
        let input = PlanInput {
            kind: "sweep_commit".into(),
            repo_id: None,
            plan_json: r#"{"files":["src/main.rs"]}"#.into(),
            risk_class: Some(PlanRisk::Low),
            risk_reasons_json: None,
            rollback_json: Some(r#"{"strategy":"git_reset"}"#.into()),
        };
        let plan = create_plan(&conn, &input).unwrap();
        assert_eq!(plan.kind, "sweep_commit");
        assert_eq!(plan.status, PlanStatus::Pending);
        assert!(plan.risk_class.is_some());

        let fetched = get_plan(&conn, &plan.id).unwrap().unwrap();
        assert_eq!(fetched.id, plan.id);
    }

    #[test]
    fn get_missing_plan_returns_none() {
        let conn = setup();
        assert!(get_plan(&conn, "nonexistent").unwrap().is_none());
    }

    #[test]
    fn approve_plan_works() {
        let conn = setup();
        let input = PlanInput {
            kind: "sync".into(),
            repo_id: None,
            plan_json: "{}".into(),
            risk_class: None,
            risk_reasons_json: None,
            rollback_json: None,
        };
        let plan = create_plan(&conn, &input).unwrap();
        approve_plan(&conn, &plan.id).unwrap();

        let fetched = get_plan(&conn, &plan.id).unwrap().unwrap();
        assert_eq!(fetched.status, PlanStatus::Approved);
    }

    #[test]
    fn reject_plan_works() {
        let conn = setup();
        let input = PlanInput {
            kind: "test".into(),
            repo_id: None,
            plan_json: "{}".into(),
            risk_class: None,
            risk_reasons_json: None,
            rollback_json: None,
        };
        let plan = create_plan(&conn, &input).unwrap();
        reject_plan(&conn, &plan.id).unwrap();

        let fetched = get_plan(&conn, &plan.id).unwrap().unwrap();
        assert_eq!(fetched.status, PlanStatus::Rejected);
    }

    #[test]
    fn approve_non_pending_fails() {
        let conn = setup();
        let input = PlanInput {
            kind: "test".into(),
            repo_id: None,
            plan_json: "{}".into(),
            risk_class: None,
            risk_reasons_json: None,
            rollback_json: None,
        };
        let plan = create_plan(&conn, &input).unwrap();
        reject_plan(&conn, &plan.id).unwrap();
        // Can't approve a rejected plan
        let err = approve_plan(&conn, &plan.id).unwrap_err();
        assert!(err.to_string().contains("not found or not pending"));
    }

    #[test]
    fn list_plans_with_filter() {
        let conn = setup();
        for i in 0..3 {
            let input = PlanInput {
                kind: format!("kind-{i}"),
                repo_id: None,
                plan_json: "{}".into(),
                risk_class: None,
                risk_reasons_json: None,
                rollback_json: None,
            };
            create_plan(&conn, &input).unwrap();
        }
        let pending = list_plans(&conn, Some(PlanStatus::Pending)).unwrap();
        assert_eq!(pending.len(), 3);

        approve_plan(&conn, &pending[0].id).unwrap();
        let approved = list_plans(&conn, Some(PlanStatus::Approved)).unwrap();
        assert_eq!(approved.len(), 1);
    }

    #[test]
    fn list_all_plans() {
        let conn = setup();
        for i in 0..5 {
            let input = PlanInput {
                kind: format!("kind-{i}"),
                repo_id: None,
                plan_json: "{}".into(),
                risk_class: Some(if i < 2 { PlanRisk::High } else { PlanRisk::Low }),
                risk_reasons_json: None,
                rollback_json: None,
            };
            create_plan(&conn, &input).unwrap();
        }
        let all = list_plans(&conn, None).unwrap();
        assert_eq!(all.len(), 5);
    }
}
