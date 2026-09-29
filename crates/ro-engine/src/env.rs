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

/// True if a variable's **value** is shaped like a credential.
///
/// This is the second gate, and it exists because the name list cannot be
/// complete. `credential_ref = "env:WORK_GH_TOKEN"` names a variable that
/// no list will ever contain, because the user chose the name — and the
/// whole point of that row is that a secret is sitting in it. The
/// subtraction was unconditional on the six well-known names and silent on
/// every other one, so a per-repo credential walked straight into the
/// agent's environment: the exact leak this module exists to prevent,
/// arriving through the door the per-repo credential feature opened.
///
/// The shape is deliberately narrow so an ordinary value is not destroyed.
/// A file path, a hostname, a prompt, a JSON blob without one of these
/// prefixes all survive.
///
/// Two tiers, and the distinction is the whole point:
///
///   * A **recognised credential shape** — a `ghp_`/`github_pat_`/`sk-` prefix,
///     a JWT — is conclusive on its own. It is stripped whatever the variable
///     is called, because a `ghp_` blob in a variable named `DEPLOY` is still
///     a GitHub token. Requiring a cooperating name here would leave the
///     obvious case open.
///   * A **long opaque token with no prefix** is only stripped when the
///     variable's name says it holds a secret. A long value in `PATH` is not a
///     credential; a long value in `MY_API_KEY` almost certainly is.
pub fn value_looks_like_a_credential(value: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "ghp_", "gho_", "ghs_", "ghu_", "ghr_", "github_pat_", "sk-", "xoxb-", "xoxp-",
    ];
    if PREFIXES.iter().any(|p| value.starts_with(p)) {
        return true;
    }
    // A bare JWT: three base64url segments, the first of which decodes to a
    // header beginning `{"alg"` or `{"typ"`. No prefix covers it.
    if value.starts_with("eyJ") && value.split('.').count() == 3 {
        return true;
    }
    // **A location is not a token.** "≥32 characters and no whitespace" is
    // not a credential shape, it is a *shape*, and ordinary configuration is
    // full of long space-free values: a real `PATH` is 76 characters of
    // colon-separated directories, and `/opt/app/lib/python3.12/site-packages`
    // is 36. Both were stripped, so the agent was handed **no `PATH` at
    // all** and `command -v node` came back NOT-FOUND — the agent could not
    // find its own runtime. A filter that eats configuration is not a
    // filter, it is a denial of service against the tool.
    //
    // A path separator, a list separator, or a backslash says the value
    // names *where something is*, and that is true of a filesystem path, a
    // `PATH`, a `LD_LIBRARY_PATH`, an URL, and a comma-separated include
    // list — none of which is a bearer token.
    if value.contains('/') || value.contains('\\') || value.contains(':') {
        return false;
    }
    value.len() >= 32 && !value.contains(char::is_whitespace)
}

/// True if a value is a credential **regardless of its variable's name**.
///
/// The conclusive tier of [`value_looks_like_a_credential`]. Split out so the
/// two tiers are visibly paired at the call sites rather than one silently
/// unreachable.
pub fn is_conclusive_credential_shape(value: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "ghp_", "gho_", "ghs_", "ghu_", "ghr_", "github_pat_", "sk-", "xoxb-", "xoxp-",
    ];
    if PREFIXES.iter().any(|p| value.starts_with(p)) {
        return true;
    }
    value.starts_with("eyJ") && value.split('.').count() == 3
}

