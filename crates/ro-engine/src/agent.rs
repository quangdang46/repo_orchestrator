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
//! is elsewhere: ro re-asserts the identity on every commit it performs
//! ([`ConfigWatch`]), and reads `.git/config` before and after so a change
//! the agent made to it is *reported* rather than silently persisted for
//! every future commit in that repo. Nothing here can prevent an agent from
//! writing to it — which is why the report is the whole of the control and
//! is worth having.
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
use crate::git_engine::changed_files;
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
            // `default_args` is `None` for the built-ins, which is what
            // makes the per-engine defaults below apply. A caller that
            // passes `Some(vec![])` is asking for "no arguments beyond the
            // prompt", which is a different request and is honoured.
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

    fn report_agent_made_commits(
        &self,
        ctx: &EngineContext<'_>,
        before: Option<&str>,
        after: Option<&str>,
    ) -> EngineOutcome {
        let commits: Vec<CommitRecord> =
            ro_git::read::commits_between(ctx.repo_root, before, after)
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

        // HEAD moved and ro found nothing to name. That is not "committed
        // nothing" — `render` turns an empty `Committed` into exactly that
        // string, and it would be false. The agent reset the branch, or
        // amended, or moved the ref somewhere ro cannot enumerate commits
        // from, and the user is told nothing happened while the ref they
        // were working on has changed underneath them.
        if commits.is_empty() {
            return EngineOutcome::Failed {
                error: format!(
                    "the agent moved HEAD on its own ({} -> {}) and ro could \
                     not enumerate the commits between them, so there is \
                     nothing to report as committed. The ref has been changed \
                     in the worktree; look at `git log` and `git reflog` \
                     before pushing.",
                    before.unwrap_or("(no HEAD)"),
                    after.unwrap_or("(no HEAD)")
                ),
                class: FailureClass::DirtyWorktree,
            };
        }

        // The identity ro resolved, if any. `None` means ro was not given
        // one, and then there is nothing to compare against — the commit is
        // reported as the agent's own on the strength of the prompt alone.
        let resolved = ctx.identity;

        // Every commit the agent made, with the author each one carries.
        // Read here rather than assumed, because the whole question is
        // "whose name is on this", and a report that asserted an author it
        // had not read would be the same blank-message shape this crate
        // keeps fixing elsewhere.
        let mut lines: Vec<String> = Vec::new();
        let mut foreign: Vec<(String, String)> = Vec::new();
        for c in &commits {
            let author = commit_author(ctx.repo_root, &c.oid);
            let who = author
                .as_ref()
                .map(|(name, email)| format!("{name} <{email}>"))
                .unwrap_or_else(|| "(no author)".to_string());
            let is_ours = match (&author, resolved) {
                (Some((name, email)), Some(id)) => name == &id.name && email == &id.email,
                // No resolved identity: nothing to compare against, and the
                // prompt is the only authority. Treated as foreign, because
                // the safe reading of "ro was not told who it is" is "do
                // not publish this".
                _ => false,
            };
            if !is_ours {
                foreign.push((c.oid.clone(), who.clone()));
            }
            lines.push(format!(
                "  {}  {}  {}",
                &c.oid[..c.oid.len().min(12)],
                who,
                c.message
            ));
        }

        if foreign.is_empty() {
            // Every commit on the branch carries the resolved identity, so
            // these are ro's own commits — the `-c` guard did its job, and
            // the work is publishable. This is the case a repo whose
            // `.git/config` disagrees with the resolved identity would
            // otherwise fail.
            return EngineOutcome::Committed { commits };
        }

        let resolved_desc = match resolved {
            Some(id) => format!("{} <{}>", id.name, id.email),
            None => "no identity was resolved for this run".to_string(),
        };
        // The base sentence is written **once the verdict is known**, so it
        // cannot contradict the clause appended below it. It used to say
        // "N commit(s) are sitting local and unreachable from any remote"
        // unconditionally, and then append "At least one of these commits is
        // already reachable from a remote" for a published commit — one
        // message asserting both halves, in the same paragraph, about the
        // same commits. The reader has to work out which half is stale, and
        // the two halves call for opposite responses.
        let stranded = stranded(ctx.repo_root, after);
        let where_it_is = match stranded {
            Publication::Stranded => "are sitting local and unreachable from any remote",
            Publication::Published => "were published under that identity by the agent itself",
            // The question could not be answered, so neither fact is
            // claimed. Saying "sitting local" when it might be published
            // would be the same contradiction the clause used to fix.
            Publication::Unknown => "are on the current branch, reachability unknown",
        };
        let mut error = format!(
            "the agent committed the work itself, under an identity that is \
             not the one ro resolved ({}). {} commit(s) {where_it_is}:\n{}\n\
             The work is on disk and has not been deleted. Re-commit it under \
             the resolved identity, or set the identity the agent should use \
             and re-run.",
            resolved_desc,
            foreign.len(),
            lines.join("\n")
        );

        // The clause is now the *only* place the verdict is stated, because
        // the sentence above it already carries it. Both arms are kept so a
        // future reader can see the two cases are still distinguished —
        // the distinction is the whole point of asking.
        match stranded {
            Publication::Published => {
                error.push_str(
                    "\nAt least one of these commits is already reachable from \
                     a remote, so it has been published under that identity.",
                );
            }
            Publication::Stranded => {
                error.push_str(
                    "\nNone of these commits is reachable from a remote, so the \
                     work is stranded locally.",
                );
            }
            Publication::Unknown => {}
        }

        EngineOutcome::Failed {
            error,
            class: FailureClass::AuthError,
        }
    }
}

struct AgentOutput {
    stdout: String,
    stderr: String,
    status: i32,
}

/// Why a spawn produced no output at all.
///
/// `Debug` because a test asserting on this has to be able to print it: a
/// failure that says "the run did not return" and not *why* is the same
/// blank-message shape this crate keeps fixing elsewhere.
#[derive(Debug)]
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
    ///
    /// A configured **path** is checked for existence, not for being on
    /// `PATH`. `which` walks `PATH` looking for a *name*, so a binary given
    /// as `/tmp/xyz/claude` — which is how `[agent] command` and every test
    /// shim names one — was reported as missing even when it was sitting
    /// right there and executable. The probe answered a question about a
    /// directory listing and was asked to answer one about a file.
    fn availability(&self) -> Availability {
        let configured = std::path::Path::new(&self.bin);
        if configured.is_absolute() || self.bin.starts_with('.') {
            return if configured.is_file() {
                Availability::Present(configured.to_path_buf())
            } else {
                Availability::Missing
            };
        }
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

        // HEAD **before** the agent runs. This has to be read here, not
        // after the spawn, because it is the only record of what HEAD was
        // when ro handed over the worktree — and it is what tells "the
        // agent changed nothing" apart from "the agent committed it
        // itself". Read after the spawn it is the agent's own commit, the
        // two ends are equal, and the branch below is dead code that can
        // never fire.
        let before = ro_git::read::head_oid(ctx.repo_root).unwrap_or(None);

        // `.git/config` as it is right now, so a change the agent makes to
        // it is reported rather than left in place. See `ConfigWatch`.
        let config = ConfigWatch::capture(ctx.repo_root);

        // Every exit below the spawn passes through `report_config_drift`,
        // so "the agent rewrote your config" is reported on the paths where
        // the run failed as well as where it succeeded. A report that only
        // appears on success is a report about the runs that did not matter.
        let output = match self.run(ctx) {
            Ok(o) => o,
            Err(RunError::TimedOut) => {
                // A kill is not a refusal. Distinct from `Failed` so the
                // caller can offer a retry.
                return report_config_drift(
                    &config,
                    ctx.repo_root,
                    EngineOutcome::TimedOut { after: ctx.timeout },
                );
            }
            Err(RunError::Spawned(e)) => {
                return report_config_drift(
                    &config,
                    ctx.repo_root,
                    EngineOutcome::Failed {
                        error: format!("could not start {}: {e}", self.bin),
                        class: FailureClass::MissingProvider,
                    },
                );
            }
        };

        report_config_drift(
            &config,
            ctx.repo_root,
            self.assess(ctx, &output, before.as_deref()),
        )
    }
}

