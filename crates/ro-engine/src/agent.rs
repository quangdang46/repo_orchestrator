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
Read the full diff of this repository and group the changes into a small \
number of logically connected commits, in dependency order.

Leave every change in the working tree. Do NOT run `git commit` — the \
caller stages and commits each group itself, with the author identity it \
resolved. A commit you make yourself is one the caller cannot attribute, \
cannot split, and will not push.

When you are done, output the plan and nothing else after it, as a JSON \
array inside a fenced block:

```ro-commits
[{\"subject\": \"one line, imperative, describing what the commit does\",
  \"files\": [\"path/one\", \"path/two\"]}]
```

Every changed path must appear in exactly one group. Paths are relative \
to the repository root. A file that is not in any group is not committed.

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
            //
            // `--verbose` is not optional. `claude` refuses
            // `--output-format stream-json` together with `--print`
            // unless `--verbose` is also present, so the first real
            // `ro commit` with the default engine failed with
            // "When using --print, --output-format=stream-json requires
            // --verbose" — the flagship path, on the very first use, and
            // only on a machine where `claude` was actually installed.
            default_args: vec![
                "-p".to_string(),
                "--verbose".to_string(),
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
                // Same pair as `claude()` above, for the same reason.
                _ => vec![
                    "-p".to_string(),
                    "--verbose".to_string(),
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

    /// The binary to actually spawn, resolved for this platform.
    ///
    /// On Unix that is the configured name. On Windows the loader does
    /// not consult `PATHEXT` for a bare name, so each known extension is
    /// tried against `PATH` and the first hit is used. A configured name
    /// that already carries an extension is spawned as given.
    fn resolve_program(&self) -> std::path::PathBuf {
        let configured = std::path::Path::new(&self.bin);
        if !cfg!(windows) || configured.extension().is_some() {
            return configured.to_path_buf();
        }
        // A configured path — anything with a separator — is taken as
        // written; only a bare name is searched for.
        if self.bin.contains('/') || self.bin.contains('\\') {
            return configured.to_path_buf();
        }
        if let Some(found) = std::env::var_os("PATH").and_then(|path| {
            let pathext =
                std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
            find_with_extensions(&self.bin, &path, &pathext)
        }) {
            return found;
        }
        // Nothing found. The bare name goes back to `Command`, which will
        // report "program not found" naming what it tried — the same
        // message a user gets for a genuinely missing engine.
        configured.to_path_buf()
    }

    /// Spawn, with the deadline and the child environment.
    fn run(&self, ctx: &EngineContext<'_>) -> std::result::Result<AgentOutput, RunError> {
        let prompt = ctx.message_override.unwrap_or(BUILTIN_PROMPT);

        let child_env = ChildEnv::from_parent()
            .with_identity(ctx.identity)
            .with_additions(ctx.env);

        // Resolve the binary the way the platform can actually spawn it.
        //
        // A probe on a real `windows-latest` runner showed that
        // `Command::new` does **not** apply `PATHEXT` to a bare name:
        // `claude` was "program not found" while `claude.cmd` in the same
        // directory ran. So on Windows the configured name is tried with
        // each extension the platform knows, and the first that exists on
        // `PATH` is what gets spawned.
        //
        // A user who configured `bin = "claude"` has configured the name
        // they type; they have not configured an extension, and making
        // them type `.cmd` would be the tool's problem, not theirs.
        let program = self.resolve_program();

        // A `.cmd` is not an executable. The loader hands one to
        // `cmd.exe`, and `cmd.exe` **cannot carry a newline in an
        // argument** — `Command` refuses the spawn outright with
        // "batch file arguments are invalid" before the child ever runs.
        //
        // `BUILTIN_PROMPT` is multi-paragraph by design, so this is not a
        // corner case: it is every Windows user whose agent was installed
        // by npm, which is what `claude` and `codex` ship. On Unix the
        // prompt is one `execve` argument and this never arises.
        //
        // So on Windows the prompt travels on **stdin**, where no shell
        // ever parses it — which keeps the invariant this module is built
        // on ("no shell sees the prompt") *more* strictly than the argv
        // path, since argv on Windows would necessarily go through
        // `cmd.exe`'s own quoting and expansion rules. The prompt arrives
        // byte-for-byte, as one value, which is the whole claim.
        let via_stdin = is_batch_file(&program);
        let mut cmd = if via_stdin {
            let mut c = std::process::Command::new(comspec());
            // `/c` then the program, as separate arguments. Passing the
            // whole thing as one quoted string is the variant where
            // `cmd.exe` strips the outer quotes and then mis-parses the
            // path, and it is the only difference between the two forms.
            c.arg("/c").arg(&program);
            c
        } else {
            std::process::Command::new(&program)
        };
        cmd.args(&self.default_args);
        if !via_stdin {
            // ONE argv element. No shell, no interpolation, no quoting: the
            // prompt carries diff text and file paths, and a shell would
            // treat every one of them as syntax.
            cmd.arg(prompt);
        }
        cmd.current_dir(ctx.repo_root);
        cmd.env_clear();
        cmd.envs(child_env.to_pairs());
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        // Piped explicitly. `spawn()` gives the child the parent's
        // streams, so without this the agent's output goes to ro's own
        // stdout and the run-deadline path has nothing to read.
        //
        // On the stdin path this is the prompt's only route to the child,
        // so it must be a pipe rather than `/dev/null`.
        cmd.stdin(if via_stdin {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        });
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let started = Instant::now();
        let out = run_with_deadline(
            &mut cmd,
            ctx.timeout,
            if via_stdin { Some(prompt) } else { None },
        )
        .map_err(|e| match e {
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
                // otherwise be *reported* on the banner, even though it
                // is classified on the whole thing. See `failure_message`.
                error: failure_message(&output.stderr),
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
            // A clean tree has two causes, and they are not the same news:
            // the agent changed nothing, or the agent **committed it
            // itself**. Only the ref tells them apart, and `before` is the
            // only record of what it was.
            //
            // The second case is not hypothetical. The prompt says to group
            // the changes into commits, and an agent that reads that as an
            // instruction rather than a description runs `git commit`. It
            // did exactly that on a scratch repo: two commits landed, and
            // ro reported "nothing to commit" — a false report about work
            // that had already happened. On `ro ship` that also means no
            // push, so the work sits local with nothing saying it is
            // stranded.
            let after = ro_git::read::head_oid(ctx.repo_root).unwrap_or(None);
            if before != after {
                let commits: Vec<CommitRecord> = ro_git::read::commits_between(
                    ctx.repo_root,
                    before.as_deref(),
                    after.as_deref(),
                )
                .unwrap_or_default()
                .into_iter()
                .filter_map(|oid| {
                    Some(CommitRecord {
                        message: ro_git::read::commit_subject(ctx.repo_root, &oid)?,
                        oid,
                        files: Vec::new(),
                    })
                })
                .collect();
                if !commits.is_empty() {
                    return EngineOutcome::Committed { commits };
                }
            }
            return EngineOutcome::NothingToCommit;
        }

        let subjects = parse_commits(&output.stdout, self.stream);
        if subjects.is_empty() {
            // The agent exited cleanly and changed nothing. Saying so is
            // more useful than inventing a commit from whatever it wrote.
            return EngineOutcome::NothingToCommit;
        }

        let mut commits = Vec::new();
        // The engine set the author on the *child's* environment. ro commits
        // some of this work itself, and a per-invocation `-c` has to be
        // re-asserted here: the agent may have run `git config user.email
        // something-else` in between, and that write persists in
        // `.git/config` for every commit after it.
        let author = ctx
            .identity
            .as_ref()
            .map(|i| (i.name.as_str(), i.email.as_str()));
        for group in subjects {
            // Each group is staged on its own, so the split the agent
            // reasoned about is the split that lands. Committing the whole
            // index N times — which is what this did before — takes every
            // file in the first commit and leaves the rest with an empty
            // index, so the second commit failed with nothing left to
            // commit and the run reported a failure after the work was
            // already half done.
            let paths: Vec<std::path::PathBuf> =
                group.files.iter().map(std::path::PathBuf::from).collect();
            if let Err(e) = ro_git::primitives::stage_paths(ctx.repo_root, &paths) {
                return EngineOutcome::Failed {
                    error: format!("staging {:?} failed: {e:#}", group.subject),
                    class: FailureClass::DirtyWorktree,
                };
            }
            let oid = match ro_git::primitives::commit_all_as(
                ctx.repo_root,
                &group.subject,
                author,
            ) {
                Ok(oid) => oid,
                Err(e) => {
                    return EngineOutcome::Failed {
                        error: format!("committing the agent's work failed: {e:#}"),
                        class: classify_agent_output(&output.stderr, &output.stdout),
                    };
                }
            };
            commits.push(CommitRecord {
                message: group.subject,
                oid,
                files: group.files,
            });
        }


        EngineOutcome::Committed { commits }
    }
}

/// One proposed commit: a subject, and the files that belong in it.
///
/// The file list is the whole point. A subject on its own cannot be turned
/// into more than one commit — `git commit` takes whatever is in the index,
/// so committing N times without staging between them produces one commit
/// and N−1 empty ones.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CommitGroup {
    subject: String,
    files: Vec<String>,
}

/// Extract the commit groups the agent proposed.
///
/// Deliberately forgiving about format and strict about nothing: a group an
/// agent could not express should not become a commit ro made up. A
/// response with no parsable plan is `NothingToCommit` rather than a single
/// commit built from whatever prose happened to be last.
fn parse_commits(stdout: &str, format: StreamFormat) -> Vec<CommitGroup> {
    let text = match format {
        StreamFormat::ClaudeStreamJson => {
            let mut all = String::new();
            for line in stdout.lines() {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                    continue;
                };
                let Some(content) = v.get("message").and_then(|m| m.get("content")) else {
                    continue;
                };
                let Some(blocks) = content.as_array() else { continue };
                // Every text block, not the last one. Claude streams a
                // *sequence* of assistant turns, and the plan lands in
                // whichever turn produced it.
                for b in blocks {
                    if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                        all.push_str(t);
                        all.push('\n');
                    }
                }
            }
            all
        }
        StreamFormat::CodexText => stdout.to_string(),
    };

    parse_plan(&text)
}

