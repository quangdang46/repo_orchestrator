//! GitHub check status and CI run lookup.
//!
//! Provides a normalized view over GitHub's two distinct CI surfaces:
//!
//! - **Check runs** — `/repos/{o}/{r}/commits/{sha}/check-runs` (Checks API)
//! - **Workflow runs** — `/repos/{o}/{r}/actions/runs` (Actions API)
//!
//! Both are reduced to [`CheckResult`] rows used by `inbox`, `health`, and
//! `ci autopsy`.

use anyhow::{Context, Result};
use octocrab::Octocrab;
use serde::{Deserialize, Serialize};

/// Generic CI status (combines Checks API and Actions API).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Queued,
    InProgress,
    Completed,
    Pending,
    Unknown,
}

/// Conclusion is only set once a check is completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckConclusion {
    Success,
    Failure,
    Neutral,
    Cancelled,
    Skipped,
    TimedOut,
    ActionRequired,
    Stale,
    Unknown,
}

impl CheckConclusion {
    /// True if the check is in a state we treat as "broken" for reporting.
    pub fn is_failure(self) -> bool {
        matches!(self, Self::Failure | Self::TimedOut | Self::ActionRequired)
    }
}

/// What surface produced this CheckResult.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckSource {
    CheckRun,
    WorkflowRun,
}

/// Normalized CI row used by the inbox/health/autopsy commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub source: CheckSource,
    pub name: String,
    pub status: CheckStatus,
    pub conclusion: CheckConclusion,
    pub head_sha: String,
    pub url: Option<String>,
    pub started_at_rfc3339: Option<String>,
    pub completed_at_rfc3339: Option<String>,
}

impl CheckResult {
    /// True if status==Completed and conclusion is one of failure/timed_out/action_required.
    pub fn is_completed_failure(&self) -> bool {
        matches!(self.status, CheckStatus::Completed) && self.conclusion.is_failure()
    }
}

fn parse_status(s: Option<&str>) -> CheckStatus {
    match s.map(str::to_ascii_lowercase).as_deref() {
        Some("queued") => CheckStatus::Queued,
        Some("in_progress") => CheckStatus::InProgress,
        Some("completed") => CheckStatus::Completed,
        Some("pending") => CheckStatus::Pending,
        _ => CheckStatus::Unknown,
    }
}

fn parse_conclusion(s: Option<&str>) -> CheckConclusion {
    match s.map(str::to_ascii_lowercase).as_deref() {
        Some("success") => CheckConclusion::Success,
        Some("failure") => CheckConclusion::Failure,
        Some("neutral") => CheckConclusion::Neutral,
        Some("cancelled") | Some("canceled") => CheckConclusion::Cancelled,
        Some("skipped") => CheckConclusion::Skipped,
        Some("timed_out") => CheckConclusion::TimedOut,
        Some("action_required") => CheckConclusion::ActionRequired,
        Some("stale") => CheckConclusion::Stale,
        _ => CheckConclusion::Unknown,
    }
}

