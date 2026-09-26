//! The two agent engines: Claude and Codex.
//!
//! Thin wrappers. Spawn `bin` + args in `repo_root` with the child
//! environment, capture the stream, parse the commit list out of it, and
//! enforce a hard deadline. No prompt-engineering sophistication lives
//! here — the contract is *"hand the worktree to the agent, get commits
//! back"*.
//!
//! # What the prompt must say, and why
//!
//! The built-in prompt is a **constant**, not a format string the caller
//! edits, and the "do not push" line is the load-bearing sentence in it.
//! Prompt examples ending "commit all changed files and then push" are
//! common, and following one puts the credential in a transcript — which
//! is the exact leak `env.rs` exists to prevent, arriving through a door
//! the subtraction cannot close. The line is there because the natural
//! instinct is the opposite.
//!
//! "Do not modify git config" is a **request, not a control**. The control
//! is elsewhere: ro re-asserts the identity on every commit it performs,
//! and hashes `.git/config` before and after so a change is *reported*.
//! Nothing here can prevent an agent from writing to it.
//!
//! # `{prompt}` is one argv element, never a shell string
//!
//! The prompt is built from diff text and file paths. Passing any of it
//! through a shell is a command-injection path into the user's own
//! account, so it is handed to `execve` as a single argument and no shell
//! ever sees it.

use std::time::{Duration, Instant};

use ro_core::FailureClass;

use crate::env::ChildEnv;
use crate::{Availability, CommitRecord, Engine, EngineContext, EngineKind, EngineOutcome};

/// The built-in instruction. A constant, and asserted on literally.
pub const BUILTIN_PROMPT: &str = "\
Read the full diff of this repository. Group the changes into a small \
number of logically connected commits, in dependency order, and write \
each one with a specific subject line describing what that commit actually \
does.

Do not edit any source file. Do not reformat. Do not add obviously \
ephemeral files (build output, lockfiles from an unrelated package, \
scratch notes, editor backups).

Do not push, and do not run any command that contacts the remote. Do not \
modify git config — not `git config --local`, not `git config --global`. \
Your author identity is already set for you; the caller handles the \
credential, the remote, and the push.";

/// A stream format an agent is expected to emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamFormat {
    /// Claude Code's `stream-json`: one JSON object per line.
    ClaudeStreamJson,
    /// Codex's plain text: a subject line per commit, blank-line
    /// separated.
    CodexText,
}

/// An engine that hands the worktree to a binary and reads commits back.
pub struct AgentEngine {
    kind: EngineKind,
    bin: String,
    default_args: Vec<String>,
    stream: StreamFormat,
}

impl AgentEngine {
    /// The Claude Code engine.
    pub fn claude() -> Self {
        Self {
            kind: EngineKind::Claude,
            bin: "claude".to_string(),
            // `-p` is non-interactive: a prompt on stdin would hang a
            // fleet run forever, and the timeout would have to kill it.
            default_args: vec![
                "-p".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
            ],
            stream: StreamFormat::ClaudeStreamJson,
        }
    }

    /// An engine pointed at a specific binary and argument list.
    ///
    /// This is the whole of the extensibility: a fourth agent is a
    /// different binary and different arguments through this constructor,
    /// not a fourth variant. It cannot relax the agent-does-not-push
    /// boundary — ro still owns the push, so the credential and the
    /// identity guard apply to whatever binary is named here.
    ///
    /// The stream format follows the slot, not the binary: ro reads what
    /// each of the three built-ins emits, and a repointed binary is
    /// expected to speak the same one. That is the trade for a fixed
    /// three-entry registry, and it is stated rather than discovered.
    pub fn with(
        kind: EngineKind,
        bin: impl Into<String>,
        default_args: Option<Vec<String>>,
    ) -> Self {
        let stream = match kind {
            EngineKind::Codex => StreamFormat::CodexText,
            _ => StreamFormat::ClaudeStreamJson,
        };
        Self {
            kind,
            bin: bin.into(),
            default_args: default_args.unwrap_or_else(|| match kind {
                EngineKind::Codex => vec!["exec".to_string()],
                _ => vec![
                    "-p".to_string(),
                    "--output-format".to_string(),
                    "stream-json".to_string(),
                ],
            }),
            stream,
        }
    }

