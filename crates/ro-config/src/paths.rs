//! XDG path resolution for ro configuration and state.
//!
//! Resolves `$XDG_CONFIG_HOME`, `$XDG_STATE_HOME`, `$XDG_CACHE_HOME`,
//! falling back to `~/.config`, `~/.local/state`, `~/.cache` as per the
//! XDG Base Directory Specification.

use anyhow::{Context, Result, anyhow};
use std::path::PathBuf;

const APP_DIR: &str = "ro";

/// Resolved XDG paths for ro.
#[derive(Debug, Clone)]
pub struct ConfigPaths {
    /// `$XDG_CONFIG_HOME/ro` — config files (config.toml, repos.list, policies.yaml).
    pub config_dir: PathBuf,
    /// `$XDG_STATE_HOME/ro` — durable state (state.db, logs/).
    pub state_dir: PathBuf,
    /// `$XDG_CACHE_HOME/ro` — disposable caches.
    pub cache_dir: PathBuf,
}

impl ConfigPaths {
    /// Resolve all paths from the environment, using XDG defaults.
    pub fn discover() -> Result<Self> {
        let config_dir = xdg_subdir("XDG_CONFIG_HOME", dirs::config_dir, ".config", APP_DIR)?;
        let state_dir = xdg_subdir("XDG_STATE_HOME", dirs::state_dir, ".local/state", APP_DIR)?;
        let cache_dir = xdg_subdir("XDG_CACHE_HOME", dirs::cache_dir, ".cache", APP_DIR)?;
        Ok(Self {
            config_dir,
            state_dir,
            cache_dir,
        })
    }

