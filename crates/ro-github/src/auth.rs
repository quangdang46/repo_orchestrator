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
/// Strategies (per PLAN.md §12.6):
/// - `"env"`: read `GH_TOKEN`, then `GITHUB_TOKEN`, from the environment.
/// - `"gh"`: shell out to `gh auth token`.
///
/// The two that used to be here are **deleted**, not deprecated:
///
/// - `auto` was `from_env().or_else(from_config).or_else(from_gh_cli)` with
///   every error swallowed. That is precisely the silent fallback the design
///   forbids: a run can push via a credential nobody chose, and the only
///   trace is that it worked. The caller now names the source it wants.
/// - `config-token` could never succeed. It required the caller to supply a
///   token, and the config key that would have held one (`token`) does not
///   exist — there is no `token` key, in any layer, in any version after the
///   credential reference landed. It was reachable and always empty.
pub fn discover_token(strategy: &str) -> Result<AuthToken> {
    match strategy {
        "env" => from_env(),
        "gh" => from_gh_cli(),
        other => bail!(
            "unknown auth strategy: {other}\n\
             expected 'env' (read GH_TOKEN or GITHUB_TOKEN) or 'gh' (run \
             `gh auth token`). There is no 'auto': a strategy that falls back \
             silently can push with a credential nobody chose."
        ),
    }
}

/// `GH_TOKEN` before `GITHUB_TOKEN`, matching gh CLI precedence.
///
/// The order is not cosmetic. When both are set they are usually *different*
/// accounts — an exported work token plus a shell-level personal one — and
/// picking the other one produces a push as the wrong person with no error
/// anywhere. gh itself prefers `GH_TOKEN`, so a user who has both set is
/// already living with gh's answer.
///
/// `var_os`, not `var`: a token is bytes, and `var` fails on a non-UTF-8 value
/// with an error that reads like the variable is missing. `AuthToken` holds a
/// `String`, so a non-UTF-8 value cannot be used — but it must be reported as
/// *this variable is not valid UTF-8*, not as *not set*, or the two are
/// indistinguishable from the outside and the fallback path gets taken.
fn from_env() -> Result<AuthToken> {
    for name in ["GH_TOKEN", "GITHUB_TOKEN"] {
        match env::var_os(name) {
            None => continue,
            Some(value) => {
                let Some(token) = value.to_str() else {
                    bail!("{name} is set but is not valid UTF-8");
                };
                if token.is_empty() {
                    bail!("{name} is set but empty");
                }
                return Ok(AuthToken::new(token));
            }
        }
    }
    bail!(
        "no token in the environment: set GH_TOKEN (preferred) or GITHUB_TOKEN.\n\
         For a per-repo credential, set `credential_ref` on the row to \
         'env:VAR_NAME' so the variable is named for that repo specifically."
    )
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

    /// Every `GH_TOKEN` / `GITHUB_TOKEN` case, in one test.
    ///
    /// Consolidated deliberately. These are process-global variables and Rust
    /// runs tests in parallel threads, so four separate tests each setting
    /// them interfere with each other — the first version of this passed
    /// `GH_TOKEN`/`GITHUB_TOKEN` as separate tests and `gh_token_wins` read the
    /// value another test had just written. One test, one thread, restored
    /// between cases.
    #[test]
    fn env_precedence_and_absence() {
        // SAFETY: single test body, no other thread in this binary touches
        // these variables, and each case restores what it set.
        unsafe {
            env::remove_var("GH_TOKEN");
            env::remove_var("GITHUB_TOKEN");

            // 1. Both set, and they are different accounts. GH_TOKEN wins,
            //    matching gh CLI. The order is load-bearing: picking the other
            //    one pushes as the wrong person with no error anywhere.
            env::set_var("GH_TOKEN", "gh-token-value");
            env::set_var("GITHUB_TOKEN", "github-token-value");
            assert_eq!(
                discover_token("env").unwrap().as_str(),
                "gh-token-value",
                "GH_TOKEN must win over GITHUB_TOKEN"
            );

            // 2. GITHUB_TOKEN alone.
            env::remove_var("GH_TOKEN");
            assert_eq!(
                discover_token("env").unwrap().as_str(),
                "github-token-value",
                "GITHUB_TOKEN is the fallback"
            );

            // 3. An empty GH_TOKEN is an error, not a fall-through. Skipping
            //    it would silently promote GITHUB_TOKEN — a different
            //    account — over what was almost certainly a typo.
            env::set_var("GH_TOKEN", "");
            let err = discover_token("env").unwrap_err();
            assert!(
                format!("{err}").contains("GH_TOKEN is set but empty"),
                "an empty variable must be reported, not skipped: {err}"
            );

            // 4. Neither. The message names both, and the per-repo form,
            //    because that is the answer for a fleet.
            env::remove_var("GH_TOKEN");
            env::remove_var("GITHUB_TOKEN");
            let err = discover_token("env").unwrap_err();
            let msg = format!("{err}");
            assert!(msg.contains("GH_TOKEN"), "must name the preferred: {msg}");
            assert!(
                msg.contains("GITHUB_TOKEN"),
                "must name the fallback: {msg}"
            );
            assert!(
                msg.contains("env:VAR_NAME"),
                "must point at the per-repo form: {msg}"
            );
        }
    }

    /// The two deleted strategies must stay deleted. A test that asserts a
    /// removal is the only thing that stops it creeping back in as a
    /// "compatibility" default.
    #[test]
    fn auto_and_config_token_are_rejected_with_a_reason() {
        for strategy in ["auto", "config-token"] {
            let err = discover_token(strategy).unwrap_err();
            let msg = format!("{err}");
            assert!(
                msg.contains("unknown auth strategy"),
                "{strategy} must not be accepted, got: {msg}"
            );
            assert!(
                msg.contains("no 'auto'"),
                "the error must say why, so nobody re-adds it: {msg}"
            );
        }
    }

    #[test]
    fn unknown_strategy() {
        let err = discover_token("nope").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unknown auth strategy: nope"), "msg: {msg}");
    }
}
