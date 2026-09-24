//! Configuration file loader.

use crate::paths::{ConfigPaths, default_config_toml};
use crate::schema::AppConfig;
use crate::validate::validate;
use anyhow::{Context, Result};
use std::path::Path;

/// Load config from a TOML file, falling back to defaults.
///
/// Validates the resulting config; returns an error if any field is out
/// of its allowed value set. Missing files yield validated defaults.
pub fn load_config(path: &Path) -> Result<AppConfig> {
    if !path.exists() {
        let cfg = AppConfig::default();
        validate(&cfg)?;
        return Ok(cfg);
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading config from {}", path.display()))?;
    let config: AppConfig =
        toml::from_str(&raw).with_context(|| format!("parsing config from {}", path.display()))?;
    validate(&config).with_context(|| format!("validating config at {}", path.display()))?;
    Ok(config)
}

/// Load config from the canonical XDG path
/// (`$XDG_CONFIG_HOME/ro/config.toml`).
pub fn load_default() -> Result<AppConfig> {
    let paths = ConfigPaths::discover()?;
    load_config(&paths.config_toml())
}

/// Write the shipped default config to `path`, creating parent
/// directories as needed. Returns `Ok(false)` if the file already
/// exists (no-op), `Ok(true)` if a new file was written.
pub fn write_default(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, default_config_toml())
        .with_context(|| format!("writing default config to {}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempdir().unwrap();
        let cfg = load_config(&dir.path().join("absent.toml")).unwrap();
        assert_eq!(cfg.core.layout, "flat");
    }

    #[test]
    fn write_default_creates_file_then_skips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ro/config.toml");
        assert!(write_default(&path).unwrap());
        assert!(path.exists());
        // Second call must be a no-op.
        assert!(!write_default(&path).unwrap());
    }

    #[test]
    fn invalid_config_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "[core]\nlayout = \"weird\"\n").unwrap();
        let err = load_config(&path).unwrap_err();
        assert!(err.to_string().contains("validating config"));
    }

    #[test]
    fn round_trip_load_then_load() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ro/config.toml");
        write_default(&path).unwrap();
        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.jobs.max_attempts, 3);
    }
}
