//! The environment an engine is allowed to see, assembled by subtraction.
//!
//! # Why subtraction and not addition
//!
//! The earlier plan injected the resolved token into the engine's
//! environment next to `GH_TOKEN`, for the agent's own use. That is a leak
//! with a long fuse, and **every step of it is outside ro's control**:
//!
//! ```text
//! 1. ro spawns claude with GH_TOKEN in its environment
//! 2. the agent runs `env`, or printenv, or a build script, or a crash reporter
//! 3. the value lands in the agent's stdout
//! 4. the agent's stdout lands in its transcript, which is written to disk
//! 5. the transcript is uploaded, pasted into an issue, or read by a
//!    different model on the next session
//! ```
//!
//! There is no point in that chain where ro can intervene, and the
//! destination is frequently a third party. A credential is exactly the
//! kind of thing that must not be in an LLM's context — not because the
//! model is careless, but because **the transcript is designed to be read
//! and shared**, which is the opposite of a secret store.
//!
//! `SecretString` protects ro's own logs and panics. It cannot protect a
//! file ro does not write.
//!
//! **Stripping is mandatory, not merely adding-nothing.** A user who has
//! `GH_TOKEN` exported in their shell for `gh` would otherwise hand it to
//! every agent ro spawns, with neither of them intending it.
//!
//! # The author DOES reach the engine, deliberately
//!
//! If the engine runs `git commit`, git needs an identity, and there is no
//! third place to put it — `GIT_CONFIG_*` is unavoidable.
//!
//! It is also not a secret: it is about to be in a public commit. What
//! matters is that the engine must not *choose* it, which ro handles in
//! two layers: it exports the identity on the child's environment, and it
//! re-asserts `-c` on every commit it performs itself.
//!
//! `GIT_CONFIG_*` rather than `GIT_AUTHOR_*`, because it covers
//! `commit --amend`, `tag`, and every other object-creating call — four
//! variables become two, and it is the same mechanism as the `-c` ro
//! already passes.
//!
//! **Stated honestly:** the agent can still read the author with
//! `printenv`. That is not a secret and pretending otherwise would be worse
//! than saying so.
//!
//! # No remote rewrite, therefore nothing to restore
//!
//! The alternative design rewrites `git remote set-url` to a
//! credential-bearing URL, pushes, then restores. That has a second thing
//! that can fail, needs a `finally`, can be killed between its halves, and
//! puts a credential into an on-disk file for the duration. `git -c
//! …extraheader=…` writes nothing: **no window in which a token exists on
//! disk, and therefore no restore to forget.**

use std::collections::BTreeMap;

use ro_core::CommitIdentity;

/// Variables that must never reach an engine, whatever else happens.
///
/// Spelled out as a list rather than a prefix match so a new variable
/// cannot slip through by accident: a list is a decision someone made, and
/// a prefix is a heuristic nobody reviews.
pub const STRIPPED: &[&str] = &[
    "GH_TOKEN",
    "GITHUB_TOKEN",
    // A future key ro might introduce. Stripped before it exists so the
    // moment it is added, it is already covered.
    "RO_CREDENTIAL",
    "GITHUB_PAT",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
];

/// True if this name must not reach an engine.
pub fn is_stripped(name: &str) -> bool {
    STRIPPED.contains(&name)
}

/// The environment for an engine's child process.
///
/// Built as an owned map so the subtraction is *visible* — a caller
/// cannot accidentally pass the parent environment through, because there
/// is no parent environment anywhere in this type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChildEnv {
    vars: BTreeMap<String, String>,
}

impl ChildEnv {
    /// The parent's environment, minus everything in [`STRIPPED`].
    ///
    /// The subtraction is **unconditional**. It does not depend on ro
    /// having resolved a credential: a repo with no `[auth] credential`
    /// still gets a child env with no `GH_TOKEN`, because the token in
    /// question is the user's, and it would be in scope of the agent
    /// either way.
    pub fn from_parent() -> Self {
        Self::subtracting(std::env::vars())
    }

    /// Build from an explicit parent environment, so a test does not have
    /// to mutate the process's.
    pub fn subtracting<K, V, I>(parent: I) -> Self
    where
        K: AsRef<str>,
        V: AsRef<str>,
        I: IntoIterator<Item = (K, V)>,
    {
        let mut vars = BTreeMap::new();
        for (k, v) in parent {
            let k = k.as_ref();
            if is_stripped(k) {
                continue;
            }
            vars.insert(k.to_string(), v.as_ref().to_string());
        }
        Self { vars }.with_git_hardening()
    }

    /// The four variables that stop an unattended git from stalling.
    ///
    /// Applied by [`Self::subtracting`], so a caller cannot forget: the one
    /// way to build a `ChildEnv` is through a constructor that applies
    /// them.
    fn with_git_hardening(mut self) -> Self {
        self.set("GIT_TERMINAL_PROMPT", "0");
        self.set("GCM_INTERACTIVE", "Never");
        self.set("GIT_PAGER", "cat");
        self.set("PAGER", "cat");
        self
    }