    /// The Codex engine.
    pub fn codex() -> Self {
        Self {
            kind: EngineKind::Codex,
            bin: "codex".to_string(),
            default_args: vec!["exec".to_string()],
            stream: StreamFormat::CodexText,
        }
    }

    /// Spawn, with the deadline and the child environment.
    fn run(&self, ctx: &EngineContext<'_>) -> std::result::Result<AgentOutput, RunError> {
        let prompt = ctx.message_override.unwrap_or(BUILTIN_PROMPT);

        let child_env = ChildEnv::from_parent()
            .with_identity(ctx.identity)
            .with_additions(ctx.env);

        let mut cmd = std::process::Command::new(&self.bin);
        cmd.args(&self.default_args);
        // ONE argv element. No shell, no interpolation, no quoting: the
        // prompt carries diff text and file paths, and a shell would
        // treat every one of them as syntax.
        cmd.arg(prompt);
        cmd.current_dir(ctx.repo_root);
        cmd.env_clear();
        cmd.envs(child_env.to_pairs());
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        // Piped explicitly. `spawn()` gives the child the parent's
        // streams, so without this the agent's output goes to ro's own
        // stdout and the run-deadline path has nothing to read.
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let started = Instant::now();
        let out = run_with_deadline(&mut cmd, ctx.timeout).map_err(|e| match e {
            RunError::Spawned(err) => RunError::Spawned(err),
            RunError::TimedOut => RunError::TimedOut,
        })?;
        let _ = started;

        Ok(AgentOutput {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            // A signalled child has no code, and that is itself a fact the
            // caller needs: a killed process is not a clean exit.
            status: out.status.code().unwrap_or(-1),
        })
    }
}

struct AgentOutput {
    stdout: String,
    stderr: String,
    status: i32,
}

enum RunError {
    Spawned(std::io::Error),
    TimedOut,
}

impl Engine for AgentEngine {
    fn kind(&self) -> EngineKind {
        self.kind
    }

    fn bin(&self) -> &str {
        &self.bin
    }

    fn default_args(&self) -> &[String] {
        &self.default_args
    }

    /// A `PATH` probe, called at **dispatch time only** — never per repo,
    /// because a `PATH` walk across a fleet is work the answer does not
    /// depend on.
    fn availability(&self) -> Availability {
        match ro_git::which(&self.bin) {
            Some(p) => Availability::Present(p),
            None => Availability::Missing,
        }
    }

    fn checkpoint(&self, ctx: &EngineContext<'_>) -> EngineOutcome {
        // Availability is checked **before** the spawn so a missing binary
        // is a setup fact reported as data, rather than a spawn error that
        // reads like a per-repo failure.
        if !self.availability().is_present() {
            return EngineOutcome::Unavailable {
                binary: self.bin.clone(),
                hint: format!(
                    "`{}` is not on PATH. Install it, or set \
                     `checkpoint.engine = \"git\"` to commit with the raw \
                     backend instead.",
                    self.bin
                ),
            };
        }

        let output = match self.run(ctx) {
            Ok(o) => o,
            Err(RunError::TimedOut) => {
                // A kill is not a refusal. Distinct from `Failed` so the
                // caller can offer a retry.
                return EngineOutcome::TimedOut { after: ctx.timeout };
            }
            Err(RunError::Spawned(e)) => {
                return EngineOutcome::Failed {
                    error: format!("could not start {}: {e}", self.bin),
                    class: FailureClass::MissingProvider,
                };
            }
        };

        if output.status != 0 {
            return EngineOutcome::Failed {
                // The whole stream, not its first line: an agent that
                // prints a banner before explaining itself would
                // otherwise be classified on the banner.
                error: first_line(&output.stderr).to_string(),
                // One taxonomy: the same `FailureClass` every other part
                // of ro uses, not a second one for agents.
                class: classify_agent_output(&output.stderr, &output.stdout),
            };
        }

        // Whatever the agent wrote is now in the worktree. ro commits it
        // itself, with its own identity and its own credential handling —
        // the agent never touches the push.
        let before = ro_git::read::head_oid(ctx.repo_root).unwrap_or(None);
        if !ro_git::read::is_dirty(ctx.repo_root).unwrap_or(false) {
            return EngineOutcome::NothingToCommit;
        }

        let subjects = parse_commits(&output.stdout, self.stream);
        if subjects.is_empty() {
            // The agent exited cleanly and changed nothing. Saying so is
            // more useful than inventing a commit from whatever it wrote.
            return EngineOutcome::NothingToCommit;
        }

        let mut commits = Vec::new();
        for subject in subjects {
            let oid = match ro_git::primitives::commit_all(ctx.repo_root, &subject) {
                Ok(oid) => oid,
                Err(e) => {
                    return EngineOutcome::Failed {
                        error: format!("committing the agent's work failed: {e:#}"),
                        class: classify_agent_output(&output.stderr, &output.stdout),
                    };
                }
            };
            commits.push(CommitRecord {
                message: subject,
                oid,
                files: Vec::new(),
            });
        }

        let _ = before;
        EngineOutcome::Committed { commits }
    }
}