/// The rest of the engine's own behaviour.
///
/// A second inherent `impl` block rather than one, so `checkpoint` stays in
/// the `Engine` impl — where the trait's method belongs — while the helpers
/// it calls sit with the rest of the engine.
impl AgentEngine {
    /// What the run's output and the worktree together mean.
    ///
    /// Split out of `checkpoint` for one reason: the `.git/config` watch has
    /// to be reported from exactly one place, whatever the run turned out to
    /// be. Every branch below — a non-zero exit, a clean tree, a commit
    /// failure, a success — returns from here, so the report is a single call
    /// in the caller rather than one per return.
    fn assess(
        &self,
        ctx: &EngineContext<'_>,
        output: &AgentOutput,
        before: Option<&str>,
    ) -> EngineOutcome {
        if output.status != 0 {
            return EngineOutcome::Failed {
                // The whole stream, not its first line: an agent that
                // prints a banner before explaining itself would
                // otherwise be *reported* on the banner, even though it
                // is classified on the whole thing. See `failure_message`.
                //
                // **Both streams, stderr first.** It read stderr only, so
                // an agent that reports its failure on stdout — which is
                // where a normal program prints things — produced
                // `failed: ` with nothing after it, and JSON
                // `"outcome": "failed: "`. The user was told the engine
                // failed and not why, while `classify_agent_output` a few
                // lines below was reading both and had correctly worked
                // out it was an auth error. The taxonomy saw the message
                // and the message was thrown away.
                error: failure_message(&output.stderr, &output.stdout),
                // One taxonomy: the same `FailureClass` every other part
                // of ro uses, not a second one for agents.
                class: classify_agent_output(&output.stderr, &output.stdout),
            };
        }

        // **The ref moved — by anything.** Checked before the dirty test,
        // and independent of it.
        //
        // The identity guard used to live inside `if !is_dirty`, so an
        // agent that committed *some* of its own work under its own
        // identity and left the rest dirty took the ordinary path: ro
        // committed the remainder under its own identity and returned
        // `Committed`. The commit the caller then pushes is the branch tip,
        // and it still carries the agent's foreign commit underneath it.
        // That is the exact outcome the prompt's "Do NOT run `git commit`"
        // exists to prevent, arriving through the door the partial-commit
        // case opens. Whether the tree happens to be clean afterwards is a
        // fact about the *leftovers*, and it has no bearing on who wrote
        // the commit that is already sitting under HEAD.
        let after = ro_git::read::head_oid(ctx.repo_root).unwrap_or(None);
        if before != after.as_deref() {
            return self.report_agent_made_commits(ctx, before, after.as_deref());
        }

        // Whatever the agent wrote is now in the worktree. ro commits it
        // itself, with its own identity and its own credential handling —
        // the agent never touches the push.
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
            return EngineOutcome::NothingToCommit;
        }

        // The engine set the author on the *child's* environment. ro commits
        // some of this work itself, and a per-invocation `-c` has to be
        // re-asserted here: the agent may have run `git config user.email
        // something-else` in between, and that write persists in
        // `.git/config` for every commit after it.
        let author = ctx
            .identity
            .as_ref()
            .map(|i| (i.name.as_str(), i.email.as_str()));

        // `--message` is the user saying *one commit, this subject*. The
        // agent's plan is not consulted at all, because the whole point of
        // the flag is that the agent's own grouping is what the user is
        // overriding.
        if let Some(subject) = subject_override(ctx) {
            return self.commit_whole_worktree(ctx, subject, author, before);
        }

        let subjects = parse_commits(&output.stdout, self.stream);
        if subjects.is_empty() {
            // The agent exited cleanly and changed nothing. Saying so is
            // more useful than inventing a commit from whatever it wrote.
            return EngineOutcome::NothingToCommit;
        }