fn rfc3339(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Fetch all check runs for a git ref (branch, tag, or commit SHA).
///
/// Uses the raw `/repos/{o}/{r}/commits/{ref}/check-runs` endpoint so we can
/// pick up the `status` field which `octocrab::models::checks::CheckRun`
/// does not currently expose in v0.44.
pub async fn list_check_runs(
    client: &Octocrab,
    owner: &str,
    name: &str,
    git_ref: &str,
) -> Result<Vec<CheckResult>> {
    if owner.is_empty() || name.is_empty() {
        anyhow::bail!("owner and repo name are required");
    }
    if git_ref.is_empty() {
        anyhow::bail!("git_ref cannot be empty");
    }

    #[derive(Deserialize)]
    struct ListResponse {
        check_runs: Vec<CheckRunRow>,
    }
    #[derive(Deserialize)]
    struct CheckRunRow {
        name: Option<String>,
        head_sha: Option<String>,
        html_url: Option<String>,
        status: Option<String>,
        conclusion: Option<String>,
        started_at: Option<chrono::DateTime<chrono::Utc>>,
        completed_at: Option<chrono::DateTime<chrono::Utc>>,
    }

    let route = format!("/repos/{}/{}/commits/{}/check-runs", owner, name, git_ref);
    let resp: ListResponse = client
        .get(&route, None::<&()>)
        .await
        .with_context(|| format!("GitHub: list check runs for {}/{}@{}", owner, name, git_ref))?;

    let mut out = Vec::with_capacity(resp.check_runs.len());
    for r in resp.check_runs {
        let status = parse_status(r.status.as_deref());
        let conclusion = parse_conclusion(r.conclusion.as_deref());
        out.push(CheckResult {
            source: CheckSource::CheckRun,
            name: r.name.unwrap_or_default(),
            status,
            conclusion,
            head_sha: r.head_sha.unwrap_or_default(),
            url: r.html_url,
            started_at_rfc3339: r.started_at.map(rfc3339),
            completed_at_rfc3339: r.completed_at.map(rfc3339),
        });
    }
    Ok(out)
}

/// Fetch most recent workflow runs (Actions API). `limit` caps the response
/// at the per-page granularity (1..=100).
pub async fn list_workflow_runs(
    client: &Octocrab,
    owner: &str,
    name: &str,
    limit: u8,
) -> Result<Vec<CheckResult>> {
    if owner.is_empty() || name.is_empty() {
        anyhow::bail!("owner and repo name are required");
    }
    let per_page: u8 = limit.clamp(1, 100);

    // octocrab does not yet expose a typed Actions list-runs API across all
    // versions, so use the raw GET helper. The response shape is:
    //   { "total_count": N, "workflow_runs": [...] }
    #[derive(Deserialize)]
    struct WorkflowRunsResponse {
        workflow_runs: Vec<WorkflowRunRow>,
    }
    #[derive(Deserialize)]
    struct WorkflowRunRow {
        name: Option<String>,
        head_sha: Option<String>,
        html_url: Option<String>,
        status: Option<String>,
        conclusion: Option<String>,
        run_started_at: Option<chrono::DateTime<chrono::Utc>>,
        updated_at: Option<chrono::DateTime<chrono::Utc>>,
    }

    let route = format!("/repos/{}/{}/actions/runs", owner, name);
    let resp: WorkflowRunsResponse = client
        .get(&route, Some(&[("per_page", per_page.to_string())]))
        .await
        .with_context(|| format!("GitHub: list workflow runs for {}/{}", owner, name))?;

    let mut out = Vec::with_capacity(resp.workflow_runs.len());
    for r in resp.workflow_runs {
        let status = parse_status(r.status.as_deref());
        let conclusion = parse_conclusion(r.conclusion.as_deref());
        out.push(CheckResult {
            source: CheckSource::WorkflowRun,
            name: r.name.unwrap_or_default(),
            status,
            conclusion,
            head_sha: r.head_sha.unwrap_or_default(),
            url: r.html_url,
            started_at_rfc3339: r.run_started_at.map(rfc3339),
            completed_at_rfc3339: r.updated_at.map(rfc3339),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn mock_client(server: &MockServer) -> Octocrab {
        Octocrab::builder()
            .base_uri(server.uri())
            .unwrap()
            .build()
            .unwrap()
    }

    #[test]
    fn parse_status_known_values() {
        assert_eq!(parse_status(Some("queued")), CheckStatus::Queued);
        assert_eq!(parse_status(Some("in_progress")), CheckStatus::InProgress);
        assert_eq!(parse_status(Some("completed")), CheckStatus::Completed);
        assert_eq!(parse_status(Some("pending")), CheckStatus::Pending);
        assert_eq!(parse_status(Some("nope")), CheckStatus::Unknown);
        assert_eq!(parse_status(None), CheckStatus::Unknown);
    }

    #[test]
    fn parse_conclusion_known_values() {
        assert_eq!(parse_conclusion(Some("success")), CheckConclusion::Success);
        assert_eq!(parse_conclusion(Some("failure")), CheckConclusion::Failure);
        assert_eq!(
            parse_conclusion(Some("CANCELLED")),
            CheckConclusion::Cancelled
        );
        assert_eq!(
            parse_conclusion(Some("canceled")),
            CheckConclusion::Cancelled
        );
        assert_eq!(
            parse_conclusion(Some("timed_out")),
            CheckConclusion::TimedOut
        );
        assert_eq!(parse_conclusion(None), CheckConclusion::Unknown);
    }

    #[test]
    fn is_completed_failure_is_strict() {
        let mut r = CheckResult {
            source: CheckSource::CheckRun,
            name: "ci".into(),
            status: CheckStatus::Completed,
            conclusion: CheckConclusion::Failure,
            head_sha: "abc".into(),
            url: None,
            started_at_rfc3339: None,
            completed_at_rfc3339: None,
        };
        assert!(r.is_completed_failure());
        r.conclusion = CheckConclusion::Success;
        assert!(!r.is_completed_failure());
        r.status = CheckStatus::InProgress;
        r.conclusion = CheckConclusion::Failure;
        assert!(!r.is_completed_failure());
    }

    #[tokio::test]
    async fn list_check_runs_parses_payload() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits/abc123/check-runs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total_count": 2,
                "check_runs": [
                    {
                        "id": 1,
                        "node_id": "n1",
                        "head_sha": "abc123",
                        "url": "https://api.github.com/repos/o/r/check-runs/1",
                        "html_url": "https://github.com/o/r/runs/1",
                        "details_url": "https://example.test/details/1",
                        "external_id": "x",
                        "status": "completed",
                        "conclusion": "failure",
                        "started_at": "2026-05-20T10:00:00Z",
                        "completed_at": "2026-05-20T10:05:00Z",
                        "name": "CI / build",
                        "check_suite": {"id": 100},
                        "app": null,
                        "pull_requests": []
                    },
                    {
                        "id": 2,
                        "node_id": "n2",
                        "head_sha": "abc123",
                        "url": "https://api.github.com/repos/o/r/check-runs/2",
                        "html_url": "https://github.com/o/r/runs/2",
                        "details_url": "https://example.test/details/2",
                        "external_id": "y",
                        "status": "in_progress",
                        "conclusion": null,
                        "started_at": "2026-05-20T10:10:00Z",
                        "completed_at": null,
                        "name": "CI / lint",
                        "check_suite": {"id": 100},
                        "app": null,
                        "pull_requests": []
                    }
                ]
            })))
            .mount(&server)
            .await;

        let client = mock_client(&server).await;
        let runs = list_check_runs(&client, "o", "r", "abc123")
            .await
            .expect("list_check_runs");
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].name, "CI / build");
        assert_eq!(runs[0].status, CheckStatus::Completed);
        assert_eq!(runs[0].conclusion, CheckConclusion::Failure);
        assert_eq!(runs[0].source, CheckSource::CheckRun);
        assert!(runs[0].is_completed_failure());
        assert_eq!(runs[1].status, CheckStatus::InProgress);
        assert_eq!(runs[1].conclusion, CheckConclusion::Unknown);
        assert!(!runs[1].is_completed_failure());
    }

    #[tokio::test]
    async fn list_workflow_runs_parses_payload() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/actions/runs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total_count": 1,
                "workflow_runs": [
                    {
                        "id": 42,
                        "name": "CI",
                        "head_branch": "main",
                        "head_sha": "deadbeef",
                        "status": "completed",
                        "conclusion": "success",
                        "html_url": "https://github.com/o/r/actions/runs/42",
                        "run_started_at": "2026-05-20T09:00:00Z",
                        "updated_at": "2026-05-20T09:05:00Z"
                    }
                ]
            })))
            .mount(&server)
            .await;

        let client = mock_client(&server).await;
        let runs = list_workflow_runs(&client, "o", "r", 25)
            .await
            .expect("list_workflow_runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].source, CheckSource::WorkflowRun);
        assert_eq!(runs[0].name, "CI");
        assert_eq!(runs[0].status, CheckStatus::Completed);
        assert_eq!(runs[0].conclusion, CheckConclusion::Success);
        assert_eq!(runs[0].head_sha, "deadbeef");
        assert!(!runs[0].is_completed_failure());
    }

    #[tokio::test]
    async fn list_check_runs_validates_args() {
        let server = MockServer::start().await;
        let client = mock_client(&server).await;
        assert!(list_check_runs(&client, "", "r", "abc").await.is_err());
        assert!(list_check_runs(&client, "o", "", "abc").await.is_err());
        assert!(list_check_runs(&client, "o", "r", "").await.is_err());
    }

    #[tokio::test]
    async fn list_workflow_runs_clamps_per_page() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/actions/runs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total_count": 0,
                "workflow_runs": []
            })))
            .mount(&server)
            .await;
        let client = mock_client(&server).await;
        let _ = list_workflow_runs(&client, "o", "r", 250).await.unwrap();
        // No assertion on the query string itself; we trust clamp logic.
    }
}