/// Pull the `ro-commits` block out of an agent's prose and parse it.
///
/// Scans **every** fenced block rather than the first, because an agent
/// that explains itself before answering puts other blocks first. A block
/// that does not parse is skipped, not fatal: the worst case for a
/// malformed one is that the run reports nothing to commit, which is
/// recoverable, whereas refusing to read the rest of the output is not.
fn parse_plan(text: &str) -> Vec<CommitGroup> {
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        let after = &rest[start + 3..];
        let (lang, body) = match after.find('\n') {
            Some(i) => (&after[..i], &after[i + 1..]),
            None => break,
        };
        let end_at = match body.find("```") {
            Some(i) => i,
            None => break,
        };
        let block = &body[..end_at];
        if lang.trim() == "ro-commits" {
            if let Ok(serde_json::Value::Array(items)) =
                serde_json::from_str::<serde_json::Value>(block.trim())
            {
                let groups: Vec<CommitGroup> = items
                    .iter()
                    .filter_map(|it| {
                        let subject = it.get("subject")?.as_str()?.trim().to_string();
                        if subject.is_empty() {
                            return None;
                        }
                        let files: Vec<String> = it
                            .get("files")?
                            .as_array()?
                            .iter()
                            .filter_map(|f| f.as_str())
                            .map(|f| f.trim().trim_matches('"').to_string())
                            .filter(|f| !f.is_empty())
                            .collect();
                        Some(CommitGroup { subject, files })
                    })
                    .collect();
                if !groups.is_empty() {
                    return groups;
                }
            }
        }
        rest = body.get(end_at + 3..).unwrap_or("");
    }
    Vec::new()
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

