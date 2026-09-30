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

    // `[agent].engine` was the one key `ro config set` wrote without checking.
    // Every sibling — `core.layout`, `github.auth`, `core.parallel`,
    // `core.timeout_secs` — is refused at write time, so `ro config set
    // 'agent.engine="bogus"'` exited 0, printed `Set agent.engine = "bogus"`,
    // and wrote it. The cost was deferred to the next fleet run, which died
    // for **every** repo: `ro commit --dry-run` → exit 70, `error planning
    // o/30: unknown engine "bogus"`. A typo in a flag value is reported at the
    // flag; a typo in a config value must be reported at the config.
    if let Some(engine) = cfg.agent.engine.as_deref() {
        if !["claude", "codex", "git"].contains(&engine) {
            bail!(
                "agent.engine: '{engine}' is not valid (expected: claude | codex | git)"
            );
        }
    }

    // Everything else this used to validate is gone with its table:
    // `git.update_strategy`, `jobs.*`, `checkpoint.*` and `safety.*`.
    //
    // Validating a key that nothing reads is worse than not having the key.
    // A user who sets `safety.secret_scan` to a reasonable value concludes
    // it is doing something, and it is not — which is the exact failure a
    // config key is supposed to rule out. A table ro does not read now
    // produces a migration note in the loader instead (see
    // `loader::deprecated_tables`), which is loud and actionable.

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
    /// `[agent].engine` is the one key `ro config set` used to write without
    /// checking. Every sibling is refused at write time, so a typo here was
    /// accepted at exit 0 and then failed the **next fleet run**, once per
    /// repo: `error planning o/30: unknown engine "bogus"`.
    #[test]
    fn an_unknown_agent_engine_is_rejected() {
        let mut cfg = AppConfig::default();
        cfg.agent.engine = Some("bogus".into());
        validate(&cfg).expect_err("an unknown engine must fail validation");

        for good in ["claude", "codex", "git"] {
            let mut cfg = AppConfig::default();
            cfg.agent.engine = Some(good.into());
            validate(&cfg).unwrap_or_else(|e| panic!("{good} must validate: {e}"));
        }
    }

    /// The negative control: `None` is "use the default", not "a bad value".
    #[test]
    fn an_unset_agent_engine_is_valid() {
        let cfg = AppConfig::default();
        validate(&cfg).expect("the shipped default validates");
    }

    fn zero_parallel_rejected() {
        let mut cfg = AppConfig::default();
        cfg.core.parallel = 0;
        validate(&cfg).expect_err("zero parallel must fail");
    }

    /// The preflight's secret scan is not configurable, so there is
    /// nothing to validate. This test exists to say so: the alternative is
    /// someone re-adding a `[safety]` section because a test once
    /// mentioned one.
    #[test]
    fn there_is_no_configurable_secret_scan_to_reject() {
        // The default file must not carry a table that is never read.
        let raw = crate::paths::default_config_toml();
        for gone in ["[safety]", "[checkpoint]", "[jobs]", "[git]", "[mcp]"] {
            assert!(
                !raw.lines().any(|l| l.trim() == gone),
                "{gone} is written into the default config but read by nothing"
            );
        }
    }
}
