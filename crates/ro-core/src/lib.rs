//! Core types shared across all ro crates.
//!
//! Defines repo specs, credential references, run/event IDs, error types,
//! redaction helpers, and shared constants.

pub mod credential;
pub mod error;
pub mod repo_spec;

pub use credential::{CredentialRef, CredentialSource};
pub use error::{CoreError, CoreResult};
pub use repo_spec::RepoSpec;
