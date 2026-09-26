//! Configuration schema for ro.
//!
//! Mirrors the TOML config from PLAN.md §12. All fields default to the
//! values shipped with `ro init`. Validation lives in [`crate::validate`].

use ro_core::CredentialRef;
use serde::{Deserialize, Serialize};

/// Top-level application configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub core: CoreConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub github: GitHubConfig,
    #[serde(default)]
    pub git: GitConfig,
    #[serde(default)]
    pub jobs: JobsConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub review: ReviewConfig,
    #[serde(default)]
    pub providers: ProvidersConfig,
    #[serde(default)]
    pub safety: SafetyConfig,
}

/// `[auth]` — the credential **source** for any repo that does not override it.
///
/// The key name is the transport and the value is a *reference* to a secret,
/// never the secret:
///
/// ```toml
/// [auth]
/// https = "env:GH_PERSONAL_TOKEN"
/// ssh   = "keychain:ssh-work"
/// ```
///
/// `deny_unknown_fields` is the load-bearing part, and it is what makes this a
/// P0 rather than a style rule. The shape a user actually reaches for is
/// `token = "ghp_…"`. Without this attribute that key is ignored in silence,
/// the credential does not work, and the pasted secret sits in a file that gets
/// backed up and pasted into issues. With it, the key is a parse error naming
/// the two forms that do work.
///
/// Omit the whole table and ro uses the machine's own credential — SSH agent,
/// git credential manager, `gh auth` — which is the right answer for a repo
/// whose SSH key is already correct and needs no configuration at all.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Reference to the HTTPS credential. `env:VAR` or `keychain:ENTRY`.
    pub https: Option<CredentialRef>,
    /// Reference to the SSH credential. `env:VAR` or `keychain:ENTRY`.
    pub ssh: Option<CredentialRef>,
    /// The login the credential is expected to resolve to, checked before a
    /// push. This is *not* the commit author, which ro sets and therefore
    /// cannot get wrong; it is the account the remote sees, which ro does not
    /// control.
    pub expected_login: Option<String>,
}

/// `[core]` — global runtime knobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreConfig {
    #[serde(default = "default_projects_dir")]
    pub projects_dir: String,
    #[serde(default = "default_layout")]
    pub layout: String,
    #[serde(default = "default_parallel")]
    pub parallel: u32,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u32,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            projects_dir: default_projects_dir(),
            layout: default_layout(),
            parallel: default_parallel(),
            timeout_secs: default_timeout(),
        }
    }
}

/// `[github]` — GitHub host + auth strategy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubConfig {
    #[serde(default = "default_github_host")]
    pub host: String,
    #[serde(default = "default_auth")]
    pub auth: String,
}

impl Default for GitHubConfig {
    fn default() -> Self {
        Self {
            host: default_github_host(),
            auth: default_auth(),
        }
    }
}

/// `[git]` — git command behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitConfig {
    #[serde(default = "default_strategy")]
    pub update_strategy: String,
    #[serde(default)]
    pub autostash: bool,
    #[serde(default)]
    pub terminal_prompt: bool,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            update_strategy: default_strategy(),
            autostash: false,
            terminal_prompt: false,
        }
    }
}

/// `[jobs]` — durable job execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_retry_backoff")]
    pub retry_backoff: String,
    #[serde(default = "default_job_timeout_secs")]
    pub default_timeout_secs: u32,
}

impl Default for JobsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_attempts: default_max_attempts(),
            retry_backoff: default_retry_backoff(),
            default_timeout_secs: default_job_timeout_secs(),
        }
    }
}

/// `[mcp]` — MCP server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub stdio: bool,
    #[serde(default)]
    pub sse: bool,
    #[serde(default = "default_sse_port")]
    pub sse_port: u16,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            stdio: true,
            sse: false,
            sse_port: default_sse_port(),
        }
    }
}

/// `[review]` — review/sweep defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewConfig {
    #[serde(default = "default_review_provider")]
    pub provider: String,
    #[serde(default = "default_quality_gates")]
    pub quality_gates: String,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            provider: default_review_provider(),
            quality_gates: default_quality_gates(),
        }
    }
}

/// `[providers]` — AI provider configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProvidersConfig {
    #[serde(default)]
    pub claude: ProviderConfig,
    #[serde(default)]
    pub codex: ProviderConfig,
}

/// Single AI provider entry: binary path + default arguments.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub bin: String,
    #[serde(default)]
    pub default_args: Vec<String>,
}

/// `[safety]` — secret scan + AI auto-apply guards.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyConfig {
    #[serde(default = "default_secret_scan")]
    pub secret_scan: String,
    #[serde(default = "default_true")]
    pub require_plan_for_ai_apply: bool,
    #[serde(default = "default_max_risk")]
    pub max_auto_apply_risk: String,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            secret_scan: default_secret_scan(),
            require_plan_for_ai_apply: true,
            max_auto_apply_risk: default_max_risk(),
        }
    }
}

