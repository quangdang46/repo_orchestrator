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
    pub agent: AgentConfig,
    #[serde(default)]
    pub engines: EnginesConfig,
    #[serde(default)]
    pub checkpoint: CheckpointConfig,
    #[serde(default)]
    pub safety: SafetyConfig,
}

/// `[checkpoint]` — what runs before a WIP commit is written.
///
/// This table exists because the preflight used to have **no**
/// configuration at all: it hardcoded `SecretScanMode::Warn`, which makes
/// `should_block` permanently false, so the `BlockedBySecrets` variant was
/// unreachable and a repo containing a live-looking credential was committed
/// anyway. A safety net nobody can switch is not a safety net.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointConfig {
    /// `off` | `warn` | `block`.
    ///
    /// `block` is the default. The preflight is the last thing standing
    /// between a WIP commit and a leaked credential, and a default that
    /// permits the leak is not a default anyone would choose knowingly.
    #[serde(default = "default_secret_scan")]
    pub secret_scan: String,
    /// `off` | `on`.
    ///
    /// **Off by default, and the reason is the cost.** `run_all` invokes
    /// `cargo test --workspace` over the *whole* tree, so one
    /// pre-existing failure in an untouched crate blocks a one-file commit
    /// — and across a twenty-repo fleet that check is the dominant cost of
    /// the run. A gate that fires on things the user did not touch trains
    /// people to reach for the override, which disables the gates that do
    /// matter.
    #[serde(default = "default_quality_gates_off")]
    pub quality_gates: String,
}

/// The gates do not run unless asked.
///
/// The old default was `"auto"`, a third value that meant "on, except when
/// it looks expensive" — decided at runtime by code that then ran the same
/// `cargo test --workspace` either way. Off is a value the user can reason
/// about; "auto" was one they had to look up.
fn default_quality_gates_off() -> String {
    "off".to_string()
}

impl AppConfig {
    /// A deprecation note, or `None` when the config is already current.
    ///
    /// `AppConfig` deliberately has no `deny_unknown_fields` — a config
    /// carrying a key from a newer ro must still load — so a renamed
    /// table is otherwise read as "your setting quietly does nothing",
    /// which is the worst failure a config rename can have. Saying so is
    /// the difference between a rename and a silent data loss.
    pub fn review_deprecation_note(&self) -> Option<String> {
        let mut moved: Vec<&str> = Vec::new();
        if self.review.quality_gates.is_some() {
            moved.push("checkpoint.quality_gates");
        }
        if self.review.provider.is_some() {
            moved.push("agent.engine");
        }
        if moved.is_empty() {
            return None;
        }
        Some(format!(
            "[review] is no longer read. Its settings moved: {}. \
             A config still carrying [review] loads, and the old keys are \
             ignored, so the setting you wrote is not the one in effect.",
            moved.join(", ")
        ))
    }
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            secret_scan: default_secret_scan(),
            quality_gates: default_quality_gates_off(),
        }
    }
}

impl CheckpointConfig {
    /// The secret-scan mode, as a string.
    ///
    /// Returns the string rather than a parsed enum because the enum
    /// lives in `ro-sweep` and `ro-config` must not depend on it —
    /// `ro-sweep` already depends on `ro-config`, so parsing there is
    /// the only layering that works. `validate` rejects bad values at
    /// load time, so an unrecognised value here means validation was
    /// skipped; `block` is the safe answer in that case, because letting
    /// a credential through is the outcome nobody can undo.
    pub fn secret_scan_mode(&self) -> &str {
        match self.secret_scan.as_str() {
            "off" => "off",
            "warn" => "warn",
            _ => "block",
        }
    }

    /// Should the quality gates run at all?
    ///
    /// Off by default, for the cost reason in the field docs. Off means
    /// *not run*, not *run and pass* — a skipped gate must not be
    /// recorded as a passed one.
    pub fn quality_gates_enabled(&self) -> bool {
        self.quality_gates == "on"
    }
}

impl AppConfig {
    /// Which engine this configuration selects, honouring the old table.
    ///
    /// The new key wins when both are present. A user who has already written
    /// `[agent] engine` has expressed the current intent, and letting a stale
    /// `[providers.claude]` override it would make the new key impossible to
    /// set.
    pub fn resolved_engine(&self) -> Option<&str> {
        self.agent
            .engine
            .as_deref()
            .or_else(|| legacy_provider_and_engine(&self.providers).map(|(_, e)| e))
    }

