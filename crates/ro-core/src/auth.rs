//! Auth policy — the resolved answer to "how, and as whom, does this push".
//!
//! Lives in `ro-core` because an earlier draft defined `AuthPolicy` twice: in
//! `ro-config` and in `ro-github`. That needed both `ro-config -> ro-github`
//! and `ro-github -> ro-config` at the same time, and `ro-github`'s manifest
//! declares no ro-config dependency at all. `ro-core` is already the leaf both
//! depend on, so the type goes there once.
//!
//! ## The two identity fields are not the same thing
//!
//! `commit_identity` is the commit **author**, which ro *sets* via
//! `GIT_CONFIG_*`. It therefore cannot be wrong, so it needs no guard.
//!
//! `expected_login` is the **pushing** account — whatever the credential turns
//! out to be, which ro does *not* control. That is what a guard is for.
//!
//! Collapsing them produces a design where fixing your author silently starts
//! author-guarding your push, or worse, where a correct author suppresses a
//! wrong-account push. They are separate fields on purpose.

use serde::{Deserialize, Serialize};

/// Which transport a push goes over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthProvider {
    /// Token over HTTPS.
    Https,
    /// An SSH key.
    Ssh,
    /// The machine's own credential — SSH agent, git credential manager,
    /// `gh auth`. The right answer for a repo whose key is already correct and
    /// needs no configuration.
    Machine,
}

impl AuthProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthProvider::Https => "https",
            AuthProvider::Ssh => "ssh",
            AuthProvider::Machine => "machine",
        }
    }
}

/// The author ro applies to a commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitIdentity {
    pub name: String,
    pub email: String,
}

/// The resolved auth posture for one push.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthPolicy {
    pub provider: AuthProvider,

    /// May this push fall back to the machine's own credential when the named
    /// one cannot be obtained?
    ///
    /// **Global-only, and deliberately not a per-repo field.** A per-repo
    /// fallback posture means one fleet run can hold two safety policies at
    /// once, and "which repo did we push as the wrong account?" then has no
    /// single answer afterwards. Resolve it from the global config and the
    /// `--allow-fallback` flag, then inject it here at the end.
    pub allow_fallback: bool,

    /// The login the credential is expected to resolve to, checked before the
    /// push. Guards the account the *remote* sees.
    pub expected_login: Option<String>,

    /// The commit author. Set by ro, so it is never in doubt.
    pub commit_identity: Option<CommitIdentity>,
}

impl Default for AuthPolicy {
    fn default() -> Self {
        Self {
            provider: AuthProvider::Machine,
            allow_fallback: false,
            expected_login: None,
            commit_identity: None,
        }
    }
}

impl AuthPolicy {
    /// Check the login a credential actually resolved to.
    ///
    /// Returns the mismatch rather than a bool so the caller can name both
    /// sides in the error. "wrong account" with no expected and no actual is a
    /// support ticket; "expected quangdang46, credential is someone-else" is
    /// a one-line fix.
    pub fn check_login(&self, actual: &str) -> Result<(), LoginMismatch> {
        let Some(expected) = &self.expected_login else {
            return Ok(());
        };
        if expected.eq_ignore_ascii_case(actual) {
            return Ok(());
        }
        Err(LoginMismatch {
            expected: expected.clone(),
            actual: actual.to_string(),
        })
    }
}

/// The credential resolved to a different account than the config requires.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "credential resolved to {actual:?} but this repo requires {expected:?} — \
     refusing to push as the wrong account"
)]
pub struct LoginMismatch {
    pub expected: String,
    pub actual: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard's whole job. A token with `repo` scope and no write access on
    /// one repository is an ordinary state; a token for a *different account*
    /// with write access is worse, because every push silently succeeds as
    /// someone else.
    #[test]
    fn a_mismatched_login_is_refused_and_names_both_sides() {
        let policy = AuthPolicy {
            expected_login: Some("quangdang46".into()),
            ..Default::default()
        };
        let err = policy.check_login("someone-else").unwrap_err();
        assert_eq!(err.expected, "quangdang46");
        assert_eq!(err.actual, "someone-else");
        // The message has to carry both, or it is not actionable.
        let msg = err.to_string();
        assert!(msg.contains("quangdang46") && msg.contains("someone-else"));
    }

    #[test]
    fn a_matching_login_passes_regardless_of_case() {
        let policy = AuthPolicy {
            expected_login: Some("QuangDang46".into()),
            ..Default::default()
        };
        assert!(policy.check_login("quangdang46").is_ok());
    }

    /// No `expected_login` means no guard, and that is the default: a user who
    /// has not asked for the check should not be blocked by it.
    #[test]
    fn an_unguarded_policy_accepts_any_login() {
        assert!(AuthPolicy::default().check_login("anyone").is_ok());
    }

    /// The default posture is no silent fallback, and the machine's own
    /// credential. Falling back silently can push via the wrong SSH key and
    /// leak the wrong account, which is the single worst outcome here.
    #[test]
    fn the_default_policy_does_not_allow_fallback() {
        let policy = AuthPolicy::default();
        assert!(!policy.allow_fallback);
        assert_eq!(policy.provider, AuthProvider::Machine);
    }
}
