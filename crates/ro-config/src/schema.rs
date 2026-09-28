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
    pub identity: IdentityConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub github: GitHubConfig,
    #[serde(default)]
    pub agent: AgentConfig,
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

/// `[identity]` — the commit author, and the named profiles a repo picks from.
///
/// The reason this is a **map of profiles** rather than a single
/// `name`/`email` pair is that the whole feature is per-repo. A developer with
/// a work address and a personal one has two identities, and which one
/// applies is a property of the repository, not of the machine. The row's
/// `author_ref` names a profile here; nothing carries the address itself, so
/// renaming an address is one edit in one file rather than a migration of
/// every row that referenced it.
///
/// ```toml
/// [identity]
/// default = "personal"
///
/// [identity.work]
/// name  = "Dang Tran Quang"
/// email = "quang@company.com"
///
/// [identity.personal]
/// name  = "Dang Tran Quang"
/// email = "me@gmail.com"
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IdentityConfig {
    /// Which profile a repo with no `author_ref` gets. Falls back to the only
    /// profile if exactly one is defined, so a single-identity setup needs
    /// no `default` key at all.
    pub default: Option<String>,
    /// Named profiles, keyed by the name a row's `author_ref` holds.
    #[serde(flatten)]
    pub profiles: std::collections::BTreeMap<String, CommitIdentityConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitIdentityConfig {
    pub name: Option<String>,
    pub email: Option<String>,
}

impl IdentityConfig {
    /// Resolve a profile name to an address.
    ///
    /// A name that is not a defined profile is an **error**, not a fallback
    /// and not a synthesised address. Synthesising one — which is what this
    /// did before — produces a commit attributed to `work@localhost` and
    /// calls it success, which is worse than refusing: the commit is real,
    /// the author is wrong, and nothing reports it.
    ///
    /// Both halves of that hold for a profile that *exists but is
    /// incomplete*, which is why both fields are `Option` and the check
    /// lives here rather than in serde. With required fields, a profile
    /// could not be built a key at a time: `ro config set
    /// identity.work.name=…` produced a `[identity.work]` table with one
    /// key, the whole file then failed to parse with `missing field
    /// email`, and the tool that exists so a config can be written without
    /// an editor could not write one. An incomplete profile is now a
    /// well-formed config that names what it is missing.
    pub fn resolve(&self, name: &str) -> Result<ro_core::CommitIdentity, String> {
        let p = self.profiles.get(name).ok_or_else(|| {
            // The name that failed goes in the message too. A caller can
            // add context, but a bare `known profiles: work, personal`
            // leaves the reader to work out which of the several things
            // named on the command line was the wrong one.
            let known = self.names();
            if known.is_empty() {
                format!("no [identity.*] profile named {name:?} is defined")
            } else {
                format!("no [identity.*] profile named {name:?}; known: {}", known.join(", "))
            }
        })?;

        let mut missing: Vec<&str> = Vec::new();
        if p.name.as_deref().unwrap_or_default().trim().is_empty() {
            missing.push("name");
        }
        if p.email.as_deref().unwrap_or_default().trim().is_empty() {
            missing.push("email");
        }
        if !missing.is_empty() {
            return Err(format!(
                "[identity.{name}] is missing {}",
                missing.join(" and ")
            ));
        }

        Ok(ro_core::CommitIdentity {
            name: p.name.clone().unwrap_or_default(),
            email: p.email.clone().unwrap_or_default(),
        })
    }

    /// The profile a repo with no explicit `author_ref` gets.
    ///
    /// Returns `Err` rather than silently falling back: a `default` that
    /// names a missing or half-written profile is a configuration mistake,
    /// and committing every repo under some other identity is what happens
    /// if that is papered over.
    pub fn fallback(&self) -> Result<Option<ro_core::CommitIdentity>, String> {
        if let Some(d) = &self.default {
            // Say the *default* is the problem. `resolve`'s own message
            // names a profile that is missing, but here the thing the user
            // wrote is `default = "..."`, and blaming the profile they did
            // not name sends them to edit the wrong line.
            return self
                .resolve(d)
                .map(Some)
                .map_err(|why| format!("`default` names {d:?}, and {why}"));
        }
        // One profile needs no `default` key. More than one is ambiguous.
        if self.profiles.len() == 1 {
            let only = self.profiles.keys().next().expect("len is 1");
            return self.resolve(only).map(Some);
        }
        Ok(None)
    }

    /// Every profile name, for error messages that have to say what *is*
    /// available.
    pub fn names(&self) -> Vec<&str> {
        self.profiles.keys().map(String::as_str).collect()
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every remaining table round-trips through TOML with the value the
    /// user wrote. The cut tables made this test impossible to write, which
    /// is part of why they were worth cutting: there was no way to assert
    /// "this config is read" about a struct nothing read.
    #[test]
    fn the_live_tables_survive_a_round_trip() {
        let raw = r#"
[core]
projects_dir = "~/code"
layout = "nested"
parallel = 3
timeout_secs = 45

[identity]
default = "work"

[identity.work]
name = "Dev Work"
email = "dev@corp.com"

[auth]
https = "env:RO_TOKEN"
expected_login = "quangdang46"

[github]
host = "github.com"
auth = "gh"

[agent]
engine = "codex"
"#;
        let cfg: AppConfig = toml::from_str(raw).expect("the live tables parse");
        assert_eq!(cfg.core.layout, "nested");
        assert_eq!(cfg.core.parallel, 3);
        assert_eq!(cfg.core.timeout_secs, 45);
        assert_eq!(cfg.identity.default.as_deref(), Some("work"));
        assert_eq!(
            cfg.identity.resolve("work").unwrap().email,
            "dev@corp.com"
        );
        assert_eq!(cfg.auth.expected_login.as_deref(), Some("quangdang46"));
        assert_eq!(cfg.github.auth, "gh");
        assert_eq!(cfg.agent.engine.as_deref(), Some("codex"));
    }

    /// A credential is a reference, and a pasted token must not survive
    /// deserialization. This is the one shape check that has to keep
    /// working as the config is edited, because `ghp_…` is the thing a
    /// user will actually type.
    #[test]
    fn a_pasted_token_is_not_a_credential_reference() {
        let parsed: Result<AppConfig, _> =
            toml::from_str("[auth]\nhttps = \"ghp_not_a_reference\"\n");
        assert!(
            parsed.is_err(),
            "a pasted token must not parse as a reference"
        );
    }

    /// The defaults a fresh install gets are the documented ones.
    #[test]
    fn the_defaults_are_the_documented_ones() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.core.layout, "flat");
        assert_eq!(cfg.github.host, "github.com");
        assert_eq!(cfg.github.auth, "auto");
    }
}
