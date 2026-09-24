//! GitHub auth discovery.
//!
//! Order: GITHUB_TOKEN env → config token → gh CLI fallback → auto (try all).
//! Never log tokens. Handle rate limits with backoff.

use anyhow::{Context, Result, bail};
use std::env;

/// A GitHub token, never displayed in full.
#[derive(Clone, Debug)]
pub struct AuthToken(String);

impl AuthToken {
    /// Create from an explicit string (e.g., from config).
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// Return the raw token (for HTTP headers).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Redact for logging: show first 4 and last 4 chars, or "****" if short.
    pub fn redact(&self) -> String {
        let s = &self.0;
        if s.len() <= 12 {
            return "****".to_string();
        }
        format!("{}…{}", &s[..4], &s[s.len() - 4..])
    }
}

/// Discover a GitHub token using the given strategy.
///
/// Strategies (per PLAN.md §12.6):
/// - `"env"`: read `GITHUB_TOKEN` env var only.
/// - `"gh"`: shell out to `gh auth token` only.
/// - `"config-token"`: caller must supply a token; this method validates it's non-empty.
/// - `"auto"`: try env → config token (if supplied) → gh CLI, in order.
pub fn discover_token(strategy: &str, config_token: Option<&str>) -> Result<AuthToken> {
    match strategy {
        "env" => from_env(),
        "gh" => from_gh_cli(),
        "config-token" => from_config(config_token),
        "auto" => auto(config_token),
        _ => bail!("unknown auth strategy: {strategy}"),
    }
}

fn from_env() -> Result<AuthToken> {
    let token = env::var("GITHUB_TOKEN").context("GITHUB_TOKEN env var not set")?;
    if token.is_empty() {
        bail!("GITHUB_TOKEN env var is empty");
    }
    Ok(AuthToken::new(token))
}

fn from_gh_cli() -> Result<AuthToken> {
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .output()
        .context("failed to run `gh auth token` — is `gh` installed?")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("`gh auth token` failed: {stderr}");
    }
    let token =
        String::from_utf8(output.stdout).context("`gh auth token` returned invalid UTF-8")?;
    let token = token.trim();
    if token.is_empty() {
        bail!("`gh auth token` returned empty output — are you logged in?");
    }
    Ok(AuthToken::new(token))
}

fn from_config(token: Option<&str>) -> Result<AuthToken> {
    let Some(token) = token else {
        bail!("no config token provided for 'config-token' strategy");
    };
    if token.is_empty() {
        bail!("config token is empty");
    }
    Ok(AuthToken::new(token))
}

fn auto(config_token: Option<&str>) -> Result<AuthToken> {
    from_env()
        .or_else(|_| from_config(config_token))
        .or_else(|_| from_gh_cli())
        .context("tried GITHUB_TOKEN, config token, and `gh auth token`; all failed")
}

/// Build an octocrab client with the given token and optional host.
pub fn build_client(token: &AuthToken, host: Option<&str>) -> Result<octocrab::Octocrab> {
    let builder = octocrab::Octocrab::builder().personal_token(token.as_str().to_string());
    let builder = match host {
        Some(h) if h != "github.com" => {
            let base_url = format!("https://{h}");
            let msg = format!("setting GitHub base URI to {base_url}");
            builder.base_uri(base_url).context(msg)?
        }
        _ => builder,
    };
    builder.build().context("building octocrab client")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_redact_long() {
        let t = AuthToken::new("ghp_12345678901234567890");
        assert_eq!(t.redact(), "ghp_…7890");
    }

    #[test]
    fn token_redact_short() {
        let t = AuthToken::new("abc");
        assert_eq!(t.redact(), "****");
    }

    #[test]
    fn config_token_happy() {
        let t = discover_token("config-token", Some("my-secret")).unwrap();
        assert_eq!(t.as_str(), "my-secret");
    }

    #[test]
    fn config_token_missing() {
        let err = discover_token("config-token", None).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("no config token"), "msg: {msg}");
    }

    #[test]
    fn config_token_empty() {
        let err = discover_token("config-token", Some("")).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("config token is empty"), "msg: {msg}");
    }

    #[test]
    fn unknown_strategy() {
        let err = discover_token("nope", None).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unknown auth strategy: nope"), "msg: {msg}");
    }

    #[test]
    fn auto_with_config_token() {
        let t = discover_token("auto", Some("fallback-token")).unwrap();
        assert_eq!(t.as_str(), "fallback-token");
    }
}
