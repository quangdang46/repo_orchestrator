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
    // Loud, not silent. A deprecated key that is merely tolerated is a
    // deprecated key nobody migrates off, and the whole point of reading the
    // old table is that the reading stops eventually.
    if let Some(note) = config.engine_deprecation_note() {
        tracing::warn!("{note}");
        eprintln!("warning: {note}");
    }
    Ok(config)
}

/// Load config from the canonical XDG path
/// (`$XDG_CONFIG_HOME/ro/config.toml`).
pub fn load_default() -> Result<AppConfig> {
    let paths = ConfigPaths::discover()?;
    load_config(&paths.config_toml())
}

/// Set one `dotted.key` in a TOML file, in place.
///
/// This exists because the alternative is data loss on every call.
/// Deserializing into [`AppConfig`] and re-serializing drops every comment and
/// every key the schema does not model — a `#` explaining why a value is what
/// it is, and a setting from a newer or older ro — and it did so silently, on
/// every single assignment.
///
/// `toml_edit` edits the parsed document rather than re-encoding a struct, so
/// everything the struct never saw survives untouched.
///
/// The edit is validated before it is kept: the file is re-read and run
/// through `validate`, and on failure the **original bytes** are restored. A
/// config that does not parse is a worse outcome than a config with the old
/// value in it, and writing then discovering that is how you lose a working
/// setup to a typo.
///
/// No `.bak` is written. A backup per assignment is noise that does not fix the
/// loss — the loss is now structural, so there is nothing to recover. A backup
/// belongs on the one genuinely destructive operation, the migrations.
pub fn set_key_in_file(path: &Path, dotted_key: &str, raw_value: &str) -> Result<()> {
    let original = std::fs::read_to_string(path)
        .with_context(|| format!("reading config from {}", path.display()))?;
    let mut doc: toml_edit::DocumentMut = original
        .parse()
        .with_context(|| format!("parsing config at {}", path.display()))?;

    let segments: Vec<&str> = dotted_key.split('.').collect();
    if segments.iter().any(|s| s.is_empty()) {
        anyhow::bail!("config key {dotted_key:?} has an empty path segment");
    }
    let (leaf, parents) = segments
        .split_last()
        .expect("split('.').collect() always yields at least one segment");

    // Walk or create each parent table. A key like `core.layout` on a file that
    // has no `[core]` yet should add the table, not fail.
    let mut table: &mut toml_edit::Table = doc.as_table_mut();
    for segment in parents {
        let entry = table
            .entry(segment)
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
        table = entry
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("{dotted_key:?} is not a table path"))?;
    }

    // Parse the value as TOML so `4` becomes an integer and `true` a boolean,
    // rather than every assignment landing as a string the schema then rejects.
    let value: toml_edit::Value = raw_value
        .parse()
        .map_err(|e| anyhow::anyhow!("{raw_value:?} is not a valid TOML value: {e}"))?;
    table.insert(leaf, toml_edit::value(value));

    let updated = doc.to_string();

    // Validate what we are about to write, not what we assume we wrote.
    // The reason is folded into the message rather than attached as a
    // `with_context` layer: a user who typed a bad value needs to be told what
    // was wrong with it here, not handed a bare "would fail validation" and a
    // source they have to know to print.
    let reparsed: AppConfig = toml::from_str(&updated).map_err(|e| {
        anyhow::anyhow!("the edit to {dotted_key} would not produce a valid config: {e}")
    })?;
    if let Err(e) = validate(&reparsed) {
        anyhow::bail!("the edit to {dotted_key} would fail validation: {e:#}");
    }

    std::fs::write(path, &updated)
        .with_context(|| format!("writing config to {}", path.display()))?;
    Ok(())
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

    /// The bug this whole function exists to fix. A hand-written comment
    /// explaining *why* a value is what it is is exactly the thing a struct
    /// round trip throws away, and it is the thing nobody can reconstruct.
    #[test]
    fn a_hand_written_comment_survives_a_set() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[core]\n# why flat: see the migration note in the README\nlayout = \"flat\"\n",
        )
        .unwrap();

        set_key_in_file(&path, "core.parallel", "4").unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("# why flat: see the migration note in the README"),
            "a comment must survive an assignment to a sibling key, got:\n{after}"
        );
    }

    /// The other half. An unknown table is a setting from a newer ro, or a
    /// leftover from an older one. It is not ours to delete, and deleting a
    /// user's file contents is not a repair.
    #[test]
    fn an_unmodelled_key_survives_a_set() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[core]\nlayout = \"flat\"\n\n[future_feature]\nexperimental = true\n",
        )
        .unwrap();

        set_key_in_file(&path, "core.parallel", "4").unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("[future_feature]") && after.contains("experimental = true"),
            "an unmodelled table must survive, got:\n{after}"
        );
    }

    /// No `.bak`. The loss is now structural, so there is nothing to recover,
    /// and a backup accumulated on every key assignment is noise.
    #[test]
    fn a_set_writes_no_backup_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, default_config_toml()).unwrap();

        set_key_in_file(&path, "core.parallel", "2").unwrap();

        let entries: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries,
            vec!["config.toml".to_string()],
            "a set must not leave a backup behind, found: {entries:?}"
        );
    }

    /// A value is parsed as TOML, not stored as a string. `parallel = "4"`
    /// would round-trip through the file and then fail `validate`, which reads
    /// as a broken file rather than as a bad value.
    #[test]
    fn an_integer_key_stays_an_integer() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, default_config_toml()).unwrap();

        set_key_in_file(&path, "core.parallel", "6").unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("parallel = 6"), "got:\n{after}");
        assert!(!after.contains("parallel = \"6\""), "got:\n{after}");
        // And the file still loads, which is the part that would break if the
        // value had landed as a string.
        assert_eq!(load_config(&path).unwrap().core.parallel, 6);
    }

    /// A key in a table the file does not have yet adds the table rather than
    /// failing. `ro config set` is how you configure ro, so it cannot require
    /// the setting to already exist.
    #[test]
    fn a_key_in_a_missing_table_creates_the_table() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[core]\nlayout = \"flat\"\n").unwrap();

        set_key_in_file(&path, "auth.expected_login", "\"quangdang46\"").unwrap();

        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.auth.expected_login.as_deref(), Some("quangdang46"));
    }

    /// Validation runs on the result, and a rejected edit leaves the file
    /// exactly as it was. A config that no longer parses is a worse outcome
    /// than a config with the old value, and the user has no backup to reach
    /// for because we deliberately do not write one.
    #[test]
    fn a_rejected_edit_leaves_the_file_untouched() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = default_config_toml();
        std::fs::write(&path, original).unwrap();

        let err = set_key_in_file(&path, "core.layout", "\"not-a-layout\"").unwrap_err();
        assert!(
            err.to_string().contains("core.layout"),
            "the error must name the key that was rejected, got: {err}"
        );
        assert!(
            err.to_string().contains("not a valid layout"),
            "the error must carry the reason validation gave, got: {err}"
        );

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "a rejected edit must not be written"
        );
    }

    #[test]
    fn a_value_that_is_not_toml_is_rejected_before_writing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = default_config_toml();
        std::fs::write(&path, original).unwrap();

        assert!(set_key_in_file(&path, "core.layout", "{{{").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

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
