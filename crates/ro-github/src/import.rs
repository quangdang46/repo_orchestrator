//! Bulk import helpers: fetch repo specs from GitHub stars, org, or user.
//!
//! Returns a list of `owner/name` strings that can be fed into
//! `ro_sync::manage::add` one at a time.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A minimal repo entry returned by the GitHub list APIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportEntry {
    pub owner: String,
    pub name: String,
    pub full_name: String,
}

/// Fetch repo specs (e.g. "owner/name") from the requested source.
///
/// Caller passes flags from the CLI; the first matching source wins.
/// Network failures bubble up as anyhow errors.
pub fn fetch_import_specs(
    stars: bool,
    org: Option<&str>,
    user: Option<&str>,
    limit: Option<usize>,
) -> Result<Vec<String>> {
    let token = crate::auth::discover_token("auto", None)
        .context("GitHub token required for import — set GITHUB_TOKEN or run `gh auth login`")?;
    let client = crate::auth::build_client(&token, None)?;

    let rt = tokio::runtime::Runtime::new().context("creating tokio runtime")?;
    rt.block_on(async {
        if stars {
            fetch_stars(&client, limit).await
        } else if let Some(o) = org {
            fetch_org(&client, o, limit).await
        } else if let Some(u) = user {
            fetch_user(&client, u, limit).await
        } else {
            anyhow::bail!("no import source specified")
        }
    })
}

async fn fetch_stars(client: &octocrab::Octocrab, limit: Option<usize>) -> Result<Vec<String>> {
    let mut all = Vec::new();
    let mut page: u32 = 1;
    loop {
        let url = format!("/user/starred?per_page=100&page={page}");
        let resp: Result<Vec<serde_json::Value>, _> = client.get(&url, None::<&()>).await;
        let items = match resp {
            Ok(v) => v,
            Err(e) => return Err(anyhow::anyhow!("GitHub stars API: {e}")),
        };
        if items.is_empty() {
            break;
        }
        for item in items {
            if let Some(full) = item.get("full_name").and_then(|v| v.as_str()) {
                all.push(full.to_string());
                if limit.is_some_and(|l| all.len() >= l) {
                    return Ok(all);
                }
            }
        }
        if all.len() < (page as usize) * 100 {
            break;
        }
        page += 1;
        if page > 50 {
            break;
        }
    }
    Ok(all)
}

async fn fetch_org(
    client: &octocrab::Octocrab,
    org: &str,
    limit: Option<usize>,
) -> Result<Vec<String>> {
    let mut all = Vec::new();
    let mut page: u32 = 1;
    loop {
        let url = format!("/orgs/{org}/repos?per_page=100&page={page}&type=all");
        let resp: Result<Vec<serde_json::Value>, _> = client.get(&url, None::<&()>).await;
        let items = match resp {
            Ok(v) => v,
            Err(e) => return Err(anyhow::anyhow!("GitHub orgs API: {e}")),
        };
        if items.is_empty() {
            break;
        }
        for item in items {
            if let Some(full) = item.get("full_name").and_then(|v| v.as_str()) {
                all.push(full.to_string());
                if limit.is_some_and(|l| all.len() >= l) {
                    return Ok(all);
                }
            }
        }
        if all.len() < (page as usize) * 100 {
            break;
        }
        page += 1;
        if page > 50 {
            break;
        }
    }
    Ok(all)
}

async fn fetch_user(
    client: &octocrab::Octocrab,
    user: &str,
    limit: Option<usize>,
) -> Result<Vec<String>> {
    let mut all = Vec::new();
    let mut page: u32 = 1;
    loop {
        let url = format!("/users/{user}/repos?per_page=100&page={page}&type=owner");
        let resp: Result<Vec<serde_json::Value>, _> = client.get(&url, None::<&()>).await;
        let items = match resp {
            Ok(v) => v,
            Err(e) => return Err(anyhow::anyhow!("GitHub users API: {e}")),
        };
        if items.is_empty() {
            break;
        }
        for item in items {
            if let Some(full) = item.get("full_name").and_then(|v| v.as_str()) {
                all.push(full.to_string());
                if limit.is_some_and(|l| all.len() >= l) {
                    return Ok(all);
                }
            }
        }
        if all.len() < (page as usize) * 100 {
            break;
        }
        page += 1;
        if page > 50 {
            break;
        }
    }
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_without_source_errors() {
        let result = fetch_import_specs(false, None, None, None);
        assert!(result.is_err());
    }
}
