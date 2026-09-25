//! Policy checking for tracked repositories.
//!
//! Offline-first checks against DB fields: archived, disabled,
//! branch presence, and visibility mismatch.

use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use ro_config::policy::Policy;
use ro_sync::manage::{TrackedRepo, list};

/// Severity of a policy violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    Info,
    Warn,
    Error,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Severity::Info => write!(f, "Info"),
            Severity::Warn => write!(f, "Warn"),
            Severity::Error => write!(f, "Error"),
        }
    }
}

/// A single policy violation for a tracked repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyViolation {
    pub repo_id: String,
    pub repo_name: String,
    pub rule: String,
    pub expected: String,
    pub actual: String,
    pub severity: Severity,
    pub suggested_fix: String,
}

/// Check a single tracked repo against a policy.
///
/// This is offline-first: it uses only the DB fields in `TrackedRepo`
/// (archived, disabled, branch, visibility).
///
/// Checks performed:
/// 1. **archived** -- if `archived == true`, flag with severity `Warn`.
/// 2. **disabled** -- if `disabled == true`, flag with severity `Error`.
/// 3. **branch** -- if a `branch` is configured but `default_branch` is not
///    `None` and the configured branch differs from `default_branch`, flag
///    with severity `Warn`.
/// 4. **visibility** -- if the policy requires `public` and the repo is not
///    `public`, flag with severity `Error`.
pub fn check_repo_policy(repo: &TrackedRepo, policy: &Policy) -> Vec<PolicyViolation> {
    let mut violations = Vec::new();

    // 1. Archived
    if repo.archived {
        violations.push(PolicyViolation {
            repo_id: repo.id.clone(),
            repo_name: format!("{}/{}", repo.owner, repo.name),
            rule: "archived".to_string(),
            expected: "not archived".to_string(),
            actual: "archived".to_string(),
            severity: Severity::Warn,
            suggested_fix: format!("Review {}/{} -- repo is archived", repo.owner, repo.name),
        });
    }

    // 2. Disabled
    if repo.disabled {
        violations.push(PolicyViolation {
            repo_id: repo.id.clone(),
            repo_name: format!("{}/{}", repo.owner, repo.name),
            rule: "disabled".to_string(),
            expected: "not disabled".to_string(),
            actual: "disabled".to_string(),
            severity: Severity::Error,
            suggested_fix: format!(
                "Investigate {}/{} -- repo is marked disabled",
                repo.owner, repo.name
            ),
        });
    }

    // 3. Branch presence / mismatch
    if let Some(ref branch) = repo.branch {
        if let Some(ref default) = repo.default_branch {
            if branch != default {
                violations.push(PolicyViolation {
                    repo_id: repo.id.clone(),
                    repo_name: format!("{}/{}", repo.owner, repo.name),
                    rule: "branch".to_string(),
                    expected: format!("default branch: {default}"),
                    actual: format!("configured branch: {branch}"),
                    severity: Severity::Warn,
                    suggested_fix: format!(
                        "{}/{}: branch '{branch}' does not match default '{default}'",
                        repo.owner, repo.name
                    ),
                });
            }
        } else {
            // branch configured but no default_branch known in DB
            violations.push(PolicyViolation {
                repo_id: repo.id.clone(),
                repo_name: format!("{}/{}", repo.owner, repo.name),
                rule: "branch".to_string(),
                expected: "known default branch".to_string(),
                actual: format!("configured branch: {branch}"),
                severity: Severity::Info,
                suggested_fix: format!(
                    "{}/{}: run sync to populate default_branch",
                    repo.owner, repo.name
                ),
            });
        }
    }

    // 4. Visibility mismatch (policy.extra["visibility"])
    if let Some(vis_req) = policy.extra.get("visibility") {
        if let Some(req_str) = vis_req.as_str() {
            let actual = repo.visibility.to_ascii_lowercase();
            let req = req_str.to_ascii_lowercase();
            if actual != req {
                let sev = if req == "public" {
                    Severity::Error
                } else {
                    Severity::Warn
                };
                violations.push(PolicyViolation {
                    repo_id: repo.id.clone(),
                    repo_name: format!("{}/{}", repo.owner, repo.name),
                    rule: "visibility".to_string(),
                    expected: req,
                    actual: actual.clone(),
                    severity: sev,
                    suggested_fix: format!(
                        "Change {}/{} visibility from '{}' to '{}'",
                        repo.owner, repo.name, actual, req_str
                    ),
                });
            }
        }
    }

    violations
}