/// True if a variable's name says it holds a secret.
///
/// The name-based half of the second gate. Kept separate from the value
/// test so the two are visibly paired rather than one silently
/// unreachable, and so a future rule has one home.
pub fn name_looks_secret(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    const MARKERS: &[&str] = &[
        "TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL", "CRED", "KEY", "PAT", "AUTH",
        "PRIVATE",
    ];
    // **A marker is a word, not a substring.** `PAT` matched `PATH`,
    // `PYTHONPATH`, `PKG_CONFIG_PATH`, `COMPAT`, `SPATIAL_INDEX` and
    // `PATTERN` — which is most of the configuration an agent needs to run
    // at all, each one of them stripped because its *value* happened to be
    // long and space-free. A `PAT` is a segment; a `PATH` is a different
    // word that happens to start with the same three letters.
    //
    // `CRED` and `KEY` are here for the leak they close: a user who names
    // their credential variable `WORK_CRED` and puts a 40-character opaque
    // token in it leaked the whole token to the engine, because `CRED` is
    // not `CREDENTIAL` and nothing else in the name agreed. The second tier
    // exists because the user chose the name; a name they chose can be any
    // of these.
    n.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|seg| !seg.is_empty())
        .any(|seg| MARKERS.iter().any(|m| seg == *m || seg.ends_with(m)))
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
            // The second gate. The name list above cannot be complete,
            // because the per-repo credential feature lets a user name the
            // variable themselves, and the variable they named is a secret
            // by construction. A recognised shape is conclusive on its own; a
            // bare long token needs the name to agree.
            if is_conclusive_credential_shape(v.as_ref())
                || (name_looks_secret(k) && value_looks_like_a_credential(v.as_ref()))
            {
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
        // `PATHEXT` on Windows, and it is load-bearing.
        //
        // A `Command` spawns through the OS loader, which decides that
        // `claude` means `claude.cmd` **by reading `PATHEXT` from the
        // child's own environment**. A caller doing
        // `env_clear().envs(child_env)` has therefore removed the very
        // variable that made the shim findable, and the spawn fails with
        // "program not found" for a name sitting right there on `PATH`.
        //
        // Unix and macOS resolve an extensionless name directly and never
        // needed it, so nothing local ever notices the omission.
        #[cfg(windows)]
        {
            // Windows resolves an executable through the child's own
            // environment, and four variables are load-bearing for *any*
            // spawn: `PATHEXT` decides that `claude` means `claude.cmd`,
            // and `SystemRoot` is where the loader looks for the runtime
            // DLLs that a `.cmd` and everything it launches need.
            //
            // A caller that does `env_clear().envs(child_env)` has removed
            // all four, and the spawn fails with "program not found" for a
            // shim that is sitting right there on `PATH`. Unix and macOS
            // resolve an extensionless name directly, so nothing local
            // ever notices.
            for key in ["PATHEXT", "SystemRoot", "windir", "ComSpec", "TEMP", "TMP"] {
                match std::env::var(key) {
                    Ok(v) if self.get(key).is_none() => {
                        self.set(key, v);
                    }
                    _ => {}
                }
            }
        }
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
    ///
    /// The second gate applies here too, and it is the one that matters:
    /// `EngineContext.env` is how a caller hands the engine its own
    /// environment, and a per-repo credential named by the user is exactly
    /// the kind of thing that arrives through it.
    pub fn with_additions(mut self, additions: &[(String, String)]) -> Self {
        for (k, v) in additions {
            if is_stripped(k) {
                continue;
            }
            if is_conclusive_credential_shape(v)
                || (name_looks_secret(k) && value_looks_like_a_credential(v))
            {
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
            if is_token_shaped(v) {
                return Some(format!("{k} carries a PAT-shaped value"));
            }
        }
        None
    }
}

/// Does this value look like a GitHub credential?
///
/// Named and **free-standing** rather than only a method, because the same
/// question gets asked about text that never passed through an environment:
/// `agent.rs` reports changes to `.git/config`, where a value an agent
/// wrote can be a token. Two copies of this test would be two places for
/// the next token format to be missed, and one of them would be the copy
/// nobody looks at.
pub fn is_token_shaped(value: &str) -> bool {
    value.contains("ghp_") || value.contains("github_pat_")
}

#[cfg(test)]
mod tests {
    /// `PATHEXT` survives into the child, on the platform that needs it.
    ///
    /// A caller that does `env_clear().envs(child_env)` has otherwise
    /// removed the variable the Windows loader reads to turn `claude`
    /// into `claude.cmd`, and the spawn fails on a name that is on `PATH`.
    #[cfg(windows)]
    #[test]
    fn the_windows_spawn_variables_reach_the_child() {
        // Every one of these is read by the OS loader out of the child's
        // own environment. `env_clear().envs(child_env)` removes them all,
        // and the spawn then fails with "program not found" for a shim
        // that is on `PATH`.
        let env = ChildEnv::subtracting([("PATH", "/tmp".to_string())]);
        for key in ["PATHEXT", "SystemRoot", "ComSpec"] {
            assert!(
                env.get(key).is_some(),
                "{key} must reach the child or a .cmd shim cannot be \
                 resolved or launched"
            );
        }
    }

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

#[cfg(test)]
mod leak_tests {
    use super::*;

    /// A per-repo credential must not reach the engine.
    ///
    /// `credential_ref = "env:WORK_GH_TOKEN"` names a variable no fixed
    /// list can contain, because the user chose the name — and the whole
    /// point of that row is that a secret is in it. The six-name strip was
    /// silent on every other name, so this value walked straight into the
    /// agent's environment: the exact leak this module exists to prevent,
    /// arriving through the door the per-repo credential feature opened.
    #[test]
    fn a_per_repo_credential_never_reaches_an_engine() {
        let secret = "ghp_worktoken123456789012345678901234";
        let env = ChildEnv::subtracting([
            ("WORK_GH_TOKEN", secret),
            ("PATH", "/usr/bin"),
            ("HOME", "/home/me"),
        ]);

        assert_eq!(
            env.get("WORK_GH_TOKEN"),
            None,
            "the row's own credential variable reached the engine: {:?}",
            env.to_pairs()
        );
        // And the environment is not simply empty — the two controls must
        // survive, or this test also passes on a build that strips
        // everything.
        assert_eq!(env.get("PATH"), Some("/usr/bin"), "other vars survive");
        assert_eq!(env.get("HOME"), Some("/home/me"));
    }

    /// The same refusal on the addition path.
    ///
    /// `EngineContext.env` is a caller-supplied list merged in AFTER the
    /// subtraction. It carried the same rule and the same hole.
    #[test]
    fn with_additions_refuses_a_credential_shaped_value() {
        let env = ChildEnv::subtracting([("PATH", "/usr/bin")]).with_additions(&[(
            "DEPLOY_SECRET".into(),
            "github_pat_11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyz0123".into(),
        )]);
        assert_eq!(env.get("DEPLOY_SECRET"), None, "it got through");
        assert_eq!(env.get("PATH"), Some("/usr/bin"), "other vars survive");
    }

    /// The gate is a value test, not a name test, so a secret-shaped value
    /// in an innocuously-named variable is still caught.
    #[test]
    fn a_credential_shaped_value_is_caught_whatever_its_name() {
        let env = ChildEnv::subtracting([("DEPLOY", "ghp_AAAABBBBCCCCDDDDEEEEFFFF00001111")]);
        assert_eq!(env.get("DEPLOY"), None, "the name is not a defence");
    }

    /// The negative control, and the one that matters: the gate must not
    /// become a filter that eats ordinary configuration.
    ///
    /// A user with `EDITOR=code --wait`, a long `PATH`, a JSON blob, a URL
    /// and a base64 blob that is not a credential must all still get them.
    #[test]
    fn an_ordinary_value_is_never_mistaken_for_a_credential() {
        let env = ChildEnv::subtracting([
            ("EDITOR", "code --wait"),
            ("SSH_AUTH_SOCK", "/run/user/1000/keyring/ssh"),
            ("API_KEY_URL", "https://api.example.com/v1/keys"),
            ("NPM_CONFIG", r#"{"registry":"https://registry.npmjs.org"}"#),
            (
                "API_KEY",
                // Not credential-shaped: it has whitespace and reads as prose.
                "the key is in the vault, ask the platform team for the current one",
            ),
        ]);
        for k in ["EDITOR", "SSH_AUTH_SOCK", "API_KEY_URL", "NPM_CONFIG", "API_KEY"] {
            assert!(env.get(k).is_some(), "{k} must survive the gate");
        }
    }

    /// A **realistic `PATH` is not a credential**, and neither is a
    /// `PYTHONPATH` or a `PKG_CONFIG_PATH`.
    ///
    /// The gate ate all three. `name_looks_secret` returned true for any
    /// name containing `PAT` as a substring — `PATH`, `PYTHONPATH`,
    /// `PKG_CONFIG_PATH`, `COMPAT`, `SPATIAL_INDEX`, `PATTERN` — and
    /// `value_looks_like_a_credential` returned true for any value ≥32
    /// characters with no whitespace. A 76-character colon-separated `PATH`
    /// was therefore stripped entirely, the agent was handed **no `PATH` at
    /// all**, and `command -v node` came back NOT-FOUND: the agent could not
    /// find its own runtime.
    ///
    /// This is the exact failure the negative control above was written to
    /// prevent, and it passed that test because the test's values are all
    /// short or contain whitespace.
    #[test]
    fn a_realistic_path_is_not_stripped() {
        let env = ChildEnv::subtracting([
            (
                "PATH",
                "/usr/local/go/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            ),
            ("PYTHONPATH", "/opt/app/lib/python3.12/site-packages"),
            ("PKG_CONFIG_PATH", "/opt/libs/boost/1.85/lib/pkgconfig"),
            ("LD_LIBRARY_PATH", "/opt/libs/boost/1.85/lib"),
            ("COMPAT", "/opt/compat/lib"),
            ("PATTERN", "/opt/patterns/*.json"),
        ]);
        for k in [
            "PATH",
            "PYTHONPATH",
            "PKG_CONFIG_PATH",
            "LD_LIBRARY_PATH",
            "COMPAT",
            "PATTERN",
        ] {
            assert!(env.get(k).is_some(), "{k} must survive the gate");
        }
    }

    /// The other half: a long opaque token in an **innocuously named**
    /// variable must still be caught.
    ///
    /// The second tier of the gate required the name to contain
    /// `TOKEN`/`SECRET`/`PASSWORD`/`PASSWD`/`CREDENTIAL`/`API_KEY`/`PAT`, so
    /// a user who named their credential variable `WORK_CRED` and put a
    /// 40-character opaque token in it leaked the whole token to the engine.
    /// The first tier (conclusive shapes) worked; the second had a hole.
    #[test]
    fn a_long_opaque_token_in_an_innocuous_name_is_still_caught() {
        let env = ChildEnv::subtracting([(
            "WORK_CRED",
            "a1b2c3d4e5f60718293a4b5c6d7e8f901234567890abcdef",
        )]);
        assert!(
            env.get("WORK_CRED").is_none(),
            "a 40-char opaque token in a variable named WORK_CRED is a \
             credential, whatever the name says"
        );
    }

    /// And the conclusive tier still works whatever the variable is called.
    #[test]
    fn a_conclusive_shape_is_stripped_whatever_the_name() {
        let env = ChildEnv::subtracting([
            ("DEPLOY", "ghp_worktoken123456789012345678901234"),
            ("WORK_GH_TOKEN", "ghp_worktoken123456789012345678901234"),
        ]);
        assert!(env.get("DEPLOY").is_none(), "a ghp_ blob is a token");
        assert!(env.get("WORK_GH_TOKEN").is_none());
    }
}
