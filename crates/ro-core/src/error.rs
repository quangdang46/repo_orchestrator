//! Core error types for ro.

use thiserror::Error;

/// Top-level error type for core operations.
#[derive(Error, Debug)]
pub enum CoreError {
    #[error("invalid repo spec: {0}")]
    InvalidRepoSpec(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience result alias.
pub type CoreResult<T> = Result<T, CoreError>;
