//! GitHub issue and PR listing.
//!
//! Fetches open issues and open pull requests for a repository, normalizing
//! the relevant subset of fields used by `ro inbox` and `ro health`.

use anyhow::{Context, Result};
use octocrab::Octocrab;
use octocrab::params::State;
use serde::{Deserialize, Serialize};

/// What kind of GitHub item this row represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    Issue,
    Pull,
}

/// Subset of fields shared between issues and pull requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueOrPr {
    pub kind: ItemKind,
    pub number: u64,
    pub title: String,
    pub state: String,
    pub labels: Vec<String>,
    pub author: Option<String>,
    pub url: Option<String>,
    pub draft: Option<bool>,
    pub head_sha: Option<String>,
    pub head_ref: Option<String>,
    pub base_ref: Option<String>,
    pub created_at_rfc3339: Option<String>,
    pub updated_at_rfc3339: Option<String>,
}

/// Filter options for listing issues / PRs.
#[derive(Debug, Clone, Default)]
pub struct ListOpts {
    /// `Some(true)` to only fetch open items, `Some(false)` for closed,
    /// `None` for all states.
    pub only_open: Option<bool>,
    /// Optional per-page (capped at 100 by GitHub).
    pub per_page: Option<u8>,
}

impl ListOpts {
    fn state(&self) -> State {
        match self.only_open {
            Some(false) => State::Closed,
            Some(true) => State::Open,
            None => State::All,
        }
    }
}