fn default_projects_dir() -> String {
    "~/projects".into()
}
fn default_layout() -> String {
    "flat".into()
}
fn default_parallel() -> u32 {
    8
}
fn default_timeout() -> u32 {
    30
}
fn default_github_host() -> String {
    "github.com".into()
}
fn default_auth() -> String {
    "auto".into()
}
fn default_strategy() -> String {
    "ff-only".into()
}
fn default_secret_scan() -> String {
    "block".into()
}
fn default_max_risk() -> String {
    "low".into()
}
fn default_true() -> bool {
    true
}
fn default_max_attempts() -> u32 {
    3
}
fn default_retry_backoff() -> String {
    "exponential".into()
}
fn default_job_timeout_secs() -> u32 {
    1800
}
fn default_sse_port() -> u16 {
    7300
}
fn default_review_provider() -> String {
    "claude".into()
}
fn default_quality_gates() -> String {
    "auto".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Path 1 of 3: the global config file.
    ///
    /// `deny_unknown_fields` is what makes this work. Without it, a
    /// `token = "ghp_…"` key is ignored in silence — no error, the credential
    /// simply does not work, and the pasted secret stays in a file that gets
    /// backed up and pasted into issues.
    #[test]
    fn auth_rejects_a_token_key() {
        let err = toml::from_str::<AppConfig>(
            r#"[auth]
token = "ghp_16C7e42F292c6912E7710c838347Ae178B4a"
"#,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("token") || msg.contains("unknown field"),
            "the error must name the offending key, got: {msg}"
        );
    }

    /// Path 1 of 3, second shape: the key is right but the value is a secret
    /// rather than a reference, so `CredentialRef`'s own deserializer rejects
    /// it and the message names the two forms that do work.
    #[test]
    fn auth_rejects_a_pasted_secret_as_a_value() {
        let err = toml::from_str::<AppConfig>(
            r#"[auth]
https = "ghp_16C7e42F292c6912E7710c838347Ae178B4a"
"#,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("env:VAR_NAME") && msg.contains("keychain:ENTRY_NAME"),
            "the message must teach the two accepted forms, got: {msg}"
        );
    }

    #[test]
    fn auth_accepts_references_and_omission() {
        let cfg: AppConfig = toml::from_str(
            r#"[auth]
https = "env:GH_PERSONAL_TOKEN"
ssh   = "keychain:ssh-work"
expected_login = "quangdang46"
"#,
        )
        .unwrap();
        assert_eq!(
            cfg.auth.https.as_ref().unwrap().to_string(),
            "env:GH_PERSONAL_TOKEN"
        );
        assert_eq!(cfg.auth.ssh.as_ref().unwrap().name(), "ssh-work");
        assert_eq!(cfg.auth.expected_login.as_deref(), Some("quangdang46"));

        // The whole table omitted is valid and means "use the machine's own
        // credential" — not "use an empty credential".
        let bare: AppConfig = toml::from_str("").unwrap();
        assert!(bare.auth.https.is_none());
        assert!(bare.auth.ssh.is_none());
    }

    #[test]
    fn defaults_match_plan_example() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.core.layout, "flat");
        assert_eq!(cfg.core.parallel, 8);
        assert_eq!(cfg.core.timeout_secs, 30);
        assert_eq!(cfg.github.host, "github.com");
        assert_eq!(cfg.github.auth, "auto");
        assert_eq!(cfg.git.update_strategy, "ff-only");
        assert!(cfg.jobs.enabled);
        assert_eq!(cfg.jobs.max_attempts, 3);
        assert_eq!(cfg.jobs.retry_backoff, "exponential");
        assert_eq!(cfg.jobs.default_timeout_secs, 1800);
        assert!(cfg.mcp.enabled);
        assert!(cfg.mcp.stdio);
        assert!(!cfg.mcp.sse);
        assert_eq!(cfg.mcp.sse_port, 7300);
        assert_eq!(cfg.review.provider, "claude");
        assert_eq!(cfg.review.quality_gates, "auto");
        assert_eq!(cfg.safety.secret_scan, "block");
        assert!(cfg.safety.require_plan_for_ai_apply);
        assert_eq!(cfg.safety.max_auto_apply_risk, "low");
    }

    #[test]
    fn round_trip_through_toml() {
        let cfg = AppConfig::default();
        let serialized = toml::to_string(&cfg).expect("serialize");
        let parsed: AppConfig = toml::from_str(&serialized).expect("parse");
        assert_eq!(parsed.core.layout, cfg.core.layout);
        assert_eq!(parsed.jobs.max_attempts, cfg.jobs.max_attempts);
        assert_eq!(
            parsed.providers.claude.default_args,
            cfg.providers.claude.default_args
        );
    }
}