/// The whole of an agent's stderr, trimmed and bounded.
///
/// # Why not the first line
///
/// An agent that prints a banner before explaining itself is not a
/// hypothetical. A real unauthenticated `codex` run on this machine wrote
/// thirteen lines to stderr — a version banner, a workdir/model/session
/// preamble, an echo of the prompt, reconnect progress — and only then
/// the cause:
///
/// ```text
/// Reading additional input from stdin...
/// OpenAI Codex v0.118.0 (research preview)
/// ...
/// ERROR: unexpected status 401 Unauthorized: Missing bearer or basic
/// authentication in header
/// ```
///
/// Reporting the first line reported the banner. The user was told
/// "Reading additional input from stdin..." and never told the token was
/// missing — the one fact that would have made them stop and fix
/// something. `classify_agent_output` reads the whole stream, so the
/// taxonomy was right while the human-readable string was a progress
/// line: the worst shape for this bug, because the JSON looks correct.
///
/// # Why not the whole stream, unbounded
///
/// Because the same string is rendered into a status-table cell
/// (`EngineOutcome::render` → `"{class}: {error}"`), and a table cell that
/// can be four hundred lines is not a table. An agent that streams a
/// progress bar, or dumps a config it could not parse, writes more than a
/// human can read in a row that is supposed to be scannable.
///
/// So: the whole stream, trimmed, and if it is still longer than
/// [`MAX_ERROR_LINES`] lines the **first and last** are kept with the
/// count dropped between them. The first line is where an agent states
/// what it is doing; the last is where it says what went wrong. The
/// middle is the part a reader skips, and the count is the part that
/// says "there is more, and here is how much". A truncation that kept
/// only the head would hide the cause in exactly the case this function
/// exists to surface; one that kept only the tail would hide the context
/// that makes the cause intelligible.
fn failure_message(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.len() <= MAX_ERROR_LINES {
        return lines.join("\n");
    }
    let mut kept: Vec<String> = lines[..MAX_ERROR_LINES / 2]
        .iter()
        .map(|l| (*l).to_string())
        .collect();
    kept.push("...".to_string());
    kept.push(format!(
        "... ({} more lines of stderr) ...",
        lines.len() - MAX_ERROR_LINES
    ));
    kept.push("...".to_string());
    kept.extend(
        lines[lines.len() - MAX_ERROR_LINES / 2..]
            .iter()
            .map(|l| (*l).to_string()),
    );
    kept.join("\n")
}