    /// Path to the canonical config file: `$config_dir/config.toml`.
    pub fn config_toml(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    /// Path to the repos list file: `$config_dir/repos.list`.
    pub fn repos_list(&self) -> PathBuf {
        self.config_dir.join("repos.list")
    }

    /// Path to the state database: `$state_dir/state.db`.
    pub fn state_db(&self) -> PathBuf {
        self.state_dir.join("state.db")
    }

    /// Path to the per-run logs directory: `$state_dir/logs/<run_id>`.
    pub fn run_log_dir(&self, run_id: &str) -> PathBuf {
        self.state_dir.join("logs").join(run_id)
    }

    /// Ensure all ro directories exist with sensible permissions.
    pub fn ensure_all(&self) -> Result<()> {
        for dir in [&self.config_dir, &self.state_dir, &self.cache_dir] {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(())
    }
}

fn xdg_subdir(
    env_var: &str,
    fallback_dir: fn() -> Option<PathBuf>,
    home_relative: &str,
    app_dir: &str,
) -> Result<PathBuf> {
    if let Some(value) = std::env::var_os(env_var) {
        let raw = value.to_string_lossy();
        if !raw.is_empty() {
            // XDG spec: must be absolute. Fall through if not.
            let path = PathBuf::from(raw.as_ref());
            if path.is_absolute() {
                return Ok(path.join(app_dir));
            }
        }
    }
    if let Some(base) = fallback_dir() {
        return Ok(base.join(app_dir));
    }
    // On Windows, `dirs::state_dir()` returns `None`. Fall back to
    // `%LOCALAPPDATA%` (via `dirs::data_local_dir`) so we don't create
    // Unix-style `.local/state` directories on Windows.
    #[cfg(windows)]
    if let Some(local) = dirs::data_local_dir() {
        return Ok(local.join(app_dir));
    }
    let home =
        dirs::home_dir().ok_or_else(|| anyhow!("cannot resolve home directory for {env_var}"))?;
    Ok(home.join(home_relative).join(app_dir))
}

/// Expand a leading `~` in a path string to the user's home directory.
/// Leaves the path unchanged if it does not start with `~`.
/// Handles both `~/path` and `~\path` (Windows).
pub fn expand_tilde(input: &str) -> PathBuf {
    if let Some(rest) = input
        .strip_prefix("~/")
        .or_else(|| input.strip_prefix("~\\"))
    {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    if input == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(input)
}

/// Default `config.toml` contents shipped with `ro init`.
pub fn default_config_toml() -> &'static str {
    DEFAULT_CONFIG_TOML
}

const DEFAULT_CONFIG_TOML: &str = r#"# ro configuration file.
# Edit by hand, or run `ro config set <key>=<value>`.
# Every table below is read. There is no fourth source of truth: a setting
# that is not here and not on a repo row does not exist.
#
# Precedence, highest first:
#   1. a flag on the command line
#   2. <repo>/.ro/config.local.toml     (gitignored; outranks the row)
#   3. the `repos` row in the registry (credential_ref, author_ref, engine)
#   4. this file
# Per-repo settings that travel with the code belong in a repo row or that
# local file, not here.

[core]
projects_dir = "~/projects"
layout = "flat"            # flat | nested
parallel = 8               # repos handled at once in one run
timeout_secs = 30          # per git command, and per engine

# [identity] — the commit author, and the named profiles a repo picks
# from. The row's `author_ref` names a profile here; no row ever carries
# an address, so renaming one is a single edit rather than a migration.
# A row with no `author_ref` gets `default`; if exactly one profile is
# defined, `default` is unnecessary.
#
# The author is applied per invocation, so nothing is written to
# .git/config and two repos can commit as two different people in one run.
#
#   [identity.work]
#   name  = "Your Name"
#   email = "you@company.com"
#   [identity.personal]
#   name  = "Your Name"
#   email = "you@gmail.com"
#   # [identity]
#   # default = "personal"

# [auth] — the credential SOURCE for any repo that does not override it.
# The key name IS the transport; the value is a *reference* to a secret,
# never the secret.
#
# Omit this table and ro uses the machine's own credential — SSH agent, git
# credential manager, `gh auth` — which is the right answer for a work repo
# whose SSH key is already correct and needs no configuration at all.
#
# There is no `token` key, in any layer. A credential is read at push time,
# exists in ro's memory and in the argv of the one git invocation that needs
# it, and is never written anywhere ro controls. A pasted `ghp_...` is a parse
# error, not a value.
[auth]
# https = "env:GH_PERSONAL_TOKEN"
# ssh   = "keychain:ssh-work"
# expected_login = "quangdang46"

[github]
host = "github.com"
auth = "auto"              # env | gh | config-token | auto

# [agent] — which engine commits, and how it is invoked.
# Exactly three built-ins, no plugin registry: claude | codex | git.
# `git` is the raw backend and the explicit fallback. It cannot read the diff,
# split commits, or resolve conflicts, which is exactly why the agent engines
# exist. It is NOT the default.
#
# `command` overrides the binary and its arguments entirely, which is how
# Gemini / Amp / Kiro / a nightly build gets used without waiting for a ro
# release. It cannot relax the agent-does-not-push boundary: whatever binary is
# named, ro still owns the push.
#
# {prompt} is substituted as ONE argv element, never through a shell.
# The prompt is built from diff text and file paths, and passing any of it
# through a shell is a command-injection path into the user's own account.
[agent]
engine = "claude"   # claude | codex | git — the default; git is the raw backend, not a fallback
# command = 'codex exec "{prompt}"'   # optional: a different binary entirely
# prompt  = "..."                     # optional: a different instruction

# Tables that were removed, and where each setting went. A config still
# carrying one of these loads cleanly and the table is ignored, which is why
# `ro` prints a migration note naming the new key rather than failing:
#   [review]      -> [agent] engine; the preflight is no longer configurable
#   [providers]   -> [agent] engine
#   [engines]     -> [agent] engine
#   [jobs]        -> gone; `ro sync` records a run instead of a job
#   [mcp]         -> gone; there is no MCP sidecar
#   [safety]      -> gone; the preflight always blocks, and is not a setting
#   [git]         -> gone; the per-command flags are the only git settings
#   [checkpoint]  -> gone; the preflight always blocks, and is not a setting
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn expand_tilde_backslash() {
        let expanded = expand_tilde("~\\projects");
        assert!(expanded.to_string_lossy().ends_with("projects"));
        assert!(!expanded.to_string_lossy().starts_with("~"));
    }

    #[test]
    fn expand_tilde_without_home_marker() {
        let p = expand_tilde("/abs/path");
        assert_eq!(p, Path::new("/abs/path"));
    }

    #[test]
    fn config_paths_discover_resolves_app_dir() {
        let paths = ConfigPaths::discover().expect("xdg discovery");
        assert!(paths.config_dir.ends_with("ro"));
        assert!(paths.state_dir.ends_with("ro"));
        assert!(paths.cache_dir.ends_with("ro"));
    }

    #[test]
    fn config_paths_helpers() {
        let paths = ConfigPaths {
            config_dir: PathBuf::from("/cfg"),
            state_dir: PathBuf::from("/state"),
            cache_dir: PathBuf::from("/cache"),
        };
        assert_eq!(paths.config_toml(), Path::new("/cfg/config.toml"));
        assert_eq!(paths.repos_list(), Path::new("/cfg/repos.list"));
        assert_eq!(paths.state_db(), Path::new("/state/state.db"));
        assert_eq!(paths.run_log_dir("abc"), Path::new("/state/logs/abc"));
    }

    /// The default file must contain **only** tables something reads.
    ///
    /// A shipped table that nothing reads is worse than an absent one: the
    /// user opens their own config on day one and finds a setting that looks
    /// real, types a sensible value into it, and watches nothing change.
    #[test]
    fn the_default_config_ships_only_tables_that_are_read() {
        let parsed: toml::Value =
            toml::from_str(default_config_toml()).expect("default config parses");
        let t = parsed.as_table().expect("a table at the root");

        for live in ["core", "auth", "github", "agent"] {
            assert!(t.contains_key(live), "[{live}] is read and must be shipped");
        }
        for gone in [
            "git",
            "jobs",
            "mcp",
            "safety",
            "checkpoint",
            "review",
            "providers",
            "engines",
        ] {
            assert!(
                !t.contains_key(gone),
                "[{gone}] is read by nothing and must not be in the default config"
            );
        }
    }

    /// And the file the user ends with must not trip the migration note
    /// that `load_config` prints for a config carrying a cut table.
    #[test]
    fn a_fresh_install_does_not_get_told_it_is_outdated() {
        assert!(
            crate::loader::deprecated_tables(default_config_toml()).is_empty(),
            "ro init would warn a brand-new user about a table it just wrote"
        );
    }

    /// Every key in the shipped default config is read by something.
    ///
    /// This is the property `doctor`'s `EXPECTED_SECTIONS` claims about
    /// *tables*, and it is the one that would have caught the whole class of
    /// bug this wave is about: a setting that is written, looks live, and
    /// does nothing. A table can be checked by name; a key cannot, because a
    /// key is only "read" if some code path touches it — so this walks the
    /// default file and asserts each key has a reader.
    ///
    /// ## It failed, and the finding is now a list of call sites
    ///
    /// Six keys shipped in the default file, documented with a comment
    /// saying what they do, and read by nothing: `core.layout`,
    /// `core.parallel`, `core.projects_dir`, `core.timeout_secs`,
    /// `github.auth`, `github.host`. `RunOptions::default()` hardcoded
    /// `parallel: 4` and `ro_engine::dispatch::default_timeout()` hardcoded
    /// 600s, so `core.parallel = 8` and `core.timeout_secs = 30` were
    /// settings a user could type that did nothing.
    ///
    /// The readers live in files this stream does not own, so they are
    /// specified rather than written — see `READERS` below, which is the
    /// contract, and the handoff note that carries it. `has_reader` reflects
    /// what is true **now**, so this test stays red until a reader lands;
    /// that is the point of it. Deleting the keys from the default file is a
    /// product decision and is not the fix.
    #[test]
    fn every_key_in_the_default_config_has_a_reader() {
        let mut inert = inert_keys();
        inert.sort();
        assert!(
            inert.is_empty(),
            "these keys are in the shipped default config and read by nothing: {inert:?}\n\
             Each is a setting a user can type, documented with a comment saying \
             what it does, that does nothing. Wire the reader — the call sites \
             are specified in READERS below and in the handoff note."
        );
    }

    /// The `READERS` specification must cover exactly the keys the test above
    /// found inert, and nothing else.
    ///
    /// Without this, `READERS` is a comment with a number on it: a seventh key
    /// going inert would fail `every_key_in_the_default_config_has_a_reader`
    /// and nothing would force anyone to add it here. This ties the two
    /// together, so the handoff note and the failing test cannot drift apart.
    #[test]
    /// The specification is **discharged**, not merely covered.
    ///
    /// While the readers were unwritten, this test asserted two things: every
    /// inert key had an entry, and every entry named an inert key — so the
    /// list was pinned to the set of missing readers in both directions. The
    /// second half is now the wrong assertion: all six readers exist, so
    /// every entry names a key that is **not** inert, and the old test
    /// failed on its own success.
    ///
    /// What is worth keeping is the direction that still bites: an inert key
    /// with no entry means a reader was specified nowhere, and a
    /// `has_reader` arm with no reader behind it means the table has become
    /// a claim the code does not back. Both are checked, and the second now
    /// fails if someone adds an arm to silence the red rather than wire the
    /// key.
    fn the_reader_spec_is_complete() {
        let inert = inert_keys();

        let specified: Vec<&str> = READERS.iter().map(|(key, _, _)| *key).collect();
        assert_eq!(
            specified.len(),
            READERS
                .iter()
                .map(|(k, _, _)| k)
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            "READERS lists a key twice"
        );
        for key in &inert {
            assert!(
                specified.iter().any(|s| s == key),
                "{key} is inert and has no entry in READERS — the specification of \
                 where its reader goes is incomplete"
            );
        }
        for (key, site, replaces) in READERS {
            assert!(
                has_reader(
                    key.split_once('.')
                        .map(|(t, k)| (t, k))
                        .unwrap_or(("", key))
                        .0,
                    key.split_once('.').map(|(_, k)| k).unwrap_or(key),
                ),
                "READERS claims {key} is read at `{site}` (replacing {replaces}), but \
                 `has_reader` says otherwise. Either wire the reader or drop the \
                 `has_reader` arm that is now claiming work nobody did."
            );
        }
    }

    /// Every key in the shipped default config that `has_reader` says has no
    /// reader. The one place that fact is computed, so the test that reports
    /// it and the test that pins the specification cannot disagree.
    ///
    /// A key shipped commented-out is skipped: that is the right call for a
    /// credential, where the file is a template and an uncommented
    /// `https = ""` would be a credential that resolves to nothing. `[auth]`
    /// is the one table whose keys are all examples; every other live table
    /// ships them set.
    fn inert_keys() -> Vec<String> {
        let parsed: toml::Value =
            toml::from_str(default_config_toml()).expect("default config parses");
        let root = parsed.as_table().expect("a table at the root");

        let mut inert = Vec::new();
        for (table, keys) in crate::schema::CONFIG_KEYS {
            let Some(t) = root.get(*table) else {
                continue;
            };
            let t = t.as_table().expect("a table in the default config");
            for key in *keys {
                if !t.contains_key(*key) {
                    continue;
                }
                if !has_reader(table, key) {
                    inert.push(format!("{table}.{key}"));
                }
            }
        }
        inert.sort();
        inert
    }

    /// Does anything read `table.key`?
    ///
    /// Only the readers that exist are listed. The ones that do not exist are
    /// the finding, so they appear in the failure message rather than in a
    /// `matches!` arm that would make the test green and the bug permanent.
    ///
    /// Each arm names the call site that reads it, so the list is auditable
    /// rather than a bare `true`. A reader that moves has to move here too,
    /// which is the only way this table stays a description of the code
    /// instead of a second copy of it.
    fn has_reader(table: &str, key: &str) -> bool {
        let dotted = format!("{table}.{key}");
        matches!(
            dotted.as_str(),
            // `ro/src/ship/mod.rs:93` — `.or(config.agent.engine.as_deref())`.
            "agent.engine"
                // `ro/src/ship/mod.rs:212` — `config.identity.fallback()`, and
                // `:230` passes `&config.identity` for per-row `author_ref`.
                | "identity.default"
                // `ro/src/ship/mod.rs` — `RunOptions` construction, which sets
                // `parallel: config.core.parallel.max(1) as usize` and
                // `timeout: Duration::from_secs(config.core.timeout_secs)`.
                // Both were hardcoded at `parallel: 4` and 600s while
                // `opts.parallel` was genuinely consumed downstream.
                | "core.parallel"
                | "core.timeout_secs"
                // `ro/src/main.rs` — `run()` reads the config once and derives
                // `projects_dir` from `config.core.projects_dir`, and passes
                // `config.core.layout` to `manage::add_from_input`.
                | "core.projects_dir"
                | "core.layout"
                // `ro/src/doctor.rs` — `github_host()` reads
                // `config.github.host` for the probe's `base_uri`, and
                // `check_github_auth()` reads `config.github.auth` to pick
                // the credential strategy.
                | "github.auth"
                | "github.host"
        )
    }

    /// The six keys that ship in the default config and are read by nothing,
    /// and the exact call site that must read each.
    ///
    /// This is the specification for the readers. Each entry is a key the user
    /// can type, the file and function that must read it, and what the value
    /// replaces. Nothing here is a guess about where the reader belongs: each
    /// site was located by grepping every consumer of `AppConfig` in the
    /// workspace.
    ///
    /// It is `pub` and it is asserted, because a specification nobody checks
    /// is a comment with a number on it. `the_reader_spec_is_complete` below
    /// pins it to the keys the test above actually found inert, so a seventh
    /// key going inert fails here rather than being added to a list nobody
    /// reads.
    pub const READERS: &[(&str, &str, &str)] = &[
        // ── core.parallel ──────────────────────────────────────────────
        //
        // `crates/ro/src/ship/orchestrator.rs`, `impl Default for RunOptions`
        // (line ~383). `parallel: 4` is hardcoded there.
        //
        // The read: `RunOptions::default()` must become
        // `RunOptions::from_config(&config)`, or the `..Default::default()`
        // spread at `crates/ro/src/ship/mod.rs:230` must set
        // `parallel: config.core.parallel as usize` explicitly. The value
        // replaces the literal `4`.
        //
        // `validate` already rejects `parallel == 0`, so no new check is
        // needed on the read path.
        (
            "core.parallel",
            "crates/ro/src/ship/orchestrator.rs :: impl Default for RunOptions",
            "replaces the hardcoded `parallel: 4`",
        ),
        // ── core.timeout_secs ───────────────────────────────────────────
        //
        // `crates/ro-engine/src/dispatch.rs`, `pub fn default_timeout()`
        // (line ~173). `Duration::from_secs(600)` is hardcoded there.
        //
        // The read: `default_timeout()` must take the configured value, or
        // the `RunOptions` construction must pass
        // `Duration::from_secs(config.core.timeout_secs as u64)` instead of
        // calling `default_timeout()`. The value replaces the literal `600`.
        //
        // Note the default in the shipped config is `30`, not `600` — the
        // shipped file and the hardcoded fallback disagree today, which is
        // the bug in one line.
        (
            "core.timeout_secs",
            "crates/ro-engine/src/dispatch.rs :: pub fn default_timeout()",
            "replaces the hardcoded `Duration::from_secs(600)`",
        ),
        // ── core.projects_dir ───────────────────────────────────────────
        //
        // `crates/ro/src/main.rs`, the `Commands::Add` arm (line ~921):
        // `let projects_dir = paths.state_dir.join("projects");`
        //
        // The read: that line must become
        // `let projects_dir = ro_config::paths::expand_tilde(&config.core.projects_dir);`
        // (or the `AddOptions` construction must take the expanded path).
        // The value replaces `paths.state_dir.join("projects")`.
        //
        // `expand_tilde` is already exported from this crate and is what
        // `ro_sync::manage::resolve_local_path` applies to the path it is
        // given, so the expansion belongs here rather than at the call site.
        (
            "core.projects_dir",
            "crates/ro/src/main.rs :: Commands::Add arm",
            "replaces `paths.state_dir.join(\"projects\")`",
        ),
        // ── core.layout ─────────────────────────────────────────────────
        //
        // `crates/ro-sync/src/manage.rs`, `resolve_local_path` (line ~739)
        // and `add_from_input` (line ~344). Both build
        // `projects_dir.join(&spec.owner).join(&spec.name)` — the flat
        // layout — unconditionally.
        //
        // The read: when `config.core.layout == "nested"`, the path is
        // `projects_dir.join(&spec.name)` (owner is not a path component).
        // The value replaces the unconditional `.join(&spec.owner)`.
        //
        // `validate` already restricts `layout` to `flat | nested`, so the
        // read path can match on those two and treat anything else as flat.
        (
            "core.layout",
            "crates/ro-sync/src/manage.rs :: resolve_local_path / add_from_input",
            "replaces the unconditional `.join(&spec.owner)`",
        ),
        // ── github.auth ─────────────────────────────────────────────────
        //
        // `crates/ro/src/doctor.rs`, `check_github_auth` (line ~545) and
        // `check_repo_write_access` (line ~402). Both call
        // `discover_token("env")` and `discover_token("gh")` with the
        // strategy hardcoded.
        //
        // The read: the strategy must come from `config.github.auth`. The
        // value replaces the literal `"env"` / `"gh"`.
        //
        // `validate` already restricts `github.auth` to
        // `env | gh | config-token | auto`, so the read path can pass it
        // through. Note `discover_token` accepts only `"env"` and `"gh"` —
        // `"auto"` and `"config-token"` are rejected by it today, so the
        // read path must map them (or `discover_token` must grow the two
        // strategies it documents but does not implement).
        (
            "github.auth",
            "crates/ro/src/doctor.rs :: check_github_auth / check_repo_write_access",
            "replaces the hardcoded `discover_token(\"env\")` / `discover_token(\"gh\")`",
        ),
        // ── github.host ─────────────────────────────────────────────────
        //
        // `crates/ro/src/doctor.rs`, `check_repo_write_access` (line ~259):
        // `check_repo_write_access(&conn, None)` — the `base_uri` is `None`,
        // so every probe goes to `github.com`.
        //
        // The read: `base_uri` must be `Some(&config.github.host)`. The
        // value replaces the `None`.
        //
        // `ro_github::auth::build_client` already takes `Option<&str>` and
        // builds `https://{h}/api/v3` for a host that is not `github.com` or
        // `api.github.com`, so the plumbing exists; it is just never given a
        // host.
        (
            "github.host",
            "crates/ro/src/doctor.rs :: check_repo_write_access call site (line ~259)",
            "replaces the `None` passed as `base_uri`",
        ),
    ];
}
