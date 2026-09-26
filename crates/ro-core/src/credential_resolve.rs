//! Turning a credential **reference** into a secret.
//!
//! The whole point of a reference is that it names *where* the secret lives.
//! So resolution is a lookup and never a guess.
//!
//! ## An `env:` reference that finds nothing is an error, not a fall-through
//!
//! This is the single most important property of the file. Falling back to
//! `GH_TOKEN` when the named variable is missing would mean one fleet run
//! pushes repo A with a work token and repo B with a personal one, because
//! someone misspelled a variable name — and the difference between the two
//! accounts would be invisible in the output. The error names the variable and
//! says plainly that nothing else was tried.
//!
//! ## The keychain is a default, not a requirement
//!
//! `keychain:` sits behind a default-on feature so a minimal build and a
//! headless CI runner both still compile and work. A hard keychain requirement
//! would make ro unusable in exactly the automation it is meant to help:
//! `credential_ref = "env:CI_GH_TOKEN"` is the answer on a runner with no
//! keyring at all.

use std::fmt;

use crate::credential::{CredentialRef, CredentialSource};
use crate::secret::SecretString;

/// Why a credential could not be obtained.
///
/// Separate from "no credential configured", because those are different
/// situations with different fixes: one is a configuration mistake, the other
/// is a machine that is set up correctly and has nothing to offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialError {
    /// The reference named a variable that is not set in this environment.
    VariableUnset { name: String },
    /// The variable is set but empty.
    VariableEmpty { name: String },
    /// The variable is set but is not valid UTF-8.
    VariableNotUtf8 { name: String },
    /// The keyring entry could not be read.
    KeychainUnavailable { entry: String, reason: String },
    /// This build has no keychain support compiled in.
    KeychainFeatureDisabled { entry: String },
}

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CredentialError::VariableUnset { name } => write!(
                f,
                "the environment variable {name} is not set. This repo names that variable \
                 specifically, so ro did not try any other credential — set it, or change the \
                 reference with `ro config set repos.<name>.credential_ref`."
            ),
            CredentialError::VariableEmpty { name } => {
                write!(f, "the environment variable {name} is set but empty")
            }
            CredentialError::VariableNotUtf8 { name } => {
                write!(f, "the environment variable {name} is not valid UTF-8")
            }
            CredentialError::KeychainUnavailable { entry, reason } => {
                write!(f, "the keyring entry {entry:?} could not be read: {reason}")
            }
            CredentialError::KeychainFeatureDisabled { entry } => write!(
                f,
                "the keyring entry {entry:?} cannot be read because this build has no keychain \
                 support. Use 'env:VAR_NAME' instead, which is also the right answer on a \
                 headless CI runner."
            ),
        }
    }
}

impl std::error::Error for CredentialError {}

/// Resolve a reference to the secret it names.
pub fn resolve(reference: &CredentialRef) -> Result<SecretString, CredentialError> {
    match reference.source() {
        CredentialSource::Env => resolve_env(reference.name()),
        CredentialSource::Keychain => resolve_keychain(reference.name()),
    }
}

fn resolve_env(name: &str) -> Result<SecretString, CredentialError> {
    // `var_os`, not `var`: a token is bytes, and `var` fails on a non-UTF-8
    // value with an error that reads like the variable is missing. Those are
    // indistinguishable from outside, and the difference decides whether the
    // user is told to set a variable or to fix an encoding.
    let value = std::env::var_os(name).ok_or(CredentialError::VariableUnset {
        name: name.to_string(),
    })?;
    if value.is_empty() {
        return Err(CredentialError::VariableEmpty {
            name: name.to_string(),
        });
    }
    let text = value
        .to_str()
        .ok_or_else(|| CredentialError::VariableNotUtf8 {
            name: name.to_string(),
        })?;
    // Whitespace around a token is a paste artefact, not part of the secret.
    // An untrimmed token produces a 401 that reads as "bad credential", which
    // sends the user looking in the wrong place.
    Ok(SecretString::new(text.trim()))
}