    /// A deprecation note, or `None` when the config is already current.
    ///
    /// Loud, because the alternative is the failure mode this exists to
    /// prevent: an unknown table is ignored, the config parses cleanly, the
    /// setting has zero effect, and nothing says so. The user finds out by
    /// watching their commits not happen.
    pub fn engine_deprecation_note(&self) -> Option<String> {
        // The new key wins, so a config that has both is not deprecated.
        if self.agent.engine.is_some() {
            return None;
        }
        let (provider, value) = legacy_provider_and_engine(&self.providers)?;
        Some(format!(
            "[providers.{provider}] is deprecated and is being read as [agent] engine. \
             Run `ro config set agent.engine {value}` to write the new form; the old key \
             will stop being read in a future release."
        ))
    }
}

/// Which engine `[providers.*]` selected, and which key named it.
///
/// The old table had one entry per provider rather than one engine setting, so
/// "which engine is this" was inferred from which entry had a binary in it.
/// That inference is the whole back-compat surface, and naming the key that
/// triggered it is what makes the deprecation note actionable.
fn legacy_provider_and_engine(providers: &ProvidersConfig) -> Option<(&'static str, &'static str)> {
    let set = |p: &ProviderConfig| !p.bin.is_empty() || !p.default_args.is_empty();
    if set(&providers.claude) {
        Some(("claude", "claude"))
    } else if set(&providers.codex) {
        Some(("codex", "codex"))
    } else {
        None
    }
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

/// `[review]` — removed; the settings moved to `[checkpoint]`.
///
/// Kept only to recognise it. `AppConfig` has no `deny_unknown_fields`, so
/// a config still carrying `[review]` parses cleanly and the table is
/// ignored — which is exactly the silent-failure mode that makes a rename
/// a data-loss event. Reading it here turns "your setting does nothing" into
/// a message naming the key to write instead.
///
/// The type is otherwise unused: nothing reads `provider` or
/// `quality_gates` from here any more.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReviewConfig {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub quality_gates: Option<String>,
}

/// One engine's binary and arguments.
///
/// `deny_unknown_fields` on **each** slot, and on the table. That is the
/// whole difference between a config that helps and one that costs an
/// afternoon: `[engines.cladue] bin = "claude"` in a free-form table would
/// parse cleanly, register a phantom engine nobody asked for, and fail at
/// dispatch. Here it is a parse error naming the key.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EngineSlot {
    /// The binary to spawn. Empty means "take the shipped value for this
    /// slot" — a slot cannot know its own field name, so `Default` must
    /// not guess, or `[engine_claude]` would default to `git`.
    #[serde(default)]
    pub bin: String,
    /// Arguments before the prompt.
    #[serde(default)]
    pub default_args: Vec<String>,
}

/// The three slots. Fixed, not a free-form table — see [`EngineSlot`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnginesConfig {
    #[serde(default)]
    pub engine_claude: EngineSlot,
    #[serde(default)]
    pub engine_codex: EngineSlot,
    #[serde(default)]
    pub engine_git: EngineSlot,
}

impl EnginesConfig {
    /// The shipped defaults, which each slot cannot know for itself: the
    /// slot named `engine_claude` defaults to `claude` by the virtue of
    /// its name, and `Default for EngineSlot` cannot read its own field
    /// name.
    pub fn shipped() -> Self {
        Self {
            engine_claude: EngineSlot {
                bin: "claude".into(),
                default_args: vec!["-p".into(), "--output-format".into(), "stream-json".into()],
            },
            engine_codex: EngineSlot {
                bin: "codex".into(),
                default_args: vec!["exec".into()],
            },
            engine_git: EngineSlot {
                bin: "git".into(),
                default_args: vec![],
            },
        }
    }

    /// The configured value for a slot, falling back to the shipped one.
    ///
    /// The fallback is per **field**, not per slot: a user who set only
    /// `default_args` still gets the right binary.
    pub fn resolve_slot(&self, kind: &str) -> Option<EngineSlot> {
        let shipped = Self::shipped();
        let (configured, fallback) = match kind {
            "claude" => (&self.engine_claude, shipped.engine_claude),
            "codex" => (&self.engine_codex, shipped.engine_codex),
            "git" => (&self.engine_git, shipped.engine_git),
            _ => return None,
        };
        Some(EngineSlot {
            bin: if configured.bin.trim().is_empty() {
                fallback.bin
            } else {
                configured.bin.clone()
            },
            default_args: if configured.default_args.is_empty() {
                fallback.default_args
            } else {
                configured.default_args.clone()
            },
        })
    }

