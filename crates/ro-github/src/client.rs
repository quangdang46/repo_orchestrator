//! GitHub API repo lookup and metadata enrichment.
//!
//! Provides typed [`RepoMetadata`] returned by [`fetch_repo`].
//! Used by `ro add` to validate and enrich repo specs before persisting.
//!
//! Cache integration is left to the caller (see `ro-state` `context_cache`),
//! since this crate has no state dependency.

use anyhow::{Context, Result};
use octocrab::Octocrab;
use serde::{Deserialize, Serialize};

/// Visibility of a GitHub repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepoVisibility {
    Public,
    Private,
    Internal,
    Unknown,
}

impl RepoVisibility {
    fn from_str(s: Option<&str>) -> Self {
        match s.map(str::to_ascii_lowercase).as_deref() {
            Some("public") => Self::Public,
            Some("private") => Self::Private,
            Some("internal") => Self::Internal,
            _ => Self::Unknown,
        }
    }
}

/// Subset of GitHub repository metadata used by ro.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoMetadata {
    pub owner: String,
    pub name: String,
    pub host: String,
    pub default_branch: String,
    pub visibility: RepoVisibility,
    pub archived: bool,
    pub disabled: bool,
    pub fork: bool,
    pub html_url: Option<String>,
    pub clone_url: Option<String>,
    pub ssh_url: Option<String>,
    pub description: Option<String>,
    pub default_remote: Option<String>,
    pub pushed_at_rfc3339: Option<String>,
}

