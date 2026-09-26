//! Credential **references**.
//!
//! A credential in ro is never a value. It is a `<scheme>:<name>` reference to
//! a secret that lives somewhere ro does not control:
//!
//! ```toml
//! [auth]
//! https = "env:GH_PERSONAL_TOKEN"   # read this environment variable
//! ssh   = "keychain:ssh-work"       # read this keyring entry
//! ```
//!
//! ## Why this is a type rather than a convention
//!
//! `state.db` is backed up, synced, and pasted into issues. `config.toml` sits
//! in a working directory. The engine's transcript is a file ro does not write
//! but the engine's process can read. A credential that is *allowed* to be a
//! value in any of those places is a credential in all of them, and no later
//! step can un-leak it.
//!
//! So the rejection happens at **parse time**, not at use time: a pasted
//! `ghp_…` fails to become a `CredentialRef` at all, which means it never
//! reaches a file ro writes. The error names the two accepted forms, because a
//! secret-shaped string failing with "invalid input" teaches the user nothing.
//!
//! ## Why a bare `GH_TOKEN` is not equivalent
//!
//! `env:WORK_GH_TOKEN` names one variable for one repo, so a single fleet run
//! can push repo A with a work account and repo B with a personal one. A bare
//! `GH_TOKEN` is one value for the whole environment, which is the "one
//! account for everything" problem per-repo config exists to solve.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::CoreError;

/// Where a named secret can be read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CredentialSource {
    /// An environment variable. `env:NAME` — a *specific* variable, which is
    /// what lets two repos in one run use two different accounts.
    Env,
    /// A keyring entry. `keychain:name` — scheme-specific, so the name is
    /// opaque here and the platform layer decides how to reach it.
    Keychain,
}

impl CredentialSource {
    pub fn as_str(self) -> &'static str {
        match self {
            CredentialSource::Env => "env",
            CredentialSource::Keychain => "keychain",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "env" => Some(CredentialSource::Env),
            "keychain" => Some(CredentialSource::Keychain),
            _ => None,
        }
    }
}

/// A reference to a secret. Never the secret.
///
/// Constructed only through `FromStr`, so a `CredentialRef` in hand is proof
/// that the value was checked. There is deliberately no public constructor
/// that skips that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRef {
    source: CredentialSource,
    name: String,
}

impl CredentialRef {
    pub fn source(&self) -> CredentialSource {
        self.source
    }

    /// The variable or keyring entry name. Not a secret, so not redacted.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl FromStr for CredentialRef {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        let Some((scheme, name)) = trimmed.split_once(':') else {
            return Err(not_a_reference(trimmed));
        };
        let Some(source) = CredentialSource::parse(scheme) else {
            return Err(not_a_reference(trimmed));
        };
        // An empty name is the `env:` case that would otherwise silently fall
        // through to the machine's own credential — the exact silent fallback
        // the design forbids.
        if name.is_empty() {
            return Err(not_a_reference(trimmed));
        }
        Ok(Self {
            source,
            name: name.to_string(),
        })
    }
}

/// One message for every rejection, so the user is told the two forms that
/// work rather than which specific rule they tripped.
fn not_a_reference(value: &str) -> CoreError {
    CoreError::InvalidCredentialRef(format!(
        "credential must be a reference, not a value: {value:?}\n\
         expected 'env:VAR_NAME' (an environment variable) or \
         'keychain:ENTRY_NAME' (a keyring entry)\n\
         ro never stores the secret itself — point at where it already lives"
    ))
}

impl fmt::Display for CredentialRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.source.as_str(), self.name)
    }
}

impl Serialize for CredentialRef {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for CredentialRef {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        CredentialRef::from_str(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_env_and_keychain() {
        let r: CredentialRef = "env:GH_PERSONAL_TOKEN".parse().unwrap();
        assert_eq!(r.source(), CredentialSource::Env);
        assert_eq!(r.name(), "GH_PERSONAL_TOKEN");

        let k: CredentialRef = "keychain:ssh-work".parse().unwrap();
        assert_eq!(k.source(), CredentialSource::Keychain);
        assert_eq!(k.name(), "ssh-work");
    }

    /// The whole point of the type. A pasted token is a plausible mistake, and
    /// it must fail here rather than becoming a value in a file.
    #[test]
    fn rejects_a_pasted_github_token() {
        let err = "ghp_16C7e42F292c6912E7710c838347Ae178B4a".parse::<CredentialRef>();
        assert!(err.is_err(), "a pasted secret must not parse");
        let msg = err.unwrap_err().to_string();
        assert!(
            msg.contains("env:VAR_NAME"),
            "message must name a form: {msg}"
        );
        assert!(
            msg.contains("keychain:ENTRY_NAME"),
            "message must name a form: {msg}"
        );
    }

    /// A `token = "..."` key is the other shape a user reaches for. It has no
    /// scheme, so it is rejected by the same rule.
    #[test]
    fn rejects_a_bare_token_string() {
        assert!("ghp_realtoken".parse::<CredentialRef>().is_err());
        assert!("sk-something".parse::<CredentialRef>().is_err());
    }

    #[test]
    fn rejects_an_unknown_scheme() {
        let err = "file:/etc/token".parse::<CredentialRef>().unwrap_err();
        assert!(err.to_string().contains("env:VAR_NAME"));
    }

    /// `env:` with no name would otherwise resolve to nothing and fall through
    /// to the machine's own credential — a silent fallback, which the design
    /// forbids more emphatically than it forbids a wrong credential.
    #[test]
    fn rejects_an_empty_name() {
        assert!("env:".parse::<CredentialRef>().is_err());
        assert!("keychain:".parse::<CredentialRef>().is_err());
    }

    #[test]
    fn round_trips_through_a_string() {
        let r: CredentialRef = "env:WORK_GH_TOKEN".parse().unwrap();
        assert_eq!(r.to_string(), "env:WORK_GH_TOKEN");
    }

    #[test]
    fn serde_rejects_a_secret_from_a_struct_or_map() {
        // The shape a TOML table deserializes into.
        let r: Result<CredentialRef, _> =
            toml::from_str::<Wrapper>(r#"value = "ghp_pasted_secret""#).map(|w| w.value);
        assert!(r.is_err(), "deserialization must reject a pasted secret");
        let msg = r.unwrap_err().to_string();
        assert!(
            msg.contains("env:VAR_NAME"),
            "the parse error must reach the user: {msg}"
        );

        assert!(toml::from_str::<Wrapper>(r#"value = "env:OK""#).is_ok());
    }

    #[derive(serde::Deserialize)]
    struct Wrapper {
        value: CredentialRef,
    }
}
