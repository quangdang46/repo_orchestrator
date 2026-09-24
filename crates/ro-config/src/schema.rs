//! Configuration schema for ro.
//!
//! Mirrors the TOML config from PLAN.md §12. All fields default to the
//! values shipped with `ro init`. Validation lives in [`crate::validate`].

use serde::{Deserialize, Serialize};

/// Top-level application configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub core: CoreConfig,
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
