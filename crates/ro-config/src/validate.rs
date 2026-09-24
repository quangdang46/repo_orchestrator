//! Configuration validation.
//!
//! Surfaces clear errors for invalid enum-like fields rather than letting
//! invalid configs flow into runtime code paths.

use crate::schema::AppConfig;
use anyhow::{Result, bail};

/// Validate an `AppConfig`. Returns `Ok(())` if all fields are within
/// their allowed value sets; returns an error describing the first
/// invalid field encountered otherwise.
pub fn validate(cfg: &AppConfig) -> Result<()> {
    if !["flat", "nested"].contains(&cfg.core.layout.as_str()) {
        bail!(
            "core.layout: '{}' is not a valid layout (expected: flat | nested)",
            cfg.core.layout
        );
    }
    if cfg.core.parallel == 0 {
        bail!("core.parallel: must be >= 1");
    }
    if cfg.core.timeout_secs == 0 {
        bail!("core.timeout_secs: must be >= 1");
    }

    if !["env", "gh", "config-token", "auto"].contains(&cfg.github.auth.as_str()) {
        bail!(
            "github.auth: '{}' is not valid (expected: env | gh | config-token | auto)",
            cfg.github.auth
        );
    }

    if !["ff-only", "rebase", "merge"].contains(&cfg.git.update_strategy.as_str()) {
        bail!(
            "git.update_strategy: '{}' is not valid (expected: ff-only | rebase | merge)",
            cfg.git.update_strategy
        );
    }

    if !["fixed", "exponential"].contains(&cfg.jobs.retry_backoff.as_str()) {
        bail!(
            "jobs.retry_backoff: '{}' is not valid (expected: fixed | exponential)",
            cfg.jobs.retry_backoff
        );
    }
    if cfg.jobs.max_attempts == 0 {
        bail!("jobs.max_attempts: must be >= 1");
    }

    if !["claude", "codex"].contains(&cfg.review.provider.as_str()) {
        bail!(
            "review.provider: '{}' is not valid (expected: claude | codex)",
            cfg.review.provider
        );
    }
    if !["auto", "on", "off"].contains(&cfg.review.quality_gates.as_str()) {
        bail!(
            "review.quality_gates: '{}' is not valid (expected: auto | on | off)",
            cfg.review.quality_gates
        );
    }

    if !["off", "warn", "block"].contains(&cfg.safety.secret_scan.as_str()) {
        bail!(
            "safety.secret_scan: '{}' is not valid (expected: off | warn | block)",
            cfg.safety.secret_scan
        );
    }
    if !["low", "medium", "high"].contains(&cfg.safety.max_auto_apply_risk.as_str()) {
        bail!(
            "safety.max_auto_apply_risk: '{}' is not valid (expected: low | medium | high)",
            cfg.safety.max_auto_apply_risk
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        let cfg = AppConfig::default();
        validate(&cfg).expect("default config must validate");
    }

    #[test]
    fn invalid_layout_rejected() {
        let mut cfg = AppConfig::default();
        cfg.core.layout = "weird".into();
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("core.layout"));
    }

    #[test]
    fn invalid_auth_rejected() {
        let mut cfg = AppConfig::default();
        cfg.github.auth = "ssh".into();
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("github.auth"));
    }

    #[test]
    fn zero_parallel_rejected() {
        let mut cfg = AppConfig::default();
        cfg.core.parallel = 0;
        validate(&cfg).expect_err("zero parallel must fail");
    }

    #[test]
    fn invalid_secret_scan_rejected() {
        let mut cfg = AppConfig::default();
        cfg.safety.secret_scan = "loud".into();
        validate(&cfg).expect_err("invalid secret_scan must fail");
    }
}
