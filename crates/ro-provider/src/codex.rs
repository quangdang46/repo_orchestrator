//! OpenAI Codex provider.
//!
//! Invokes `codex exec` with configurable args, pipes the prompt on stdin,
//! collects stdout/stderr, parses any JSON-shaped output, and returns an
//! [`InvokeOutput`]. Same shape as [`crate::claude`] so the two providers
//! are interchangeable from the caller's perspective.

use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::traits::{InvokeOptions, InvokeOutput, Provider};

/// OpenAI Codex provider.
pub struct CodexProvider {
    program: PathBuf,
    base_args: Vec<String>,
}

impl CodexProvider {
    /// Default provider using `codex exec --json` from PATH.
    pub fn new() -> Self {
        Self {
            program: PathBuf::from("codex"),
            base_args: vec!["exec".to_string(), "--json".to_string()],
        }
    }

    /// Override the binary path (useful for tests).
    pub fn with_program(mut self, program: impl Into<PathBuf>) -> Self {
        self.program = program.into();
        self
    }

    /// Replace the base argument list (advanced).
    pub fn with_base_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.base_args = args.into_iter().map(Into::into).collect();
        self
    }
}

impl Default for CodexProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Best-effort PATH lookup for a binary name or absolute path.
fn which_exists(program: &PathBuf) -> bool {
    let s = program.as_os_str();
    if s.is_empty() {
        return false;
    }
    if program.is_absolute() || program.components().count() > 1 {
        return program.exists();
    }
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    let exts: Vec<&str> = if cfg!(windows) {
        vec!["", ".exe", ".cmd", ".bat"]
    } else {
        vec![""]
    };
    let sep = if cfg!(windows) { ';' } else { ':' };
    for dir in path.split(sep) {
        let base = std::path::Path::new(dir).join(program);
        for ext in &exts {
            let mut candidate = base.clone();
            if !ext.is_empty() {
                let mut osstr = candidate.into_os_string();
                osstr.push(ext);
                candidate = PathBuf::from(osstr);
            }
            if candidate.exists() {
                return true;
            }
        }
    }
    false
}

/// Parse `codex exec --json` stdout. One JSON object per non-empty line;
/// garbage lines are dropped to survive version drift.
pub(crate) fn parse_codex_output(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

/// Best-effort final-message extraction. Codex's exact event shape varies by
/// version so we try several common keys.
pub(crate) fn extract_final_message(events: &[Value]) -> Option<String> {
    let mut last: Option<String> = None;
    for ev in events {
        if let Some(obj) = ev.as_object() {
            if obj.get("type").and_then(Value::as_str) == Some("result") {
                if let Some(s) = obj.get("result").and_then(Value::as_str) {
                    last = Some(s.to_string());
                    continue;
                }
            }
            for key in ["message", "output", "text", "content"] {
                if let Some(s) = obj.get(key).and_then(Value::as_str) {
                    last = Some(s.to_string());
                }
            }
        }
    }
    last
}

impl Provider for CodexProvider {
    fn name(&self) -> &str {
        "codex"
    }

    fn is_available(&self) -> bool {
        which_exists(&self.program)
    }

    fn invoke<'a>(
        &'a self,
        prompt: &'a str,
        opts: &'a InvokeOptions,
    ) -> Pin<Box<dyn Future<Output = Result<InvokeOutput>> + Send + 'a>> {
        Box::pin(async move {
            if !self.is_available() {
                return Err(anyhow!(
                    "codex binary not found on PATH (looked for {:?})",
                    self.program
                ));
            }

            let mut cmd = Command::new(&self.program);
            cmd.args(&self.base_args)
                .args(&opts.extra_args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            if let Some(cwd) = &opts.cwd {
                cmd.current_dir(cwd);
            }
            cmd.env("CI", "true");
            for (k, v) in &opts.env {
                cmd.env(k, v);
            }

            let started = Instant::now();
            let mut child = cmd
                .spawn()
                .with_context(|| format!("failed to spawn codex at {:?}", self.program))?;

            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(prompt.as_bytes())
                    .await
                    .context("failed to write prompt to codex stdin")?;
                stdin
                    .shutdown()
                    .await
                    .context("failed to close codex stdin")?;
            }

            let mut stdout = child.stdout.take().expect("stdout piped");
            let mut stderr = child.stderr.take().expect("stderr piped");

            let mut stdout_buf = Vec::new();
            let mut stderr_buf = Vec::new();

            let stdout_fut = stdout.read_to_end(&mut stdout_buf);
            let stderr_fut = stderr.read_to_end(&mut stderr_buf);
            let wait_fut = child.wait();

            let (status, _, _) = match opts.timeout {
                Some(timeout) => {
                    match tokio::time::timeout(timeout, async {
                        let (s, a, b) = tokio::join!(wait_fut, stdout_fut, stderr_fut);
                        (s, a, b)
                    })
                    .await
                    {
                        Ok((s, a, b)) => (s?, a?, b?),
                        Err(_) => {
                            let _ = child.start_kill();
                            return Err(anyhow!("codex invocation timed out after {:?}", timeout));
                        }
                    }
                }
                None => {
                    let (s, a, b) = tokio::join!(wait_fut, stdout_fut, stderr_fut);
                    (s?, a?, b?)
                }
            };

            let stdout_text = String::from_utf8_lossy(&stdout_buf).to_string();
            let stderr_text = String::from_utf8_lossy(&stderr_buf).to_string();
            let events = parse_codex_output(&stdout_text);
            let message = extract_final_message(&events);

            Ok(InvokeOutput {
                provider: "codex".to_string(),
                exit_code: status.code().unwrap_or(-1),
                duration_ms: started.elapsed().as_millis(),
                message,
                events,
                stderr: stderr_text,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_codex() {
        assert_eq!(CodexProvider::new().name(), "codex");
    }

    #[test]
    fn unavailable_for_bogus_path() {
        let p = CodexProvider::new().with_program("/definitely/does/not/exist/codex-zzz");
        assert!(!p.is_available());
    }

    #[test]
    fn parses_codex_json_skipping_blanks() {
        let stdout = "\n{\"type\":\"start\"}\nnot-json\n{\"type\":\"result\",\"result\":\"ok\"}\n";
        let events = parse_codex_output(stdout);
        assert_eq!(events.len(), 2);
        assert_eq!(events[1]["type"], "result");
    }

    #[test]
    fn extracts_message_from_result_event() {
        let events =
            parse_codex_output("{\"type\":\"start\"}\n{\"type\":\"result\",\"result\":\"ok\"}\n");
        assert_eq!(extract_final_message(&events), Some("ok".to_string()));
    }

    #[test]
    fn extracts_message_from_message_field() {
        let events = parse_codex_output("{\"message\":\"hello\"}\n");
        assert_eq!(extract_final_message(&events), Some("hello".to_string()));
    }

    #[tokio::test]
    async fn errors_fast_when_unavailable() {
        let p = CodexProvider::new().with_program("/definitely/does/not/exist/codex-zzz");
        let opts = InvokeOptions::default();
        let err = p.invoke("hi", &opts).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not found"), "unexpected: {msg}");
    }
}
