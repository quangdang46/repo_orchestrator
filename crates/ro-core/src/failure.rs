//! Failure classification — one taxonomy, defined once.
//!
//! This type lives in `ro-core` rather than in `ro-jobs` for a layering
//! reason. The engine layer needs to name *why* something failed, and an
//! earlier draft had it reach for `ro-jobs::FailureClass` to do so — which
//! pulled `ro-engine -> ro-jobs -> ro-state -> rusqlite` into the graph purely
//! to spell an error discriminant. A SQLite-shaped jobs crate is the wrong
//! place for a vocabulary the engine, the orchestrator, and the reporter all
//! have to agree on.
//!
//! `ro-jobs` re-exports it, so no caller churns at once.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Classification of a run failure. Each variant maps to a specific recovery
/// strategy in the orchestrator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    AuthError,
    RateLimited,
    MergeConflict,
    DirtyWorktree,
    NetworkTimeout,
    /// The `git` binary itself could not be spawned.
    ///
    /// Previously unreachable: classification ran on stderr, and a process that
    /// never started produces no stderr. It is reachable now because the
    /// spawn error is classified directly rather than inferred from text.
    MissingGit,
    MissingProvider,
    QualityGateFailed,
    SecretScanBlocked,
    GithubPermissionDenied,
}

impl FailureClass {
    /// Whether a retry could plausibly succeed. Anything else is a decision
    /// the user has to make.
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            FailureClass::RateLimited | FailureClass::NetworkTimeout | FailureClass::MergeConflict
        )
    }

    pub fn is_fatal(self) -> bool {
        !self.is_retryable()
    }

    /// Classify a failure from what a process actually said.
    ///
    /// Deliberately text-based rather than exit-code-based: git and gh use
    /// overlapping exit codes for things that need opposite responses, and
    /// "could not push because of auth" and "could not push because the
    /// network flaked" both exit 1.
    ///
    /// A spawn failure is *not* classified here — it never produced text. Pass
    /// [`FailureClass::MissingGit`] explicitly when the child could not be
    /// started at all; that is the only way to tell "git failed" from "git is
    /// not installed", and the difference decides whether a retry is even a
    /// question worth asking.
    pub fn classify(stderr: &str) -> Self {
        let s = stderr.to_ascii_lowercase();
        if s.contains("authentication") || s.contains("permission denied") || s.contains("403") {
            if s.contains("github") || s.contains("api.github.com") {
                FailureClass::GithubPermissionDenied
            } else {
                FailureClass::AuthError
            }
        } else if s.contains("rate limit") || s.contains("429") {
            FailureClass::RateLimited
        } else if s.contains("merge conflict") || s.contains("conflict in") {
            FailureClass::MergeConflict
        } else if s.contains("non-fast-forward")
            || s.contains("updates were rejected")
            || s.contains("fetch first")
            || s.contains("stale info")
        {
            // A push the remote rejected because the branch moved under us.
            //
            // The divergence is a conflict in the sense the taxonomy means:
            // two histories that have to be reconciled, and a rebase onto the
            // new base is the reconciliation. `MergeConflict` is the class
            // that says so, and it is retryable — which is exactly what a
            // non-fast-forward is.
            //
            // This matters because the text a rejected push produces
            // contains none of the words the classes above look for. Left
            // unclassified it fell through to the conservative default, so a
            // divergence was reported as `auth_error` — a credential
            // problem, on a repository the tool had just fetched from
            // successfully.
            FailureClass::MergeConflict
        } else if s.contains("dirty")
            || s.contains("uncommitted changes")
            || s.contains("your local changes")
        {
            FailureClass::DirtyWorktree
        } else if s.contains("timed out")
            || s.contains("connection refused")
            || s.contains("network")
            || s.contains("did not finish within")
            || s.contains("was killed")
        {
            FailureClass::NetworkTimeout
        } else if s.contains("git: not found")
            || s.contains("'git' is not recognized")
            || s.contains("git is not installed")
        {
            FailureClass::MissingGit
        } else if s.contains("quality gate") || s.contains("clippy") || s.contains("test failed") {
            FailureClass::QualityGateFailed
        } else if s.contains("secret") || s.contains("blocked by secret scan") {
            FailureClass::SecretScanBlocked
        } else if s.contains("is not installed")
            || (s.contains("provider") && (s.contains("not found") || s.contains("missing")))
        {
            // "the engine is not installed" is the shape the message
            // actually takes — it names the binary, not the word
            // "provider" — so the phrase on its own is enough.
            FailureClass::MissingProvider
        } else {
            // Nothing matched. AuthError is the conservative default: it is the
            // class whose remedy is for a human, so an unclassified failure
            // stops rather than retrying something that will not change.
            FailureClass::AuthError
        }
    }
}

