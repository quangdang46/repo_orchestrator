//! Apply execution for approved plans.
//!
//! Only approved plans can be applied. Applying marks the plan as applied
//! and records the timestamp. The actual mutation is done by the caller;
//! this module handles the state transitions and audit.

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};

use super::plan::{PlanStatus, get_plan};

/// Result of applying a plan.
#[derive(Debug, Clone)]
pub struct ApplyResult {
    pub plan_id: String,
    pub status: PlanStatus,
    pub applied_at: i64,
}

/// Apply an approved plan. The plan must be in `approved` status.
/// Returns the apply result with the new timestamp.
pub fn apply_plan(conn: &Connection, plan_id: &str) -> Result<ApplyResult> {
    let plan = get_plan(conn, plan_id)?.context(format!("plan {plan_id} not found"))?;

    if plan.status != PlanStatus::Approved {
        bail!("plan {plan_id} is {} (expected approved)", plan.status);
    }

    let now = now_secs();
    let n = conn
        .execute(
            "UPDATE plans SET status = 'applied', applied_at = ?1 WHERE id = ?2",
            params![now, plan_id],
        )
        .context("applying plan")?;
    if n == 0 {
        bail!("plan {plan_id} update failed");
    }

    Ok(ApplyResult {
        plan_id: plan_id.to_string(),
        status: PlanStatus::Applied,
        applied_at: now,
    })
}

/// Mark a plan as rolled back. Must be in `applied` status.
pub fn rollback_plan(conn: &Connection, plan_id: &str) -> Result<()> {
    let plan = get_plan(conn, plan_id)?.context(format!("plan {plan_id} not found"))?;

    if plan.status != PlanStatus::Applied {
        bail!(
            "plan {plan_id} is {} (expected applied for rollback)",
            plan.status
        );
    }

    conn.execute(
        "UPDATE plans SET status = 'rolled_back' WHERE id = ?1",
        params![plan_id],
    )
    .context("rolling back plan")?;

    Ok(())
}

/// Check if a plan requires explicit confirmation based on risk class.
/// LOW risk: auto-approved. MEDIUM: requires confirmation. HIGH: requires --yes flag.
pub fn requires_confirmation(risk: Option<&str>) -> bool {
    match risk {
        Some("LOW") => false,
        Some("MEDIUM") | Some("HIGH") | None | Some(_) => true,
    }
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
    use crate::plan::{PlanInput, PlanStatus, create_plan, get_plan};

    fn setup() -> Connection {
        ro_state::open_memory().unwrap()
    }

    fn create_approved_plan(conn: &Connection) -> String {
        let input = PlanInput {
            kind: "test".into(),
            repo_id: None,
            plan_json: "{}".into(),
            risk_class: None,
            risk_reasons_json: None,
            rollback_json: None,
        };
        let plan = create_plan(conn, &input).unwrap();
        crate::plan::approve_plan(conn, &plan.id).unwrap();
        plan.id
    }

    #[test]
    fn apply_approved_plan() {
        let conn = setup();
        let plan_id = create_approved_plan(&conn);

        let result = apply_plan(&conn, &plan_id).unwrap();
        assert_eq!(result.status, PlanStatus::Applied);
        assert!(result.applied_at > 0);

        let plan = get_plan(&conn, &plan_id).unwrap().unwrap();
        assert_eq!(plan.status, PlanStatus::Applied);
        assert!(plan.applied_at.is_some());
    }

    #[test]
    fn apply_pending_plan_fails() {
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
        let err = apply_plan(&conn, &plan.id).unwrap_err();
        assert!(err.to_string().contains("expected approved"));
    }

    #[test]
    fn rollback_applied_plan() {
        let conn = setup();
        let plan_id = create_approved_plan(&conn);
        apply_plan(&conn, &plan_id).unwrap();

        rollback_plan(&conn, &plan_id).unwrap();
        let plan = get_plan(&conn, &plan_id).unwrap().unwrap();
        assert_eq!(plan.status, PlanStatus::RolledBack);
    }

    #[test]
    fn rollback_non_applied_fails() {
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
        let err = rollback_plan(&conn, &plan.id).unwrap_err();
        assert!(err.to_string().contains("expected applied"));
    }

    #[test]
    fn requires_confirmation_logic() {
        assert!(!requires_confirmation(Some("LOW")));
        assert!(requires_confirmation(Some("MEDIUM")));
        assert!(requires_confirmation(Some("HIGH")));
        assert!(requires_confirmation(None));
    }

    #[test]
    fn apply_nonexistent_plan_fails() {
        let conn = setup();
        let err = apply_plan(&conn, "nonexistent").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }
}
