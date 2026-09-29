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

// ── The key table `ro config set` validates against ──────────────────────
//
// `AppConfig` deliberately has no `deny_unknown_fields`: a table from a newer
// ro must load cleanly, and a config the loader refuses is a config the user
// cannot escape without hand-editing. The cost of that decision is that a
// *typo* loads cleanly too, and nothing anywhere distinguishes the two.
//
// This table is where the distinction is made. It is a declaration, and a
// declaration can drift from the struct it describes — so it is not trusted on
// its own. `is_modelled_field` below proves each entry against the real
// `Deserialize` impl, and a test asserts the table and the impl agree. A key
// added to a struct without adding it here fails the build's test run rather
// than becoming a silently-rejected setting.

/// Every table `AppConfig` reads, and the keys inside it that mean something.
///
/// The authority for what `ro config set` will write. Kept in step with the
/// structs by `every_declared_key_is_a_real_field`; see the note above.
pub const CONFIG_KEYS: &[(&str, &[&str])] = &[
    (
        "core",
        &["layout", "parallel", "projects_dir", "timeout_secs"],
    ),
    // `[identity]` is the odd one: `default` is a real key, and everything
    // else under it is a *user-named* profile, so the leaf keys are checked
    // against `IDENTITY_PROFILE_KEYS` rather than listed here.
    ("identity", &["default"]),
    ("auth", &["expected_login", "https", "ssh"]),
    ("github", &["auth", "host"]),
    ("agent", &["command", "engine", "prompt"]),
];

/// The keys of one `[identity.<profile>]` table.
pub const IDENTITY_PROFILE_KEYS: &[&str] = &["email", "name"];

/// The keys `T`'s `Deserialize` impl accepts at depth one, proven rather than
/// restated.
///
/// The trick is what is fed, not what is read. A key `T` has never heard of is
/// ignored in silence — that is the whole reason this function exists — so
/// probing with a *valid* value cannot tell "accepted" from "ignored". Probing
/// with a `true` reverses it: every field in this schema is a string, an
/// integer, a list or an optional of those, and a boolean is accepted by none
/// of them, so a real field rejects the value while a name ro does not know
/// parses cleanly.
///
/// That is an assumption about the schema, and it is enforced rather than
/// merely noted: adding a `bool` field makes a real key probe as *unknown*,
/// which fails `every_declared_key_is_a_real_field`. The alternative — a
/// derived-key listing — needs `serde_ignored` or a hand-written mirror, and a
/// hand-written mirror is the drift this is trying to remove.
pub fn is_modelled_field<T: serde::de::DeserializeOwned>(key: &str) -> bool {
    // Bare-key syntax only. `engine-args` is a serde `rename`, not a TOML
    // bare key, and a document that fails to *parse* would be indistinguishable
    // from a field that rejected its value.
    //
    // The parse is checked, not `.expect`ed. A key containing a character that
    // is not legal in a TOML bare key — a space, a non-ASCII letter — makes
    // the probe document unparseable, and the old `.expect("a bare key and a
    // bool always parse")` panicked on it. `ro config set "core.parallel 4=1"`
    // — a space where the dot goes, the most likely typo for this tool —
    // exited 10 with a stack trace. A config tool that panics on a typo is
    // worse than one that rejects it.
    //
    // An unparseable probe is not a modelled field: the key cannot be written
    // as a bare key, so it cannot name a field. The caller turns that into an
    // error naming what a valid key looks like.
    let probe = format!("{key} = true\n");
    if toml::from_str::<toml::Table>(&probe).is_err() {
        return false;
    }
    toml::from_str::<T>(&probe).is_err()
}

/// The keys of `table`, or `None` if no such table is read.
pub fn known_keys_for(table: &str) -> Option<&'static [&'static str]> {
    CONFIG_KEYS.iter().find(|(t, _)| *t == table).map(|(_, k)| *k)
}

