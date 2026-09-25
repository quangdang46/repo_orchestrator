//! Core types shared across all ro crates.
//!
//! Defines repo specs, run/event IDs, error types, redaction helpers,
//! and shared constants.

pub mod error;
pub mod repo_spec;

pub use error::{CoreError, CoreResult};
pub use repo_spec::RepoSpec;
