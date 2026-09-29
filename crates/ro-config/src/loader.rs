//! Configuration file loader.

use crate::paths::{ConfigPaths, default_config_toml};
use crate::schema::{
    AppConfig, IDENTITY_PROFILE_KEYS, is_modelled_field, known_keys_for, nearest_key,
};
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
    // Loud, not silent. A deprecated table that is merely tolerated is one
    // nobody migrates off, and the point of reading the old form is that the
    // reading stops eventually.
    //
    // This checks the **raw** file rather than typed fields, for two reasons.
    // The tables it names no longer exist in `AppConfig` — that is the cut —
    // and `AppConfig` deliberately has no `deny_unknown_fields`, so a config
    // carrying one loads cleanly and does nothing. Detecting the table in the
    // raw document is the only way to tell the user their setting stopped
    // being read, and it covers every table that was cut, not just the two
    // that had a hand-written note.
    for note in deprecated_tables(&raw) {
        tracing::warn!("{note}");
        eprintln!("warning: {note}");
    }
    Ok(config)
}

/// Tables ro no longer reads, and where each key went.
///
/// Each entry is the table as it appeared in a shipped config, and the
/// replacement the user should migrate to. Detection is by table name in the
/// raw document, so it works for a table `AppConfig` has never heard of — which
/// is the only way it can fire at all, since an unmodelled table is ignored
/// rather than rejected.
pub(crate) const CUT_TABLES: &[(&str, &str)] = &[
    ("review", "its settings moved to [agent] and the preflight defaults"),
    ("providers", "use [agent] engine"),
    ("engines", "use [agent] engine"),
    ("jobs", "the job runner is gone; `ro sync` records a run instead"),
    ("mcp", "the MCP sidecar is gone"),
    ("safety", "the preflight is not configurable; it always blocks"),
    ("git", "the per-command flags are the only git settings"),
    ("checkpoint", "the preflight is not configurable; it always blocks"),
];

pub fn deprecated_tables(raw: &str) -> Vec<String> {
    let doc: toml::Value = match raw.parse() {
        Ok(v) => v,
        // Unparseable is not this function's problem: `load_config` has
        // already reported it with a real message by now.
        Err(_) => return Vec::new(),
    };
    let Some(root) = doc.as_table() else {
        return Vec::new();
    };
    CUT_TABLES
        .iter()
        .filter(|(name, _)| root.contains_key(*name))
        .map(|(name, to)| {
            format!(
                "[{name}] is no longer read — {to}. The file still loads, so the \
                 settings in it are not the ones in effect; run `ro config set` to \
                 write the current form."
            )
        })
        .collect()
}

/// Load config from the canonical XDG path
/// (`$XDG_CONFIG_HOME/ro/config.toml`).
pub fn load_default() -> Result<AppConfig> {
    let paths = ConfigPaths::discover()?;
    load_config(&paths.config_toml())
}

/// What `ro config set` decided about a dotted key.
///
/// Three verdicts, not two, and the middle one is the load-bearing choice.
///
/// # Why an unknown KEY is fatal and an unknown TABLE is not
///
/// `ro config set` is how a config is written without an editor, so its
/// failure mode matters more than usual: a key it accepts and nothing reads
/// leaves the file holding a setting that *looks* live. The user changes it,
/// nothing happens, and the only evidence is the absence of an effect — which
/// is indistinguishable from ro being broken.
///
/// The tempting fix is to reject everything unrecognised. That is wrong, and
/// it breaks a promise ro has already made twice:
///
///   - **The loader tolerates unknown tables.** `AppConfig` has no
///     `deny_unknown_fields` on purpose, so a config written for a newer ro
///     loads. A script that provisions a box for the *next* ro must be able to
///     seed a key this ro has never heard of.
///   - **doctor's `--fix` adds and never removes, and says so.** It writes
///     `left unrecognised table(s) in place: <names>`. A repair command that
///     names a table it declined to touch is making a promise; `config set`
///     deleting that table would break it.
///
/// So: an unknown **table** is written, with a warning naming it — the same
/// thing `--fix` does. An unknown **key inside a table ro reads** is refused,
/// because a table ro reads is a closed set: nothing is gained by a typo there,
/// and forward compatibility for a *new key on an existing table* is a much
/// smaller promise than a new table, and one a hand-edited file still offers.
/// The escape hatch is not "silently accept it" — it is editing the file, which
/// was always available and is now not contradicted by the tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyVerdict {
    /// ro reads this key.
    Known,
    /// A table ro no longer reads. Refused, naming where the setting went.
    Cut {
        table: &'static str,
        migrate_to: &'static str,
    },
    /// A table ro has never heard of. Written anyway, with a warning — a
    /// setting from a newer ro, and not ours to delete.
    UnknownTable,
    /// A key inside a table ro reads that this ro does not have.
    UnknownKey {
        /// The full dotted key, for the message.
        dotted: String,
        /// The keys that are valid under the same prefix.
        valid: Vec<String>,
        /// The nearest valid key, when the edit distance says it was a typo.
        suggestion: Option<String>,
    },
}