/// How many lines of an agent's stderr a failure report carries before the
/// middle is elided.
///
/// Long enough that a real cause is in the kept part: the `codex` run this
/// was written from put its error on line 14, and a banner plus preamble
/// plus progress is the normal shape rather than the exception. Short
/// enough that the result is still a table cell.
const MAX_ERROR_LINES: usize = 24;

/// The first spelling of `bin` that exists on `path`, Windows extensions
/// included.
///
/// # The order is the whole point: `PATH` first, extensions second
///
/// The obvious loop — for each extension, walk the whole `PATH` — is
/// wrong, and it is wrong in a way that only shows up on a machine that
/// has the real agent installed. With `for ext { for dir { ... } }`, a
/// `.exe` anywhere on `PATH` beats a `.cmd` in the **first** entry, so a
/// shim or a wrapper a user deliberately put first loses to the real
/// binary they were trying to shadow. That is not a test-only problem: it
/// is the semantics of `PATH`, and it is what a user who prepends a
/// directory means.
///
/// So the loops are the other way round: walk `PATH` in order, and within
/// each entry try the extensions. The first entry that has *any* spelling
/// of the name wins, and the extension order only breaks a tie inside one
/// directory. This is also the rule `ro-testkit` established for `gh`
/// (`git_on_path` in `shim.rs`), and the reason it is written down there
/// applies here too: a probe on a real `windows-latest` runner showed
/// `Command::new` not applying `PATHEXT` to a bare name at all, so the
/// extension walk is the only thing that finds anything.
///
/// The extension list is the platform's own, in the platform's own order
/// (`.COM;.EXE;.BAT;.CMD`), read from `PATHEXT` when it is set — the same list the
/// loader applies, minus the loader. A hardcoded `[".exe", ".cmd",
/// ".bat", ".com"]` would be a guess made on a Unix machine about what
/// Windows does, and it would be wrong in the direction that matters:
/// `.COM` before `.EXE` is the platform's rule, not an accident.
///
/// Empty `PATH` entries are skipped rather than read as the current
/// directory, for the reason `git_on_path` gives: a binary found relative
/// to wherever the process happens to be is a resolution that depends on
/// the working directory.
///
/// # Why this is not `#[cfg(windows)]`
///
/// The *caller* is Windows-only, because that is the only platform where a
/// bare name is not a file. The ordering rule is not: it is the semantics
/// of `PATH`, and the bug it fixes is a loop order, which is platform-neutral
/// logic. Gating the helper would put the one part of this that can be
/// tested on a Unix machine behind the one platform that cannot run the
/// test — and the failure it prevents is invisible exactly where it happens,
/// on a machine with the real agent installed. So the helper takes `path`
/// and `PATHEXT` as arguments and is exercised directly; `resolve_program`
/// is the only Windows-specific part, and it is a three-line call.
fn find_with_extensions(
    bin: &str,
    path: &std::ffi::OsString,
    pathext: &str,
) -> Option<std::path::PathBuf> {
    let extensions: Vec<String> = pathext
        .split(';')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(|e| e.strip_prefix('.').unwrap_or(e).to_ascii_lowercase())
        .collect();
    for dir in std::env::split_paths(path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        // The bare name first: a file called exactly `claude` is what a
        // user who typed `claude` means, and on Windows it is what
        // `which` resolves by name.
        let bare = dir.join(bin);
        if is_spawnable(&bare) {
            return Some(bare);
        }
        for ext in &extensions {
            let candidate = dir.join(format!("{bin}.{ext}"));
            if is_spawnable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// A file that can actually be spawned: present, and a file rather than a
/// directory an install left behind.
fn is_spawnable(path: &std::path::Path) -> bool {
    matches!(std::fs::metadata(path), Ok(m) if m.is_file())
}

/// Is this a Windows batch file?
///
/// Always `false` off Windows: there, a program path is a program path, and
/// returning `true` would send every prompt down a route that does not
/// exist.
fn is_batch_file(program: &std::path::Path) -> bool {
    #[cfg(not(windows))]
    {
        let _ = program;
        false
    }
    #[cfg(windows)]
    {
        program
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| {
                let e = e.to_ascii_lowercase();
                e == "cmd" || e == "bat"
            })
            .unwrap_or(false)
    }
}

/// The command interpreter — the only thing on Windows that can run a batch
/// file.
///
/// Read from the environment rather than hardcoded, because `ComSpec` is
/// what actually defines it on this machine; a hardcoded `cmd.exe` would be
/// right until someone moves it.
fn comspec() -> std::path::PathBuf {
    std::env::var_os("ComSpec")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("cmd.exe"))
}

/// Spawn with a deadline.
///
/// The child's **tree** is killed, not just the child: an agent spawns
/// tools of its own, and killing only the direct child leaves those
/// holding the worktree while the fleet reports success.
///
/// `stdin_text` is the prompt, on the one platform where it cannot be an
/// argv element. It is written and the handle dropped **before** the wait,
/// so the child sees the whole prompt followed by end-of-input; a child
/// that blocked reading a pipe nobody fed would sit here until the
/// deadline killed it, and the run would be reported as a timeout rather
/// than as the success it was.
fn run_with_deadline(
    cmd: &mut std::process::Command,
    limit: Duration,
    stdin_text: Option<&str>,
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
    if let Some(text) = stdin_text {
        if let Some(mut pipe) = child.stdin.take() {
            // Written on its own thread, and the reason is a deadlock.
            //
            // A child that never reads stdin — and `ro` cannot know that in
            // advance — leaves `write_all` blocked once the pipe fills, and
            // that write happens *before* the poll loop below, so a large
            // prompt would hang the run with the deadline not yet armed. The
            // deadline is the only thing standing between a wedged agent and
            // a wedged fleet, so it must never be behind a blocking write.
            //
            // The thread owns the pipe and drops it, so the child still sees
            // the whole prompt followed by end-of-input. If the child dies
            // first, the write fails and the thread ends; a thread blocked on
            // a pipe that just broke is a thread that returns.
            let owned = text.to_string();
            std::thread::spawn(move || {
                use std::io::Write;
                let _ = pipe.write_all(owned.as_bytes());
                let _ = pipe.flush();
            });
        }
    }
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

    /// The plan is a list of (subject, files) groups, and the file list is
    /// load-bearing: a subject on its own cannot become more than one
    /// commit, because `git commit` takes whatever is in the index.
    #[test]
    fn a_plan_becomes_one_group_per_entry() {
        let out = "reasoning\n\n```ro-commits\n[{\"subject\":\"add the engine trait\",\"files\":[\"src/lib.rs\"]},\n {\"subject\":\"add git engine\",\"files\":[\"src/git.rs\",\"src/read.rs\"]}]\n```\n";
        let groups = parse_commits(out, StreamFormat::CodexText);
        assert_eq!(
            groups,
            vec![
                CommitGroup {
                    subject: "add the engine trait".into(),
                    files: vec!["src/lib.rs".into()],
                },
                CommitGroup {
                    subject: "add git engine".into(),
                    files: vec!["src/git.rs".into(), "src/read.rs".into()],
                },
            ],
            "the split the agent reasoned about has to survive into the plan"
        );
    }

    /// Every assistant turn is scanned. Taking only the last would drop the
    /// plan entirely whenever the agent explained itself after answering —
    /// the "nothing to commit" outcome that hides work that was done.
    #[test]
    fn claude_stream_json_is_scanned_across_every_text_block() {
        let out = concat!(
            r#"{"message":{"content":[{"type":"text","text":"thinking out loud"}]}}"#,
            "\n",
            r#"{"message":{"content":[{"type":"text","text":"here you go\n\n```ro-commits\n[{\"subject\":\"the real subject\",\"files\":[\"a.txt\"]}]\n```"}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success"}"#,
            "\n",
        );
        let groups = parse_commits(out, StreamFormat::ClaudeStreamJson);
        assert_eq!(
            groups,
            vec![CommitGroup {
                subject: "the real subject".into(),
                files: vec!["a.txt".into()],
            }],
            "the plan may land in any turn, not only the last"
        );
    }

    /// The plan is looked for in every fenced block, not just the first. An
    /// agent that shows its work puts a shell block before the answer.
    #[test]
    fn the_plan_is_found_past_an_earlier_fenced_block() {
        let out = "here\n\n```bash\ngit status\n```\n\nthen:\n\n```ro-commits\n[{\"subject\":\"real\",\"files\":[\"a\"]}]\n```\n";
        let groups = parse_commits(out, StreamFormat::CodexText);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].subject, "real");
    }

    /// Prose with no plan block is not a commit. ro does not invent a subject
    /// from whatever the agent happened to say last.
    #[test]
    fn prose_with_no_plan_block_is_nothing_to_commit() {
        let out = "I looked at the diff and it seems fine.\n";
        assert!(parse_commits(out, StreamFormat::CodexText).is_empty());
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

    /// The report keeps the cause, which was never on the first line.
    ///
    /// A real unauthenticated `codex` run on this machine, verbatim in
    /// shape: thirteen lines of banner and preamble before the one line
    /// that says the token is missing.
    #[test]
    fn a_failure_message_carries_the_cause_from_a_late_line() {
        let stderr = "Reading additional input from stdin...\n\
                       OpenAI Codex v0.118.0 (research preview)\n\
                       --------\n\
                       workdir: /tmp/proj\n\
                       model: gpt-5.3-codex\n\
                       session id: 01a0e787-5e6d-7e51-bca8-e1ac87a74aab\n\
                       --------\n\
                       ERROR: Reconnecting... 1/5\n\
                       ERROR: Reconnecting... 2/5\n\
                       ERROR: unexpected status 401 Unauthorized: Missing bearer \
                       or basic authentication in header\n";
        let msg = failure_message(stderr);
        assert!(
            msg.contains("401") && msg.contains("Unauthorized"),
            "the cause is the whole point of the report, got: {msg}"
        );
        assert!(
            !msg.starts_with("Reading additional input from stdin...\nERROR"),
            "the report must not stop where the banner stops, got: {msg}"
        );
    }

    /// And it is not the *only* thing it carries: a report that kept just
    /// the last line would satisfy the assertion above, and would have
    /// thrown away everything that makes the cause readable.
    #[test]
    fn a_failure_message_keeps_the_stream_around_the_cause() {
        let stderr = "banner\nworkdir: /tmp\nERROR: Reconnecting... 1/5\n\
                       ERROR: boom\n";
        let msg = failure_message(stderr);
        assert!(msg.contains("banner"), "got: {msg}");
        assert!(msg.contains("Reconnecting"), "got: {msg}");
        assert!(msg.contains("boom"), "got: {msg}");
    }

    /// An unbounded report would be a four-hundred-line table cell, so the
    /// middle is elided — and the elision says how much. A silent
    /// truncation is the original bug wearing a different hat: the reader
    /// cannot tell an absent line from a dropped one.
    #[test]
    fn a_very_long_failure_is_bounded_and_says_so() {
        let mut stderr = String::new();
        for i in 0..500 {
            stderr.push_str(&format!("line {i}\n"));
        }
        let msg = failure_message(&stderr);
        assert!(
            msg.lines().count() <= MAX_ERROR_LINES + 3,
            "the report must stay a table cell, got {} lines",
            msg.lines().count()
        );
        assert!(
            msg.contains("more lines of stderr"),
            "an elision that does not announce itself is indistinguishable \
             from a complete report, got: {msg}"
        );
        assert!(
            msg.contains("line 0"),
            "the head survives: an agent says what it is doing first, got: {msg}"
        );
        assert!(
            msg.contains("line 499"),
            "the tail survives: the cause is at the end, got: {msg}"
        );
    }

    /// The negative control for the test above: a short stream is not
    /// annotated, because an elision marker on a complete report is a lie
    /// about what is missing.
    #[test]
    fn a_short_failure_is_not_annotated() {
        let msg = failure_message("one\ntwo\n");
        assert_eq!(msg, "one\ntwo");
    }

    /// `PATH` order beats extension order — the loop nesting, pinned.
    ///
    /// This is the fix for the four engine tests that only fail on a
    /// machine with a real agent installed. It is written here, against
    /// the loop, rather than only as a spawn test because the spawn test
    /// passes on CI (no agent installed) and on a dev box whose `PATH`
    /// happens to work, and fails everywhere else. The order is the claim;
    /// this is the claim with a machine-independent shape.
    #[test]
    fn a_shim_earlier_on_path_beats_a_real_binary_later() {
        let dir = tempfile::TempDir::new().unwrap();
        let early = dir.path().join("early");
        let late = dir.path().join("late");
        std::fs::create_dir_all(&early).unwrap();
        std::fs::create_dir_all(&late).unwrap();
        // The shim a test (or a user) put FIRST, and the real binary LATER.
        // A `.exe` anywhere beat a `.cmd` anywhere when the loops were
        // nested the other way round, so the real binary won.
        std::fs::write(early.join("claude.cmd"), "shim").unwrap();
        std::fs::write(late.join("claude.exe"), "real").unwrap();

        let path = std::env::join_paths([&early, &late]).unwrap();
        let found = find_with_extensions("claude", &path, ".COM;.EXE;.BAT;.CMD")
            .expect("a name on PATH must resolve");

        assert_eq!(
            found,
            early.join("claude.cmd"),
            "the first PATH entry that has ANY spelling wins; the extension \
             order only breaks a tie inside one directory"
        );
    }

    /// Within one directory the platform's own order decides — `.COM`
    /// before `.EXE`, because that is what `PATHEXT` says. A hardcoded
    /// `[".exe", ".cmd", …]` would silently be a different rule.
    #[test]
    fn within_one_directory_the_platform_extension_order_wins() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("claude.com"), "com").unwrap();
        std::fs::write(dir.path().join("claude.exe"), "exe").unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();

        let found = find_with_extensions("claude", &path, ".COM;.EXE;.BAT;.CMD").unwrap();
        assert_eq!(found, dir.path().join("claude.com"));
    }

    /// A directory named after the binary is not a binary. Without the
    /// `is_file` check, an install that left `claude/` behind would read
    /// as present and the spawn would fail with a message about the wrong
    /// thing.
    #[test]
    fn a_directory_on_path_is_not_a_spawnable_binary() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("claude.exe")).unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(find_with_extensions("claude", &path, ".COM;.EXE;.BAT;.CMD"), None);
    }

    /// An empty `PATH` entry means "the current directory" to a shell, and
    /// is a resolution that depends on where the process happens to be
    /// running. `ro-testkit::git_on_path` skips them for the same reason.
    #[test]
    fn an_empty_path_entry_is_skipped_rather_than_read_as_cwd() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("claude.exe"), "exe").unwrap();
        // Leading empty entry: if it were honoured, the lookup would
        // consult the process's working directory and this test's answer
        // would depend on where the test binary was started.
        let path = std::env::join_paths([
            std::path::PathBuf::from(""),
            dir.path().to_path_buf(),
        ])
        .unwrap();
        let found = find_with_extensions("claude", &path, ".COM;.EXE;.BAT;.CMD").unwrap();
        assert_eq!(found, dir.path().join("claude.exe"));
    }
}
