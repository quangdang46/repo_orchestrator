//! GitHub auth discovery.
//!
//! Order: GITHUB_TOKEN env → config token → gh CLI fallback → auto (try all).
//! Never log tokens. Handle rate limits with backoff.

use anyhow::{Context, Result, bail};
use std::env;

// The auth vocabulary lives in ro-core, which both this crate and ro-config
// depend on. An earlier draft had `AuthPolicy` defined here as well as in
// ro-config, which required the two crates to depend on each other. Re-exported
// rather than redefined so existing callers of `ro_github::auth::*` keep
// working and keep getting the one definition.
pub use ro_core::{AuthPolicy, AuthProvider, CommitIdentity, CredentialRef};

/// A GitHub token, never displayed in full.
///
/// This **used to** `#[derive(Debug)]`, which meant `{:?}` printed the raw
/// token. Redaction that is opt-in and manual is not redaction: one
/// `tracing` field, one `dbg!`, or one Debug-formatted panic message and the
/// value is in a log file. `Debug` is now manual and delegates to the single
/// rule in `ro-core::secret`, so there is nothing left to forget to call.
///
/// There is deliberately **no `Display`**. `format!("{token}")` is then a
/// compile error rather than a silent `***`, and a caller who genuinely wants
/// the value has to say `as_str()` — which makes every read greppable.
pub struct AuthToken(ro_core::SecretString);

impl std::fmt::Debug for AuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("AuthToken").field(&self.0.preview()).finish()
    }
}

impl AuthToken {
    /// Create from an explicit string (e.g., from config).
    pub fn new(token: impl Into<String>) -> Self {
        Self(ro_core::SecretString::new(token))
    }

    /// Return the raw token (for HTTP headers).
    pub fn as_str(&self) -> &str {
        self.0.expose()
    }

    /// Redact for logging: first 4 and last 4, or `****` if too short to
    /// reveal both ends without revealing most of it.
    ///
    /// Delegates to the one rule rather than implementing a second. The old
    /// version byte-sliced `&s[..4]`, which panics when byte 4 lands inside a
    /// multi-byte character.
    pub fn redact(&self) -> String {
        self.0.preview()
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

    /// True if `haystack` contains a PAT-shaped token.
    ///
    /// The same rule the end-to-end output check uses, duplicated rather than
    /// shared: this crate cannot depend on the `ro` binary's test tree, and a
    /// shared helper that both copies could drift from is worse than a
    /// four-line predicate.
    fn contains_pat(haystack: &str) -> bool {
        haystack
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|t| {
                (t.starts_with("ghp_") || t.starts_with("github_pat_"))
                    && t.len() >= 20
                    && t.chars().any(|c| c.is_ascii_digit())
            })
    }

    /// The bug: `AuthToken` derived `Debug`, so `{:?}` printed the raw token.
    /// Asserted here rather than only end-to-end because nothing in the tree
    /// currently Debug-formats a token, which means an end-to-end check alone
    /// would pass even with `Debug` reverted to leaking.
    #[test]
    fn debug_of_a_token_contains_no_pat() {
        for fake in [
            "ghp_16C7e42F292c6912E7710c838347Ae178B4a",
            "github_pat_11ABCDEFG0aBcDeFgHiJkLmNoPqRsT",
        ] {
            let rendered = format!("{:?}", AuthToken::new(fake));
            assert!(!contains_pat(&rendered), "Debug leaked a PAT: {rendered}");
        }
    }

    /// The negative control. Without this, the test above is vacuous — it
    /// would pass just as happily against a `Debug` that prints the raw value,
    /// because nothing else in the tree prints one either way.
    #[test]
    fn the_debug_assertion_has_teeth() {
        let fake = "ghp_16C7e42F292c6912E7710c838347Ae178B4a";
        // What the old derived Debug produced.
        assert!(
            contains_pat(&format!("AuthToken(\"{fake}\")")),
            "the predicate must flag the raw value the old Debug printed, \
             otherwise debug_of_a_token_contains_no_pat proves nothing"
        );
    }

    /// A Debug-formatted struct is how a token actually reaches a log line:
    /// some field holds it and the whole record is printed.
    #[test]
    fn a_derived_debug_on_a_holder_does_not_leak_it() {
        #[derive(Debug)]
        struct Request {
            endpoint: &'static str,
            token: AuthToken,
        }
        let r = Request {
            endpoint: "/repos/acme/api",
            token: AuthToken::new("ghp_16C7e42F292c6912E7710c838347Ae178B4a"),
        };
        let rendered = format!("{r:?}");
        assert!(
            rendered.contains("/repos/acme/api"),
            "other fields stay visible"
        );
        assert!(!contains_pat(&rendered), "the token leaked: {rendered}");
    }

    /// The old implementation byte-sliced, which panics in the middle of a
    /// multi-byte character.
    #[test]
    fn redaction_does_not_panic_on_a_multibyte_token() {
        let t = AuthToken::new("héllo wörld — a token, but not ASCII");
        assert!(t.redact().contains('…'));
    }

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
