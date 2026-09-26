//! Capturing everything ro wrote, for the leak-chain tests.
//!
//! The leak chain is: a credential is read, handed to one `git` invocation,
//! and must not appear in anything the user can see afterwards. Asserting
//! that means reading the real output, not inspecting a type's `Debug` —
//! which is the mistake that made the first version of ro-dab.8's test
//! pass vacuously.
//!
//! [`Captured`] collects both streams and offers predicates over the
//! combined text, so a test asserts on "this token is nowhere in what ro
//! printed" in one call.

use std::process::{Command, Output};
/// What a run wrote.
#[derive(Debug, Clone, Default)]
pub struct Captured {
    pub stdout: String,
    pub stderr: String,
}

impl Captured {
    /// Run a command, capturing both streams.
    ///
    /// Does **not** assert on the exit status: several of the tests this
    /// exists for are about a run that legitimately fails (`ro doctor` on a
    /// broken repo must exit non-zero *and* still print every row), so the
    /// status is returned rather than enforced.
    pub fn run(mut cmd: Command) -> (Self, Output) {
        let out = cmd
            .output()
            .unwrap_or_else(|e| panic!("spawning {:?}: {e}", cmd.get_program()));
        let captured = Self {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        };
        (captured, out)
    }

    /// Both streams as one string.
    ///
    /// Combined rather than separate because a leak does not care which
    /// stream it took: a token on stderr is just as leaked as one on stdout.
    /// A test that only checks stdout passes while the token goes to stderr.
    pub fn combined(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }

    /// Does any PAT-shaped value appear in the output?
    ///
    /// Matches on the *shape*, not on one known string, so it keeps working
    /// when the token format changes. Requires a digit so prose containing
    /// the word `ghp_` does not trip it.
    pub fn contains_pat(&self) -> bool {
        contains_pat(&self.combined())
    }

    /// Every token-shaped run of 20+ chars, **truncated** for a message.
    ///
    /// Owned strings rather than borrowed slices because the failure message
    /// goes to CI logs, and a full token printed to explain a leak is a
    /// second leak. Truncating here rather than at the call site means no
    /// caller can forget.
    pub fn suspicious_tokens(&self) -> Vec<String> {
        self.combined()
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .filter(|t| {
                (t.starts_with("ghp_") || t.starts_with("github_pat_"))
                    && t.len() >= 20
                    && t.chars().any(|c| c.is_ascii_digit())
            })
            .map(redact_token)
            .collect()
    }

    /// Assert nothing PAT-shaped was printed, quoting the leak if it was.
    ///
    /// The message lists the offending tokens *truncated*, because the
    /// failure message lands in CI logs and a full token printed to explain
    /// a leak is a second leak.
    pub fn assert_no_pat(&self) {
        let found = self.suspicious_tokens();
        assert!(
            found.is_empty(),
            "a credential-shaped value reached the output: {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            found,
            self.stdout,
            self.stderr
        );
    }

    /// Assert `needle` appears somewhere, quoting the whole capture.
    pub fn assert_contains(&self, needle: &str) {
        assert!(
            self.combined().contains(needle),
            "expected {needle:?} in the output, got:\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.stdout,
            self.stderr
        );
    }
}

/// First four and last four, `…` between. Char-safe: byte slicing would
/// panic on a multi-byte boundary, which is the bug ro-dab.8 removed from
/// the tree and one this crate must not reintroduce.
pub fn redact_token(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() <= 8 {
        return "****".to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// The PAT predicate, shared so every caller agrees on the shape.
pub fn contains_pat(haystack: &str) -> bool {
    haystack
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|t| {
            (t.starts_with("ghp_") || t.starts_with("github_pat_"))
                && t.len() >= 20
                && t.chars().any(|c| c.is_ascii_digit())
        })
}