#[cfg(feature = "keychain")]
fn resolve_keychain(entry: &str) -> Result<SecretString, CredentialError> {
    let entry_object =
        keyring::Entry::new("ro", entry).map_err(|e| CredentialError::KeychainUnavailable {
            entry: entry.to_string(),
            reason: e.to_string(),
        })?;
    let secret = entry_object
        .get_password()
        .map_err(|e| CredentialError::KeychainUnavailable {
            entry: entry.to_string(),
            reason: e.to_string(),
        })?;
    Ok(SecretString::new(secret.trim()))
}

#[cfg(not(feature = "keychain"))]
fn resolve_keychain(entry: &str) -> Result<SecretString, CredentialError> {
    Err(CredentialError::KeychainFeatureDisabled {
        entry: entry.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The env cases live in one test body. `set_var` is process-global and
    /// Rust runs tests in parallel, so four separate tests setting variables
    /// interfere with each other — which happened once already, with one test
    /// reading another's value and failing for no visible reason.
    #[test]
    fn an_env_reference_resolves_only_the_variable_it_names() {
        let name = "RO_TEST_CREDENTIAL";
        let reference: CredentialRef = format!("env:{name}").parse().expect("a valid reference");

        unsafe {
            std::env::set_var(name, "  the-named-value  ");
            // A different variable, deliberately set to something else. A
            // resolver that reached for it would still "work" and pick the
            // wrong account.
            std::env::set_var("GH_TOKEN", "a-different-account");
        }
        let resolved = resolve(&reference).expect("the named variable is set");
        unsafe {
            std::env::remove_var(name);
            std::env::remove_var("GH_TOKEN");
        }
        assert_eq!(resolved.expose(), "the-named-value");
    }

    /// The failure that matters. An unset variable is an error, and it is not
    /// allowed to quietly become some other credential.
    #[test]
    fn an_unset_variable_is_an_error_and_not_a_fallback() {
        let name = "RO_TEST_ABSENT_CREDENTIAL";
        let reference: CredentialRef = format!("env:{name}").parse().expect("a valid reference");

        unsafe {
            std::env::remove_var(name);
            // Present, and deliberately not the one named.
            std::env::set_var("GH_TOKEN", "should-never-be-used");
        }
        let outcome = resolve(&reference);
        unsafe { std::env::remove_var("GH_TOKEN") };

        let err = outcome.expect_err("an unset variable must not resolve");
        assert_eq!(
            err,
            CredentialError::VariableUnset {
                name: name.to_string()
            }
        );
        let msg = err.to_string();
        assert!(msg.contains(name), "must name the variable: {msg}");
        assert!(
            msg.contains("did not try any other credential"),
            "must say why nothing else was attempted, or the user sets GH_TOKEN and is \
             surprised: {msg}"
        );
    }

    #[test]
    fn an_empty_variable_is_its_own_error() {
        let name = "RO_TEST_EMPTY_CREDENTIAL";
        let reference: CredentialRef = format!("env:{name}").parse().expect("valid");
        unsafe { std::env::set_var(name, "") };
        let err = resolve(&reference).expect_err("an empty variable must not resolve");
        unsafe { std::env::remove_var(name) };
        assert_eq!(
            err,
            CredentialError::VariableEmpty {
                name: name.to_string()
            }
        );
    }

    /// Whatever this build supports, a `keychain:` reference must produce a
    /// named answer rather than a panic or a silent success.
    #[test]
    fn a_keychain_reference_never_silently_succeeds() {
        let reference: CredentialRef = "keychain:ro-test-no-such-entry"
            .parse()
            .expect("a valid reference");
        match resolve(&reference) {
            Err(CredentialError::KeychainUnavailable { entry, .. })
            | Err(CredentialError::KeychainFeatureDisabled { entry }) => {
                assert!(entry.contains("ro-test-no-such-entry"), "got: {entry}")
            }
            // A machine that happens to hold that entry may legitimately
            // resolve it; the assertion is about the silent case.
            Ok(_) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
}