/// Check all tracked repos against the given policy.
///
/// Returns a flat list of every violation found across every repo.
pub fn check_all_policies(conn: &Connection, policy: &Policy) -> Result<Vec<PolicyViolation>> {
    let repos = list(conn, None)?;
    let mut all = Vec::new();
    for repo in &repos {
        let mut v = check_repo_policy(repo, policy);
        all.append(&mut v);
    }
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ro_config::policy::Policy;
    use ro_sync::manage::add;
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
    fn no_violations_for_fresh_repo() {
        let (tmp, conn) = setup();
        let repo = add(&conn, "alice/clean", &projects_dir(&tmp)).unwrap();
        let policy = Policy::default();
        let v = check_repo_policy(&repo, &policy);
        assert!(v.is_empty(), "fresh repo should have no violations: {v:?}");
    }

    #[test]
    fn archived_repo_warns() {
        let (tmp, conn) = setup();
        let repo = add(&conn, "alice/old", &projects_dir(&tmp)).unwrap();
        // Mutate archived flag
        conn.execute(
            "UPDATE repos SET archived = 1 WHERE id = ?1",
            rusqlite::params![repo.id],
        )
        .unwrap();
        let repo = ro_sync::manage::list(&conn, None)
            .unwrap()
            .into_iter()
            .find(|r| r.id == repo.id)
            .unwrap();
        let policy = Policy::default();
        let v = check_repo_policy(&repo, &policy);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].rule, "archived");
        assert_eq!(v[0].severity, Severity::Warn);
    }

    #[test]
    fn disabled_repo_errors() {
        let (tmp, conn) = setup();
        let repo = add(&conn, "alice/broken", &projects_dir(&tmp)).unwrap();
        conn.execute(
            "UPDATE repos SET disabled = 1 WHERE id = ?1",
            rusqlite::params![repo.id],
        )
        .unwrap();
        let repo = ro_sync::manage::list(&conn, None)
            .unwrap()
            .into_iter()
            .find(|r| r.id == repo.id)
            .unwrap();
        let policy = Policy::default();
        let v = check_repo_policy(&repo, &policy);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].rule, "disabled");
        assert_eq!(v[0].severity, Severity::Error);
    }

    #[test]
    fn branch_mismatch_warns() {
        let (tmp, conn) = setup();
        let repo = add(&conn, "alice/proj#main", &projects_dir(&tmp)).unwrap();
        // Simulate default_branch being different from configured branch
        conn.execute(
            "UPDATE repos SET default_branch = 'develop' WHERE id = ?1",
            rusqlite::params![repo.id],
        )
        .unwrap();
        let repo = ro_sync::manage::list(&conn, None)
            .unwrap()
            .into_iter()
            .find(|r| r.id == repo.id)
            .unwrap();
        let policy = Policy::default();
        let v = check_repo_policy(&repo, &policy);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].rule, "branch");
        assert_eq!(v[0].severity, Severity::Warn);
        assert!(v[0].actual.contains("main"));
        assert!(v[0].expected.contains("develop"));
    }

    #[test]
    fn visibility_mismatch_from_extra() {
        let (tmp, conn) = setup();
        let repo = add(&conn, "alice/secret", &projects_dir(&tmp)).unwrap();
        conn.execute(
            "UPDATE repos SET visibility = 'private' WHERE id = ?1",
            rusqlite::params![repo.id],
        )
        .unwrap();
        let repo = ro_sync::manage::list(&conn, None)
            .unwrap()
            .into_iter()
            .find(|r| r.id == repo.id)
            .unwrap();

        let mut policy = Policy::default();
        policy.extra.insert(
            "visibility".into(),
            serde_yaml::Value::String("public".into()),
        );

        let v = check_repo_policy(&repo, &policy);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].rule, "visibility");
        assert_eq!(v[0].severity, Severity::Error);
    }

    #[test]
    fn check_all_policies_runs() {
        let (tmp, conn) = setup();
        add(&conn, "alice/a", &projects_dir(&tmp)).unwrap();
        add(&conn, "bob/b", &projects_dir(&tmp)).unwrap();

        let policy = Policy::default();
        let v = check_all_policies(&conn, &policy).unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn check_all_policies_finds_mixed() {
        let (tmp, conn) = setup();
        let r1 = add(&conn, "alice/arch", &projects_dir(&tmp)).unwrap();
        let _r2 = add(&conn, "bob/ok", &projects_dir(&tmp)).unwrap();
        let r3 = add(&conn, "charlie/dis", &projects_dir(&tmp)).unwrap();

        conn.execute(
            "UPDATE repos SET archived = 1 WHERE id = ?1",
            rusqlite::params![r1.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE repos SET disabled = 1 WHERE id = ?1",
            rusqlite::params![r3.id],
        )
        .unwrap();

        let mut policy = Policy::default();
        policy.extra.insert(
            "visibility".into(),
            serde_yaml::Value::String("public".into()),
        );

        let v = check_all_policies(&conn, &policy).unwrap();
        let archived = v.iter().filter(|x| x.rule == "archived").count();
        let disabled = v.iter().filter(|x| x.rule == "disabled").count();
        assert_eq!(archived, 1);
        assert_eq!(disabled, 1);
    }
}
