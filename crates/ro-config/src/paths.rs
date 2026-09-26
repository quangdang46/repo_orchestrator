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
# Edit by hand, or run `ro config edit`.
# See `ro config show --json` for the resolved configuration.

[core]
projects_dir = "~/projects"
layout = "flat"            # flat | nested
parallel = 8
timeout_secs = 30

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
[git]
update_strategy = "ff-only" # ff-only | rebase | merge
autostash = false
terminal_prompt = false

[jobs]
enabled = true
max_attempts = 3
retry_backoff = "exponential" # fixed | exponential
default_timeout_secs = 1800

[mcp]
enabled = true
stdio = true
sse = false
sse_port = 7300

[review]
provider = "claude"        # claude | codex
quality_gates = "auto"     # auto | on | off

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
# engine = "claude"
# command = 'codex exec "{prompt}"'   # optional: a different binary entirely
# prompt  = "..."                     # optional: a different instruction
#
# [providers.claude] and [providers.codex] are the old form. They are still
# read, with a deprecation warning, and stop being read in a future release.

[safety]
secret_scan = "block"      # off | warn | block
require_plan_for_ai_apply = true
max_auto_apply_risk = "low" # low | medium | high
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn expand_tilde_with_home() {
        let expanded = expand_tilde("~/projects");
        assert!(expanded.to_string_lossy().ends_with("projects"));
        assert!(!expanded.to_string_lossy().starts_with("~"));
    }

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

    #[test]
    fn default_config_parses_as_toml() {
        let raw = default_config_toml();
        let parsed: toml::Value = toml::from_str(raw).expect("default config parses");
        assert!(parsed.get("core").is_some());
        assert!(parsed.get("github").is_some());
        assert!(parsed.get("git").is_some());
        assert!(parsed.get("jobs").is_some());
        assert!(parsed.get("mcp").is_some());
        assert!(parsed.get("review").is_some());
        assert!(parsed.get("agent").is_some());
        assert!(parsed.get("auth").is_some());
        assert!(parsed.get("safety").is_some());
        // The default template must NOT ship the deprecated table. If it did,
        // every new user would open their own config and be told they were
        // using something deprecated on day zero.
        assert!(
            parsed.get("providers").is_none(),
            "the default config must not ship the deprecated [providers] table"
        );
    }
}