        let mut commits = Vec::new();
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
            let oid = match ro_git::primitives::commit_all_as(ctx.repo_root, &group.subject, author)
            {
                Ok(oid) => oid,
                Err(e) => {
                    return EngineOutcome::Failed {
                        error: commit_failure_detail(ctx.repo_root, &format!("{e:#}"), &group),
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

    /// One commit of the whole worktree, under the subject the user gave.
    ///
    /// # Why the plan is discarded rather than merged
    ///
    /// `--message` is documented as "One commit per repo, with this subject",
    /// and the design says supplying the subject *is* that request — it is
    /// the only way to stop an agent from splitting the work. An agent that
    /// proposed three groups is not a source of truth ro is entitled to
    /// override the user on; the user has already said what they want. So
    /// the plan is not read at all, and the commit carries every changed
    /// file rather than the union of the agent's groups.
    ///
    /// The union would sound more careful and is worse. A file the agent
    /// forgot to name would be left behind, so `ro commit --message` would
    /// report success with work still uncommitted — and a single-commit
    /// request that silently drops files is the one outcome the flag exists
    /// to prevent. `git_engine.rs` already does exactly this, which is why
    /// the two engines now agree about what the flag means.
    fn commit_whole_worktree(
        &self,
        ctx: &EngineContext<'_>,
        subject: &str,
        author: Option<(&str, &str)>,
        before: Option<&str>,
    ) -> EngineOutcome {
        if let Err(e) = ro_git::primitives::stage_all(ctx.repo_root) {
            return EngineOutcome::Failed {
                error: format!("staging the worktree for {subject:?} failed: {e:#}"),
                class: FailureClass::DirtyWorktree,
            };
        }
        let oid = match ro_git::primitives::commit_all_as(ctx.repo_root, subject, author) {
            Ok(oid) => oid,
            Err(e) => {
                return EngineOutcome::Failed {
                    error: commit_failure_detail(
                        ctx.repo_root,
                        &format!("{e:#}"),
                        &CommitGroup {
                            subject: subject.to_string(),
                            files: Vec::new(),
                        },
                    ),
                    class: FailureClass::DirtyWorktree,
                };
            }
        };
        EngineOutcome::Committed {
            commits: vec![CommitRecord {
                message: subject.to_string(),
                oid,
                files: changed_files(ctx.repo_root, before),
            }],
        }
    }
}
/// The author of one commit, as `(name, email)`.
///
/// Read from the object rather than assumed. The question this answers is
/// "whose name is on this commit", and a report that asserted an author it
/// had not read would be the same blank-message shape this crate keeps
/// fixing elsewhere.
fn commit_author(repo_root: &std::path::Path, oid: &str) -> Option<(String, String)> {
    let out = std::process::Command::new("git")
        .args(["log", "-1", "--format=%an%n%ae", oid])
        .current_dir(repo_root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let name = lines.next()?.trim().to_string();
    let email = lines.next()?.trim().to_string();
    if name.is_empty() && email.is_empty() {
        return None;
    }
    Some((name, email))
}

/// Whether the work an agent committed on its own has got anywhere.
///
/// # Why this is asked at all
///
/// "The agent committed it" and "the agent committed it and it is stranded"
/// are not the same news. A commit already reachable from a remote has been
/// published under whatever identity it carries, and the user's response is
/// to look at what went out. A commit that is not reachable is sitting
/// local, and the user's response is to re-commit it. Reporting the first
/// as the second would have the user hunting for a push that happened;
/// reporting the second as the first would leave work stranded with
/// nothing saying so.
///
/// # Why the question is asked about the agent's commits, not the base
///
/// The obvious ref to ask about is the base — "was what ro started from
/// already on a remote?" — and it is the wrong one. A repo whose remote has
/// the base and nothing the agent added is the *ordinary* shape of a normal
/// `ro ship`: the user ran the fleet, the base was pushed last time, and
/// this run's work has not gone anywhere. Answering that "published" tells
/// the user their stranded work went out.
enum Publication {
    /// At least one of the agent's commits is reachable from a remote.
    Published,
    /// None of them is.
    Stranded,
    /// Could not be determined. Reported as nothing rather than guessed at:
    /// an invented "stranded" over a published commit is the worse error.
    Unknown,
}

/// Is the tip the agent left behind on a remote?
///
/// `after` is HEAD as the agent left it. A commit is published if some
/// remote ref contains it, and the tip is the one that settles it: a push
/// that carried any of the agent's commits necessarily carried the tip,
/// because the tip is the newest of them.
fn stranded(repo_root: &std::path::Path, after: Option<&str>) -> Publication {
    let Some(tip) = after else {
        // No `after` means ro could not read HEAD, so there is nothing to
        // ask about.
        return Publication::Unknown;
    };
    // The **remote's** refs, not the local remote-tracking refs.
    //
    // `git branch -r --contains` only ever lists refs under `refs/remotes/`,
    // and a push the agent made **by URL** (`git push $URL HEAD:refs/heads/x`)
    // creates no tracking ref at all. So the commit was on the remote and the
    // row said both "unreachable from any remote" and "stranded locally" —
    // the exact contradiction the message at the call site was written to
    // stop, fixed for the named-remote case and unfixed for the URL case.
    //
    // `ls-remote` asks the remote. A remote that cannot be reached is
    // **unknown**, not empty: "we could not ask" is not the same claim as
    // "nothing is there", and reporting the second is how work that is
    // published gets described as lost.
    let out = std::process::Command::new("git")
        // `credential.helper=` / `core.askPass=` for the same reason as every
        // other network call ro makes: an empty helper list leaves git nothing
        // that can open a window. On a machine whose git config names Git
        // Credential Manager as its helper, this `ls-remote` was the one call
        // on the stranded-publication path that could pop a password dialog
        // at the user. A remote that cannot be reached is `Unknown` below
        // regardless, so declining to authenticate costs nothing.
        .args(["-c", "credential.helper=", "-c", "core.askPass="])
        .args(["ls-remote", "--heads"])
        .current_dir(repo_root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .env("GCM_UI", "Never")
        .env("LC_ALL", "C")
        .output();
    let Ok(o) = out else {
        return Publication::Unknown;
    };
    if !o.status.success() {
        return Publication::Unknown;
    }
    // `ls-remote` reports the *tip* of every branch, not the history. A
    // commit that is an ancestor of a branch tip is still published, so the
    // comparison is "is the tip an ancestor of ours", which is the same
    // relation `git merge-base --is-ancestor` answers. Doing it per candidate
    // costs one cheap local process per branch and asks the question the
    // verdict actually turns on.
    for line in String::from_utf8_lossy(&o.stdout).lines() {
        let Some((oid, _ref)) = line.split_once('\t') else {
            continue;
        };
        let oids = oid.trim();
        if oids.is_empty() {
            continue;
        }
        let contains = std::process::Command::new("git")
            .args(["merge-base", "--is-ancestor", tip, oids])
            .current_dir(repo_root)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .output();
        if let Ok(c) = contains
            && c.status.success()
        {
            return Publication::Published;
        }
    }
    Publication::Stranded
}

/// The subject `--message` asked for, if it is one.
///
/// Blank is not a subject. `git_engine.rs` reads the field the same way,
/// and a `--message ""` that silently committed under a generated subject
/// would be the flag doing the opposite of what it says.
fn subject_override<'a>(ctx: &'a EngineContext<'_>) -> Option<&'a str> {
    ctx.subject_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// `.git/config` as it was before one agent run.
///
/// # Why this exists
///
/// The module doc promises that a change to `.git/config` is reported, and
/// for a long time nothing did it: there was no hash, no read, no report,
/// just a sentence in a doc comment describing a control that did not exist.
/// An agent that runs `git config user.email something-else` therefore left
/// that write in place for every commit after it — ro's own commits are
/// safe, because they carry `-c user.email`, but the user's own `git commit`
/// in that repo is not, and ro had already reported success.
///
/// ro cannot prevent the write. It can notice it, and "your config changed
/// while an agent was in your worktree" is the difference between a repo
/// that was tampered with and one that was told about it.
struct ConfigWatch {
    before: Vec<u8>,
}

impl ConfigWatch {
    fn capture(repo_root: &std::path::Path) -> Self {
        Self {
            before: config_bytes(repo_root),
        }
    }
}

/// The bytes of this repository's `.git/config`, or none.
///
/// # Why `--git-common-dir` and not `--git-dir`
///
/// A **linked worktree's `.git` is a file**, not a directory, so
/// `<root>/.git/config` does not exist and the report never fires. The
/// obvious repair — `git rev-parse --git-dir` — is also wrong, and wrong in
/// the one direction that matters: for a linked worktree it returns
/// `.git/worktrees/<id>`, and **there is no `config` file there**. Every key
/// this report is about — `user.name`, `user.email`, `remote.*.url`,
/// `http.*.extraheader` — lives in the main repository's config, and that is
/// where a `git config` run from inside the linked worktree writes. So the
/// read has to follow the same resolution git's own write does, which is
/// `--git-common-dir`.
///
/// This is not a rare setup. `ro` is a fleet tool, and a fleet is exactly
/// where linked worktrees are used to run many repos at once — so the one
/// arrangement in which the control would have been silently dead is the one
/// it exists for.
///
/// A missing config is not an error: `unwrap_or_default` turns "no file"
/// into "unchanged", and a report that failed to build because there was
/// nothing to report would be the wrong kind of loud.
fn config_bytes(repo_root: &std::path::Path) -> Vec<u8> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(repo_root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output();
    let dir = match out {
        Ok(o) if o.status.success() => {
            let raw = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if raw.is_empty() {
                return Vec::new();
            }
            let p = std::path::PathBuf::from(&raw);
            if p.is_absolute() {
                p
            } else {
                repo_root.join(p)
            }
        }
        _ => return Vec::new(),
    };
    std::fs::read(dir.join("config")).unwrap_or_default()
}

/// What changed in `.git/config`, or `None` if nothing did.
///
/// A pure function over the two byte strings so the report can be tested
/// without a repository, which is the only way to test the parts that
/// matter — that a *removed* key is reported, and that a secret is not.
fn config_drift_report(before: &[u8], after: &[u8]) -> Option<String> {
    if before == after {
        return None;
    }
    let old = parse_config(&String::from_utf8_lossy(before));
    let now = parse_config(&String::from_utf8_lossy(after));

    let mut keys: Vec<&String> = old.keys().chain(now.keys()).collect();
    keys.sort();
    keys.dedup();

    let mut changes: Vec<String> = Vec::new();
    for key in keys {
        let from = old.get(key);
        let to = now.get(key);
        if from == to {
            continue;
        }
        changes.push(format!(
            "{key}: {} -> {}",
            show_value(key, from),
            show_value(key, to)
        ));
    }
    if changes.is_empty() {
        // The bytes differ but no key does: a comment, a blank line, or the
        // file being reformatted. Reporting "the agent changed your config"
        // for a reformat trains the user to ignore the report, which is how
        // a real one stops being noticed.
        return None;
    }
    Some(format!(
        "the agent changed this repository's .git config ({} change(s)): {}",
        changes.len(),
        changes.join("; ")
    ))
}

/// One config value, ready to print — or not.
///
/// The key is always safe to print and is most of what makes the report
/// actionable: "user.email" says what to look at. The *value* is not
/// always safe, and `.git/config` is full of places a credential lives —
/// `http.extraheader` holds an `Authorization:`, and any `*.url` can hold
/// `https://user:token@host`. A report that printed those would move a
/// secret from a file into a terminal, a log, and possibly a bug report, so
/// a secret-bearing key reports the key and nothing else.
fn show_value(key: &str, value: Option<&String>) -> String {
    match value {
        None => "(unset)".to_string(),
        Some(v) if is_secret_bearing(key) || crate::env::is_token_shaped(v) => {
            "(a value that is not printed)".to_string()
        }
        Some(v) => v.clone(),
    }
}

/// Does this config key's value carry something that must not be printed?
///
/// Deliberately a **substring** test rather than a list of exact keys. A
/// list is a decision someone made about keys that exist today, and the
/// first new place a credential goes — `http.<url>.extraheader`, a remote
/// URL with a token in it, a `[credential]` helper — is a key the list did
/// not have. The cost of a false positive is one redacted value in a
/// warning; the cost of a false negative is a token in a transcript.
fn is_secret_bearing(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "passwd",
        "credential",
        "extraheader",
        "authorization",
        "cookie",
        "apikey",
        "api_key",
        ".url",
    ]
    .iter()
    .any(|needle| k.contains(needle))
}

/// `.git/config` as a flat `section.key -> value` map.
///
/// A parser rather than `git config --list` because the report is about
/// *what the file now says*, and a report produced by running git in a
/// repository an agent has just been running commands in inherits whatever
/// that agent did to the environment. Reading the file cannot.
fn parse_config(text: &str) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let mut section = String::new();
    let mut last: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('[') {
            // `[section]`, `[section "sub"]` and `[a.b]` all name a prefix.
            let name = rest.split(']').next().unwrap_or("").trim().replace('"', "");
            section = name.split_whitespace().collect::<Vec<_>>().join(".");
            // `[a.b.c]` is a dotted subsection, not a nesting; the dots are
            // already the separator this map uses.
            last = None;
            continue;
        }
        // A continuation is a line that starts with whitespace — git's own
        // rule — and belongs to the value above it.
        let (key, value) = match trimmed.split_once('=') {
            Some((k, v)) => (format!("{section}.{}", k.trim()), v.trim().to_string()),
            None => match &last {
                Some(k) => (k.clone(), String::new()),
                None => continue,
            },
        };
        if trimmed.contains('=') {
            last = Some(key.clone());
        }
        out.insert(key, value);
    }
    out
}

/// Report a change to `.git/config`, and carry it into a failure.
///
/// # Why stderr, and why also here
///
/// `EngineOutcome` has no warning arm, and adding one means a change in
/// every caller in `ro` — a cross-crate edit this crate does not get to
/// make for a warning. So the report goes where a warning already goes on
/// this codebase (`orchestrator.rs` does the same for `--onto`), and it is
/// *additionally* appended to a failure that is already being reported, so
/// the machine-readable row carries it too and a user reading JSON is not
/// the one person who never hears about it.
///
/// # Why the run is not failed
///
/// Because ro's own commits carry `-c user.email`/`-c user.name`, so the
/// identity the agent wrote did **not** leak into anything this run
/// committed. Turning a good commit into a failure would also be the one
/// thing that teaches users to ignore the report. What was actually
/// damaged is the user's own future `git commit`, and that is a warning,
/// not a lost commit.
fn report_config_drift(
    watch: &ConfigWatch,
    repo_root: &std::path::Path,
    outcome: EngineOutcome,
) -> EngineOutcome {
    let Some(report) = config_drift_report(&watch.before, &config_bytes(repo_root)) else {
        return outcome;
    };
    eprintln!(
        "warning: {report}. ro's own commits are unaffected — they carry the \
         identity it resolved — but your own `git commit` in this repository \
         will use the new value until you undo it."
    );
    match outcome {
        EngineOutcome::Failed { error, class } => EngineOutcome::Failed {
            error: format!("{error}\n\n{report}"),
            class,
        },
        other => other,
    }
}

/// Why a commit came back with no reason of its own.
///
/// # The blank message this exists for
///
/// `git commit` writes "nothing to commit, working tree clean" to
/// **stdout**. `ro_git::primitives::commit_all_as` builds its error from
/// stderr alone, so that one failure — the only one where the cause is a
/// fact about the index rather than a git error — arrives as
/// `git commit failed: ` and nothing else. It is reachable by an ordinary
/// agent: a plan that names a file which does not differ from HEAD commits
/// nothing, and the run reports a failure with a blank cause after the
/// earlier groups have already landed.
///
/// ro cannot fix the helper from here, so it goes and looks itself. What
/// the status says about the group's own paths says exactly why the index
/// was empty, and it is one cheap git call away. The one-line change that
/// fixes this at the source is in `primitives.rs`.
fn commit_failure_detail(repo_root: &std::path::Path, err: &str, group: &CommitGroup) -> String {
    let mut message = format!("committing {:?} failed: {err}", group.subject);
    if has_reason(err) {
        return message;
    }
    message.push_str(
        "\ngit reported no reason of its own. (It writes 'nothing to commit, \
         working tree clean' to stdout, and the commit helper reads stderr \
         only, so the cause is lost before it gets here.)",
    );
    if !group.files.is_empty() {
        let status = status_porcelain(repo_root, &group.files);
        message.push_str(&describe_empty_index(&status, &group.files));
    }
    message
}

/// Does an error string carry anything after its last colon?
///
/// The shape is `git commit failed: <stderr>`, so a blank tail is a
/// missing cause. A colon inside the cause itself is harmless: only the
/// text after the **last** one is examined, and a cause that exists is
/// reported whatever else it contains.
fn has_reason(err: &str) -> bool {
    match err.rsplit_once(':') {
        Some((_, tail)) => !tail.trim().is_empty(),
        None => !err.trim().is_empty(),
    }
}

/// `git status --porcelain` restricted to some paths.
fn status_porcelain(repo_root: &std::path::Path, files: &[String]) -> String {
    let mut cmd = std::process::Command::new("git");
    cmd.args(["status", "--porcelain", "--"])
        .args(files)
        .current_dir(repo_root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C");
    match cmd.output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        // Could not be read. The caller reports the failure without this
        // rather than inventing a reason for it.
        _ => String::new(),
    }
}

/// Say why the index was empty, in terms of the paths the group named.
///
/// Pure over its input, so the shape of the explanation is testable without
/// a repository — which is the only way to test that the "nothing was ever
/// different" case and the "staged after all" case are told apart.
fn describe_empty_index(porcelain: &str, files: &[String]) -> String {
    let named: Vec<&String> = files
        .iter()
        .filter(|f| {
            porcelain
                .lines()
                .any(|l| l.len() > 3 && l[3..].trim() == f.as_str())
        })
        .collect();
    if named.is_empty() {
        return format!(
            "\nNone of the paths this group named differ from HEAD ({}), so \
             staging them left the index empty and git had nothing to commit.",
            files.join(", ")
        );
    }
    format!(
        "\ngit status reports these paths as still changed ({}), so the index \
         was not empty when the commit ran: {}",
        named
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        porcelain.trim()
    )
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
                let Some(blocks) = content.as_array() else {
                    continue;
                };
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
fn failure_message(stderr: &str, stdout: &str) -> String {
    // stderr first, because that is where a failure is *supposed* to be
    // reported and where the cause usually is; stdout is the fallback for
    // the agent that prints its error to the stream a normal program uses.
    // An empty stderr is not a reason to report nothing — it is a reason
    // to look at the other stream.
    let mut lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        lines = stdout
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
    }
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
///
/// # The pipes are drained while the child runs, not after it exits
///
/// An OS pipe buffer is 64 KiB. A child that writes more than that before
/// exiting blocks in `write` and never exits — so a poll loop that only
/// reads the pipes *after* `try_wait` returns `Some` never gets there, and
/// the run is reported as a timeout for an agent that had already done all
/// its work. The real `claude` on this machine emits 73–86 KB of
/// stream-json from one `commands_changed` event listing every skill, so
/// `ro commit` with the default engine sits right at that edge.
///
/// So stdout and stderr are each read on their own thread into a buffer,
/// `try_wait` is polled on this thread, and the readers are joined once the
/// child has exited. The join is **after** the exit and never before it:
/// joining a reader that is still blocked on a full pipe is the same
/// deadlock wearing the other hat. A killed child closes its pipes, so the
/// readers return on their own — which is why `kill_tree` cannot deadlock
/// against them.
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

    // Both readers are taken **before** the poll loop starts, so there is no
    // window in which the child can fill a buffer nobody is draining. The
    // handles are moved into the threads and the threads are joined after
    // the child exits; the buffers are plain `Vec<u8>` because a child can
    // write more than a pipe buffer and the whole stream has to survive.
    let stdout_reader = drain(child.stdout.take());
    let stderr_reader = drain(child.stderr.take());

    let started = Instant::now();
    const POLL: Duration = Duration::from_millis(25);

    let status = loop {
        // `try_wait` here is a **poll only**. It reaps the child on exit,
        // so it must never be the call that collects the output: after it
        // returns `Some`, the pipes are still readable but `wait_with_output`
        // on the same `Child` would find nothing to wait for and return
        // empty stdout/stderr. That is not a subtle edge — it makes every
        // non-zero agent exit arrive with a blank stderr, which is exactly
        // the message the classification below needs.
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => return Err(RunError::Spawned(e)),
        }
        if started.elapsed() >= limit {
            // The kill is what makes the readers return. A killed child
            // closes its write ends, so a reader blocked on a full buffer
            // gets `Ok(0)` and ends; the join below therefore cannot wait
            // on a pipe nobody is going to drain. The output is dropped
            // rather than collected — a timed-out run has no stream to
            // classify, and the deadline is the fact the caller needs.
            kill_tree(&mut child);
            return Err(RunError::TimedOut);
        }
        std::thread::sleep(POLL);
    };

    // The join is **inside** the deadline, and the comment above it was the
    // claim this fixes.
    //
    // It used to read: "A killed child closes its pipes, so a reader that
    // was blocked on a full buffer returns rather than holding the join
    // open." That is true of the direct child and false of its
    // **descendants**. A shim that exits 0 while a background job it
    // started inherits the write ends leaves both readers blocked on a pipe
    // that will not see an EOF for as long as the descendant lives — and
    // the poll loop had already broken on the exit, so the join below it
    // sat outside the deadline with nothing armed to stop it. `ro` then
    // waited on that pipe forever: no output, no exit, no message. The
    // deadline had already been satisfied.
    //
    // So the readers get whatever is left of the budget, and if they are
    // still blocked the process **group** is killed, which is what
    // actually closes the write ends.
    let remaining = limit.saturating_sub(started.elapsed());
    let deadline = Instant::now() + remaining;
    let mut readers_done = stdout_reader.is_finished() && stderr_reader.is_finished();
    while !readers_done && Instant::now() < deadline {
        std::thread::sleep(POLL);
        readers_done = stdout_reader.is_finished() && stderr_reader.is_finished();
    }
    if !readers_done {
        // The descendants are what is holding the pipes open, so the group
        // is what has to die. `kill_tree` on the already-exited direct child
        // still reaches the group because the child was spawned with
        // `process_group(0)`.
        kill_tree(&mut child);
    }

    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();

    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// Read one pipe to the end on its own thread, and hand back what it got.
///
/// A thread rather than a `read_to_end` on the calling thread because the
/// two pipes have to be drained **concurrently**: reading stdout to the end
/// first would leave stderr to fill its own 64 KiB buffer and wedge the
/// child, which is the same deadlock one pipe over.
fn drain<P: std::io::Read + Send + 'static>(pipe: Option<P>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
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
    // The integration tests in `tests/agent_engines.rs` drive a real
    // binary through `ro_testkit`; the unit tests here that need the same
    // harness import it directly.
    use ro_testkit::TestEnv;

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new(ro_testkit::git_path())
            .args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn head_oid(repo: &std::path::Path) -> Option<String> {
        let out = std::process::Command::new(ro_testkit::git_path())
            .args(["rev-parse", "HEAD"])
            .current_dir(repo)
            .output()
            .expect("git runs");
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            None
        }
    }

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

    /// A commit the agent pushed **by URL** is published.
    ///
    /// `stranded` asked `git branch -r --contains <tip>`, which only ever
    /// lists refs under `refs/remotes/`. A push by URL
    /// (`git push $URL HEAD:refs/heads/x`) creates no tracking ref at all,
    /// so the commit was on the remote and the row said both "unreachable
    /// from any remote" and "stranded locally" — the exact contradiction the
    /// message at the call site was written to stop, fixed for the
    /// named-remote case and unfixed for the URL case.
    #[test]
    fn a_commit_published_by_url_is_not_stranded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let bare = root.join("remote.git");
        std::fs::create_dir_all(&bare).unwrap();
        run_git(&bare, &["init", "--bare", "-q", "--initial-branch=main"]);

        let work = root.join("work");
        run_git(&root, &["clone", "-q", &bare.to_string_lossy(), "work"]);
        run_git(&work, &["config", "user.email", "agent@example.com"]);
        run_git(&work, &["config", "user.name", "Agent"]);
        run_git(&work, &["checkout", "-q", "-b", "agent-work"]);
        std::fs::write(work.join("a.txt"), "agent work\n").unwrap();
        run_git(&work, &["add", "-A"]);
        run_git(&work, &["commit", "-q", "-m", "agent commit"]);
        // By URL, so no `refs/remotes/origin/...` ref is created.
        run_git(
            &work,
            &[
                "push",
                "-q",
                &bare.to_string_lossy(),
                "HEAD:refs/heads/agent-published",
            ],
        );

        let tip = head_oid(&work).expect("HEAD resolves");
        assert!(
            !work
                .join(".git/refs/remotes/origin/agent-published")
                .exists(),
            "the fixture must have no tracking ref, or it proves nothing"
        );
        assert!(
            matches!(stranded(&work, Some(&tip)), Publication::Published),
            "the commit IS on the remote; `git branch -r --contains` cannot see it"
        );
    }

