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
        } else if s.contains("dirty")
            || s.contains("uncommitted changes")
            || s.contains("your local changes")
        {
            FailureClass::DirtyWorktree
        } else if s.contains("timed out")
            || s.contains("connection refused")
            || s.contains("network")
        {
            FailureClass::NetworkTimeout
        } else if s.contains("git: not found") || s.contains("'git' is not recognized") {
            FailureClass::MissingGit
        } else if s.contains("quality gate") || s.contains("clippy") || s.contains("test failed") {
            FailureClass::QualityGateFailed
        } else if s.contains("secret") || s.contains("blocked by secret scan") {
            FailureClass::SecretScanBlocked
        } else if s.contains("provider") && (s.contains("not found") || s.contains("missing")) {
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

    /// Unclassified output stops rather than retries. Retrying something whose
    /// cause nobody identified is how a fleet run spins.
    #[test]
    fn an_unrecognised_failure_is_not_retryable() {
        let class = FailureClass::classify("something nobody anticipated");
        assert!(class.is_fatal());
    }
}
