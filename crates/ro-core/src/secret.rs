//! The one redaction rule in ro.
//!
//! There were three, and they disagreed:
//!
//! | implementation | policy | broken how |
//! |---|---|---|
//! | `ro_core::redaction::redact_secret` | first-N | byte-sliced `&s[..n]`, which **panics on a multi-byte boundary** |
//! | `ro_github::auth::AuthToken::redact` | first-4/last-4 | also byte-sliced |
//! | `ro_sweep::secret_scan::redact` | first-4 | correct, but a second rule |
//!
//! An identity guard needs exactly one, because a guard that redacts
//! differently on each side of a comparison is not a guard.
//!
//! ## `Debug` is the point
//!
//! [`SecretString`] has a **manual** `Debug` that renders `***`. The bug this
//! replaces was an `AuthToken` that *derived* `Debug`, so `{:?}` printed the
//! raw token — and any `tracing` field, any `dbg!`, and any panic message that
//! Debug-formats it leaked the value. Redaction that is opt-in and manual is
//! not redaction.
//!
//! ## What this does not do
//!
//! It protects ro's own logs. It cannot protect a file ro does not write: a
//! secret handed to a subprocess can reach that process's stdout, and an
//! agent's transcript is designed to be read and shared — the opposite of a
//! secret store. So the real defence is not putting the secret where the agent
//! can see it at all. `Debug` is the second line, not the first.

use std::fmt;

/// Characters kept at each end when a preview is genuinely wanted.
const VISIBLE_EACH_END: usize = 4;

/// A string that cannot be printed by accident.
///
/// There is no public constructor that skips anything, and there is no
/// accessor returning the inner value without an explicit, greppable name —
/// `expose()` — so every read of the secret is a place a reviewer can find.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    /// The secret itself.
    ///
    /// Named for what it does, not for what it returns. Every call site is a
    /// place where the value leaves the type, so `grep expose` is a complete
    /// inventory of them.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// A short, non-reversible preview for a log line that needs to say
    /// *which* credential was used.
    ///
    /// `chars()`, not byte slicing: a secret is arbitrary bytes, and slicing
    /// at a fixed index panics in the middle of a multi-byte character. That
    /// was a real defect in the implementation this replaces.
    pub fn preview(&self) -> String {
        redact(&self.0)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl fmt::Display for SecretString {
    /// `Display` is `***` as well, not the value.
    ///
    /// `Display` is what `{}` and string interpolation reach for, and it is the
    /// one a careless `format!("{secret}")` would land on. Making it the value
    /// would leave a hole exactly the width of the bug being fixed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl From<String> for SecretString {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// first-4 + `…` + last-4, or `****` when the secret is too short to reveal
/// both ends without revealing most of it.
///
/// "Too short" is measured in characters, because that is the unit a human
/// reads, and a 6-character secret is not meaningfully protected by showing its
/// first 4.
pub fn redact(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= VISIBLE_EACH_END * 2 {
        return "****".to_string();
    }
    let head: String = chars[..VISIBLE_EACH_END].iter().collect();
    let tail: String = chars[chars.len() - VISIBLE_EACH_END..].iter().collect();
    format!("{head}…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug. A derived `Debug` printed the raw token; any tracing field,
    /// `dbg!`, or Debug-formatted panic message leaked it.
    #[test]
    fn debug_renders_stars_and_never_the_value() {
        let s = SecretString::new("ghp_16C7e42F292c6912E7710c838347Ae178B4a");
        assert_eq!(format!("{s:?}"), "***");
        assert!(!format!("{s:?}").contains("ghp_"));

        // `Display` is the other interpolation, and the one a careless
        // `format!("{secret}")` lands on.
        assert_eq!(format!("{s}"), "***");
        assert!(!format!("{s}").contains("ghp_"));
    }

    /// Nested in a struct, which is how it actually reaches a log line.
    #[test]
    fn a_derived_debug_on_a_holder_still_renders_stars() {
        #[derive(Debug)]
        struct Holder {
            name: &'static str,
            credential: SecretString,
        }
        let h = Holder {
            name: "work",
            credential: SecretString::new("github_pat_11ABCDEFG0abcdefghij"),
        };
        let rendered = format!("{h:?}");
        assert!(rendered.contains("work"), "non-secret fields stay visible");
        assert!(
            !rendered.contains("11ABCDEFG"),
            "the secret must not appear, got: {rendered}"
        );
    }

    /// The defect the old `redact_secret` had: `&secret[..4]` panics when byte
    /// 4 lands inside a multi-byte character.
    #[test]
    fn redaction_does_not_panic_on_a_multibyte_secret() {
        let s = SecretString::new("héllo wörld — this is a secret");
        let preview = s.preview();
        assert!(preview.contains('…'));
        // Same for a secret that is *entirely* multi-byte, where a byte-slice
        // at any small index would land mid-character.
        let all_multibyte = SecretString::new("日本語のシークレット");
        assert!(all_multibyte.preview().contains('…'));
    }

    /// A short secret must not be revealed by showing most of it.
    #[test]
    fn a_short_secret_is_hidden_completely() {
        assert_eq!(redact(""), "****");
        assert_eq!(redact("abc"), "****");
        assert_eq!(redact("abcdefgh"), "****");
        assert_eq!(redact("abcdefghi"), "abcd…fghi");
    }

    /// The one rule, applied. A guard that redacted differently on each side of
    /// a comparison would not be a guard.
    #[test]
    fn the_preview_is_first_four_plus_last_four() {
        // Last four characters of ...e178B4a are "8B4a".
        assert_eq!(
            redact("ghp_16C7e42F292c6912E7710c838347Ae178B4a"),
            "ghp_…8B4a"
        );
    }

    /// A PAT-shaped string must not survive a log line. This is the property
    /// the whole type exists to provide, asserted against the actual pattern
    /// rather than against the absence of one known value.
    #[test]
    fn no_pat_shaped_string_survives_debug_output() {
        let patterns = [
            "ghp_16C7e42F292c6912E7710c838347Ae178B4a",
            "github_pat_11ABCDEFG0aBcDeFgHiJkLmNoPqRsT",
        ];
        for raw in patterns {
            let s = SecretString::new(raw);
            for rendered in [format!("{s:?}"), format!("{s}"), s.preview()] {
                let looks_like_a_pat = rendered
                    .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .any(|token| {
                        (token.starts_with("ghp_") || token.starts_with("github_pat_"))
                            && token.len() >= 20
                    });
                assert!(
                    !looks_like_a_pat,
                    "a PAT-shaped token survived in {rendered:?}"
                );
            }
        }
    }

    /// `expose` is the one door, and it is greppable.
    #[test]
    fn expose_returns_the_value() {
        assert_eq!(SecretString::new("s3cret").expose(), "s3cret");
    }
}