    /// The positive control: a commit that is genuinely local is stranded.
    ///
    /// Without this, the test above would pass for a function that answered
    /// `Published` for everything.
    #[test]
    fn a_commit_that_was_never_pushed_is_stranded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let bare = root.join("remote.git");
        std::fs::create_dir_all(&bare).unwrap();
        run_git(&bare, &["init", "--bare", "-q", "--initial-branch=main"]);
        let work = root.join("work");
        run_git(&root, &["clone", "-q", &bare.to_string_lossy(), "work"]);
        run_git(&work, &["config", "user.email", "agent@example.com"]);
        run_git(&work, &["config", "user.name", "Agent"]);
        std::fs::write(work.join("a.txt"), "local only\n").unwrap();
        run_git(&work, &["add", "-A"]);
        run_git(&work, &["commit", "-q", "-m", "local only"]);

        let tip = head_oid(&work).expect("HEAD resolves");
        assert!(
            matches!(stranded(&work, Some(&tip)), Publication::Stranded),
            "a commit that was never pushed is stranded"
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
        let msg = failure_message(stderr, "");
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
        let msg = failure_message(stderr, "");
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
        let msg = failure_message(&stderr, "");
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
        let msg = failure_message("one\ntwo\n", "");
        assert_eq!(msg, "one\ntwo");
    }