impl fmt::Display for FailureClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| format!("{self:?}"));
        f.write_str(&s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spawn_failure_is_distinguishable_from_git_failing() {
        // The whole point of MissingGit being reachable: a git that refused to
        // start says nothing on stderr, so classification cannot find it.
        assert!(matches!(
            FailureClass::classify(""),
            FailureClass::AuthError
        ));
        assert_eq!(FailureClass::MissingGit.to_string(), "missing_git");
    }

    #[test]
    fn permission_denied_splits_by_whether_it_is_github() {
        assert_eq!(
            FailureClass::classify("remote: Permission to repository denied"),
            FailureClass::AuthError
        );
        assert_eq!(
            FailureClass::classify("https://api.github.com/repos: 403 Forbidden"),
            FailureClass::GithubPermissionDenied
        );
    }

    #[test]
    fn transient_classes_are_retryable_and_the_rest_are_not() {
        assert!(FailureClass::RateLimited.is_retryable());
        assert!(FailureClass::NetworkTimeout.is_retryable());
        assert!(FailureClass::MergeConflict.is_retryable());
        for permanent in [
            FailureClass::AuthError,
            FailureClass::MissingGit,
            FailureClass::MissingProvider,
            FailureClass::QualityGateFailed,
            FailureClass::SecretScanBlocked,
            FailureClass::GithubPermissionDenied,
        ] {
            assert!(!permanent.is_retryable(), "{permanent} must not retry");
        }
    }

    /// A push the remote rejected as non-fast-forward is a **divergence**.
    ///
    /// This is the classification that was missing, and its absence had a
    /// visible cost: every rejected push was reported as
    /// `missing_provider` — "the `claude`/`codex` binary is missing" — on a
    /// run whose engine had never been dispatched, because the push is the
    /// step after it. The user was sent to install a binary that was
    /// already installed.
    ///
    /// `MergeConflict` is the honest class: two histories that have to be
    /// reconciled, and a rebase onto the new base is the reconciliation.
    #[test]
    fn a_non_fast_forward_push_is_a_divergence() {
        let stderr = "To /tmp/origin.git
 ! [rejected]        HEAD -> feature/x (non-fast-forward)
error: failed to push some refs to '/tmp/origin.git'
hint: Updates were rejected because the tip of your current branch is behind
its remote counterpart. If you want to integrate the remote changes,
use 'git pull' before pushing again.
hint: See the 'Note about fast-forwards' in 'git push --help' for details.";
        assert_eq!(
            FailureClass::classify(stderr),
            FailureClass::MergeConflict,
            "a branch that moved under us is a divergence, not a missing binary"
        );
        assert!(
            FailureClass::MergeConflict.is_retryable(),
            "rebasing onto the new base is the retry"
        );
    }

    /// A push the remote refused for want of permission is not a
    /// divergence, and must not be classified as one.
    ///
    /// The negative control for the test above: a rule that matched every
    /// rejected push would pass it too.
    #[test]
    fn a_push_refused_for_permission_is_not_a_divergence() {
        let stderr = "remote: Permission to owner/repo.git denied to user.
fatal: unable to access 'https://github.com/owner/repo.git/': The requested URL returned error: 403";
        assert_eq!(
            FailureClass::classify(stderr),
            FailureClass::GithubPermissionDenied,
            "a 403 from github is a permission problem, and the class that \
             sends the reader to the credentials is the useful one"
        );
    }

    /// "the engine is not installed" is a missing provider.
    ///
    /// The message names the binary, not the word "provider", so a rule
    /// that required the word would have missed the one case the class was
    /// written for.
    #[test]
    fn an_uninstalled_binary_is_a_missing_provider() {
        assert_eq!(
            FailureClass::classify(
                "resolving the conflict: ro-test-agent is not installed. \
                 `ro-test-agent` is not on PATH."
            ),
            FailureClass::MissingProvider
        );
    }

    /// Unclassified output stops rather than retries. Retrying something whose
    /// cause nobody identified is how a fleet run spins.
    #[test]
    fn an_unrecognised_failure_is_not_retryable() {
        let class = FailureClass::classify("something nobody anticipated");
        assert!(class.is_fatal());
    }
}