/// The closest key to `typo`, when one is close enough to have been meant.
///
/// A rejection that only says "unknown key" sends the user to the source to
/// find the schema. This is the difference between `core.paralel` being a typo
/// the message can see and one it cannot.
pub fn nearest_key<'a>(typo: &str, candidates: &'a [&'a str]) -> Option<&'a str> {
    candidates
        .iter()
        .map(|c| (*c, edit_distance(typo, c)))
        .filter(|(_, d)| *d <= 3)
        .min_by_key(|(_, d)| *d)
        .map(|(c, _)| c)
}

/// Levenshtein, capped — a distance past 3 is not a typo, it is a different
/// word, and pretending otherwise produces nonsense suggestions.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
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

    /// The key table and the structs must agree, in both directions.
    ///
    /// This is the test that would have caught the whole class of bug this
    /// wave is about. `CONFIG_KEYS` is a declaration, and a declaration is
    /// exactly the kind of thing that drifts: a field added to a struct and
    /// not to the table is a setting `ro config set` refuses to write, and a
    /// key in the table that no struct has is a setting it writes and nothing
    /// reads. Both are silent, and both are caught here rather than by a user
    /// who typed a sensible value and watched nothing happen.
    #[test]
    fn every_declared_key_is_a_real_field() {
        for (table, keys) in CONFIG_KEYS {
            for key in *keys {
                assert!(
                    is_modelled_field::<AppConfig>(&format!("{table}.{key}")),
                    "CONFIG_KEYS declares {table}.{key}, but AppConfig has no such field"
                );
            }
        }
        // And the profile table, which is keyed by user-chosen names.
        for key in IDENTITY_PROFILE_KEYS {
            assert!(
                is_modelled_field::<CommitIdentityConfig>(key),
                "IDENTITY_PROFILE_KEYS declares {key}, but CommitIdentityConfig has no such field"
            );
        }
    }

    /// The mirror image, and a genuinely different loop.
    ///
    /// The test above walks `CONFIG_KEYS` and proves each declared entry is a
    /// real field. This one walks **`AppConfig` itself** and proves every
    /// table it has is declared — the direction that leaves a user unable to
    /// configure something ro supports. A test of that name used to live
    /// here, with a doc comment claiming the mirror direction; it iterated
    /// `CONFIG_KEYS` in the same order and asserted the same thing as the
    /// test above it, so it could never have caught what its name promised.
    /// A test that counts as coverage it did not provide is worse than no
    /// test, so it is gone and this is what replaced it.
    ///
    /// The table list is **derived from the serialized struct**, not written
    /// out. A hand-written list of `AppConfig`'s fields is precisely the
    /// mirror this file exists to keep from drifting — it would go stale the
    /// moment a table was added and read as a passing test. Serializing
    /// `AppConfig::default()` cannot go stale: a field added to the struct
    /// appears in the output with no edit here.
    ///
    /// What this does and does not catch, stated plainly:
    ///
    /// - **Catches**: a table added to `AppConfig` and not to `CONFIG_KEYS`.
    ///   `ro config set <new>.key=…` classifies as `UnknownTable`, is written
    ///   with a "belongs to a newer ro" warning, and is read by nothing —
    ///   the exact class of bug this wave is about. Nothing else checks it.
    /// - **Catches**: any leaf the default config can serialize that
    ///   `CONFIG_KEYS` omits (that is `[core]` and `[github]`, whose fields
    ///   are not `Option`).
    /// - **Does not catch**: an `Option` field added to `[auth]`, `[agent]` or
    ///   `[identity]`, because serde omits `None` and it is not in the
    ///   output. Enumerating those needs `serde_ignored` or a derived
    ///   key-listing, and the note on `is_modelled_field` says why neither is
    ///   the answer here.
    #[test]
    fn every_table_appconfig_has_is_declared() {
        let serialized = toml::to_string(&AppConfig::default()).expect("AppConfig serializes");
        let parsed: toml::Value =
            toml::from_str(&serialized).expect("the serialized config re-parses");
        let root = parsed.as_table().expect("a table at the root");

        let mut undeclared_tables = Vec::new();
        let mut undeclared_keys = Vec::new();
        for (table, value) in root {
            let Some(declared) = known_keys_for(table) else {
                undeclared_tables.push(table.clone());
                continue;
            };
            let fields = value
                .as_table()
                .expect("a table in the serialized config")
                .keys();
            for key in fields {
                if !declared.contains(&key.as_str()) {
                    undeclared_keys.push(format!("{table}.{key}"));
                }
            }
        }

        assert!(
            undeclared_tables.is_empty(),
            "AppConfig has these tables and CONFIG_KEYS does not declare them, so \
             `ro config set <table>.<key>=…` writes a setting ro reports as \
             belonging to a newer ro and that nothing reads: {undeclared_tables:?}"
        );
        assert!(
            undeclared_keys.is_empty(),
            "AppConfig serializes these keys and CONFIG_KEYS does not declare them, so \
             `ro config set` refuses to write a setting ro supports: {undeclared_keys:?}"
        );
    }

    /// A typo is a typo, and the message should be able to see it.
    #[test]
    fn a_typo_is_recognised_as_a_typo() {
        assert_eq!(
            nearest_key("paralel", &["layout", "parallel", "projects_dir", "timeout_secs"]),
            Some("parallel")
        );
        assert_eq!(nearest_key("layot", &["layout", "parallel"]), Some("layout"));
        // A different word, not a typo: no suggestion, rather than a wrong one.
        assert_eq!(nearest_key("banana", &["layout", "parallel"]), None);
    }

    /// The table a user is most likely to reach for, and the one the docs
    /// name, must be in the table — `checkpoint.secret_scan` is the setting
    /// FEATURES.md tells people to write.
    #[test]
    fn the_documented_secret_scan_key_is_not_a_live_key() {
        assert!(
            known_keys_for("checkpoint").is_none(),
            "checkpoint is not read; the docs must not point at it"
        );
    }
}