    /// A child that writes more than a pipe buffer before exiting must be
    /// drained while it runs, and the whole stream must come back.
    ///
    /// The integration test in `tests/agent_engines.rs` proves this through
    /// the engine, with a 5 MB stream. This one proves the property of
    /// `run_with_deadline` directly, on a child that is not an agent at all:
    /// a shell that writes a known number of bytes and exits 0. It is the
    /// same claim with the engine's parsing taken out of the way, so a
    /// regression in the drain is not masked by a change in the parser.
    ///
    /// The byte count is asserted exactly. A reader that stopped at the
    /// first buffer boundary would return promptly and be short by
    /// everything after 64 KiB, which is the failure this exists to catch.
    ///
    /// Unix-only, and the *portable* proof of the same property is the
    /// integration test in `tests/agent_engines.rs`, whose shim is built
    /// for both platforms by `ro-testkit`. This one exists to pin the
    /// behaviour of `run_with_deadline` itself with the engine's parsing
    /// out of the way, and `sh`/`true` are the cheapest way to say that.
    #[cfg(unix)]
    #[test]
    fn a_child_writing_past_the_pipe_buffer_is_drained_whole() {
        let dir = tempfile::TempDir::new().expect("the fixture dir is creatable");
        let payload = dir.path().join("payload.bin");
        // 1 MiB, comfortably past the 64 KiB pipe buffer on every platform
        // ro runs on, and small enough that the test stays fast.
        let bytes = 1024 * 1024;
        std::fs::write(&payload, vec![b'z'; bytes]).expect("the payload is writable");

        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c")
            .arg(format!("cat '{}'", payload.display()))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let out = run_with_deadline(&mut cmd, Duration::from_secs(30), None)
            .expect("a child that exits must not be reported as a timeout");

        assert_eq!(
            out.stdout.len(),
            bytes,
            "every byte the child wrote must come back; a reader that stopped \
             at the pipe buffer boundary would be short by the rest"
        );
        assert_eq!(out.status.code(), Some(0));
    }