    /// The configured value for a kind, or `None` for a name that is not
    /// one of the three.
    pub fn slot(&self, kind: &str) -> Option<&EngineSlot> {
        match kind {
            "claude" => Some(&self.engine_claude),
            "codex" => Some(&self.engine_codex),
            "git" => Some(&self.engine_git),
            _ => None,
        }
    }
}

/// `[agent]` — which engine commits, and how it is invoked.
///
/// Replaces `[providers.claude]` / `[providers.codex]`. The old table is still
/// read, and see [`AppConfig::engine_deprecation_note`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentConfig {
    /// `claude` | `codex` | `git` — exactly three built-ins, no plugin
    /// registry. `git` is the raw backend and the explicit fallback; it cannot
    /// read a diff or split commits, which is exactly why the agent engines
    /// exist, and it is not the default.
    #[serde(default)]
    pub engine: Option<String>,
    /// Overrides the binary and its arguments entirely, which is how Gemini /
    /// Amp / Kiro / a nightly gets used without waiting for a ro release.
    ///
    /// This cannot relax the agent-does-not-push boundary: whatever binary is
    /// named, ro still owns the push.
    #[serde(default)]
    pub command: Option<String>,
    /// A different instruction. `{prompt}` is substituted as ONE argv
    /// element, never through a shell — the prompt is built from diff text and
    /// file paths, and passing any of it through a shell is a
    /// command-injection path into the user's own account.
    #[serde(default)]
    pub prompt: Option<String>,
}