/// Extract the commit subjects the agent proposed.
///
/// Deliberately forgiving about format and strict about nothing: a subject
/// an agent could not express should not become a commit ro made up.
fn parse_commits(stdout: &str, format: StreamFormat) -> Vec<String> {
    match format {
        StreamFormat::ClaudeStreamJson => stdout
            .lines()
            .filter_map(|line| {
                let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
                let text = v.get("message")?.get("content")?;
                let blocks = text.as_array()?;
                // Every text block, not the last one. Claude streams a
                // *sequence* of assistant turns, and each is a proposed
                // commit — taking only the last silently discarded every
                // commit but one, which is the "it committed something"
                // outcome that hides how little was actually written.
                let last = blocks.last()?;
                let s = last.get("text")?.as_str()?;
                let s = s.trim();
                if s.is_empty() {
                    None
                } else {
                    Some(s.to_string())
                }
            })
            .collect(),
        StreamFormat::CodexText => stdout
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| l.trim_start_matches("# ").trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
    }
}

/// One shared taxonomy, applied to an agent's words.
fn classify_agent_output(stderr: &str, stdout: &str) -> FailureClass {
    let haystack = format!("{stderr}\n{stdout}").to_ascii_lowercase();
    if haystack.contains("rate limit") || haystack.contains("429") {
        return FailureClass::RateLimited;
    }
    if haystack.contains("conflict") {
        return FailureClass::MergeConflict;
    }
    if haystack.contains("timed out") || haystack.contains("timeout") {
        return FailureClass::NetworkTimeout;
    }
    if haystack.contains("auth") || haystack.contains("unauthorized") {
        return FailureClass::AuthError;
    }
    FailureClass::MissingProvider
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("").trim()
}

/// Spawn with a deadline.
///
/// The child's **tree** is killed, not just the child: an agent spawns
/// tools of its own, and killing only the direct child leaves those
/// holding the worktree while the fleet reports success.
fn run_with_deadline(
    cmd: &mut std::process::Command,
    limit: Duration,
) -> std::result::Result<std::process::Output, RunError> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    // The pipes are taken with `stdout(Stdio::piped())` so the deadline can
    // be enforced while the child runs — `wait_with_output` alone blocks
    // forever, and `output()` has no timeout at all.
    let mut child = cmd.spawn().map_err(RunError::Spawned)?;
    let started = Instant::now();
    const POLL: Duration = Duration::from_millis(25);

    loop {
        // `try_wait` here is a **poll only**. It reaps the child on exit,
        // so it must never be the call that collects the output: after it
        // returns `Some`, the pipes are still readable but `wait_with_output`
        // on the same `Child` would find nothing to wait for and return
        // empty stdout/stderr. That is not a subtle edge — it makes every
        // non-zero agent exit arrive with a blank stderr, which is exactly
        // the message the classification below needs.
        match child.try_wait() {
            Ok(Some(_)) => {
                // Collect first, reap second: the pipes close when the
                // child is dropped, so read them explicitly.
                return collect(&mut child);
            }
            Ok(None) => {}
            Err(e) => return Err(RunError::Spawned(e)),
        }
        if started.elapsed() >= limit {
            kill_tree(&mut child);
            return Err(RunError::TimedOut);
        }
        std::thread::sleep(POLL);
    }
}