    /// The negative control for the test above: a child that writes nothing
    /// must not be reported as a timeout either, and must come back empty.
    /// Without it, the test above would also pass on a run that never
    /// started the child at all.
    #[test]
    fn a_child_writing_nothing_is_not_a_timeout() {
        let mut cmd = std::process::Command::new("true");
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let out = run_with_deadline(&mut cmd, Duration::from_secs(30), None)
            .expect("a child that exits immediately must not time out");
        assert!(out.stdout.is_empty());
        assert_eq!(out.status.code(), Some(0));
    }

    /// A child that fills **stderr** while stdout is quiet must be drained
    /// whole, and must not be reported as a timeout.
    ///
    /// # The gap in the fixture this closes
    ///
    /// `a_child_writing_past_the_pipe_buffer_is_drained_whole` is a `cat` of
    /// one file: **stdout alone**. The stderr reader is a separate thread
    /// over a separate pipe with its own 64 KiB buffer, and the shape that
    /// breaks it is the mirror image of the one that breaks the stdout
    /// reader — the child blocks on stderr while the stdout reader has
    /// already seen EOF and its thread has gone. An agent that streams
    /// progress on stderr and answers on stdout is the ordinary case, not an
    /// edge, and the two are not interchangeable.
    ///
    /// The byte count is asserted exactly, for the reason the stdout test
    /// gives: a reader that stopped at the first buffer boundary would
    /// return promptly and be short by everything after 64 KiB.
    #[cfg(unix)]
    #[test]
    fn a_child_filling_stderr_while_stdout_is_quiet_is_drained_whole() {
        let dir = tempfile::TempDir::new().expect("the fixture dir is creatable");
        let payload = dir.path().join("payload.bin");
        let bytes = 1024 * 1024;
        std::fs::write(&payload, vec![b'e'; bytes]).expect("the payload is writable");

        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c")
            .arg(format!("cat '{}' 1>&2", payload.display()))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let out = run_with_deadline(&mut cmd, Duration::from_secs(30), None)
            .expect("a child that exits must not be reported as a timeout");

        assert_eq!(
            out.stderr.len(),
            bytes,
            "every byte the child wrote to stderr must come back; a reader \
             that stopped at the pipe buffer boundary would be short by the \
             rest"
        );
        assert!(out.stdout.is_empty());
        assert_eq!(out.status.code(), Some(0));
    }

    /// A child that exits 0 while a descendant still holds both pipes open
    /// must not hang the run.
    ///
    /// # The hang the deadline does not cover
    ///
    /// The direct child exits, so `try_wait` returns `Some` on the first poll
    /// and the poll loop breaks — but the background job inherited the write
    /// ends of stdout and stderr, so neither read end ever sees an EOF. A
    /// join performed *after* the loop has broken sits outside the deadline
    /// entirely, so `ro` waits on that pipe forever with nothing armed to
    /// stop it. The buffer-full deadlock was fixed; this is what is left.
    ///
    /// The call is moved onto its own thread with a hard stop, for the
    /// reason the integration test gives: a test that hangs takes the whole
    /// suite with it and prints nothing, which is indistinguishable from a
    /// machine that ran out of memory.
    #[cfg(unix)]
    #[test]
    fn a_child_that_exits_while_a_descendant_holds_the_pipes_does_not_hang() {
        let dir = tempfile::TempDir::new().expect("the fixture dir is creatable");
        // The descendant holds the pipes for 300 s. The deadline is 300 ms,
        // so a run that honours it returns in well under a second and a run
        // that waits on the pipes is still waiting when the hard stop fires.
        //
        // The script is written under the **name the engine looks for**,
        // for the same reason the integration fixture does: `FakeBinary::at`
        // expects the file at `dir/<name>`, and a script written under any
        // other name is a shim that resolves to nothing.
        let script = dir.path().join("claude");
        std::fs::write(&script, "#!/bin/sh\nsleep 300 &\nexit 0\n")
            .expect("the script is writable");
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(&script)
                .expect("the script exists")
                .permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(&script, p).expect("the mode is settable");
        }

