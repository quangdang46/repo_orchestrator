//! Core types shared across all ro crates.
//!
//! Defines repo specs, credential references, run/event IDs, error types,
//! redaction helpers, and shared constants.

pub mod auth;
pub mod credential;
pub mod error;
pub mod failure;
pub mod repo_spec;
pub mod secret;

pub use auth::{AuthPolicy, AuthProvider, CommitIdentity, LoginMismatch};
pub use credential::{CredentialRef, CredentialSource};
pub use error::{CoreError, CoreResult};
pub use failure::FailureClass;
pub use repo_spec::RepoSpec;
pub use secret::{SecretString, redact};

/// The layering this crate exists to protect, asserted rather than documented.
///
/// `ro-core` is depended on by ro-config, ro-github, ro-jobs and ro-sweep, and
/// depends on nothing internal. That is what lets the auth vocabulary and the
/// failure taxonomy be defined here once. An earlier draft defined
/// `AuthPolicy` in both ro-config and ro-github, which needed those two crates
/// to depend on each other — a cycle that `cargo` would have rejected, so the
/// draft could never have compiled.
///
/// This is a test rather than a comment because a comment does not fail. It
/// parses the real manifest, so adding the edge back is a red test rather than
/// a review finding someone has to notice.
#[cfg(test)]
mod layering {
    #[test]
    fn ro_core_depends_on_no_other_workspace_crate() {
        let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
            .expect("ro-core's own manifest is readable");

        let offenders: Vec<&str> = manifest
            .lines()
            .filter(|l| {
                let l = l.trim();
                l.starts_with("ro-") && l.contains("workspace = true")
            })
            .collect();

        assert!(
            offenders.is_empty(),
            "ro-core must stay a leaf of the workspace graph, but declares: {offenders:?}. \
             The auth vocabulary and failure taxonomy are defined here precisely so \
             that no crate needs a second, and the two crates that shared one cannot \
             point at each other."
        );
    }

    /// The two crates the cycle was between, checked from their own manifests.
    /// Checking only ro-core would miss an edge added on the other side.
    #[test]
    fn ro_config_and_ro_github_do_not_depend_on_each_other() {
        let mut offenders = Vec::new();
        for (crate_name, forbidden) in [("ro-config", "ro-github"), ("ro-github", "ro-config")] {
            let path = format!("{}/../{crate_name}/Cargo.toml", env!("CARGO_MANIFEST_DIR"));
            let Ok(manifest) = std::fs::read_to_string(&path) else {
                continue;
            };
            for line in manifest.lines() {
                let line = line.trim();
                if line.starts_with(forbidden) && line.contains("workspace = true") {
                    offenders.push(format!("{crate_name} -> {forbidden}"));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "these two crates must not depend on each other: {offenders:?}"
        );
    }
}