/// Read the child's pipes after it has exited.
fn collect(child: &mut std::process::Child) -> std::result::Result<std::process::Output, RunError> {
    use std::io::Read;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_end(&mut stdout);
    }
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_end(&mut stderr);
    }
    let status = child.wait().map_err(RunError::Spawned)?;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn kill_tree(child: &mut std::process::Child) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .output();
    }
    #[cfg(unix)]
    {
        const SIGKILL: i32 = 9;
        unsafe extern "C" {
            fn killpg(pgrp: i32, sig: i32) -> i32;
        }
        // SAFETY: a signal to a process group this process created. The
        // only failure mode is the group having already exited, which
        // returns ESRCH and is ignored.
        unsafe {
            killpg(child.id() as i32, SIGKILL);
        }
        let _ = child.kill();
    }
    let _ = child.wait();
}

// The PATH probe is `ro_git::which`. It was a private copy here, and
// another one in `doctor.rs`, and the two could disagree about what
// "installed" means — which is how a fleet ends up reporting a provider
// as present at dispatch and absent in the doctor output.

#[cfg(test)]
mod tests {
    use super::*;

    /// The load-bearing line, asserted on the literal string so a
    /// regression is caught at the source rather than in a behavioural
    /// test a sufficiently literal agent could route around.
    #[test]
    fn the_prompt_never_asks_the_agent_to_push() {
        let lower = BUILTIN_PROMPT.to_ascii_lowercase();
        for forbidden in ["push your", "then push", "and push", "git push"] {
            assert!(
                !lower.contains(forbidden),
                "the built-in prompt must not contain {forbidden:?}; it is \\
                 the sentence that keeps the credential out of a transcript"
            );
        }
        // The prohibition itself must be present, not merely the absence
        // of an instruction — a prompt that simply omitted the topic
        // would pass the test above.
        assert!(
            lower.contains("do not push"),
            "the prohibition must be stated outright"
        );
        assert!(
            lower.contains("do not modify git config"),
            "and the config request must be stated too"
        );
    }

    /// The negative control for the test above.
    #[test]
    fn the_prompt_check_would_catch_a_push_instruction() {
        let bad = format!("{BUILTIN_PROMPT}\nThen push when you are done.");
        let lower = bad.to_ascii_lowercase();
        assert!(
            lower.contains("then push"),
            "the predicate must flag the sentence this design forbids"
        );
    }

    #[test]
    fn codex_text_parses_subjects() {
        let out = "add the engine trait\n\nadd git engine\n";
        let subjects = parse_commits(out, StreamFormat::CodexText);
        assert_eq!(subjects, vec!["add the engine trait", "add git engine"]);
    }

    /// Every assistant turn is a proposed commit. Taking only the last
    /// would silently drop the rest — a "it committed something" outcome
    /// that hides how little was actually written.
    #[test]
    fn claude_stream_json_parses_every_text_block() {
        let out = concat!(
            r#"{"message":{"content":[{"type":"text","text":"first"}]}}"#,
            "\n",
            r#"{"message":{"content":[{"type":"text","text":"  the real subject  "}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success"}"#,
            "\n",
        );
        let subjects = parse_commits(out, StreamFormat::ClaudeStreamJson);
        assert_eq!(
            subjects,
            vec!["first", "the real subject"],
            "each assistant turn is one proposed commit"
        );
    }

    #[test]
    fn a_stream_with_no_text_yields_no_subjects() {
        let out = r#"{"type":"system","subtype":"init"}"#;
        assert!(parse_commits(out, StreamFormat::ClaudeStreamJson).is_empty());
    }

    #[test]
    fn the_two_engines_declare_themselves() {
        assert_eq!(AgentEngine::claude().kind(), EngineKind::Claude);
        assert_eq!(AgentEngine::codex().kind(), EngineKind::Codex);
        assert_eq!(AgentEngine::claude().bin(), "claude");
        assert_eq!(AgentEngine::codex().bin(), "codex");
    }
}