/// `[providers]` — the pre-`[agent]` table. Read for back-compat only.
///
/// It carries no weight of its own. Nothing reads it except the deprecation
/// path, so a config carrying it is understood rather than ignored, which is
/// the entire difference between a rename and a silent data loss.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A shipped table that is renamed is a silent data loss, not a rename.
    /// Unknown tables are ignored, `AppConfig` has no `deny_unknown_fields`,
    /// so the config parses cleanly, the setting has zero effect, and the user
    /// finds out by watching their commits not happen. This is the whole
    /// reason the old key is still read.
    #[test]
    fn a_legacy_providers_table_still_resolves_the_engine() {
        let cfg: AppConfig = toml::from_str(
            r#"[providers.claude]
bin = "claude"
default_args = ["-p", "--output-format", "stream-json"]
"#,
        )
        .unwrap();
        assert_eq!(cfg.resolved_engine(), Some("claude"));

        let codex: AppConfig = toml::from_str(
            r#"[providers.codex]
bin = "codex"
default_args = ["exec"]
"#,
        )
        .unwrap();
        assert_eq!(codex.resolved_engine(), Some("codex"));
    }

    /// Loudly, not silently. A deprecated key that is merely tolerated is a
    /// deprecated key nobody migrates off, and the reading stops eventually —
    /// which is only fair if the user was told it started.
    #[test]
    fn a_legacy_table_produces_a_note_naming_the_old_key() {
        let cfg: AppConfig = toml::from_str("[providers.claude]\nbin = \"claude\"\n").unwrap();
        let note = cfg
            .engine_deprecation_note()
            .expect("a legacy key must warn");
        assert!(note.contains("[providers.claude]"), "got: {note}");
        assert!(note.contains("ro config set agent.engine"), "got: {note}");
    }

    /// The new key wins when both are present. A user who has already written
    /// `[agent] engine` has expressed the current intent, and letting a stale
    /// `[providers.claude]` override it would make the new key unsettable.
    #[test]
    fn the_new_key_wins_and_silences_the_note() {
        let cfg: AppConfig =
            toml::from_str("[agent]\nengine = \"git\"\n\n[providers.claude]\nbin = \"claude\"\n")
                .unwrap();
        assert_eq!(cfg.resolved_engine(), Some("git"));
        assert_eq!(
            cfg.engine_deprecation_note(),
            None,
            "a config that already names the engine is not deprecated"
        );
    }

    #[test]
    fn a_current_config_produces_no_note() {
        let cfg: AppConfig = toml::from_str("[agent]\nengine = \"codex\"\n").unwrap();
        assert_eq!(cfg.resolved_engine(), Some("codex"));
        assert!(cfg.engine_deprecation_note().is_none());
    }

    /// The gate that actually blocks a leaked credential has to be the
    /// default. Anything else and the safe path is opt-in, which is the
    /// one thing a safety net cannot be.
    #[test]
    fn the_secret_scan_blocks_by_default_and_the_gates_do_not_run() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.checkpoint.secret_scan, "block");
        assert_eq!(
            cfg.checkpoint.secret_scan_mode(),
            "block",
            "and the accessor must agree with the field, or the two can drift"
        );
        assert!(
            !cfg.checkpoint.quality_gates_enabled(),
            "quality gates are off by default; see the field docs for why"
        );
    }

    /// A renamed table that is merely ignored is a silent data loss: the
    /// config parses, the setting does nothing, and the user finds out by
    /// watching their commits not happen.
    #[test]
    fn a_config_still_carrying_review_is_told_where_the_settings_moved() {
        let cfg: AppConfig =
            toml::from_str("[review]\nprovider = \"claude\"\nquality_gates = \"on\"\n").unwrap();
        let note = cfg
            .review_deprecation_note()
            .expect("a moved table must warn");
        assert!(note.contains("checkpoint.quality_gates"), "got: {note}");
        assert!(note.contains("agent.engine"), "got: {note}");
    }

    /// And a current config must not warn, or the warning is noise people
    /// learn to scroll past — and then the one that matters is missed too.
    #[test]
    fn a_current_config_says_nothing_about_review() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.review_deprecation_note(), None);
    }

    /// The phantom engine, stopped at parse time.
    ///
    /// A free-form `[engines.cladue]` table would parse, register an
    /// engine nobody named, and fail at dispatch. With three fixed slots
    /// each carrying `deny_unknown_fields`, one wrong character is a
    /// parse error naming the key.
    #[test]
    fn a_misspelled_slot_is_a_parse_error_not_a_phantom_engine() {
        let err = toml::from_str::<AppConfig>("[engines.cladue]\nbin = \"claude\"\n").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("engines") || msg.contains("unknown field"),
            "the error must name the table or the key, got: {msg}"
        );
    }

    #[test]
    fn a_misspelled_key_inside_a_real_slot_is_a_parse_error() {
        let err = toml::from_str::<AppConfig>("[engines.engine_claude]\ncladue = \"claude\"\n")
            .unwrap_err();
        assert!(
            err.to_string().contains("cladue"),
            "the error must name the key the user typed, got: {err}"
        );
    }

    /// And the positive case: the three real slots load.
    #[test]
    fn the_three_real_slots_load() {
        let cfg: AppConfig = toml::from_str(concat!(
            "[engines.engine_claude]\n",
            "bin = \"claude\"\n",
            "default_args = [\"-p\"]\n",
            "[engines.engine_codex]\n",
            "bin = \"codex\"\n",
            "[engines.engine_git]\n",
            "bin = \"git\"\n",
        ))
        .unwrap();
        assert_eq!(cfg.engines.engine_claude.bin, "claude");
        assert_eq!(cfg.engines.engine_claude.default_args, vec!["-p"]);
        assert_eq!(cfg.engines.engine_codex.bin, "codex");
        assert_eq!(cfg.engines.engine_git.bin, "git");
        assert!(
            cfg.engines.slot("gemini").is_none(),
            "a fourth slot must not exist; a fourth agent is a config \
             override of an existing one, not a new entry"
        );
    }

    /// A slot that sets only `default_args` still gets the right binary.
    ///
    /// This is the trap a guessing `Default` sets: `engine_claude` would
    /// default to `git`, and the user would silently get a git engine
    /// invoked with an agent's arguments.
    #[test]
    fn a_partial_slot_falls_back_per_field() {
        let cfg: AppConfig =
            toml::from_str("[engines.engine_claude]\ndefault_args = [\"-p\"]\n").unwrap();
        let resolved = cfg.engines.resolve_slot("claude").unwrap();
        assert_eq!(
            resolved.bin, "claude",
            "an unset bin must take the shipped one, not another slot's"
        );
        assert_eq!(resolved.default_args, vec!["-p"]);
    }

    #[test]
    fn the_shipped_defaults_name_the_right_binaries() {
        let e = EnginesConfig::shipped();
        assert_eq!(e.engine_claude.bin, "claude");
        assert_eq!(e.engine_codex.bin, "codex");
        assert_eq!(e.engine_git.bin, "git");
        assert!(e.engine_claude.default_args.contains(&"-p".to_string()));
        assert_eq!(e.engine_git.default_args.len(), 0);
    }

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
        // `[review]` is gone; its settings moved to `[checkpoint]`, and a
        // default config must not trip its own deprecation note.
        assert_eq!(cfg.review_deprecation_note(), None);
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