fn rfc3339(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn issue_state_to_string(s: octocrab::models::IssueState) -> String {
    match s {
        octocrab::models::IssueState::Open => "open".to_string(),
        octocrab::models::IssueState::Closed => "closed".to_string(),
        _ => "unknown".to_string(),
    }
}

/// List open (or filtered) issues for `owner/name`.
///
/// Note: GitHub's REST `/issues` endpoint returns both issues *and* PRs;
/// this function filters out PRs by checking the `pull_request` field via
/// the underlying octocrab model.
pub async fn list_issues(
    client: &Octocrab,
    owner: &str,
    name: &str,
    opts: &ListOpts,
) -> Result<Vec<IssueOrPr>> {
    let per_page = opts.per_page.unwrap_or(50);
    let page = client
        .issues(owner, name)
        .list()
        .state(opts.state())
        .per_page(per_page)
        .send()
        .await
        .with_context(|| format!("GitHub: list issues {}/{}", owner, name))?;

    let mut out = Vec::with_capacity(page.items.len());
    for issue in page.items {
        if issue.pull_request.is_some() {
            // GitHub returns PRs through /issues; we only want pure issues here.
            continue;
        }
        out.push(IssueOrPr {
            kind: ItemKind::Issue,
            number: issue.number,
            title: issue.title,
            state: issue_state_to_string(issue.state),
            labels: issue.labels.into_iter().map(|l| l.name).collect(),
            author: Some(issue.user.login),
            url: Some(issue.html_url.to_string()),
            draft: None,
            head_sha: None,
            head_ref: None,
            base_ref: None,
            created_at_rfc3339: Some(rfc3339(issue.created_at)),
            updated_at_rfc3339: Some(rfc3339(issue.updated_at)),
        });
    }
    Ok(out)
}

/// List open (or filtered) pull requests for `owner/name`.
pub async fn list_pulls(
    client: &Octocrab,
    owner: &str,
    name: &str,
    opts: &ListOpts,
) -> Result<Vec<IssueOrPr>> {
    let per_page = opts.per_page.unwrap_or(50);
    let page = client
        .pulls(owner, name)
        .list()
        .state(opts.state())
        .per_page(per_page)
        .send()
        .await
        .with_context(|| format!("GitHub: list PRs {}/{}", owner, name))?;

    let mut out = Vec::with_capacity(page.items.len());
    for pr in page.items {
        out.push(IssueOrPr {
            kind: ItemKind::Pull,
            number: pr.number,
            title: pr.title.unwrap_or_default(),
            state: pr
                .state
                .map(|s| match s {
                    octocrab::models::IssueState::Open => "open".to_string(),
                    octocrab::models::IssueState::Closed => "closed".to_string(),
                    other => format!("{other:?}").to_lowercase(),
                })
                .unwrap_or_default(),
            labels: pr
                .labels
                .unwrap_or_default()
                .into_iter()
                .map(|l| l.name)
                .collect(),
            author: pr.user.map(|u| u.login),
            url: pr.html_url.as_ref().map(ToString::to_string),
            draft: pr.draft,
            head_sha: Some(pr.head.sha.clone()),
            head_ref: Some(pr.head.ref_field.clone()),
            base_ref: Some(pr.base.ref_field.clone()),
            created_at_rfc3339: pr.created_at.map(rfc3339),
            updated_at_rfc3339: pr.updated_at.map(rfc3339),
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

    fn user(login: &str) -> serde_json::Value {
        json!({
            "login": login,
            "id": 1,
            "node_id": "u",
            "avatar_url": "https://example.test/a.png",
            "gravatar_id": "",
            "url": format!("https://api.github.com/users/{}", login),
            "html_url": format!("https://github.com/{}", login),
            "followers_url": format!("https://api.github.com/users/{}/followers", login),
            "following_url": format!("https://api.github.com/users/{}/following", login),
            "gists_url": format!("https://api.github.com/users/{}/gists", login),
            "starred_url": format!("https://api.github.com/users/{}/starred", login),
            "subscriptions_url": format!("https://api.github.com/users/{}/subscriptions", login),
            "organizations_url": format!("https://api.github.com/users/{}/orgs", login),
            "repos_url": format!("https://api.github.com/users/{}/repos", login),
            "events_url": format!("https://api.github.com/users/{}/events", login),
            "received_events_url": format!("https://api.github.com/users/{}/received_events", login),
            "type": "User",
            "site_admin": false,
        })
    }

    #[tokio::test]
    async fn list_issues_filters_out_pulls() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {
                    "id": 1,
                    "node_id": "n1",
                    "url": "https://api.github.com/repos/o/r/issues/1",
                    "repository_url": "https://api.github.com/repos/o/r",
                    "labels_url": "https://api.github.com/repos/o/r/issues/1/labels{/name}",
                    "comments_url": "https://api.github.com/repos/o/r/issues/1/comments",
                    "events_url": "https://api.github.com/repos/o/r/issues/1/events",
                    "html_url": "https://github.com/o/r/issues/1",
                    "number": 1,
                    "state": "open",
                    "title": "real issue",
                    "body": "",
                    "user": user("alice"),
                    "labels": [
                        {"id": 1, "node_id": "l", "url": "https://api.github.com/repos/o/r/labels/bug", "name": "bug", "color": "f00", "default": false}
                    ],
                    "assignees": [],
                    "milestone": null,
                    "locked": false,
                    "comments": 0,
                    "author_association": "OWNER",
                    "created_at": "2026-05-20T10:00:00Z",
                    "updated_at": "2026-05-20T10:00:00Z"
                },
                {
                    "id": 2,
                    "node_id": "n2",
                    "url": "https://api.github.com/repos/o/r/issues/2",
                    "repository_url": "https://api.github.com/repos/o/r",
                    "labels_url": "https://api.github.com/repos/o/r/issues/2/labels{/name}",
                    "comments_url": "https://api.github.com/repos/o/r/issues/2/comments",
                    "events_url": "https://api.github.com/repos/o/r/issues/2/events",
                    "html_url": "https://github.com/o/r/pull/2",
                    "number": 2,
                    "state": "open",
                    "title": "a pr",
                    "body": "",
                    "user": user("bob"),
                    "labels": [],
                    "assignees": [],
                    "milestone": null,
                    "locked": false,
                    "comments": 0,
                    "author_association": "CONTRIBUTOR",
                    "created_at": "2026-05-20T10:00:00Z",
                    "updated_at": "2026-05-20T10:00:00Z",
                    "pull_request": {
                        "url": "https://api.github.com/repos/o/r/pulls/2",
                        "html_url": "https://github.com/o/r/pull/2",
                        "diff_url": "https://github.com/o/r/pull/2.diff",
                        "patch_url": "https://github.com/o/r/pull/2.patch"
                    }
                }
            ])))
            .mount(&server)
            .await;

        let client = mock_client(&server).await;
        let issues = list_issues(&client, "o", "r", &ListOpts::default())
            .await
            .expect("list_issues");
        assert_eq!(issues.len(), 1);
        let only = &issues[0];
        assert_eq!(only.kind, ItemKind::Issue);
        assert_eq!(only.number, 1);
        assert_eq!(only.title, "real issue");
        assert_eq!(only.state, "open");
        assert_eq!(only.labels, vec!["bug".to_string()]);
        assert_eq!(only.author.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn list_pulls_returns_typed_rows() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {
                    "id": 10,
                    "node_id": "p1",
                    "url": "https://api.github.com/repos/o/r/pulls/10",
                    "html_url": "https://github.com/o/r/pull/10",
                    "diff_url": "https://github.com/o/r/pull/10.diff",
                    "patch_url": "https://github.com/o/r/pull/10.patch",
                    "issue_url": "https://api.github.com/repos/o/r/issues/10",
                    "commits_url": "https://api.github.com/repos/o/r/pulls/10/commits",
                    "review_comments_url": "https://api.github.com/repos/o/r/pulls/10/comments",
                    "review_comment_url": "https://api.github.com/repos/o/r/pulls/comments{/number}",
                    "comments_url": "https://api.github.com/repos/o/r/issues/10/comments",
                    "statuses_url": "https://api.github.com/repos/o/r/statuses/abc",
                    "number": 10,
                    "state": "open",
                    "title": "fix bug",
                    "user": user("carol"),
                    "body": "",
                    "labels": [
                        {"id": 7, "node_id": "l", "url": "https://api.github.com/repos/o/r/labels/needs-review", "name": "needs-review", "color": "ff0", "default": false}
                    ],
                    "milestone": null,
                    "active_lock_reason": null,
                    "created_at": "2026-05-19T10:00:00Z",
                    "updated_at": "2026-05-19T11:00:00Z",
                    "closed_at": null,
                    "merged_at": null,
                    "merge_commit_sha": null,
                    "assignee": null,
                    "assignees": [],
                    "requested_reviewers": [],
                    "requested_teams": [],
                    "draft": false,
                    "head": {
                        "label": "carol:feature",
                        "ref": "feature",
                        "sha": "abc123",
                        "user": user("carol"),
                        "repo": null
                    },
                    "base": {
                        "label": "o:main",
                        "ref": "main",
                        "sha": "def456",
                        "user": user("o"),
                        "repo": null
                    },
                    "_links": {
                        "self": {"href": "https://api.github.com/repos/o/r/pulls/10"},
                        "html": {"href": "https://github.com/o/r/pull/10"},
                        "issue": {"href": "https://api.github.com/repos/o/r/issues/10"},
                        "comments": {"href": "https://api.github.com/repos/o/r/issues/10/comments"},
                        "review_comments": {"href": "https://api.github.com/repos/o/r/pulls/10/comments"},
                        "review_comment": {"href": "https://api.github.com/repos/o/r/pulls/comments{/number}"},
                        "commits": {"href": "https://api.github.com/repos/o/r/pulls/10/commits"},
                        "statuses": {"href": "https://api.github.com/repos/o/r/statuses/abc"}
                    },
                    "author_association": "CONTRIBUTOR",
                    "auto_merge": null
                }
            ])))
            .mount(&server)
            .await;

        let client = mock_client(&server).await;
        let pulls = list_pulls(&client, "o", "r", &ListOpts::default())
            .await
            .expect("list_pulls");
        assert_eq!(pulls.len(), 1);
        let p = &pulls[0];
        assert_eq!(p.kind, ItemKind::Pull);
        assert_eq!(p.number, 10);
        assert_eq!(p.title, "fix bug");
        assert_eq!(p.draft, Some(false));
        assert_eq!(p.head_sha.as_deref(), Some("abc123"));
        assert_eq!(p.head_ref.as_deref(), Some("feature"));
        assert_eq!(p.base_ref.as_deref(), Some("main"));
        assert_eq!(p.author.as_deref(), Some("carol"));
        assert_eq!(p.labels, vec!["needs-review".to_string()]);
    }

    #[test]
    fn list_opts_state_mapping() {
        let mut o = ListOpts::default();
        assert!(matches!(o.state(), State::All));
        o.only_open = Some(true);
        assert!(matches!(o.state(), State::Open));
        o.only_open = Some(false);
        assert!(matches!(o.state(), State::Closed));
    }

    #[tokio::test]
    async fn list_issues_propagates_http_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/issues"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = mock_client(&server).await;
        let err = list_issues(&client, "o", "r", &ListOpts::default())
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("o/r"));
    }
}
