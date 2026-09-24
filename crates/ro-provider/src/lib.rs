//! AI provider abstraction for ro.
//!
//! Providers are isolated behind a common trait.
//! - Claude: `claude -p --output-format stream-json --verbose`
//! - Codex: `codex exec`
//!
//! All providers return [`InvokeOutput`] so callers don't have to special-case
//! each one.

pub mod claude;
pub mod codex;
pub mod traits;

pub use claude::ClaudeCodeProvider;
pub use codex::CodexProvider;
pub use traits::{InvokeOptions, InvokeOutput, Provider, ProviderErrorKind};
