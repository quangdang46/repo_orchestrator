#![recursion_limit = "512"]

//! GitHub API client for ro.
//!
//! Auth discovery and the permission probe `ro doctor` runs. The repo /
//! issue / check listing API was removed: nothing in the workspace ever
//! called it, and an unused client is a maintenance surface that drifts
//! from the API it wraps.

pub mod auth;
pub mod permissions;

#[cfg(test)]
mod fake_github;

pub use auth::{AuthToken, build_client, discover_token};