        let mut cmd = std::process::Command::new(&script);
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_with_deadline(
                &mut cmd,
                Duration::from_millis(300),
                None,
            ));
        });

        let out = rx
            .recv_timeout(Duration::from_secs(15))
            .expect("the run did not return within 15s; the child exited 0 but a descendant still held the pipes, and the join that collects them is outside the deadline");

        let out = out.expect("a child that exits must not be reported as a timeout");
        assert_eq!(out.status.code(), Some(0));
    }

    /// The deadline the caller gives is the deadline that fires.
    ///
    /// `core.timeout_secs` is documented as bounding a command and does not
    /// reach the engine at all — `orchestrator.rs` builds its context with
    /// `dispatch::default_timeout()` and nothing reads the configured value.
    /// This pins the half that is in this crate's power: whatever deadline
    /// the context carries is the deadline the engine honours. A context
    /// built with 300 ms against a child that sleeps for 300 s must come
    /// back as `TimedOut` in about 300 ms; if the engine ignored the field
    /// and used its own default, this would take ten minutes.
    #[cfg(unix)]
    #[test]
    fn a_short_context_timeout_really_does_fire() {
        let dir = tempfile::TempDir::new().expect("the fixture dir is creatable");
        let script = dir.path().join("sleeper.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 300\n").expect("the script is writable");
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(&script)
                .expect("the script exists")
                .permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(&script, p).expect("the mode is settable");
        }

        let mut cmd = std::process::Command::new("sh");
        cmd.arg(&script)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let started = Instant::now();
        let out = run_with_deadline(&mut cmd, Duration::from_millis(300), None);
        let elapsed = started.elapsed();

        match out {
            Err(RunError::TimedOut) => {}
            other => panic!("a child that never exits must time out, got {other:?}"),
        }
        assert!(
            elapsed < Duration::from_secs(5),
            "the run took {elapsed:?}; a 300 ms deadline that is not honoured \
             would take the engine's own default"
        );
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
        assert_eq!(
            find_with_extensions("claude", &path, ".COM;.EXE;.BAT;.CMD"),
            None
        );
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
        let path =
            std::env::join_paths([std::path::PathBuf::from(""), dir.path().to_path_buf()]).unwrap();
        let found = find_with_extensions("claude", &path, ".COM;.EXE;.BAT;.CMD").unwrap();
        assert_eq!(found, dir.path().join("claude.exe"));
    }

    // ---- `.git/config` is read before and after, and a change is reported ----

    /// The module doc promises that a change to `.git/config` is reported.
    /// For a long time nothing did it — no hash, no read, no report, just a
    /// sentence describing a control that did not exist. An agent that ran
    /// `git config user.email something-else` left that write in place for
    /// every commit after it, and ro had already reported success.
    ///
    /// The report is a pure function over the two files, so it is tested
    /// here rather than only through a repository: the parts that matter are
    /// that a *removed* key is reported, and that a secret is not printed.
    #[test]
    fn a_change_to_git_config_is_reported_with_the_key_and_both_values() {
        let before = "[user]\n\tname = Ro User\n\temail = ro@example.com\n";
        let after = "[user]\n\tname = Ro User\n\temail = agent@elsewhere.invalid\n";
        let report = config_drift_report(before.as_bytes(), after.as_bytes())
            .expect("a changed value must be reported");
        assert!(
            report.contains("user.email"),
            "the key is what makes the report actionable, got: {report}"
        );
        assert!(
            report.contains("ro@example.com") && report.contains("agent@elsewhere.invalid"),
            "and both ends of the change, so the user can see what it became: \
             {report}"
        );
    }

    /// A key that was *removed* is a change too, and it is the one a user is
    /// least likely to notice on their own. Reporting only additions would
    /// leave "the agent deleted your user.name" silent.
    #[test]
    fn a_removed_config_key_is_reported() {
        let before = "[user]\n\tname = Ro User\n\temail = ro@example.com\n";
        let after = "[user]\n\temail = ro@example.com\n";
        let report = config_drift_report(before.as_bytes(), after.as_bytes())
            .expect("a removed key must be reported");
        assert!(
            report.contains("user.name"),
            "the removed key must be named, got: {report}"
        );
        assert!(
            report.contains("(unset)"),
            "and its new state must be stated rather than omitted: {report}"
        );
    }

    /// A key that was *added* is reported the same way.
    #[test]
    fn an_added_config_key_is_reported() {
        let before = "[user]\n\temail = ro@example.com\n";
        let after = "[user]\n\temail = ro@example.com\n\tname = Agent\n";
        let report = config_drift_report(before.as_bytes(), after.as_bytes())
            .expect("an added key must be reported");
        assert!(report.contains("user.name"), "got: {report}");
        assert!(report.contains("Agent"), "got: {report}");
    }

    /// The negative control. Without it, the tests above would also pass on
    /// a report that fired on every run — and a report that fires on every
    /// run is a report nobody reads, which is how the original silence
    /// happened.
    #[test]
    fn an_unchanged_config_is_not_reported() {
        let config = "[user]\n\tname = Ro User\n\temail = ro@example.com\n";
        assert_eq!(
            config_drift_report(config.as_bytes(), config.as_bytes()),
            None
        );
    }

    /// A reformat is not a change. Reporting "the agent changed your config"
    /// for a comment or a blank line trains the user to ignore the report,
    /// and the next real one is the one that gets missed.
    #[test]
    fn a_reformatted_config_is_not_reported() {
        let before = "[user]\n\tname = Ro User\n";
        let after = "# a comment\n[user]\n\n\tname = Ro User\n";
        assert_eq!(
            config_drift_report(before.as_bytes(), after.as_bytes()),
            None,
            "no key changed, so nothing was changed"
        );
    }

    /// # The one assertion that matters most
    ///
    /// `.git/config` is full of places a credential lives:
    /// `http.extraheader` holds an `Authorization:` header, and any `*.url`
    /// can hold `https://user:token@host`. A report that printed those would
    /// move a secret out of a file and into a terminal, a log, and quite
    /// possibly a bug report — the exact leak `env.rs` exists to prevent,
    /// arriving through the one door nobody thought to check.
    ///
    /// The key is still printed. "http.extraheader" says what to look at,
    /// and that is most of what makes the report actionable.
    #[test]
    fn a_secret_in_git_config_is_never_printed() {
        let before = "[user]\n\temail = ro@example.com\n";
        let after = concat!(
            "[user]\n\temail = ro@example.com\n",
            "[http \"https://github.com\"]\n",
            "\textraheader = Authorization: Bearer ghp_16C7e42F292c6912E7710c838347Ae178B4a\n",
        );
        let report = config_drift_report(before.as_bytes(), after.as_bytes())
            .expect("a changed config must be reported");
        assert!(
            report.contains("extraheader"),
            "the key is safe to print and is what makes this actionable: {report}"
        );
        assert!(
            !report.contains("ghp_16C7e42F292c6912E7710c838347Ae178B4a"),
            "the value must not appear anywhere in the report, got: {report}"
        );
        assert!(
            !report.contains("Authorization"),
            "nor the header name with its value: {report}"
        );
    }

    /// A token in a *remote URL* is the other place this bites, and it is
    /// the one an agent is most plausibly going to write.
    #[test]
    fn a_token_in_a_remote_url_is_not_printed() {
        let before = "[remote \"origin\"]\n\turl = https://github.com/a/b\n";
        let after = "[remote \"origin\"]\n\turl = https://x-access-token:ghp_16C7e42F292c6912E7710c838347Ae178B4a@github.com/a/b\n";
        let report = config_drift_report(before.as_bytes(), after.as_bytes())
            .expect("a changed URL must be reported");
        assert!(!report.contains("ghp_"), "got: {report}");
        assert!(report.contains("url"), "the key is still named: {report}");
    }

    /// A value that merely *looks* like a token is redacted too. The cost of
    /// a false positive is one redacted value in a warning; the cost of a
    /// false negative is a credential in a transcript.
    #[test]
    fn a_token_shaped_value_is_redacted_even_in_an_innocent_key() {
        let before = "[core]\n\trepositoryformatversion = 0\n";
        let after = "[core]\n\trepositoryformatversion = 0\n\teditor = ghp_16C7e42F292c6912E7710c838347Ae178B4a\n";
        let report = config_drift_report(before.as_bytes(), after.as_bytes())
            .expect("a changed value must be reported");
        assert!(!report.contains("ghp_"), "got: {report}");
    }

    /// A linked worktree's config is the **main repository's** config.
    ///
    /// # The bug this caught
    ///
    /// The first version of this read used `git rev-parse --git-dir`, which
    /// is the obvious repair for "a linked worktree's `.git` is a file". It
    /// is wrong: for a linked worktree `--git-dir` returns
    /// `.git/worktrees/<id>`, and **there is no `config` file there**. Every
    /// key the report is about lives in the main repository's config, and
    /// that is where a `git config` run from inside the linked worktree
    /// writes. So the read followed the same resolution git's own write
    /// does — `--git-common-dir` — and this test is what proved the
    /// difference: it failed against the first version, on a machine with
    /// nothing unusual installed.
    ///
    /// A fleet is exactly where linked worktrees are used, so the one
    /// arrangement in which the control would have been silently dead is the
    /// one it exists for.
    #[cfg(unix)]
    #[test]
    fn a_linked_worktrees_config_is_read() {
        let w = ro_testkit::Worktree::with_one_commit();
        w.write("a.txt", "x\n");
        w.commit("the base");

        // A linked worktree: `.git` here is a *file* pointing at the main
        // worktree's `.git/worktrees/<id>`.
        let linked = tempfile::TempDir::new().expect("the worktree dir is creatable");
        let out = std::process::Command::new(ro_testkit::git_path())
            .args([
                "worktree",
                "add",
                "-q",
                "-b",
                "side",
                linked.path().to_str().unwrap(),
            ])
            .current_dir(w.path())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        // The fixture: a config change made *from inside* the linked
        // worktree. It lands in the main repository's config, which is the
        // file the report has to read.
        std::process::Command::new(ro_testkit::git_path())
            .args(["config", "user.email", "agent@elsewhere.invalid"])
            .current_dir(linked.path())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .output()
            .expect("git runs");

        let bytes = config_bytes(linked.path());
        assert!(
            !bytes.is_empty(),
            "a linked worktree's config must be readable, or the report \
             never fires for the setup that needs it most"
        );
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("agent@elsewhere.invalid"),
            "and it must be the shared config the write landed in, got: {text}"
        );
    }

    /// A repo whose config cannot be read is not an error.
    ///
    /// `unwrap_or_default` turns "no file" into "unchanged", and a report
    /// that failed to build because there was nothing to report would be the
    /// wrong kind of loud. The directory here is not a repository at all,
    /// which is the cheapest way to make the read fail without depending on
    /// what the machine happens to have.
    #[test]
    fn an_unreadable_config_is_not_an_error() {
        let dir = tempfile::TempDir::new().expect("the dir is creatable");
        assert!(
            config_bytes(dir.path()).is_empty(),
            "a repo with no config to read is a fact, not a failure"
        );
    }

    // ---- a commit failure carries a reason ----

    /// `git commit` writes "nothing to commit, working tree clean" to
    /// **stdout**, and `ro_git::primitives::commit_all_as` builds its error
    /// from stderr alone — so that one failure arrives as
    /// `git commit failed: ` with nothing after it. It is reachable by an
    /// ordinary agent: a plan naming a file that does not differ from HEAD
    /// commits nothing, and the run reports a failure with a blank cause
    /// after the earlier groups have already landed.
    #[test]
    fn a_blank_commit_failure_is_given_a_reason() {
        let group = CommitGroup {
            subject: "the group".into(),
            files: vec!["a.txt".into()],
        };
        let detail = commit_failure_detail(
            std::path::Path::new("/nonexistent"),
            "git commit failed: ",
            &group,
        );
        assert!(
            detail.contains("committing \"the group\" failed"),
            "the failure itself is still the headline: {detail}"
        );
        assert!(
            detail.contains("nothing to commit"),
            "the cause git actually gave must be named, not left blank: {detail}"
        );
        assert!(
            detail.contains("a.txt"),
            "and the paths the group named, so the user can check them: {detail}"
        );
    }

    /// A failure that *does* carry a reason is left alone. Padding a real
    /// git error with an explanation of a different failure is how a report
    /// stops being believed.
    #[test]
    fn a_failure_with_a_reason_is_left_alone() {
        let group = CommitGroup {
            subject: "the group".into(),
            files: vec!["a.txt".into()],
        };
        let detail = commit_failure_detail(
            std::path::Path::new("/nonexistent"),
            "git commit failed: pathspec 'a.txt' did not match any file(s) known to git",
            &group,
        );
        assert!(
            !detail.contains("nothing to commit"),
            "a real reason must not be replaced by an invented one: {detail}"
        );
        assert!(
            detail.contains("pathspec"),
            "and the real one survives: {detail}"
        );
    }

    /// The explanation distinguishes the two ways the index can be empty.
    /// "None of these paths differ from HEAD" and "git says they are still
    /// changed" are different news with different fixes, and a report that
    /// merged them would send the user to look at the wrong thing.
    #[test]
    fn an_empty_index_explanation_names_the_paths_that_differ() {
        let porcelain = " M a.txt\n?? b.txt\n";
        let files = ["a.txt".to_string(), "b.txt".to_string()];
        let detail = describe_empty_index(porcelain, &files);
        assert!(
            detail.contains("a.txt") && detail.contains("b.txt"),
            "both paths are named: {detail}"
        );
        assert!(
            detail.contains("still changed"),
            "this is the case where the index was NOT empty: {detail}"
        );
    }

    #[test]
    fn an_empty_index_explanation_says_when_nothing_differs() {
        let porcelain = "";
        let files = ["a.txt".to_string()];
        let detail = describe_empty_index(porcelain, &files);
        assert!(
            detail.contains("differ from HEAD"),
            "this is the case where the group named nothing that changed: {detail}"
        );
        assert!(
            !detail.contains("still changed"),
            "and it must not claim the opposite: {detail}"
        );
    }

    // ---- a moved HEAD with nothing enumerable is not "committed nothing" ----

    /// `EngineOutcome::render` turns an empty `Committed` into "committed
    /// nothing", and HEAD moving while ro enumerates nothing would therefore
    /// be reported as a success that did not happen — the same false report
    /// this module keeps fixing, one layer down.
    ///
    /// # Why this is not the test it replaced
    ///
    /// The version that stood here constructed an `EngineOutcome::Failed`
    /// literal and asserted its `render()` did not contain "committed
    /// nothing". It never called `report_agent_made_commits`, and it would
    /// have passed unchanged against a build that reported a moved HEAD as
    /// `Committed { commits: vec![] }` — the exact bug it was written to
    /// catch. A test that counts as coverage it did not provide is worse
    /// than no test.
    ///
    /// So this one drives the real path: a repository whose HEAD moves while
    /// `commits_between` enumerates nothing, through `checkpoint`, and asserts
    /// on the outcome that comes back. The `render` assertion is kept as the
    /// second half, because the false report is what the user would have
    /// seen.
    #[cfg(unix)]
    #[test]
    fn an_empty_commit_list_is_not_rendered_as_committed_nothing() {
        let w = ro_testkit::Worktree::with_one_commit();
        w.write("a.txt", "x\n");
        w.commit("the tip the agent will undo");

        // The agent resets back to the first commit: HEAD moves, the tree is
        // clean, and there is nothing between the two refs for
        // `commits_between` to enumerate.
        let dir = tempfile::TempDir::new().expect("the shim dir is creatable");
        // The script is written under the **name the engine will look
        // for**. `FakeBinary::at` takes the directory and the name and
        // expects the file to already be at `dir/<name>`; this wrote
        // `dir/reset.sh` and named the shim `claude`, so the probe found
        // nothing and the run reported `Unavailable` for a shim that was
        // sitting right there.
        let script = dir.path().join("claude");
        std::fs::write(&script, "#!/bin/sh\ngit reset -q --hard HEAD~1\nexit 0\n")
            .expect("the script is writable");
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(&script)
                .expect("the script exists")
                .permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(&script, p).expect("the mode is settable");
        }
        let shim = ro_testkit::FakeBinary::at(dir.path().to_path_buf(), script, "claude");
        let engine = AgentEngine::with(
            EngineKind::Claude,
            shim.program().to_string_lossy().to_string(),
            None,
        );
        let ctx = EngineContext::new(w.path(), "main").with_timeout(Duration::from_secs(30));

        let outcome = unsafe { TestEnv::new().shim(&shim).run(|| engine.checkpoint(&ctx)) };

        match &outcome {
            EngineOutcome::Failed { error, .. } => {
                assert!(
                    error.contains("could not enumerate"),
                    "the report must say that ro could not name what happened. \
                     Got: {error}"
                );
            }
            other => panic!(
                "a moved HEAD with nothing enumerable must not be reported as a \
                 clean run, got {other:?}"
            ),
        }
        assert!(
            !outcome.render().contains("committed nothing"),
            "and it must never render as a success: {}",
            outcome.render()
        );
    }

    // ---- `--message` ----

    /// A blank `--message` is not a subject. `git_engine.rs` reads the field
    /// the same way, and a `--message ""` that silently committed under a
    /// generated subject would be the flag doing the opposite of what it
    /// says.
    #[test]
    fn a_blank_message_is_not_a_subject() {
        let ctx = EngineContext::new(std::path::Path::new("/nonexistent"), "main")
            .with_subject(Some("   "));
        assert_eq!(subject_override(&ctx), None);
    }

    #[test]
    fn a_real_message_is_a_subject() {
        let ctx = EngineContext::new(std::path::Path::new("/nonexistent"), "main")
            .with_subject(Some("  the user's own words  "));
        assert_eq!(subject_override(&ctx), Some("the user's own words"));
    }
}