/// Decide what to do with a `dotted.key` before anything is written.
///
/// Split out from [`set_key_in_file`] so the decision is testable on its own,
/// and so the rule lives in one place rather than being re-derived at each
/// call site. `repos.<name>.<key>` never reaches here — `main.rs` routes it to
/// the registry, which is a different store with a different key space.
pub fn classify_key(dotted_key: &str) -> KeyVerdict {
    let segments: Vec<&str> = dotted_key.split('.').collect();
    let table = segments[0];

    // A table ro no longer reads. This is the case the docs get wrong:
    // FEATURES.md tells a user to write `checkpoint.secret_scan = "warn"`,
    // and the file would then carry a table `load_config` warns about on the
    // very next run. Refusing with the migration note is the honest answer —
    // the preflight is unconditional, and there is no warn mode to select.
    if let Some((name, to)) = CUT_TABLES.iter().find(|(name, _)| *name == table) {
        return KeyVerdict::Cut {
            table: name,
            migrate_to: to,
        };
    }

    // A table ro has never heard of. Forward compatibility: written, warned
    // about, never deleted. This is the same posture `doctor --fix` takes and
    // reports ("left unrecognised table(s) in place").
    let Some(valid) = known_keys_for(table) else {
        return KeyVerdict::UnknownTable;
    };

    // `[identity]` is a map of user-named profiles, so `identity.work.name` is
    // a real key and `identity.work.emali` is a typo inside a real profile.
    if table == "identity" && segments.len() >= 3 {
        let leaf = segments[segments.len() - 1];
        let profile = format!("{}.<profile>", segments[..2].join("."));
        let valid: Vec<String> = IDENTITY_PROFILE_KEYS
            .iter()
            .map(|k| format!("{profile}.{k}"))
            .collect();
        if IDENTITY_PROFILE_KEYS.contains(&leaf) {
            return KeyVerdict::Known;
        }
        return KeyVerdict::UnknownKey {
            dotted: dotted_key.to_string(),
            suggestion: nearest_key(leaf, IDENTITY_PROFILE_KEYS)
                .map(|k| format!("{profile}.{k}")),
            valid,
        };
    }

    // A table with no leaf: `ro config set core=4`. The value would land where
    // a table is expected and the file would then fail to parse — the one
    // outcome the round-trip check exists to prevent. Refusing earlier, with
    // the keys named, is strictly kinder.
    let Some(leaf) = segments.last().copied() else {
        return KeyVerdict::UnknownKey {
            dotted: dotted_key.to_string(),
            valid: valid.iter().map(|k| format!("{table}.{k}")).collect(),
            suggestion: None,
        };
    };

    if valid.contains(&leaf) {
        return KeyVerdict::Known;
    }

    // The depth is wrong even though the leaf is real: `core.layout.x`.
    if segments.len() > 2 {
        return KeyVerdict::UnknownKey {
            dotted: dotted_key.to_string(),
            valid: valid.iter().map(|k| format!("{table}.{k}")).collect(),
            suggestion: None,
        };
    }

    // A key ro does not read. Back it with the struct, so the message cannot
    // name a key the schema rejects: `identity.default` and
    // `identity.personal` are both structurally `Option<String>`-shaped, and
    // only the first is a real key.
    if !is_modelled_field::<AppConfig>(dotted_key) {
        return KeyVerdict::UnknownKey {
            dotted: dotted_key.to_string(),
            valid: valid.iter().map(|k| format!("{table}.{k}")).collect(),
            suggestion: nearest_key(leaf, valid).map(|k| format!("{table}.{k}")),
        };
    }

    // Structurally real but not a key of this table — a flatten collision.
    KeyVerdict::UnknownKey {
        dotted: dotted_key.to_string(),
        valid: valid.iter().map(|k| format!("{table}.{k}")).collect(),
        suggestion: None,
    }
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

    // The key is checked before the document is touched, so a rejected write
    // has nothing to roll back. The round-trip validation further down is
    // still load-bearing — it is what catches a bad *value* — but it cannot
    // catch a bad *key*, because `AppConfig` ignores what it does not model.
    match classify_key(dotted_key) {
        KeyVerdict::Known => {}
        KeyVerdict::UnknownTable => {
            // Loud, and non-fatal. This is the forward-compatibility case, and
            // it is the same posture `doctor --fix` takes: the table stays,
            // and the user is told which one ro did not recognise. Silently
            // writing it is what makes a future key look like a live one.
            let note = format!(
                "warning: [{table}] is not a table this version of ro reads; \
                 writing {dotted_key} anyway in case it belongs to a newer ro. \
                 `ro doctor` will name it as unrecognised.",
                table = segments[0]
            );
            tracing::warn!("{note}");
            eprintln!("{note}");
        }
        KeyVerdict::Cut { table, migrate_to } => {
            // Refused. The setting would be written into a table the loader
            // warns about on the next run, so the file would contradict
            // itself within one command. This is also the shape FEATURES.md
            // walks a user into with `checkpoint.secret_scan`.
            anyhow::bail!(
                "[{table}] is no longer read — {migrate_to}. \
                 `ro config set {dotted_key}` would write a setting that has no effect, \
                 so it is refused; edit the file by hand if you need it for a different version."
            );
        }
        KeyVerdict::UnknownKey {
            dotted,
            valid,
            suggestion,
        } => {
            let valid = valid.join(", ");
            let mut msg = format!(
                "{dotted} is not a setting ro reads. Valid keys here: {valid}."
            );
            if let Some(s) = suggestion {
                msg.push_str(&format!(" Did you mean {s}?"));
            }
            anyhow::bail!(
                "{msg} Nothing was written. A key that no longer exists and one that \
                 was never read look identical from the outside, so this is an error \
                 rather than a silent no-op."
            );
        }
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
        // The default file must load into defaults that survive a second
        // load, and must not be tripping the cut-table warning.
        let again = load_config(&path).unwrap();
        assert_eq!(cfg.core.parallel, again.core.parallel);
        assert!(deprecated_tables(&std::fs::read_to_string(&path).unwrap()).is_empty(),
            "the shipped default config must not look like a config to migrate");
    }

    /// A config carrying a table ro no longer reads must SAY SO.
    ///
    /// `AppConfig` has no `deny_unknown_fields` on purpose, so a stale
    /// table loads cleanly and does nothing. That is the worst failure a
    /// config cut can have: the user changes a setting, watches nothing
    /// happen, and has no way to tell whether ro is broken or the key is
    /// wrong.
    #[test]
    fn a_cut_table_in_a_users_config_warns_instead_of_going_quiet() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[core]
parallel = 4

[jobs]
enabled = true

[providers.claude]
bin = \"claude\"
",
        )
        .unwrap();
        let notes = deprecated_tables(&std::fs::read_to_string(&path).unwrap());
        assert!(notes.len() >= 2, "both stale tables must be reported, got {notes:?}");
        let joined = notes.join(" ");
        assert!(joined.contains("[jobs]") && joined.contains("[providers]"));
        assert!(joined.contains("ro config set"), "and the note must say what to do");
        // And the file still loads — a warning, not a refusal.
        assert!(load_config(&path).is_ok());
    }

    #[test]
    fn a_current_config_warns_about_nothing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ro/config.toml");
        write_default(&path).unwrap();
        assert!(deprecated_tables(&std::fs::read_to_string(&path).unwrap()).is_empty());
    }

    // ── Key validation ────────────────────────────────────────────────────
    //
    // The bug: `ro config set core.paralel=4` succeeded, wrote `paralel`, and
    // nothing read it. The file then held a setting that looked live and was
    // not, and the only evidence was that nothing happened.

    /// The bug itself. A typo is refused, the valid keys are named, and the
    /// file is untouched.
    #[test]
    fn a_typo_is_refused_with_the_valid_keys_named() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = default_config_toml();
        std::fs::write(&path, original).unwrap();

        let err = set_key_in_file(&path, "core.paralel", "4").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("core.paralel"),
            "the error must name the key that was rejected, got: {msg}"
        );
        assert!(
            msg.contains("parallel"),
            "the error must name the valid keys, got: {msg}"
        );
        assert!(
            msg.contains("Did you mean core.parallel?"),
            "a typo this close should be named, got: {msg}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "a refused key must not be written"
        );
    }

    /// The key the docs name. FEATURES.md's Safety-net section tells a user to
    /// write `checkpoint.secret_scan = "warn"` to downgrade the preflight. The
    /// preflight is unconditional, so that key does not exist — and writing it
    /// would put a table in the file that `load_config` warns about on the
    /// very next run.
    #[test]
    fn the_documented_secret_scan_key_is_refused() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = default_config_toml();
        std::fs::write(&path, original).unwrap();

        let err = set_key_in_file(&path, "checkpoint.secret_scan", "\"warn\"").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("checkpoint"),
            "the error must name the table, got: {msg}"
        );
        assert!(
            msg.contains("no longer read"),
            "the error must say the table is not read, got: {msg}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "a dead key must not be written"
        );
    }

    /// The same shape under the other name the docs and the migration note
    /// use for it.
    #[test]
    fn the_safety_table_is_refused_too() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, default_config_toml()).unwrap();
        assert!(set_key_in_file(&path, "safety.secret_scan", "\"warn\"").is_err());
    }

    /// Forward compatibility. A script provisioning a box for a newer ro must
    /// be able to seed a key this ro has never heard of — and `doctor --fix`
    /// already promises to leave unrecognised tables alone rather than delete
    /// them. So an unknown *table* is written, with a warning, and never
    /// deleted.
    #[test]
    fn an_unknown_table_is_written_with_a_warning_not_deleted() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, default_config_toml()).unwrap();

        set_key_in_file(&path, "future_feature.experimental", "true").unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("[future_feature]") && after.contains("experimental = true"),
            "an unmodelled table must survive, got:\n{after}"
        );
        // And the rest of the file is intact — the point of the warning is
        // that the user can see what was written and decide.
        assert!(after.contains("[core]"), "got:\n{after}");
    }

    /// A key inside a table ro reads is a closed set, so a typo there is
    /// refused even though the table is fine.
    #[test]
    fn a_typo_inside_a_live_table_is_refused() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, default_config_toml()).unwrap();

        let err = set_key_in_file(&path, "github.aut", "\"gh\"").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("github.aut"), "got: {msg}");
        assert!(msg.contains("auth"), "the valid keys must be named, got: {msg}");
        assert!(msg.contains("Did you mean github.auth?"), "got: {msg}");
    }

    /// A key that is structurally real but not a key of this table. The
    /// `identity` table is a `BTreeMap` of user-named profiles, so
    /// `identity.personal` parses cleanly and is not a setting.
    #[test]
    fn a_profile_name_is_not_a_setting() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, default_config_toml()).unwrap();

        let err = set_key_in_file(&path, "identity.personal", "\"work\"").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("identity.personal"), "got: {msg}");
        assert!(msg.contains("identity.default"), "got: {msg}");
    }

    /// The other direction: a real key inside a real profile is accepted, and
    /// two sets into the same profile accumulate rather than replace. The
    /// check must not be so strict that it refuses the settings it exists to
    /// write — and a profile that can only be built by a struct round-trip
    /// would be a profile no one could configure.
    #[test]
    fn a_real_key_inside_a_profile_is_accepted() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, default_config_toml()).unwrap();

        set_key_in_file(&path, "identity.work.name", "\"Dev Work\"").unwrap();
        set_key_in_file(&path, "identity.work.email", "\"dev@corp.com\"").unwrap();

        let cfg = load_config(&path).unwrap();
        let id = cfg.identity.resolve("work").expect("both keys were written");
        assert_eq!(id.name, "Dev Work");
        assert_eq!(id.email, "dev@corp.com");
    }

    /// Half a profile is a well-formed config that names what it is missing,
    /// rather than a file that no longer parses.
    #[test]
    fn a_profile_may_be_built_a_key_at_a_time() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, default_config_toml()).unwrap();

        set_key_in_file(&path, "identity.work.email", "\"dev@corp.com\"").unwrap();

        // The file still loads — this is the point of `name`/`email` being
        // optional. What is refused is the *use* of an incomplete profile.
        let cfg = load_config(&path).expect("an incomplete profile still loads");
        let err = cfg
            .identity
            .resolve("work")
            .expect_err("an incomplete profile must not resolve to an address")
            .to_string();
        assert!(err.contains("name"), "it must say what is missing, got: {err}");
    }

    /// A typo inside a profile is refused, and the message names the profile
    /// keys rather than the top-level ones.
    #[test]
    fn a_typo_inside_a_profile_is_refused() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, default_config_toml()).unwrap();

        let err = set_key_in_file(&path, "identity.work.emali", "\"x@y.z\"").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("identity.work.emali"), "got: {msg}");
        assert!(msg.contains("name"), "the profile keys must be named, got: {msg}");
        assert!(msg.contains("email"), "the profile keys must be named, got: {msg}");
    }

    /// Every key in the shipped default config must be one `ro config set`
    /// will write. A default the tool refuses to set is a setting the user
    /// cannot change from the command line.
    #[test]
    fn every_key_in_the_default_config_is_settable() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, default_config_toml()).unwrap();

        for key in [
            "core.projects_dir",
            "core.layout",
            "core.parallel",
            "core.timeout_secs",
            "github.host",
            "github.auth",
            "agent.engine",
        ] {
            assert_eq!(
                classify_key(key),
                KeyVerdict::Known,
                "{key} is in the shipped default config and must be settable"
            );
        }
    }

    /// The mirror: a key the table declares must be one the structs accept.
    /// This is the property that would have caught a key added to the schema
    /// and forgotten in the table, or vice versa.
    #[test]
    fn classify_key_agrees_with_the_schema_table() {
        for (table, keys) in crate::schema::CONFIG_KEYS {
            for key in *keys {
                let dotted = format!("{table}.{key}");
                assert_eq!(
                    classify_key(&dotted),
                    KeyVerdict::Known,
                    "{dotted} is declared in CONFIG_KEYS and must classify as Known"
                );
            }
        }
    }

    /// A key with no leaf at all. `ro config set core=4` would put a value
    /// where a table is expected and the file would then fail to parse.
    #[test]
    fn a_bare_table_name_is_refused() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, default_config_toml()).unwrap();

        let err = set_key_in_file(&path, "core", "4").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("core"), "got: {msg}");
        assert!(msg.contains("layout"), "the valid keys must be named, got: {msg}");
    }

    /// A key too deep to be real. `core.layout.x` parses, and nothing reads
    /// it.
    #[test]
    fn a_key_too_deep_to_be_real_is_refused() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, default_config_toml()).unwrap();

        let err = set_key_in_file(&path, "core.layout.x", "1").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("core.layout.x"), "got: {msg}");
        assert!(msg.contains("core.layout"), "got: {msg}");
    }
}