impl RepoMetadata {
    /// Owner/name pair (canonical short form).
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

/// Look up repository metadata.
///
/// `host` defaults to `github.com` when None; for GitHub Enterprise pass the
/// API host as resolved by `auth::build_client`.
pub async fn fetch_repo(
    client: &Octocrab,
    owner: &str,
    name: &str,
    host: Option<&str>,
) -> Result<RepoMetadata> {
    if owner.is_empty() {
        anyhow::bail!("owner cannot be empty");
    }
    if name.is_empty() {
        anyhow::bail!("repo name cannot be empty");
    }

    let repo = client
        .repos(owner, name)
        .get()
        .await
        .with_context(|| format!("GitHub: fetch repo {}/{}", owner, name))?;

    let default_branch = repo
        .default_branch
        .clone()
        .unwrap_or_else(|| "main".to_string());
    let archived = repo.archived.unwrap_or(false);
    let disabled = repo.disabled.unwrap_or(false);
    let fork = repo.fork.unwrap_or(false);
    let visibility = RepoVisibility::from_str(repo.visibility.as_deref());
    let html_url = repo.html_url.as_ref().map(ToString::to_string);
    let clone_url = repo.clone_url.as_ref().map(ToString::to_string);
    let ssh_url = repo.ssh_url.clone();
    let description = repo.description.clone();
    let pushed_at_rfc3339 = repo
        .pushed_at
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));

    Ok(RepoMetadata {
        owner: owner.to_string(),
        name: name.to_string(),
        host: host.unwrap_or("github.com").to_string(),
        default_branch,
        visibility,
        archived,
        disabled,
        fork,
        html_url,
        clone_url,
        ssh_url,
        description,
        default_remote: Some("origin".to_string()),
        pushed_at_rfc3339,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header_regex, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn mock_client(server: &MockServer) -> Octocrab {
        Octocrab::builder()
            .base_uri(server.uri())
            .unwrap()
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn fetch_repo_parses_metadata() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/quangdang46/repo_orchestrator"))
            .and(header_regex("user-agent", ".*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": 1,
                "node_id": "n",
                "name": "repo_orchestrator",
                "full_name": "quangdang46/repo_orchestrator",
                "owner": {
                    "login": "quangdang46",
                    "id": 1,
                    "node_id": "u",
                    "avatar_url": "https://example.test/a.png",
                    "gravatar_id": "",
                    "url": "https://api.github.com/users/quangdang46",
                    "html_url": "https://github.com/quangdang46",
                    "followers_url": "https://api.github.com/users/quangdang46/followers",
                    "following_url": "https://api.github.com/users/quangdang46/following",
                    "gists_url": "https://api.github.com/users/quangdang46/gists",
                    "starred_url": "https://api.github.com/users/quangdang46/starred",
                    "subscriptions_url": "https://api.github.com/users/quangdang46/subscriptions",
                    "organizations_url": "https://api.github.com/users/quangdang46/orgs",
                    "repos_url": "https://api.github.com/users/quangdang46/repos",
                    "events_url": "https://api.github.com/users/quangdang46/events",
                    "received_events_url": "https://api.github.com/users/quangdang46/received_events",
                    "type": "User",
                    "site_admin": false,
                },
                "private": false,
                "html_url": "https://github.com/quangdang46/repo_orchestrator",
                "fork": false,
                "url": "https://api.github.com/repos/quangdang46/repo_orchestrator",
                "default_branch": "main",
                "visibility": "public",
                "archived": false,
                "disabled": false,
                "clone_url": "https://github.com/quangdang46/repo_orchestrator.git",
                "ssh_url": "git@github.com:quangdang46/repo_orchestrator.git",
                "description": "Repo Forge orchestrator",
                "pushed_at": "2026-05-20T10:00:00Z"
            })))
            .mount(&server)
            .await;

        let client = mock_client(&server).await;
        let meta = fetch_repo(&client, "quangdang46", "repo_orchestrator", None)
            .await
            .expect("fetch_repo");

        assert_eq!(meta.owner, "quangdang46");
        assert_eq!(meta.name, "repo_orchestrator");
        assert_eq!(meta.host, "github.com");
        assert_eq!(meta.default_branch, "main");
        assert_eq!(meta.visibility, RepoVisibility::Public);
        assert!(!meta.archived);
        assert!(!meta.disabled);
        assert!(!meta.fork);
        assert_eq!(meta.slug(), "quangdang46/repo_orchestrator");
        assert_eq!(
            meta.clone_url.as_deref(),
            Some("https://github.com/quangdang46/repo_orchestrator.git")
        );
        assert_eq!(meta.default_remote.as_deref(), Some("origin"));
    }

    #[tokio::test]
    async fn fetch_repo_missing_default_branch_falls_back_to_main() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/x/y"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": 1,
                "node_id": "n",
                "name": "y",
                "full_name": "x/y",
                "owner": {
                    "login": "x",
                    "id": 1,
                    "node_id": "u",
                    "avatar_url": "https://example.test/a.png",
                    "gravatar_id": "",
                    "url": "https://api.github.com/users/x",
                    "html_url": "https://github.com/x",
                    "followers_url": "https://api.github.com/users/x/followers",
                    "following_url": "https://api.github.com/users/x/following",
                    "gists_url": "https://api.github.com/users/x/gists",
                    "starred_url": "https://api.github.com/users/x/starred",
                    "subscriptions_url": "https://api.github.com/users/x/subscriptions",
                    "organizations_url": "https://api.github.com/users/x/orgs",
                    "repos_url": "https://api.github.com/users/x/repos",
                    "events_url": "https://api.github.com/users/x/events",
                    "received_events_url": "https://api.github.com/users/x/received_events",
                    "type": "User",
                    "site_admin": false,
                },
                "private": true,
                "html_url": "https://github.com/x/y",
                "fork": false,
                "url": "https://api.github.com/repos/x/y",
                "visibility": "private"
            })))
            .mount(&server)
            .await;

        let client = mock_client(&server).await;
        let meta = fetch_repo(&client, "x", "y", Some("github.example.com"))
            .await
            .expect("fetch_repo");

        assert_eq!(meta.default_branch, "main");
        assert_eq!(meta.visibility, RepoVisibility::Private);
        assert_eq!(meta.host, "github.example.com");
    }

    #[tokio::test]
    async fn fetch_repo_404_is_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/none/none"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "Not Found"})))
            .mount(&server)
            .await;

        let client = mock_client(&server).await;
        let err = fetch_repo(&client, "none", "none", None).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("none/none"), "{msg}");
    }

    #[test]
    fn empty_owner_or_name_rejected() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let server = MockServer::start().await;
            let client = mock_client(&server).await;
            assert!(fetch_repo(&client, "", "x", None).await.is_err());
            assert!(fetch_repo(&client, "x", "", None).await.is_err());
        });
    }

    #[test]
    fn visibility_parses_known_values() {
        assert_eq!(
            RepoVisibility::from_str(Some("public")),
            RepoVisibility::Public
        );
        assert_eq!(
            RepoVisibility::from_str(Some("Private")),
            RepoVisibility::Private
        );
        assert_eq!(
            RepoVisibility::from_str(Some("internal")),
            RepoVisibility::Internal
        );
        assert_eq!(
            RepoVisibility::from_str(Some("ghost")),
            RepoVisibility::Unknown
        );
        assert_eq!(RepoVisibility::from_str(None), RepoVisibility::Unknown);
    }
}
