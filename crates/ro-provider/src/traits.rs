//! Provider trait + shared invocation types for `ro-provider`.
//!
//! Providers are intentionally small: they shell out to a configured CLI
//! (e.g. `claude`, `codex`), feed it a prompt, and return a structured
//! [`InvokeOutput`]. The trait stays object-safe by using a `Pin<Box<dyn Future>>`
//! return type so we can store providers as `Arc<dyn Provider>`.

use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Failure category for provider invocations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    /// Provider binary not found on PATH / missing.
    Unavailable,
    /// Process timed out before producing a final result.
    Timeout,
    /// Process exited with a non-zero status.
    NonZeroExit,
    /// Provider produced unparseable / unexpected output.
    BadOutput,
    /// I/O error talking to the child process.
    Io,
}

/// Options controlling a single provider invocation.
#[derive(Debug, Clone, Default)]
pub struct InvokeOptions {
    /// Working directory for the spawned process. `None` ⇒ inherit.
    pub cwd: Option<PathBuf>,
    /// Hard timeout. `None` ⇒ no timeout (still bounded by the OS).
    pub timeout: Option<Duration>,
    /// Extra environment variables to set on the child.
    pub env: Vec<(String, String)>,
    /// Additional arguments appended to the provider command line.
    pub extra_args: Vec<String>,
}

impl InvokeOptions {
    /// Convenience builder: fluent timeout setter.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Convenience builder: fluent cwd setter.
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }
}

/// Structured output from a provider call.
///
/// Providers normalize their output into a common shape so callers (sweep,
/// review, MCP) don't have to special-case each one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvokeOutput {
    /// Provider that produced the output (e.g. `"claude"`, `"codex"`).
    pub provider: String,
    /// Process exit code.
    pub exit_code: i32,
    /// Wall-clock duration of the invocation in milliseconds.
    pub duration_ms: u128,
    /// Final assistant-style message (best-effort).
    pub message: Option<String>,
    /// Raw JSON events parsed from stdout (one per line for stream-json).
    pub events: Vec<Value>,
    /// Captured stderr (truncated by the caller if very large).
    pub stderr: String,
}

impl InvokeOutput {
    /// True if the process exited successfully.
    pub fn ok(&self) -> bool {
        self.exit_code == 0
    }
}

/// AI provider trait.
///
/// Providers are object-safe; we return a boxed future so that callers can
/// hold `Arc<dyn Provider>`.
pub trait Provider: Send + Sync {
    /// Provider name (e.g., "claude", "codex").
    fn name(&self) -> &str;

    /// Check if the provider binary is available on PATH.
    fn is_available(&self) -> bool;

    /// Invoke the provider with a prompt and return structured output.
    fn invoke<'a>(
        &'a self,
        prompt: &'a str,
        opts: &'a InvokeOptions,
    ) -> Pin<Box<dyn Future<Output = Result<InvokeOutput>> + Send + 'a>>;
}
