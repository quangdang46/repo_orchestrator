//! Claude Code provider.
//!
//! Invokes `claude -p --output-format stream-json --verbose` with configurable
//! args. Parses the stream-json output (one JSON event per line) and returns
//! a structured [`InvokeOutput`]. Handles missing binaries, non-zero exits,
//! timeouts, and bad output.

use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::traits::{InvokeOptions, InvokeOutput, Provider};

/// Claude Code provider.
///
/// Spawns the `claude` CLI in stream-json mode, feeds the prompt on stdin,
/// then collects the resulting JSON events and a best-effort final message.
pub struct ClaudeCodeProvider {
    program: PathBuf,
    base_args: Vec<String>,
}

impl ClaudeCodeProvider {
    /// Default provider using `claude` from PATH and the canonical
    /// stream-json arguments.
    pub fn new() -> Self {
        Self {
            program: PathBuf::from("claude"),
            base_args: vec![
                "-p".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--verbose".to_string(),
            ],
        }
    }

    /// Override the binary path (useful for tests / non-standard installs).
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

impl Default for ClaudeCodeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for ClaudeCodeProvider {
    fn name(&self) -> &str {
        "claude"
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
                    "claude binary not found on PATH (looked for {:?})",
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
            // Sane defaults so headless invocations never block on TTY.
            cmd.env("CI", "true");
            cmd.env("CLAUDE_NO_TELEMETRY", "1");
            for (k, v) in &opts.env {
                cmd.env(k, v);
            }

            let started = Instant::now();
            let mut child = cmd
                .spawn()
                .with_context(|| format!("failed to spawn claude at {:?}", self.program))?;

            // Pipe the prompt to stdin and close it.
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(prompt.as_bytes())
                    .await
                    .context("failed to write prompt to claude stdin")?;
                stdin
                    .shutdown()
                    .await
                    .context("failed to close claude stdin")?;
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
                            // Timed out - try to clean up the child.
                            let _ = child.start_kill();
                            return Err(anyhow!("claude invocation timed out after {:?}", timeout));
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
            let events = parse_stream_json(&stdout_text);
            let message = extract_final_message(&events);

            Ok(InvokeOutput {
                provider: "claude".to_string(),
                exit_code: status.code().unwrap_or(-1),
                duration_ms: started.elapsed().as_millis(),
                message,
                events,
                stderr: stderr_text,
            })
        })
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

/// Parse stream-json output: one JSON object per non-empty line, lenient about
/// malformed lines (they're dropped rather than failing the whole call).
pub(crate) fn parse_stream_json(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

/// Extract the final assistant message text from a stream-json event sequence.
///
/// Claude emits events of various shapes; we look at:
///   * `result` events with a `result` field (newer format)
///   * `assistant`/`message` events with a `text` field
///   * any object containing `content` as a string, last one wins
pub(crate) fn extract_final_message(events: &[Value]) -> Option<String> {
    let mut last: Option<String> = None;
    for ev in events {
        if let Some(obj) = ev.as_object() {
            // 1. result event (claude --output-format stream-json final summary)
            if obj.get("type").and_then(Value::as_str) == Some("result") {
                if let Some(s) = obj.get("result").and_then(Value::as_str) {
                    last = Some(s.to_string());
                    continue;
                }
            }
            // 2. assistant / message events
            if let Some(text) = obj.get("text").and_then(Value::as_str) {
                last = Some(text.to_string());
                continue;
            }
            // 3. message.content (array of text parts)
            if let Some(message) = obj.get("message").and_then(Value::as_object) {
                if let Some(arr) = message.get("content").and_then(Value::as_array) {
                    let mut combined = String::new();
                    for part in arr {
                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                            combined.push_str(t);
                        }
                    }
                    if !combined.is_empty() {
                        last = Some(combined);
                    }
                }
            }
            // 4. plain content string
            if let Some(text) = obj.get("content").and_then(Value::as_str) {
                last = Some(text.to_string());
            }
        }
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stream_json_skipping_blanks_and_garbage() {
        let stdout = "\n{\"type\":\"system\"}\nnot-json\n{\"type\":\"result\",\"result\":\"hi\"}\n";
        let events = parse_stream_json(stdout);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "system");
        assert_eq!(events[1]["type"], "result");
    }

    #[test]
    fn extracts_final_message_from_result_event() {
        let events = parse_stream_json(
            "{\"type\":\"system\"}\n{\"type\":\"result\",\"result\":\"hello world\"}\n",
        );
        assert_eq!(
            extract_final_message(&events),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn extracts_final_message_from_message_content_array() {
        let events = parse_stream_json(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"},{\"type\":\"text\",\"text\":\" you\"}]}}\n",
        );
        assert_eq!(extract_final_message(&events), Some("hi you".to_string()));
    }

    #[test]
    fn extract_returns_none_for_no_message() {
        let events = parse_stream_json("{\"type\":\"system\"}\n");
        assert_eq!(extract_final_message(&events), None);
    }

    #[test]
    fn name_is_claude_and_unavailable_for_bogus_path() {
        let p = ClaudeCodeProvider::new().with_program("/definitely/does/not/exist/claude-zzz");
        assert_eq!(p.name(), "claude");
        assert!(!p.is_available());
    }

    #[tokio::test]
    async fn errors_fast_when_unavailable() {
        let p = ClaudeCodeProvider::new().with_program("/definitely/does/not/exist/claude-zzz");
        let opts = InvokeOptions::default();
        let err = p.invoke("hi", &opts).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not found"), "unexpected: {msg}");
    }
}
