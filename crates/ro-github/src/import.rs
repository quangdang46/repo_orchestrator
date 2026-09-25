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
    // Validate the arguments *before* doing anything expensive. The old code
    // built the Octocrab client and only then discovered, inside the async
    // block, that no source had been named at all.
    if !stars && org.is_none() && user.is_none() {
        anyhow::bail!("no import source specified");
    }

    let token = crate::auth::discover_token("auto", None)
        .context("GitHub token required for import — set GITHUB_TOKEN or run `gh auth login`")?;

    let rt = tokio::runtime::Runtime::new().context("creating tokio runtime")?;

    // The client MUST be constructed inside the runtime. `Octocrab::builder()
    // .build()` panics with "there is no reactor running" when called
    // outside one, which made `ro import --stars` crash on any machine where
    // a token was discoverable. It passed CI only because `gh` is absent
    // there, so the test never entered this path with a live token.
    rt.block_on(async {
        let client = crate::auth::build_client(&token, None)?;
        if stars {
            fetch_stars(&client, limit).await
        } else if let Some(o) = org {
            fetch_org(&client, o, limit).await
        } else if let Some(u) = user {
            fetch_user(&client, u, limit).await
        } else {
            // Unreachable: guarded above. Kept so the chain stays total.
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

    /// The regression that mattered.
    ///
    /// `fetch_import_specs` used to build the Octocrab client *before*
    /// constructing the tokio runtime, so `Octocrab::builder().build()`
    /// panicked with "there is no reactor running". The existing test could
    /// not catch it because it only asserted `is_err()` — a panic is not an
    /// `Err`, and the test never reached the client because no source was
    /// given.
    ///
    /// This asserts the *specific* error, which pins the argument guard in
    /// front of the credential lookup and the client construction: naming no
    /// source must fail for that reason, not for want of a token and not by
    /// panicking. That is the property that keeps `ro import` from reaching
    /// the runtime-less `build_client` at all.
    ///
    /// It passes on CI for the same reason the old code did: `gh` is absent
    /// there, so `discover_token` would fail with a token message. That is
    /// fine — the assertion distinguishes the two failure modes, and if the
    /// guard is ever moved back below `discover_token` the message changes
    /// and this test goes red on CI too.
    #[test]
    fn fetch_without_source_fails_before_touching_credentials() {
        let result = fetch_import_specs(false, None, None, None);
        let err = format!("{:#}", result.expect_err("no source must be an error"));
        assert!(
            err.contains("no import source specified"),
            "expected the argument guard to fire first, got: {err}"
        );
    }
}