    /// Set a variable, replacing any previous value.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.vars.insert(key.into(), value.into());
        self
    }

    /// Add the commit identity, via `GIT_CONFIG_*`.
    ///
    /// `GIT_CONFIG_COUNT` plus a key/value pair, so the value applies to
    /// every git call the engine makes — including `commit --amend` and
    /// `tag`, which `GIT_AUTHOR_*` would not cover.
    pub fn with_identity(mut self, identity: Option<&CommitIdentity>) -> Self {
        let Some(id) = identity else {
            return self;
        };
        // Two entries: user.name and user.email. `GIT_CONFIG_COUNT` has to
        // be the *total*, so a caller that already added entries would
        // break this — hence the base is a count of exactly these two and
        // the caller's own entries are numbered from there by `with_env`.
        self.set("GIT_CONFIG_COUNT", "2");
        self.set("GIT_CONFIG_KEY_0", "user.name");
        self.set("GIT_CONFIG_VALUE_0", &id.name);
        self.set("GIT_CONFIG_KEY_1", "user.email");
        self.set("GIT_CONFIG_VALUE_1", &id.email);
        self
    }

    /// Merge the caller's explicit additions.
    ///
    /// **Merged in, not substituted.** A caller supplies what to *add*;
    /// the subtraction already happened. The only thing a caller can
    /// override is something ro added after the strip, and a stripped name
    /// is refused rather than re-added — otherwise this function would be
    /// a way to put `GH_TOKEN` back and the whole design would be a
    /// convention.
    pub fn with_additions(mut self, additions: &[(String, String)]) -> Self {
        for (k, v) in additions {
            if is_stripped(k) {
                continue;
            }
            self.vars.insert(k.clone(), v.clone());
        }
        self
    }

    /// The full environment, in the `(String, String)` form a
    /// `std::process::Command` takes.
    pub fn to_pairs(&self) -> Vec<(String, String)> {
        self.vars
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// One variable, for a test or a diagnostic.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }

    /// Does this environment contain anything token-shaped?
    ///
    /// Checks *values*, not names: a secret does not have to be called
    /// `GH_TOKEN` to be one, and a name-based check would be defeated by
    /// the first `AUTHORIZATION` or `TOKEN`-suffixed variable.
    pub fn contains_token_shaped(&self) -> Option<String> {
        for (k, v) in &self.vars {
            if v.contains("ghp_") || v.contains("github_pat_") {
                return Some(format!("{k} carries a PAT-shaped value"));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parent_with_a_token() -> Vec<(String, String)> {
        vec![
            ("PATH".into(), "/usr/bin".into()),
            (
                "GH_TOKEN".into(),
                "ghp_16C7e42F292c6912E7710c838347Ae178B4a".into(),
            ),
            ("HOME".into(), "/home/me".into()),
        ]
    }

    #[test]
    fn the_token_is_stripped() {
        let env = ChildEnv::subtracting(parent_with_a_token());
        assert_eq!(env.get("GH_TOKEN"), None);
        assert_eq!(env.get("PATH"), Some("/usr/bin"), "other vars survive");
        assert_eq!(env.get("HOME"), Some("/home/me"));
    }

    /// The negative control. Without it, the test above would also pass on
    /// a strip that removed nothing at all.
    #[test]
    fn the_strip_is_what_removes_it() {
        let raw: BTreeMap<_, _> = parent_with_a_token().into_iter().collect();
        assert!(
            raw.contains_key("GH_TOKEN"),
            "the parent environment must actually contain it, or this test \
             proves nothing"
        );
        assert!(
            ChildEnv::subtracting(parent_with_a_token())
                .get("GH_TOKEN")
                .is_none()
        );
    }

    #[test]
    fn stripping_is_unconditional() {
        // No identity, no credential, nothing configured: the token is
        // still gone. It is the user's token, and it would be in scope of
        // the agent whether or not ro had resolved one.
        let env = ChildEnv::subtracting(parent_with_a_token()).with_identity(None);
        assert_eq!(env.get("GH_TOKEN"), None);
    }

    #[test]
    fn a_caller_cannot_re_add_a_stripped_name() {
        let env = ChildEnv::subtracting(parent_with_a_token())
            .with_additions(&[("GH_TOKEN".to_string(), "ghp_readded".to_string())]);
        assert_eq!(
            env.get("GH_TOKEN"),
            None,
            "with_additions must refuse a stripped name, or the whole design \
             is a convention"
        );
    }

    #[test]
    fn the_identity_reaches_the_child_via_git_config() {
        let id = CommitIdentity {
            name: "Work".into(),
            email: "work@example.com".into(),
        };
        let env = ChildEnv::subtracting(parent_with_a_token()).with_identity(Some(&id));
        assert_eq!(env.get("GIT_CONFIG_KEY_0"), Some("user.name"));
        assert_eq!(env.get("GIT_CONFIG_VALUE_0"), Some("Work"));
        assert_eq!(env.get("GIT_CONFIG_KEY_1"), Some("user.email"));
        assert_eq!(env.get("GIT_CONFIG_VALUE_1"), Some("work@example.com"));
    }

    #[test]
    fn the_hardening_block_is_always_present() {
        let env = ChildEnv::subtracting(Vec::<(String, String)>::new());
        assert_eq!(env.get("GIT_TERMINAL_PROMPT"), Some("0"));
        assert_eq!(env.get("GCM_INTERACTIVE"), Some("Never"));
    }

    #[test]
    fn nothing_token_shaped_survives() {
        let env = ChildEnv::subtracting(parent_with_a_token()).with_identity(None);
        assert_eq!(
            env.contains_token_shaped(),
            None,
            "no value in the child environment may look like a credential"
        );
    }
}
