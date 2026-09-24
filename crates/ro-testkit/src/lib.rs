//! Test fixtures, fakes, and utilities for ro.
//!
//! Reusable test helpers across the workspace. Every test that touches
//! the filesystem or state should use `tempfile::TempDir` via helpers here.

pub mod fakes;
pub mod fixtures;

pub use fixtures::{git_repo, rust_project};
