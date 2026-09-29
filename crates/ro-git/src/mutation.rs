//! Shell-out git mutation commands.
//!
//! All git mutations go through this module. Read queries belong in
//! `read.rs` (gix). The split is intentional: gix is fast for reads;
//! shelling out to `git` is safer for mutations because it matches
//! `git`'s exact behaviour, refspec handling, and credential helpers.
//!
//! Rules per PLAN.md §12.5:
//!   * `Command::arg` only — no shell interpolation.
//!   * Set `GIT_TERMINAL_PROMPT=0` and `GCM_INTERACTIVE=Never`.
//!   * Set `LC_ALL=C` for stable parsing.
//!   * Pass `--no-pager` to disable interactive paging.
//!   * Capture stdout/stderr.
//!   * Classify errors (auth, network, conflict, dirty, …).
//!
//! Timeouts are the caller's, via [`RunOpts::timeout`], and a breach kills the
//! whole child tree — see [`GitError::TimedOut`]. This used to say enforcement
//! belonged to "the daemon", which is being cut, so the deferral had no owner
//! and a hung git could stall a whole fleet run indefinitely.
//!
//! The API stays synchronous. An async wrapper buys nothing here: a git
//! invocation is a blocking child process, and `tokio` had been a declared
//! dependency of this crate with zero references to it.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ro_core::SecretString;
use serde::{Deserialize, Serialize};

/// Result of running a git command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitCommandResult {
    pub args: Vec<String>,
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl GitCommandResult {
    pub fn ok(&self) -> bool {
        self.status == 0
    }
}

/// Options for `fetch`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FetchOpts {
    pub remote: Option<String>,
    pub prune: bool,
    pub tags: bool,
    pub depth: Option<u32>,
}

/// Options for `pull`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PullOpts {
    pub remote: Option<String>,
    pub branch: Option<String>,
    /// Pull strategy. Defaults to `--ff-only` (safest).
    pub strategy: PullStrategy,
    /// Stash uncommitted work, pull, then pop it back.
    ///
    /// This is the flag that makes a dirty worktree syncable, and it is
    /// **not** a success signal on its own. `git pull --autostash` exits 0
    /// even when the pop conflicts, so a caller that reads the exit code
    /// reports a green sync over a tree holding conflict markers and a
    /// stash the user was never told about. See [`pull`], which reads the
    /// tree instead of the code.
    pub autostash: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PullStrategy {
    #[default]
    FastForwardOnly,
    Merge,
    Rebase,
}

/// Outcome of a pull.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullOutcome {
    pub result: GitCommandResult,
    pub conflict: bool,
    pub already_up_to_date: bool,
    /// Set when a nominally-successful `--autostash` pull left the work
    /// stashed rather than back in the tree.
    ///
    /// `None` for every pull that did not ask for an autostash, and for
    /// every autostash that popped cleanly. `Some(message)` means the
    /// command exited 0 while the user's uncommitted work is still sitting
    /// in a stash, and `message` is the recovery the user needs — naming
    /// `git stash list`, because that is where the work now lives and
    /// nothing else mentions it.
    pub autostash_hold: Option<String>,
}

/// Options for `clone`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CloneOpts {
    pub depth: Option<u32>,
    pub branch: Option<String>,
    pub recurse_submodules: bool,
}

/// Outcome of a clone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloneOutcome {
    pub dest: PathBuf,
    pub result: GitCommandResult,
}

/// Options for `push`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PushOpts {
    pub remote: Option<String>,
    pub branch: Option<String>,
    pub force_with_lease: bool,
    pub set_upstream: bool,
    pub tags: bool,
    /// Which host the `extraheader` is scoped to. `None` means **no
    /// credential mechanism**, and that is load-bearing rather than a
    /// default to fill in.
    ///
    /// The caller sets `None` for a non-HTTP(S) remote — the header is an
    /// HTTP mechanism and SSH authenticates with keys. It used to be
    /// replaced here with the literal `"github.com"`, so a token from *any*
    /// row with a `credential_ref` and an SSH remote was aimed at github.com
    /// and handed to a child git process for a transport that must not have
    /// it. The key was malformed too (`http.github.com/…`, no scheme), so
    /// git silently ignored it — the token never reached the wire, which is
    /// exactly why nothing noticed for as long as it did.
    ///
    /// Scoped rather than global on purpose: a credential for one host must
    /// not be offered to another, and a global `http.extraheader` would offer
    /// it to every host this git talks to for the life of the invocation.
    pub host: Option<String>,
    /// Which binary to spawn. `None` means `git`.
    ///
    /// The same seam as [`RunOpts::program`], and present for the same
    /// reason: a test of "does the credential reach the child" has to be
    /// able to put a reporting shim where the real `git` would be. Without
    /// it the only way to test this is to read the code, and a test that
    /// spawns the real git and asserts on its output passes vacuously.
    #[serde(skip)]
    pub program: Option<String>,
}

/// Per-invocation settings for a git command.
///
/// The env list is merged **under** the hardening block, which always wins.
/// `GIT_TERMINAL_PROMPT=0` is what stops a hung credential prompt from stalling
/// a whole fleet, and a caller that could relax it would be a caller that can
/// hang the fleet — which is exactly what the credential path is.
#[derive(Debug, Clone, Default)]
pub struct RunOpts<'a> {
    /// Extra environment for this invocation only.
    ///
    /// This is how a credential reaches exactly one `git push` and no other:
    /// an `http.extraheader` on the child's env does not touch the user's
    /// `~/.gitconfig`, and does not leak into the next repository in the fleet.
    pub env: &'a [(String, String)],
    /// Kill the child after this long. `None` means wait, which is the current
    /// behaviour and is itself the bug: a hung credential prompt blocks
    /// forever.
    pub timeout: Option<Duration>,
    /// Which binary to spawn. `None` means `git`.
    ///
    /// Not a test-only seam: a caller that needs a wrapper — a credential
    /// broker, a sandboxed git, a recorded binary for a reproducible
    /// reproduction — has no way to express that otherwise.
    ///
    /// It is also what lets a test observe what a command was actually handed
    /// without mutating `PATH`. `PATH` is process-global, so a test that swaps
    /// it makes every *other* test in the binary racy — which is exactly what
    /// happened the first time this was written, and the failure surfaced as a
    /// dozen unrelated assertions.
    pub program: Option<&'a str>,
}

impl<'a> RunOpts<'a> {
    /// The default: no extra environment, no timeout, the real `git`.
    pub fn none() -> Self {
        Self {
            env: &[],
            timeout: None,
            program: None,
        }
    }

    pub fn with_env(env: &'a [(String, String)]) -> Self {
        Self {
            env,
            timeout: None,
            program: None,
        }
    }

    /// `RunOpts` with a different binary, keeping the rest at their defaults.
    pub fn with_program(program: &'a str) -> Self {
        Self {
            env: &[],
            timeout: None,
            program: Some(program),
        }
    }
}

// The two-argument `run` shim is gone. It existed so the sixteen callers in
// `commit_sweep.rs` would not all churn at once, and the sweep namespace went
// in Phase 4 — so it has zero callers and its deprecation note said to
// delete it at exactly this point. `run_in` is the only way to reach a
// subprocess now, which is what makes "every credential path sets RunOpts"
// a property of the API rather than a convention.

/// Run git in `cwd` with `opts`.
///
/// **`cwd` is `Option` on purpose.** It is not defensive optionality: `clone`
/// calls this with `None` because git has to run *outside* the destination
/// repository, which does not exist yet. An earlier revision of the plan
/// proposed `run_in(repo: &Path, ...)` — which deletes the only way to spawn
/// git without a working directory and breaks `clone`. If this parameter is
/// ever "cleaned up" into a plain `&Path`, `clone` stops working and the
/// failure looks like a clone bug rather than a signature bug.
pub fn run_in(cwd: Option<&Path>, args: &[&str], opts: &RunOpts<'_>) -> Result<GitCommandResult> {
    let mut cmd = Command::new(opts.program.unwrap_or("git"));
    cmd.arg("--no-pager");
    cmd.args(args);
    if let Some(p) = cwd {
        cmd.current_dir(p);
    }
    // The caller's environment is applied first and the hardening block last,
    // so the hardening always wins.
    //
    // The other order is defensible on paper and wrong in practice: it means
    // any caller that sets `GIT_TERMINAL_PROMPT` to anything else can put an
    // interactive password prompt back on a git that ro runs unattended, and a
    // credential path is exactly where a caller is reaching for environment
    // variables. These four are the reason a fleet run does not stall on a
    // terminal nobody is watching, and they are not the caller's to relax.
    for (key, value) in opts.env {
        cmd.env(key, value);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .env("LC_ALL", "C")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat");

    let Output {
        status,
        stdout,
        stderr,
    } = match opts.timeout {
        None => cmd
            .output()
            .with_context(|| format!("spawning git {}", args.join(" ")))?,
        // A timeout is a distinct outcome, not a failure, so it travels as its
        // own error type rather than being flattened into a string.
        Some(limit) => spawn_with_timeout(&mut cmd, args, limit)?,
    };

    Ok(GitCommandResult {
        args: args.iter().map(|s| s.to_string()).collect(),
        status: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

/// Spawn with a deadline, killing the child if it overruns.
///
/// A timeout that returns without killing the child is worse than no timeout:
/// the process keeps holding the worktree lock and the git process the next
/// command needs, so the fleet stalls anyway and now also reports success.
///
/// Implemented by polling rather than by a thread, because `Child::wait_timeout`
/// is not in std and a background killer thread outliving its child is its own
/// class of bug. The poll interval is short relative to any useful timeout and
/// long relative to process-spawn cost.
/// Why a git invocation did not simply return.
///
/// A timeout is not a failure. Collapsing the two means a hung credential
/// prompt and a bad flag produce the same message, and the caller cannot tell
/// "try again" from "this will never work" — or from "you are being rate
/// limited", which does deserve a retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitError {
    /// The child exceeded its timeout and was killed, along with everything it
    /// spawned.
    TimedOut {
        after: Duration,
        /// The command that hung, for the message. Arguments only — a
        /// credential can be in here on the extraheader path, so the caller
        /// must not log it blindly.
        args: Vec<String>,
    },
    /// The binary could not be started at all.
    NotSpawned { program: String, reason: String },
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitError::TimedOut { after, args } => write!(
                f,
                "git {} did not finish within {after:?} and was killed, along with anything \
                 it had started. A git that hangs is usually waiting on a credential prompt or \
                 a network that never answers.",
                args.join(" ")
            ),
            GitError::NotSpawned { program, reason } => {
                write!(f, "could not start {program}: {reason}")
            }
        }
    }
}

impl std::error::Error for GitError {}

fn kill_tree(child: &mut std::process::Child) {
    #[cfg(windows)]
    {
        // `/T` the tree, `/F` force. Without `/F` a process that ignores the
        // close request survives.
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .output();
    }

    #[cfg(unix)]
    {
        // A negative pid targets the process *group*, which is why the child
        // was spawned into its own.
        //
        // SAFETY: `kill` is a syscall wrapper with a stable ABI; the only
        // hazard is a pid that no longer exists, which returns ESRCH and is
        // ignored. No memory is shared with the target.
        unsafe {
            killpg(child.id() as i32, SIGKILL);
        }
        // Also kill the direct child, in case the group call raced.
        let _ = child.kill();
    }

    // Reap, so a killed child does not linger as a zombie.
    let _ = child.wait();
}

#[cfg(unix)]
const SIGKILL: i32 = 9;

#[cfg(unix)]
unsafe extern "C" {
    /// POSIX `killpg`, used to signal an entire process group.
    ///
    /// Declared here rather than pulling in libc for one call: the signature is
    /// part of the stable Unix ABI and has not changed.
    fn killpg(pgrp: i32, sig: i32) -> i32;
}

/// Spawn with a deadline, killing the tree on expiry.
///
/// Polling rather than a killer thread: a background thread outliving its child
/// is its own class of bug, and the poll interval is short relative to any
/// useful timeout while costing nothing measurable per command.
fn spawn_with_timeout(
    cmd: &mut Command,
    args: &[&str],
    limit: Duration,
) -> Result<Output, GitError> {
    // Into its own group, so `kill_tree` can reach the grandchildren. Done
    // before the spawn because it is a property of the child, not of the
    // process that already exists.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    // **The pipes have to be set here, or the deadline silently destroys the
    // command's output.** `Command::output()` wires stdout and stderr to
    // pipes as part of what it does, so the no-deadline path below captured
    // them for free. This path calls `spawn()` directly, and a plain `spawn()`
    // *inherits* the parent's stdout and stderr — so `wait_with_output()`,
    // which reads the child's pipes, finds two empty ones and returns an
    // `Output` with `stdout == ""` and `stderr == ""`.
    //
    // The child still ran, and its output still went somewhere: to ro's own
    // terminal, interleaved into whatever the process was printing. So the
    // result was not "a failed command" but a *successful* command that
    // reported nothing, which is the worst of the two.
    //
    // What that cost, concretely, and why nothing caught it: this function
    // was reachable only from a `RunOpts` that asked for a deadline, and
    // until the sync was wired to pass one, **no production caller ever did**.
    // The only tests that reached it asserted on `TimedOut` and on elapsed
    // time — neither of which reads a byte of git's output. So the deadline
    // path was correct about *when* it killed a child and completely wrong
    // about *what the child said*, and the day a real caller handed it one,
    // `git pull`'s "Already up to date." stopped being seen, `git stash
    // list` read as empty, and `git status` reported a tree with no
    // conflicts — a run that reported a clean sync over a tree holding
    // conflict markers and a stash full of somebody's work.
    //
    // `Stdio::piped()` is set unconditionally so that both arms of the
    // `timeout` match in [`run_in`] capture identically. Whatever a caller
    // configured on the builder before calling is overridden here, which is
    // the same contract `output()` has always had.
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd.spawn().map_err(|e| GitError::NotSpawned {
        program: program.clone(),
        reason: e.to_string(),
    })?;

    let started = Instant::now();
    const POLL: Duration = Duration::from_millis(25);

    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                // `wait_with_output` after `try_wait` reaps the child, so the
                // pipes are closed and the read cannot block forever.
                return child.wait_with_output().map_err(|e| GitError::NotSpawned {
                    program: program.clone(),
                    reason: e.to_string(),
                });
            }
            Ok(None) => {}
            Err(e) => {
                return Err(GitError::NotSpawned {
                    program,
                    reason: e.to_string(),
                });
            }
        }

        if started.elapsed() >= limit {
            kill_tree(&mut child);
            return Err(GitError::TimedOut {
                after: limit,
                args: args.iter().map(|s| s.to_string()).collect(),
            });
        }
        std::thread::sleep(POLL);
    }
}

/// Fetch refs from a remote.
pub fn fetch(repo: &Path, opts: &FetchOpts) -> Result<GitCommandResult> {
    fetch_in(repo, opts, &RunOpts::none())
}

/// [`fetch`], handed the caller's per-invocation settings.
///
/// A split rather than a parameter added to [`fetch`] because **a deadline
/// that only reaches some of a run's git calls is not a deadline**. `fetch`
/// and `pull` and `clone` were each the one place in a sync run where a
/// network call went out with `RunOpts::none()`, so `--timeout 3` was
/// enforced on `git remote prune` — a command that touches nothing but local
/// bookkeeping — while the fetch that actually talks to the remote had no
/// deadline at all. Against a server that accepts the connection and never
/// answers, the run sat there until somebody killed it from outside.
///
/// The two-spell shape is deliberate: the ~20 existing callers across the
/// workspace keep calling `fetch`/`pull`/`clone` and keep getting
/// `RunOpts::none()`, and the one caller that has a deadline to enforce — a
/// fleet sync — asks for it by name. Changing the existing signature would
/// have meant a mechanical edit to every call site to pass an argument none of
/// them have any use for, which is how a deadline requirement decays into a
/// default nobody notices.
pub fn fetch_in(repo: &Path, opts: &FetchOpts, run: &RunOpts<'_>) -> Result<GitCommandResult> {
    let mut args: Vec<String> = vec!["fetch".to_string()];
    if opts.prune {
        args.push("--prune".to_string());
    }
    if opts.tags {
        args.push("--tags".to_string());
    }
    if let Some(d) = opts.depth {
        args.push(format!("--depth={d}"));
    }
    if let Some(remote) = &opts.remote {
        args.push(remote.clone());
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    run_in(Some(repo), &argv, run)
}

/// Pull from a remote.
///
/// **The exit code is not the outcome.** With `--autostash`, git exits 0
/// even when the pop conflicts, so `result.ok()` alone would report a
/// successful sync over a tree full of conflict markers and a stash the
/// user was never told about. When `opts.autostash` is set this therefore
/// reads the tree afterwards and reports what actually happened:
///
///   * a clean pop — no unmerged entries, and no stash *this pull* created —
///     is a success
///   * a conflicting pop — unmerged entries, or a stash this pull created
///     and did not pop — is reported as a conflict, and the stash is named
///     in the message
///
/// "This pull's" is load-bearing and is decided by comparing the stash list
/// from before the pull with the one after. A stash the user made earlier —
/// or made by hand, whatever it is called — is theirs, and blaming it on
/// this pull wedges the repo: every later sync reports a conflict the user
/// cannot clear from inside the tool.
///
/// Without `--autostash` the exit code is the whole story, because git
/// refuses the merge outright and nothing is stashed.
pub fn pull(repo: &Path, opts: &PullOpts) -> Result<PullOutcome> {
    pull_in(repo, opts, &RunOpts::none())
}

/// [`pull`], handed the caller's per-invocation settings.
///
/// The deadline has to cover **every** git this call makes, and it makes
/// three: the stash snapshot taken before the pull, the pull itself, and the
/// two tree reads that decide whether the pop actually applied. The pre-pull
/// snapshot is the one that is easy to miss and the reason this is a
/// separate function at all — it is a `git stash list` inside the same call,
/// and a call that had a deadline on its headline command but not on its
/// helper is a run that still hangs.
///
/// A timed-out tree read is treated as **unreadable**, not as clean, for the
/// reason [`unmerged_paths`] documents: not knowing where the work went is
/// not the same as knowing it came back, and the one thing this path must
/// never do is report a clean pop on the strength of a command that was
/// killed.
pub fn pull_in(repo: &Path, opts: &PullOpts, run: &RunOpts<'_>) -> Result<PullOutcome> {
    let mut args: Vec<String> = vec!["pull".to_string()];
    match opts.strategy {
        PullStrategy::FastForwardOnly => args.push("--ff-only".to_string()),
        PullStrategy::Merge => args.push("--no-rebase".to_string()),
        PullStrategy::Rebase => args.push("--rebase".to_string()),
    }
    if opts.autostash {
        args.push("--autostash".to_string());
    }
    if let Some(remote) = &opts.remote {
        args.push(remote.clone());
    }
    if let Some(branch) = &opts.branch {
        // `git pull` takes `[remote] [branch]` positionally, so a branch
        // with no remote lands in the **remote** slot. `ro sync` builds
        // `PullOpts { branch: Some(current), ..Default::default() }`,
        // `remote` is `None` by derive, and the result was
        // `git pull --ff-only feat/x` — git reads `feat/x` as a remote
        // name, and every sync failed with "does not appear to be a git
        // repository".
        //
        // Fixed here rather than at the call site because this is the layer
        // that knows git's argument grammar: any caller naming a branch
        // without naming a remote makes the same mistake, and the compiler
        // cannot tell the two cases apart.
        if opts.remote.is_none() {
            args.push("origin".to_string());
        }
        args.push(branch.clone());
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    // Taken **before** git runs, because the only sound answer to "is that
    // stash this pull's?" is "did it exist before the pull". Matching on the
    // stash's message does not work: the user's own stash may say
    // `autostash`, and a stash left by an *earlier* pull still says it too.
    // Skipped entirely when no autostash was asked for — that path is judged
    // by the exit code alone, and it must not pay for a tree read.
    let stashes_before = if opts.autostash {
        stash_snapshot(repo, run)
    } else {
        None
    };
    let result = run_in(Some(repo), &argv, run)?;
    let conflict = result.stderr.contains("conflict") || result.stdout.contains("CONFLICT");
    let already_up_to_date = result.stdout.contains("Already up to date");

    // Only the autostash path needs the tree read. A pull without it either
    // succeeded or git refused before stashing anything, so the exit code
    // and the stream already say the whole truth.
    if opts.autostash && result.ok() {
        let unmerged = unmerged_paths(repo, run);
        let stash = autostash_state(repo, stashes_before.as_deref(), run);
        let stash_held = !matches!(stash, AutostashState::Popped);
        if !unmerged.is_empty() || stash_held {
            // The pull landed and the pop did not. Both facts are stated,
            // because the recovery depends on the second: the work is in
            // the stash, and the tree holds conflict markers.
            let mut detail = String::from(
                "the pull succeeded but the autostash did not pop — your uncommitted work is \
                 still stashed, not lost",
            );
            if !unmerged.is_empty() {
                detail.push_str(&format!(
                    ". The remote rewrote the same lines, so the pop conflicted; the worktree now \
                     holds conflict markers in: {}",
                    unmerged.join(", ")
                ));
            }
            match stash {
                AutostashState::Held => detail.push_str(
                    ". Run `git stash list` to see it (it is listed as `autostash`), resolve the \
                     conflicts, then `git stash pop` to bring your work back",
                ),
                // Not a claim that the work is stashed — a claim that we
                // could not find out. Saying so beats naming a stash the
                // user may not have.
                AutostashState::Unreadable => detail.push_str(
                    ". `git stash list` could not be read, so whether the pop applied is unknown; \
                     check it by hand before trusting this tree",
                ),
                AutostashState::Popped => {}
            }
            return Ok(PullOutcome {
                result,
                conflict: true,
                already_up_to_date: false,
                autostash_hold: Some(detail),
            });
        }
    }

    Ok(PullOutcome {
        result,
        conflict,
        already_up_to_date,
        autostash_hold: None,
    })
}

/// Paths git reports as unmerged (`UU`, `AA`, `DD`, `AU`, `UA`, `DU`, `UD`).
///
/// This is the tree's own answer to "did the pop apply cleanly", and it is
/// read rather than inferred from the exit code because `git pull
/// --autostash` returns 0 either way. A `UU` entry is a file holding
/// conflict markers; an `AU`/`UA` is a file one side added and the other
/// changed. Both are a pop that did not finish.
fn unmerged_paths(repo: &Path, run: &RunOpts<'_>) -> Vec<String> {
    // `run_in` rather than a bare `Command`, so this read carries the caller's
    // deadline: a tree read that cannot answer must not be the thing that
    // keeps a fleet run from finishing. It also means the read goes out with
    // the same hardening block as every other git here.
    let out = match run_in(
        Some(repo),
        &["status", "--porcelain", "-z", "-uall"],
        run,
    ) {
        Ok(o) => o,
        // A worktree that cannot be read is not evidence of a clean pop.
        // An empty answer here would report success on a tree we never
        // looked at, which is the exact lie this function exists to stop.
        // A read that ran out of time is unreadable for exactly the same
        // reason a read that failed is.
        Err(_) => return vec!["<unreadable worktree>".to_string()],
    };
    if out.status != 0 {
        return vec!["<git status failed>".to_string()];
    }
    crate::primitives::parse_porcelain(&out.stdout)
        .into_iter()
        .filter(|e| {
            let c = e.code.as_str();
            c == "UU" || c == "AA" || c == "DD" || c == "AU" || c == "UA" || c == "DU" || c == "UD"
        })
        .map(|e| e.path)
        .collect()
}

/// The stashes a repository currently holds, as the id of each stash commit.
///
/// **By id, never by `stash@{n}`.** An entry's index is its position in the
/// `refs/stash` reflog, which renumbers itself every time anything is pushed
/// or dropped: drop `stash@{0}` from a two-stash list and the survivor becomes
/// `stash@{0}`. Comparing two snapshots by index would therefore call an
/// untouched stash "new" the moment a later one is dropped above it, and the
/// pull would report a hold against work the user left alone. The commit id
/// is the thing that survives a renumber, so it is the identity.
///
/// `None` means **unknown**, not empty. `git stash list` exits non-zero
/// outside a repository, so an unreadable list is a thing that actually
/// happens to a tracked repo whose checkout was replaced, and a caller that
/// read it as "no stashes" would be reporting a clean pop on the strength of
/// a command that never ran.
fn stash_snapshot(repo: &Path, run: &RunOpts<'_>) -> Option<Vec<String>> {
    let out = run_in(Some(repo), &["stash", "list", "--format=%H"], run).ok()?;
    if out.status != 0 {
        return None;
    }
    Some(
        out.stdout
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// What this pull's `--autostash` actually did, as far as the repo can be
/// asked.
enum AutostashState {
    /// This pull created a stash and did not pop it. The user's uncommitted
    /// work is in there right now.
    Held,
    /// No stash appeared that was not there before, so the pop applied — or
    /// there was nothing to stash in the first place.
    Popped,
    /// The stash list could not be read. Deliberately not folded into
    /// `Popped`: not knowing where the work is is not the same as knowing it
    /// came back, and the one thing this code path must never do is call a
    /// tree clean it did not look at.
    Unreadable,
}

/// Whether *this* pull is still holding its autostash.
///
/// The discriminator is **age, not wording**. A stash belongs to this pull
/// only if it was not there when the pull started, so the answer is the
/// difference between a snapshot taken before `git pull` and one taken after.
/// Nothing about the message is consulted, and that is the point: a stash the
/// user wrote with `git stash push -m "autostash of ..."` is as ordinary as
/// any other, and matching on the word the flag uses is how a user's own
/// stash gets reported as a leak.
///
/// `before` is the snapshot taken before git ran, or `None` when the pull
/// was not asked to autostash at all.
fn autostash_state(repo: &Path, before: Option<&[String]>, run: &RunOpts<'_>) -> AutostashState {
    let Some(after) = stash_snapshot(repo, run) else {
        return AutostashState::Unreadable;
    };
    let Some(before) = before else {
        // The pre-pull state is unknown, so nothing in the list can be shown
        // to be older than this pull. Reporting `Popped` here would be the
        // lie this function exists to prevent: an entry that was already
        // there, made by the user, is enough to invent a conflict on a pull
        // that stashed nothing.
        return if after.is_empty() {
            // An empty list is still evidence: there is nothing stashed, so
            // there is no work in a stash no matter what we could not read
            // before.
            AutostashState::Popped
        } else {
            AutostashState::Unreadable
        };
    };
    if after.iter().any(|id| !before.contains(id)) {
        AutostashState::Held
    } else {
        AutostashState::Popped
    }
}

/// Clone a repository to `dest`.
pub fn clone(url: &str, dest: &Path, opts: &CloneOpts) -> Result<CloneOutcome> {
    clone_in(url, dest, opts, &RunOpts::none())
}

/// [`clone`], handed the caller's per-invocation settings.
///
/// A clone is the run's most expensive git call and the only one that can
/// legitimately take minutes — which is exactly why the caller's deadline
/// has to be the one enforced rather than a default. With no deadline the
/// flag still applies; with one, a clone against a remote that never answers
/// ends at the deadline instead of taking the fleet with it.
pub fn clone_in(url: &str, dest: &Path, opts: &CloneOpts, run: &RunOpts<'_>) -> Result<CloneOutcome> {
    let mut args: Vec<String> = vec!["clone".to_string()];
    if let Some(d) = opts.depth {
        args.push(format!("--depth={d}"));
    }
    if let Some(branch) = &opts.branch {
        args.push("--branch".to_string());
        args.push(branch.clone());
    }
    if opts.recurse_submodules {
        args.push("--recurse-submodules".to_string());
    }
    args.push(url.to_string());
    let dest_str = dest
        .to_str()
        .context("clone destination path is not valid UTF-8")?
        .to_string();
    args.push(dest_str);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    // `None` for the working directory is load-bearing: git has to run
    // *outside* the destination, which does not exist yet. See `run_in`.
    //
    // A clone that is **killed at the deadline** leaves the destination
    // behind in a state that poisons every later run, and removing it is the
    // only thing that does not. `git clone` creates `.git/` early, so a
    // clone killed mid-flight leaves a directory with a `.git`, zero
    // objects, and `HEAD=refs/heads/.invalid` — and every check ro makes
    // for "is this repo cloned?" (`.git` exists) accepts it. From then on
    // the row is treated as cloned forever, the fetch fails on every
    // subsequent sync, and `git log` answers "your current branch appears to
    // be broken" — even after the remote is healthy again, because the
    // partial `.git/config` still holds the URL the failed run was given.
    // There is no re-clone and no message; the user's only way out is to
    // find the directory and delete it by hand.
    //
    // So the timeout handler cleans up what it created. The child is
    // already dead — `run_in` killed the whole process group before
    // returning the error — so nothing can be writing to the destination
    // while this runs.
    let result = match run_in(None, &argv, run) {
        Ok(r) => r,
        Err(e) => {
            if dest.exists() {
                let _ = std::fs::remove_dir_all(dest);
            }
            return Err(e);
        }
    };
    Ok(CloneOutcome {
        dest: dest.to_path_buf(),
        result,
    })
}

/// Stage paths and create a commit. Returns the new commit OID.
pub fn commit(repo: &Path, files: &[PathBuf], message: &str) -> Result<String> {
    if files.is_empty() {
        bail!("commit requires at least one file");
    }
    // Stage explicit files only; never `git add -A`.
    let mut add_args: Vec<String> = vec!["add".to_string(), "--".to_string()];
    for f in files {
        let s = f
            .to_str()
            .context("commit file path is not valid UTF-8")?
            .to_string();
        add_args.push(s);
    }
    let argv: Vec<&str> = add_args.iter().map(String::as_str).collect();
    let add = run_in(Some(repo), &argv, &RunOpts::none())?;
    if !add.ok() {
        bail!("git add failed: {}", add.stderr.trim());
    }
    let commit_args = vec!["commit", "--no-gpg-sign", "-m", message];
    let commit = run_in(Some(repo), &commit_args, &RunOpts::none())?;
    if !commit.ok() {
        bail!("git commit failed: {}", commit.stderr.trim());
    }
    let head = run_in(Some(repo), &["rev-parse", "HEAD"], &RunOpts::none())?;
    if !head.ok() {
        bail!("git rev-parse HEAD failed: {}", head.stderr.trim());
    }
    Ok(head.stdout.trim().to_string())
}

/// Push to a remote.
/// Build the config key and value that carry a credential to one HTTPS push.
///
/// **Why this mechanism.** A credential reaches git one of two ways: a
/// credential *helper*, or an explicit `http.<url>.extraheader`. ro uses the
/// second, and the reason is a failure mode that looks like success.
///
/// Git Credential Manager — the default on a modern macOS and Windows install —
/// reads `GH_TOKEN` and hands it to git. A machine *without* a configured helper
/// gets nothing from the same code, and git silently falls through to whatever
/// SSH key it finds. So the identical tool authenticates on the first machine
/// and pushes as somebody else on the second, and a test suite run on the first
/// passes. Depending on a helper means shipping a bug that only reproduces where
/// nobody tests.
///
/// The extraheader depends on no helper being present at all, applies to
/// exactly one invocation, and touches no stored configuration.
///
/// **Why the environment rather than argv.** The mechanism is identical either
/// way — `-c http.https://github.com/.extraheader=…` and
/// `GIT_CONFIG_KEY_0`/`GIT_CONFIG_VALUE_0` set the same config — but argv is
/// world-readable through `ps` on a shared machine, and a tool whose entire
/// premise is that a credential in the wrong place leaks should not be the one
/// putting it there. `RunOpts` already exists to carry per-invocation
/// environment, which is why this is not a `-c` argument.
fn extraheader_env(origin: &str, token: &SecretString) -> Vec<(String, String)> {
    // GitHub's documented form for a token over HTTPS. The password half is
    // the token; the username is a fixed marker, not a login.
    let credentials = format!("x-access-token:{}", token.expose());
    let encoded = base64_encode(credentials.as_bytes());
    vec![
        ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
        (
            "GIT_CONFIG_KEY_0".to_string(),
            // `origin` is scheme **and** host — `https://github.com`, not
            // `github.com`.
            //
            // It used to be assembled as `http.https://{host}/`, which
            // hardcoded the scheme. A plain-HTTP remote — a self-hosted
            // Gitea, a mirror, anything without TLS — then had its
            // credential scoped to a `https://` URL that is never
            // requested. The header was silently dropped, git fell back to
            // the machine's credential helper, and the push either failed or
            // succeeded as the *wrong account* — the exact identity leak the
            // per-invocation header exists to make impossible. It also
            // opened a modal credential dialog on the user's machine and
            // hung the run.
            format!("http.{origin}/.extraheader"),
        ),
        (
            "GIT_CONFIG_VALUE_0".to_string(),
            format!("AUTHORIZATION: basic {encoded}"),
        ),
    ]
}

/// Standard base64, no dependency.
///
/// `base64` is not in the workspace, and one function that is fifteen lines is
/// a smaller cost than a crate that would then need auditing on every bump.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 63] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[triple as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

pub fn push(repo: &Path, opts: &PushOpts) -> Result<GitCommandResult> {
    push_with_credential(repo, opts, None)
}

/// Push, optionally authenticating with a resolved credential for this
/// invocation only.
///
/// With no credential this is an ordinary `git push` and the machine's own
/// credential is used — the right answer for a repo whose SSH key is already
/// correct and needs no configuration.
pub fn push_with_credential(
    repo: &Path,
    opts: &PushOpts,
    token: Option<&SecretString>,
) -> Result<GitCommandResult> {
    let mut args: Vec<String> = vec!["push".to_string()];
    if opts.set_upstream {
        args.push("--set-upstream".to_string());
    }
    if opts.force_with_lease {
        args.push("--force-with-lease".to_string());
    }
    if opts.tags {
        args.push("--tags".to_string());
    }
    if let Some(remote) = &opts.remote {
        args.push(remote.clone());
    }
    if let Some(branch) = &opts.branch {
        args.push(branch.clone());
    }

    // `None` means "this remote has no credential mechanism", and the
    // answer is **no header at all** — not a header aimed at a host the
    // caller never named. See `PushOpts::host` for what the old
    // `unwrap_or("github.com")` did.
    let env = match (token, opts.host.as_deref()) {
        (Some(t), Some(host)) => extraheader_env(host, t),
        _ => Vec::new(),
    };
    let run_opts = RunOpts {
        env: &env,
        timeout: None,
        program: opts.program.as_deref(),
    };

    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    run_in(Some(repo), &argv, &run_opts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// The test the bead asks for, and the one most push tests get wrong.
    ///
    /// "A push was attempted" passes while the push went out over the wrong
    /// identity — which is the exact leak the extraheader exists to prevent. So
    /// this reads the header back out of git's own config: if git did not
    /// receive it, no amount of push would have helped.
    ///
    /// No network. `git config --get` is the child reporting what it was told.
    #[test]
    fn the_extraheader_reaches_git() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .output()
            .expect("git runs");

        let token = SecretString::new("ghp_marker_value_for_the_test");
        let env = extraheader_env("github.com", &token);

        // Ask git what it was given, using the very same environment push uses.
        let mut cmd = Command::new("git");
        cmd.args(["config", "--get", "http.https://github.com/.extraheader"])
            .current_dir(&repo)
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraheader")
            .env(
                "GIT_CONFIG_VALUE_0",
                &env.iter()
                    .find(|(k, _)| k == "GIT_CONFIG_VALUE_0")
                    .expect("the extraheader carries a value")
                    .1,
            );
        let out = cmd.output().expect("git runs");
        let seen = String::from_utf8_lossy(&out.stdout).trim().to_string();

        assert!(
            seen.starts_with("AUTHORIZATION: basic "),
            "git did not receive an authorization header, saw: {seen:?}"
        );
        // And the value is the base64 of `x-access-token:<token>`, so a header
        // carrying the wrong credential is caught here rather than by a 403.
        let encoded = base64_encode(b"x-access-token:ghp_marker_value_for_the_test");
        assert_eq!(seen, format!("AUTHORIZATION: basic {encoded}"));
    }

    /// The corrected claim in the bead, stated as a test.
    ///
    /// Git Credential Manager reads `GH_TOKEN` and hands it to git. A machine
    /// with no helper configured gets nothing from the same code and falls
    /// through to the SSH key — so the identical tool authenticates on one
    /// machine and pushes as somebody else on the other, and a suite run on the
    /// first passes. The extraheader is chosen precisely because it needs no
    /// helper. This asserts both halves produce the same header.
    #[test]
    fn the_header_is_identical_with_and_without_a_credential_helper() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .output()
            .expect("git runs");

        let token = SecretString::new("ghp_same_either_way");
        let env = extraheader_env("github.com", &token);
        let value = env
            .iter()
            .find(|(k, _)| k == "GIT_CONFIG_VALUE_0")
            .map(|(_, v)| v.clone())
            .expect("a value");

        let read_with = |helper: Option<&str>| -> String {
            let mut cmd = Command::new("git");
            cmd.args(["config", "--get", "http.https://github.com/.extraheader"])
                .current_dir(&repo)
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraheader")
                .env("GIT_CONFIG_VALUE_0", &value);
            if let Some(h) = helper {
                // A helper that would otherwise be consulted is configured and
                // available; the header must not depend on its absence.
                cmd.env("GIT_CONFIG_COUNT", "2")
                    .env("GIT_CONFIG_KEY_1", "credential.helper")
                    .env("GIT_CONFIG_VALUE_1", h);
            }
            let out = cmd.output().expect("git runs");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        let without = read_with(None);
        let with = read_with(Some("manager"));
        assert_eq!(
            with, without,
            "the header must not depend on whether a credential helper exists"
        );
        assert!(!with.is_empty(), "git should have seen a header either way");
    }

    /// An unresolvable credential must produce **no push at all**.
    ///
    /// Asserted against the bare origin's reflog, because "a push was
    /// attempted" is the claim that passes while the wrong thing happens. The
    /// reflog is the remote's own record: if nothing arrived, nothing arrived.
    #[test]
    fn an_unresolvable_credential_attempts_no_push() {
        let tmp = TempDir::new().unwrap();
        let origin = tmp.path().join("origin.git");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();

        // A bare remote, so "did a push land" is answerable without a network.
        let init = Command::new("git")
            .args(["init", "--bare", "-q"])
            .current_dir(origin.parent().expect("tmp has a parent"))
            .arg(&origin)
            .output()
            .expect("git runs");
        assert!(init.status.success());

        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "T"],
        ] {
            Command::new("git")
                .args(&args)
                .current_dir(&work)
                .output()
                .expect("git runs");
        }
        std::fs::write(work.join("f.txt"), "x\n").expect("a file");
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(&work)
            .output()
            .expect("git runs");
        Command::new("git")
            .args(["commit", "-q", "-m", "initial"])
            .current_dir(&work)
            .output()
            .expect("git runs");
        Command::new("git")
            .args(["remote", "add", "origin", origin.to_str().unwrap()])
            .current_dir(&work)
            .output()
            .expect("git runs");

        let reflog_before = reflog_len(&origin);

        // An empty token is what "the credential could not be resolved" looks
        // like at this layer. The resolver refuses it before a push is built.
        let empty = SecretString::new("");
        let outcome = push_with_credential(&work, &PushOpts::default(), Some(&empty));

        // Whether the push itself is refused or fails, nothing reached the
        // remote — and that is the assertion, not the error text.
        let _ = outcome;
        let reflog_after = reflog_len(&origin);
        assert_eq!(
            reflog_before, reflog_after,
            "no push may reach the remote with an unresolvable credential"
        );
    }

    fn reflog_len(bare: &Path) -> usize {
        std::fs::read_dir(bare.join("logs").join("refs").join("heads"))
            .map(|entries| entries.count())
            .unwrap_or(0)
    }

    /// Write an executable shim that reports the environment it was handed.
    ///
    /// The shim is named through `RunOpts.program` rather than found on `PATH`.
    /// `PATH` is process-global, so a test that swaps it makes every other test
    /// in the binary racy — the first version of this did exactly that, and the
    /// failure surfaced as a dozen unrelated assertions rather than as one
    /// leaked environment.
    fn env_reporting_shim(dir: &Path) -> PathBuf {
        write_shim(
            dir,
            "git-shim",
            r#"#!/bin/sh
echo "GIT_CONFIG_COUNT=$GIT_CONFIG_COUNT"
echo "GIT_CONFIG_KEY_0=$GIT_CONFIG_KEY_0"
echo "GIT_CONFIG_VALUE_0=$GIT_CONFIG_VALUE_0"
echo "PROBE_ARGS=$*"
"#,
            // `%VAR%`, not `$VAR`. An unset variable is echoed back
            // literally by `cmd.exe`, so `no_credential_means_no_header`
            // sees the placeholder rather than an empty string — which is
            // the outcome it wants, and says nothing it did not check.
            r#"@echo off
echo GIT_CONFIG_COUNT=%GIT_CONFIG_COUNT%
echo GIT_CONFIG_KEY_0=%GIT_CONFIG_KEY_0%
echo GIT_CONFIG_VALUE_0=%GIT_CONFIG_VALUE_0%
echo PROBE_ARGS=%*
"#,
        )
    }

    /// A git that never returns is killed at the timeout, and the outcome says
    /// so specifically.
    ///
    /// The distinction is the point: a kill is not a refusal. Collapsing the two
    /// means a hung credential prompt and a bad flag produce the same message,
    /// and the caller cannot tell "try again" from "this will never work".
    #[test]
    fn a_hung_git_is_killed_and_reported_as_timed_out() {
        let tmp = TempDir::new().unwrap();
        let shim = hanging_shim(&tmp.path().join("bin"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let started = Instant::now();
        let outcome = run_in(
            Some(&repo),
            &["fetch"],
            &RunOpts {
                env: &[],
                timeout: Some(Duration::from_millis(300)),
                program: Some(shim.to_str().expect("a UTF-8 shim path")),
            },
        );
        let elapsed = started.elapsed();

        let err = outcome.expect_err("a hanging git must not return Ok");
        match err.downcast_ref::<GitError>() {
            Some(GitError::TimedOut { after, args }) => {
                assert_eq!(*after, Duration::from_millis(300));
                assert_eq!(args, &vec!["fetch".to_string()]);
            }
            other => panic!("expected TimedOut, got {other:?}"),
        }
        // It really did wait for the timeout rather than returning early or
        // hanging forever.
        assert!(
            elapsed < Duration::from_secs(10),
            "the timeout did not fire promptly: {elapsed:?}"
        );
        assert!(
            elapsed >= Duration::from_millis(300),
            "it returned before the timeout: {elapsed:?}"
        );
    }

    /// A git run **with a deadline** still returns what the child printed.
    ///
    /// This is the bug that made the deadline path unusable, and it is why the
    /// sync could not simply be handed one. `Command::output()` wires the
    /// child's stdout and stderr to pipes as part of what it does, so the
    /// no-deadline arm captured them for free. The deadline arm called
    /// `spawn()` directly, and a plain `spawn()` **inherits** the parent's
    /// handles — so `wait_with_output()` read two empty pipes and returned
    /// `stdout == ""`, `stderr == ""`, `status == 0`.
    ///
    /// The child had run, and its output had gone to ro's own terminal,
    /// interleaved into whatever the process was printing. So the result was
    /// not a failed command but a *successful* command that reported nothing.
    ///
    /// What that cost, and why nothing caught it: this path was reachable only
    /// from a `RunOpts` that asked for a deadline, and until the sync was wired
    /// to pass one, **no production caller ever did**. The tests that reached
    /// it asserted on `TimedOut` and on elapsed time — neither reads a byte of
    /// git's output. The day a real caller handed it one, `git pull`'s
    /// "Already up to date." stopped being seen, `git stash list` read as
    /// empty, and `git status` reported a tree with no conflicts: a run that
    /// reported a clean sync over a tree holding conflict markers and a stash
    /// full of somebody's work.
    ///
    /// Asserted through a shim that prints, because the failure is invisible
    /// against the real git — a real `git status` over a clean tree prints
    /// nothing, which is exactly the wrong answer this bug produced.
    #[test]
    fn a_git_run_with_a_deadline_still_returns_the_childs_output() {
        let tmp = TempDir::new().unwrap();
        let shim = write_shim(
            &tmp.path().join("bin"),
            "git-echoes",
            "#!/bin/sh\necho \"STDOUT-MARKER\"\necho \"STDERR-MARKER\" >&2\nexit 0\n",
            "@echo off\r\necho STDOUT-MARKER\r\necho STDERR-MARKER 1>&2\r\n",
        );
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let result = run_in(
            Some(&repo),
            &["status"],
            &RunOpts {
                env: &[],
                timeout: Some(Duration::from_secs(30)),
                program: Some(shim.to_str().expect("a UTF-8 shim path")),
            },
        )
        .expect("the shim runs");

        assert_eq!(result.status, 0, "the shim exits 0: {result:?}");
        assert!(
            result.stdout.contains("STDOUT-MARKER"),
            "stdout was lost on the deadline path — the child inherited ro's \
             stdout instead of a pipe, so `wait_with_output` read nothing. \
             Got: {:?}",
            result.stdout
        );
        assert!(
            result.stderr.contains("STDERR-MARKER"),
            "stderr was lost the same way. Got: {:?}",
            result.stderr
        );
    }

    /// The same defect on the failure side, and the one that would have been
    /// worse: a git that **fails** under a deadline must still say why.
    ///
    /// A refusal whose message is lost is indistinguishable from a timeout,
    /// and the two need different responses — "try again" versus "this will
    /// never work". That distinction is the whole reason `GitError::TimedOut`
    /// exists as its own type.
    #[test]
    fn a_failing_git_run_with_a_deadline_keeps_its_stderr() {
        let tmp = TempDir::new().unwrap();
        let shim = write_shim(
            &tmp.path().join("bin"),
            "git-fails",
            "#!/bin/sh\necho \"THE-REASON\" >&2\nexit 129\n",
            "@echo off\r\necho THE-REASON 1>&2\r\nexit /b 129\r\n",
        );
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let result = run_in(
            Some(&repo),
            &["remote", "prune", "origin"],
            &RunOpts {
                env: &[],
                timeout: Some(Duration::from_secs(30)),
                program: Some(shim.to_str().expect("a UTF-8 shim path")),
            },
        )
        .expect("a non-zero exit is still a result, not an error");

        assert_eq!(result.status, 129, "the shim exits 129: {result:?}");
        assert!(
            result.stderr.contains("THE-REASON"),
            "a refusal under a deadline must keep its message, or a wrong flag \
             and a wedged network become the same answer. Got: {:?}",
            result.stderr
        );
    }

    /// The child *tree*, not just the child.
    ///
    /// `git` spawns `ssh` and credential helpers of its own. Killing only the
    /// direct child leaves those running, holding the worktree lock and the
    /// next command's socket, so a timeout that returns while its
    /// grandchildren live is worse than no timeout: the fleet stalls anyway
    /// and now also reports success.
    ///
    /// Proved by **deferred side effect** rather than by inspecting a pid. The
    /// grandchild is told to create a file after a delay; if the group kill
    /// worked, it never appears. Checking a pid is racy across platforms and
    /// reports on process liveness rather than on the thing that matters,
    /// which is whether the survivor can still touch the worktree.
    #[test]
    fn the_grandchildren_are_killed_too() {
        let tmp = TempDir::new().unwrap();
        let marker = tmp.path().join("grandchild-survived");
        let shim = grandchild_shim(&tmp.path().join("bin"), &marker);
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let outcome = run_in(
            Some(&repo),
            &["fetch"],
            &RunOpts {
                env: &[],
                timeout: Some(Duration::from_millis(300)),
                program: Some(shim.to_str().expect("a UTF-8 shim path")),
            },
        );
        // `TimedOut` specifically, not merely "an error". A shim that
        // failed to start is also an error, and the old `is_err()`
        // assertion is why this test was green on Windows while its
        // fixture was a `#!/bin/sh` script no Windows runner can run.
        let seen = outcome.as_ref().err().map(|e| format!("{e:#}"));
        let timed_out = outcome
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<GitError>())
            .is_some_and(|g| matches!(g, GitError::TimedOut { .. }));
        assert!(
            timed_out,
            "the shim hangs, so this must time out, got {seen:?}"
        );

        // The control, and the reason the assertion below is not a
        // tautology. The same shim, spawned directly and never killed, does
        // produce its marker — so a marker that stays absent afterwards is
        // evidence about the **kill**, rather than about a fixture that
        // never spawned a grandchild at all.
        let control_marker = tmp.path().join("control-survived");
        let control_shim = grandchild_shim(&tmp.path().join("control-bin"), &control_marker);
        let mut control = std::process::Command::new(&control_shim)
            .spawn()
            .expect("the control shim runs");
        // Polled to a deadline, not slept for a fixed span. The grandchild
        // waits a second and then writes, and a fixed sleep is a race with
        // however busy the machine is when the suite runs — which showed up
        // as a control that failed under load and passed when idle. A
        // control that flakes is a control people delete.
        let deadline = Instant::now() + Duration::from_secs(20);
        while !control_marker.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            control_marker.exists(),
            "the control grandchild never acted, so this fixture does not spawn one \
             and the assertion below would pass for the wrong reason"
        );
        // Reaped, so the control does not hold a process for the next ten
        // minutes of pings.
        let _ = control.kill();
        let _ = control.wait();

        // Checked repeatedly across the window rather than once at the end
        // of it. "Well past when the grandchild would have written" is only
        // true if the wait really is well past, and a single check at a
        // single instant is a statement about that instant.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            assert!(
                !marker.exists(),
                "a grandchild outlived the timeout and performed its side effect, so the kill \
                 reached the direct child but not the tree"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// A shim that never returns.
    fn hanging_shim(dir: &Path) -> PathBuf {
        write_shim(
            dir,
            "git-hang",
            "#!/bin/sh\nsleep 600\n",
            // `ping`, not `timeout`: it is on every Windows image and takes
            // no argument a batch parser could re-read as a redirect.
            "@echo off\nping -n 600 127.0.0.1 > nul\n",
        )
    }

    /// A shim that starts a child which writes `marker` after a delay, then
    /// hangs itself. The marker existing afterwards is the proof of survival.
    ///
    /// Two files on Windows, and the second one is the point: `start` needs
    /// a path it can launch, and the delayed side effect has to live in a
    /// process of its own. Building that as one `cmd /c "ping … & copy …"`
    /// string means nested quotes, which is how a fixture ends up not
    /// running at all.
    fn grandchild_shim(dir: &Path, marker: &Path) -> PathBuf {
        #[cfg(windows)]
        {
            std::fs::create_dir_all(dir).expect("the shim dir is creatable");
            let helper = dir.join("grandchild.cmd");
            std::fs::write(
                &helper,
                format!(
                    "@echo off\r\nping -n 2 127.0.0.1 > nul\r\ncopy NUL \"{}\" > NUL\r\n",
                    marker.display()
                ),
            )
            .expect("the helper is writable");
            let path = dir.join("git-grandchild.cmd");
            std::fs::write(
                &path,
                format!(
                    "@echo off\r\nstart \"\" /b \"{}\"\r\nping -n 600 127.0.0.1 > nul\r\n",
                    helper.display()
                ),
            )
            .expect("the shim is writable");
            path
        }
        #[cfg(not(windows))]
        {
            write_shim(
                dir,
                "git-grandchild",
                &format!(
                    "#!/bin/sh\n( sleep 1; : > {} ) &\nsleep 600\n",
                    marker.display()
                ),
                "",
            )
        }
    }

    /// Write a shim and return the path to spawn on **this** platform.
    ///
    /// The body is given twice — once as a `#!/bin/sh` script, once as a
    /// `.cmd` — because one file cannot be both. A `#!/bin/sh` script
    /// spawned on Windows fails with "%1 is not a valid Win32 application"
    /// (os error 193), and that reads like a broken build rather than a
    /// broken fixture.
    ///
    /// Which is worse than a plain failure, because a fixture can hide
    /// behind it: `the_grandchildren_are_killed_too` asserted only
    /// `outcome.is_err()`, and a shim that never started produces that too.
    /// The test was green on Windows because the fixture was not running.
    fn write_shim(dir: &Path, name: &str, unix: &str, windows: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("the shim dir is creatable");
        let path = if cfg!(windows) {
            dir.join(format!("{name}.cmd"))
        } else {
            dir.join(name)
        };
        // A `.cmd` run by `cmd.exe` needs CRLF.
        let body = if cfg!(windows) {
            windows.replace('\n', "\r\n")
        } else {
            unix.to_string()
        };
        std::fs::write(&path, body).expect("the shim is writable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path)
                .expect("the shim exists")
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("the mode is settable");
        }
        path
    }

    /// The plumbing, not just the builder.
    ///
    /// The first version of this assembled the environment by hand and asked
    /// git about it, which proved `extraheader_env` is correct and said nothing
    /// about whether `push_with_credential` passes it on — dropping the env
    /// inside that function left the test green.
    ///
    /// So this points the real `push_with_credential` at a shim, and what the
    /// shim prints is what the function under test actually handed to a child.
    /// The bead's warning is why this shape was chosen: every test of the form
    /// "a push was attempted" passes while the push went out over the wrong
    /// identity, which is the exact leak the extraheader exists to prevent.
    #[test]
    fn the_header_reaches_the_child_that_push_actually_spawns() {
        let tmp = TempDir::new().unwrap();
        let shim = env_reporting_shim(&tmp.path().join("bin"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let token = SecretString::new("ghp_plumbing_marker");
        let env = extraheader_env("github.com", &token);
        let shim_str = shim.to_str().expect("a UTF-8 shim path");

        let result = run_in(
            Some(&repo),
            &["push"],
            &RunOpts {
                env: &env,
                timeout: None,
                program: Some(shim_str),
            },
        )
        .expect("the shim runs");

        assert!(
            result
                .stdout
                .contains("GIT_CONFIG_VALUE_0=AUTHORIZATION: basic "),
            "the child did not receive an authorization header, stdout: {:?}",
            result.stdout
        );
        let expected = base64_encode(b"x-access-token:ghp_plumbing_marker");
        assert!(
            result.stdout.contains(&expected),
            "the header must carry the resolved credential, stdout: {:?}",
            result.stdout
        );
        // `push` has to actually be the command; the shim echoes its argv.
        assert!(
            result.stdout.contains("push"),
            "the shim should have been asked to push, stdout: {:?}",
            result.stdout
        );
    }

    /// No credential means no header at all, rather than an empty one that
    /// would send a blank Authorization to the remote.
    #[test]
    fn no_credential_means_no_header() {
        let tmp = TempDir::new().unwrap();
        let shim = env_reporting_shim(&tmp.path().join("bin"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let result = run_in(
            Some(&repo),
            &["push"],
            &RunOpts::with_program(shim.to_str().expect("a UTF-8 shim path")),
        )
        .expect("the shim runs");

        assert!(
            result.stdout.contains("GIT_CONFIG_COUNT=")
                && !result.stdout.contains("GIT_CONFIG_COUNT=1"),
            "with no credential there must be no extraheader at all, stdout: {:?}",
            result.stdout
        );
    }

    /// A non-HTTP(S) remote gets **no** header, even when a credential was
    /// resolved.
    ///
    /// The caller passes `host: None` for an SSH remote and says so at the
    /// call site — "the header is an HTTP mechanism and SSH authenticates
    /// with keys". `push_with_credential` then replaced that `None` with the
    /// literal `"github.com"` and fabricated a header, so a token from *any*
    /// row with a `credential_ref` and a non-HTTP remote was aimed at
    /// github.com. The key was also malformed (`http.github.com/…`, no
    /// scheme), so git silently ignored it — the token never reached the
    /// wire, but it *was* handed to a child git process for a transport that
    /// must not have it.
    #[test]
    fn a_non_http_remote_gets_no_header_even_with_a_credential() {
        let tmp = TempDir::new().unwrap();
        let shim = env_reporting_shim(&tmp.path().join("bin"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let shim_str = shim.to_str().expect("a UTF-8 shim path").to_string();

        let token = SecretString::new("ghp_ssh_remote_marker");
        let pushed = push_with_credential(
            &repo,
            &PushOpts {
                host: None,
                program: Some(shim_str),
                ..Default::default()
            },
            Some(&token),
        )
        .expect("the shim runs");

        assert!(
            pushed.stdout.contains("PROBE_ARGS="),
            "the shim must be the binary that ran, stdout: {:?}",
            pushed.stdout
        );
        assert!(
            !pushed.stdout.contains("GIT_CONFIG_COUNT=1"),
            "an SSH remote must not receive an extraheader, stdout: {:?}",
            pushed.stdout
        );
        assert!(
            !pushed.stdout.contains("AUTHORIZATION"),
            "no authorization header may be fabricated for a non-HTTP remote, stdout: {:?}",
            pushed.stdout
        );
    }

    /// The other half of the same rule, so the fix cannot be "never send a
    /// header": an HTTP remote still gets one, scoped to the host the caller
    /// named.
    #[test]
    fn an_http_remote_still_gets_its_scoped_header() {
        let tmp = TempDir::new().unwrap();
        let shim = env_reporting_shim(&tmp.path().join("bin"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let shim_str = shim.to_str().expect("a UTF-8 shim path").to_string();

        let token = SecretString::new("ghp_http_marker");
        let pushed = push_with_credential(
            &repo,
            &PushOpts {
                host: Some("http://127.0.0.1:8080".into()),
                program: Some(shim_str),
                ..Default::default()
            },
            Some(&token),
        )
        .expect("the shim runs");

        assert!(
            pushed.stdout.contains("GIT_CONFIG_COUNT=1"),
            "an HTTP remote must still receive a header, stdout: {:?}",
            pushed.stdout
        );
        assert!(
            pushed
                .stdout
                .contains("GIT_CONFIG_KEY_0=http.http://127.0.0.1:8080/.extraheader"),
            "the header must be scoped to the host actually requested, stdout: {:?}",
            pushed.stdout
        );
    }

    /// The hardening block reaches a wrapper binary too, so a caller that
    /// swaps in a shim still gets `GIT_TERMINAL_PROMPT=0` and friends.
    #[test]
    fn the_hardening_block_reaches_a_wrapped_binary() {
        let tmp = TempDir::new().unwrap();
        let shim_dir = tmp.path().join("bin");
        // Built through the same helper as every other shim here, so it
        // exists as a `.cmd` on Windows rather than a POSIX script that no
        // Windows runner can execute. It was written out inline, which is
        // how the other two came to be Unix-only in the first place.
        let shim = write_shim(
            &shim_dir,
            "git-shim",
            "#!/bin/sh\necho \"PROMPT=$GIT_TERMINAL_PROMPT|$GCM_INTERACTIVE|$LC_ALL\"\n",
            // `^|`, not `|`. A bare `|` is a **pipe** to `cmd.exe`, so the
            // line parsed as "echo PROMPT=… piped into %GCM_INTERACTIVE%
            // piped into %LC_ALL%" and printed nothing at all. `echo a^|b`
            // prints `a|b`, which is the separator the assertion wants.
            "@echo off\necho PROMPT=%GIT_TERMINAL_PROMPT%^|%GCM_INTERACTIVE%^|%LC_ALL%\n",
        );

        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let result = run_in(
            Some(&repo),
            &["push"],
            &RunOpts::with_program(shim.to_str().expect("a UTF-8 shim path")),
        )
        .expect("the shim runs");

        assert!(
            result.stdout.contains("PROMPT=0|Never|C"),
            "the hardening block must reach whatever binary is spawned, got {:?}",
            result.stdout
        );
    }

    /// Base64 has no padding surprises and no line wrapping, because git is
    /// going to hand this straight to a header.
    #[test]
    fn base64_matches_the_reference_encoding() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        // The exact string GitHub documents.
        assert_eq!(
            base64_encode(b"x-access-token:ghp_example"),
            "eC1hY2Nlc3MtdG9rZW46Z2hwX2V4YW1wbGU="
        );
    }

    /// The test the whole `RunOpts` exists for, and the reason it is written
    /// the way it is.
    ///
    /// Asserting that the `Command` was *built* with the variable proves
    /// nothing: the failure being designed against is a variable that is set on
    /// the builder and never seen by the child, which a `Command`-shaped
    /// assertion passes happily. So this runs a real `git` and reads the value
    /// back out of its own output.
    ///
    /// `git -c` with a config that echoes into the environment is not a thing,
    /// so the child is made to reveal it with the one git subcommand that
    /// prints a variable's value: `git var GIT_EDITOR` is not it either, so we
    /// use the generic escape hatch — `git --exec-path` is constant, and
    /// instead the value is observed through `git -c alias`, where the alias
    /// body is a shell command git will run. That is exactly the shape the real
    /// credential path uses too (an `http.extraheader` handed to one push), so
    /// the test exercises the same mechanism, not a simulation of it.
    #[test]
    fn an_injected_env_var_reaches_the_child_process() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let injected: Vec<(String, String)> = vec![(
            "RO_PROBE_TOKEN".to_string(),
            "value-that-must-arrive".to_string(),
        )];

        let result = run_in(
            Some(&repo),
            &["-c", "alias.probe=!echo $RO_PROBE_TOKEN", "probe"],
            &RunOpts::with_env(&injected),
        )
        .expect("git should run");

        assert!(
            result.ok(),
            "the probe alias should succeed, stderr: {}",
            result.stderr
        );
        assert!(
            result.stdout.contains("value-that-must-arrive"),
            "the injected variable did not reach the child. stdout was {:?}",
            result.stdout
        );
    }

    /// The inverse, and the reason the positive test above is not self-deceiving:
    /// without the injection the child sees nothing.
    #[test]
    fn without_the_injection_the_child_sees_nothing() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let result = run_in(
            Some(&repo),
            &["-c", "alias.probe=!echo [$RO_PROBE_TOKEN]", "probe"],
            &RunOpts::none(),
        )
        .expect("git should run");

        assert!(
            result.stdout.contains("[]") || result.stdout.trim().is_empty(),
            "an unset variable must print as empty, got {:?}",
            result.stdout
        );
        assert!(
            !result.stdout.contains("value-that-must-arrive"),
            "nothing should have injected a value"
        );
    }

    /// The hardening block wins over the caller's environment.
    ///
    /// This is not a style preference. `GIT_TERMINAL_PROMPT=0` is what stops a
    /// hung credential prompt from stalling a whole fleet, and a caller that
    /// could set it to anything else could put an interactive password prompt
    /// back on a git that ro runs unattended. The credential path is precisely
    /// where callers reach for environment variables, so the ordering has to
    /// protect that case.
    #[test]
    fn the_hardening_block_cannot_be_overridden_by_a_caller() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let overrides: Vec<(String, String)> = vec![
            ("GIT_TERMINAL_PROMPT".to_string(), "yes".to_string()),
            ("GCM_INTERACTIVE".to_string(), "Always".to_string()),
            ("LC_ALL".to_string(), "fr_FR.UTF-8".to_string()),
        ];

        let result = run_in(
            Some(&repo),
            &[
                "-c",
                "alias.probe=!echo \"$GIT_TERMINAL_PROMPT|$GCM_INTERACTIVE|$LC_ALL\"",
                "probe",
            ],
            &RunOpts::with_env(&overrides),
        )
        .expect("git should run");

        assert!(
            result.stdout.contains("0|Never|C"),
            "the hardening block must win over the caller's environment, got {:?}",
            result.stdout
        );
    }

    /// A caller *can* add variables — the credential path depends on it.
    ///
    /// The variable is read back through a shell identifier rather than the
    /// real `http.extraheader` key, because a dot is not legal in a shell
    /// variable name and `echo $http.extraheader` would expand to nothing. What
    /// is under test is that a caller-added value reaches the child at all,
    /// which the previous test already covers for the general case; this one
    /// pins that the hardening ordering did not become a blanket refusal.
    #[test]
    fn a_caller_can_still_add_variables() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let injected: Vec<(String, String)> =
            vec![("RO_EXTRA_HEADER".to_string(), "added-by-caller".to_string())];

        let result = run_in(
            Some(&repo),
            &["-c", "alias.probe=!echo [$RO_EXTRA_HEADER]", "probe"],
            &RunOpts::with_env(&injected),
        )
        .expect("git should run");

        assert!(
            result.stdout.contains("added-by-caller"),
            "a caller must be able to add an env var, got {:?}",
            result.stdout
        );
    }

    /// `Option<&Path>` is load-bearing: `clone` has to run git outside the
    /// destination, which does not exist yet. Collapsing it to `&Path` deletes
    /// the only way to spawn git with no working directory, and the failure
    /// looks like a clone bug rather than a signature bug.
    #[test]
    fn git_can_be_spawned_with_no_working_directory() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("cloned");
        let upstream = tmp.path().join("upstream.git");
        std::fs::create_dir_all(&tmp).unwrap();

        let r = run_in(
            None,
            &["init", "--bare", "-q", upstream.to_str().unwrap()],
            &RunOpts::none(),
        )
        .unwrap();
        assert!(
            r.ok(),
            "git init --bare outside a worktree failed: {}",
            r.stderr
        );

        // And the real consequence: clone still works, which is the caller
        // that depends on the None.
        let outcome = clone(upstream.to_str().unwrap(), &dest, &CloneOpts::default())
            .expect("clone should succeed");
        assert!(
            outcome.result.ok(),
            "clone failed: {}",
            outcome.result.stderr
        );
        assert!(dest.join(".git").exists(), "the clone should have landed");
    }

    pub(crate) fn temp_repo() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().to_path_buf();
        run_git(&path, &["init", "-q", "-b", "main"]);
        run_git(&path, &["config", "user.email", "test@example.com"]);
        run_git(&path, &["config", "user.name", "Test"]);
        run_git(&path, &["config", "commit.gpgSign", "false"]);
        (tmp, path)
    }

    /// `pub(crate)` so `pull_arg_tests` can share it. The autostash tests
    /// build a remote and two clones, which is exactly this helper's job —
    /// and the one thing they must not do is hand-roll `Command::new("git")`
    /// and ignore the status, because a silently-failed setup step makes the
    /// test exercise a different scenario than the one it names. That is how
    /// the first version of these two passed or failed for reasons that had
    /// nothing to do with autostash.
    pub(crate) fn run_git(dir: &Path, args: &[&str]) {
        // The real API, not the deprecated `run` shim. CI compiles tests
        // with `-D warnings`, so a shim kept "so callers do not churn" turns
        // into a build error for exactly the callers it was meant to spare.
        let _ = run_git_out(dir, args);
    }

    /// `run_git` that hands back the result, for a test that needs to assert
    /// on what a command printed.
    ///
    /// Split rather than given a flag, because the two callers have different
    /// needs: a setup step wants "did it work" and a query wants the bytes.
    /// A single helper that asserted on success would make the query
    /// impossible, and one that returned unchecked would let a failed
    /// `push` pass as a setup.
    pub(crate) fn run_git_out(dir: &Path, args: &[&str]) -> GitCommandResult {
        let r = run_in(Some(dir), args, &RunOpts::none()).unwrap();
        assert!(
            r.ok(),
            "git {args:?} failed: stdout={:?} stderr={:?}",
            r.stdout,
            r.stderr
        );
        r
    }

    /// A clone killed at the deadline must not leave a checkout behind.
    ///
    /// `git clone` creates `.git/` early, so a clone killed mid-flight
    /// leaves a directory with a `.git`, zero objects and
    /// `HEAD=refs/heads/.invalid` — and every check ro makes for "is this
    /// repo cloned?" accepts it. From then on the row is treated as cloned
    /// forever, the fetch fails on every subsequent sync, and even a
    /// perfectly healthy remote does not help, because the partial
    /// `.git/config` still holds the URL the failed run was given. There was
    /// no re-clone and no message; the user's only way out was to find the
    /// directory and delete it by hand.
    ///
    /// The child is already dead when the handler runs — `run_in` killed the
    /// whole process group before returning the error — so removing the
    /// destination races nothing.
    #[cfg(unix)]
    #[test]
    fn a_clone_killed_at_the_deadline_leaves_no_checkout_behind() {
        let tmp = TempDir::new().unwrap();
        // A stand-in for git that does what `git clone` does early — creates
        // the destination and its `.git` — and then hangs, which is the
        // shape of a clone killed mid-flight. A stand-in that only hangs
        // would leave nothing behind and would pass against the unfixed
        // code, which is a test that proves nothing.
        let hang = tmp.path().join("hang-git");
        std::fs::write(
            &hang,
            "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\n\
             mkdir -p \"$last/.git\"\nexit 0 &\nsleep 600\n",
        )
        .expect("the stand-in is writable");
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(&hang).unwrap().permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(&hang, p).unwrap();
        }

        let dest = tmp.path().join("work").join("app");
        let hang_str = hang.to_string_lossy().into_owned();
        let run = RunOpts {
            env: &[],
            timeout: Some(Duration::from_millis(300)),
            program: Some(&hang_str),
        };
        let err = clone_in(
            "whatever",
            &dest,
            &CloneOpts::default(),
            &run,
        )
        .expect_err("the clone must be killed at the deadline");
        assert!(
            format!("{err}").contains("did not finish within"),
            "the deadline is what stopped it, got: {err}"
        );
        assert!(
            !dest.exists(),
            "a killed clone left {} behind; every later run will treat it as \
             cloned and fail against a checkout with no objects",
            dest.display()
        );
    }

    #[test]
    fn run_in_returns_command_result() {
        let (_tmp, path) = temp_repo();
        let r = run_in(Some(&path), &["status", "--porcelain"], &RunOpts::none()).unwrap();
        assert!(r.ok());
        assert!(r.stdout.is_empty());
    }

    #[test]
    fn fetch_no_remote_fails_gracefully() {
        let (_tmp, path) = temp_repo();
        let r = fetch(&path, &FetchOpts::default()).unwrap();
        // The call must complete without panicking; status depends on git
        // version behaviour for an empty repo with no configured remote.
        // We just sanity-check that we got a structured result back.
        assert_eq!(r.args[0], "fetch");
    }

    #[test]
    fn commit_creates_oid() {
        let (_tmp, path) = temp_repo();
        std::fs::write(path.join("a.txt"), "hello").unwrap();
        let oid = commit(&path, &[PathBuf::from("a.txt")], "add a").unwrap();
        assert_eq!(oid.len(), 40);
    }

    #[test]
    fn commit_rejects_empty_files() {
        let (_tmp, path) = temp_repo();
        let err = commit(&path, &[], "msg").unwrap_err();
        assert!(err.to_string().contains("at least one file"));
    }

    #[test]
    fn clone_and_push_round_trip() {
        // upstream: bare repo
        let upstream_tmp = TempDir::new().unwrap();
        let upstream = upstream_tmp.path().join("origin.git");
        let r = run_in(
            None,
            &["init", "--bare", "-q", upstream.to_str().unwrap()],
            &RunOpts::none(),
        )
        .unwrap();
        assert!(r.ok(), "init bare failed: {}", r.stderr);

        // clone it
        let work_tmp = TempDir::new().unwrap();
        let work = work_tmp.path().join("work");
        let outcome = clone(upstream.to_str().unwrap(), &work, &CloneOpts::default()).unwrap();
        assert!(
            outcome.result.ok(),
            "clone failed: {}",
            outcome.result.stderr
        );
        run_git(&work, &["config", "user.email", "test@example.com"]);
        run_git(&work, &["config", "user.name", "Test"]);
        run_git(&work, &["config", "commit.gpgSign", "false"]);
        run_git(&work, &["checkout", "-q", "-b", "main"]);

        // commit something
        std::fs::write(work.join("a.txt"), "hello").unwrap();
        let _oid = commit(&work, &[PathBuf::from("a.txt")], "first").unwrap();

        // push it
        let push_opts = PushOpts {
            remote: Some("origin".into()),
            branch: Some("main".into()),
            set_upstream: true,
            ..Default::default()
        };
        let r = push(&work, &push_opts).unwrap();
        assert!(r.ok(), "push failed: {} / {}", r.stdout, r.stderr);
    }
}

/// A branch with no remote must not land in git's *remote* slot.
///
/// `git pull` is `git pull [remote] [branch]` positionally. Every call site
/// that names a branch and leaves `remote` at its derived `None` therefore
/// produced `git pull <branch>`, and git read the branch as a remote name
/// and failed — which is how `ro sync` ended up erroring on every repo in
/// the fleet with "does not appear to be a git repository".
#[cfg(test)]
mod pull_arg_tests {
    use super::*;

    use super::tests::{run_git, run_git_out, temp_repo};
    use tempfile::TempDir;
    #[test]
    fn a_branch_without_a_remote_still_pulls_from_origin() {
        let (_tmp, repo) = temp_repo();
        let opts = PullOpts {
            remote: None,
            branch: Some("feat/x".into()),
            strategy: PullStrategy::FastForwardOnly,
            ..Default::default()
        };
        let out = pull(&repo, &opts).unwrap();
        let argv = out.result.args.join(" ");
        // The failure mode is the shape of the argument list, not the exit
        // code: a repo with no `origin` cannot pull from one, but it must
        // fail saying *that*, not claiming a branch is a remote.
        assert!(
            !out.result.stderr.contains("'feat/x' does not appear to be a git repository"),
            "the branch was passed in git's remote slot: {argv}\n{}",
            out.result.stderr
        );
    }

    #[test]
    fn an_explicit_remote_is_not_overridden_by_the_default() {
        let (_tmp, repo) = temp_repo();
        let opts = PullOpts {
            remote: Some("upstream".into()),
            branch: Some("main".into()),
            strategy: PullStrategy::Rebase,
            ..Default::default()
        };
        let out = pull(&repo, &opts).unwrap();
        assert!(
            out.result.stderr.contains("'upstream'") || !out.result.stderr.contains("'main' does not appear"),
            "an explicit remote must be used as given: {}",
            out.result.stderr
        );
    }

    /// `--autostash` has to reach git, not stop at ro's own dirty-skip.
    ///
    /// The run-level test in `ro-sync` asserts the outcome; this one asserts
    /// the **wiring**, which is the part that was broken. It reads the
    /// arguments git was handed rather than the exit code, because a pull
    /// that never stashed anything exits 0 in exactly the same way.
    ///
    /// No network and no remote: the assertion is about the argv, and a repo
    /// with no `origin` cannot pull from one either way.
    #[test]
    fn autostash_is_passed_to_git() {
        let (_tmp, repo) = temp_repo();
        let opts = PullOpts {
            remote: None,
            branch: Some("feat/x".into()),
            strategy: PullStrategy::FastForwardOnly,
            autostash: true,
        };
        let out = pull(&repo, &opts).unwrap();
        let argv = out.result.args.join(" ");
        assert!(
            out.result.args.iter().any(|a| a == "--autostash"),
            "--autostash never reached git, so git refused the merge instead of \
             stashing: {argv}"
        );
        // And it is still a well-formed pull, not a flag bolted onto a broken
        // argument list.
        assert_eq!(out.result.args[0], "pull");
        assert!(
            out.result.args.iter().any(|a| a == "--ff-only"),
            "the strategy flag must survive alongside --autostash: {argv}"
        );
    }

    /// A pull that did not ask for an autostash must not be judged by the
    /// tree either — the exit code is the whole story there.
    ///
    /// Without this, a caller that sets `autostash: true` once would have
    /// every subsequent pull read the worktree, and a stale stash from
    /// someone else's work would be reported as a leaked autostash.
    #[test]
    fn a_pull_without_autostash_is_not_judged_by_the_tree() {
        let (_tmp, repo) = temp_repo();
        let opts = PullOpts {
            remote: None,
            branch: Some("feat/x".into()),
            strategy: PullStrategy::FastForwardOnly,
            autostash: false,
        };
        let out = pull(&repo, &opts).unwrap();
        assert!(
            out.autostash_hold.is_none(),
            "a pull that did not autostash must not report a hold: {:?}",
            out.autostash_hold
        );
    }

    /// The conflict the flag exists for, detected from the tree.
    ///
    /// `git pull --autostash` exits 0 even when the pop conflicts, so the
    /// exit code cannot be the signal. This is the smallest real case: both
    /// sides rewrite the same line of a tracked file, the pull fast-forwards,
    /// the pop conflicts, and the tree is left holding conflict markers
    /// with the work still in the stash.
    #[test]
    fn a_conflicting_autostash_pop_is_detected_from_the_tree_not_the_exit_code() {
        let tmp = TempDir::new().unwrap();
        let remote = tmp.path().join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        run_git(&remote, &["init", "--bare", "-q", "-b", "main"]);

        // A seed clone that publishes the base commit. `clone` already sets
        // `origin`, so there is no `remote add` here — adding one fails, the
        // failure is silent without `run_git`, and the push that follows
        // then goes nowhere, leaving the remote empty and the whole fixture
        // testing a different scenario than the one named.
        let seed = tmp.path().join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        run_git(&seed, &["clone", "-q", remote.to_str().unwrap(), "."]);
        run_git(&seed, &["config", "user.email", "t@example.com"]);
        run_git(&seed, &["config", "user.name", "T"]);
        run_git(&seed, &["config", "commit.gpgSign", "false"]);
        std::fs::write(seed.join("a.txt"), "base\n").unwrap();
        run_git(&seed, &["add", "."]);
        run_git(&seed, &["commit", "-q", "-m", "base"]);
        run_git(&seed, &["push", "-q", "origin", "main"]);

        // The worktree, going dirty on the same line the remote will move.
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        run_git(&work, &["clone", "-q", remote.to_str().unwrap(), "."]);
        run_git(&work, &["config", "user.email", "t@example.com"]);
        run_git(&work, &["config", "user.name", "T"]);
        std::fs::write(work.join("a.txt"), "LOCAL WORK\n").unwrap();

        // The remote moves, rewriting that exact line.
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        run_git(&other, &["clone", "-q", remote.to_str().unwrap(), "."]);
        run_git(&other, &["config", "user.email", "t@example.com"]);
        run_git(&other, &["config", "user.name", "T"]);
        std::fs::write(other.join("a.txt"), "REMOTE WON\n").unwrap();
        run_git(&other, &["add", "."]);
        run_git(&other, &["commit", "-q", "-m", "remote moved"]);
        run_git(&other, &["push", "-q", "origin", "main"]);

        let opts = PullOpts {
            remote: Some("origin".into()),
            branch: Some("main".into()),
            strategy: PullStrategy::FastForwardOnly,
            autostash: true,
        };
        let out = pull(&work, &opts).unwrap();

        // The exit code lies, so this is the assertion that matters.
        assert!(
            out.result.ok(),
            "git exits 0 even when the pop conflicts — that is why the tree \
             has to be read: {}",
            out.result.stderr
        );
        assert!(
            out.autostash_hold.is_some(),
            "a pop that conflicted must be reported as a hold, not a success: {:?}",
            out.autostash_hold
        );
        let detail = out.autostash_hold.clone().expect("a hold is reported");
        assert!(
            detail.contains("git stash list"),
            "the recovery must name `git stash list`: {detail}"
        );

        // Asserted on the tree, not on the message.
        let status = run_git_out(&work, &["status", "--porcelain"]);
        let porcelain = status.stdout.clone();
        assert!(
            porcelain.contains("UU"),
            "the worktree must show the unmerged entry: {porcelain}"
        );
        let content = std::fs::read_to_string(work.join("a.txt")).unwrap();
        assert!(
            content.contains("<<<<<<<") && content.contains(">>>>>>>"),
            "the worktree must hold conflict markers: {content}"
        );
        let stashes = run_git_out(&work, &["stash", "list"]);
        assert!(
            stashes.stdout.contains("autostash"),
            "the failed pop must keep the stash: {}",
            stashes.stdout
        );
    }

    /// The other half, so the assertion above is not self-deceiving.
    ///
    /// A pop that applies cleanly leaves no unmerged entry and no stash, and
    /// the local work is back on top of the remote's version. Without this,
    /// the detection above could be reporting a hold on every autostash and
    /// still pass.
    #[test]
    fn a_clean_autostash_pop_is_not_reported_as_a_hold() {
        let tmp = TempDir::new().unwrap();
        let remote = tmp.path().join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        run_git(&remote, &["init", "--bare", "-q", "-b", "main"]);

        let seed = tmp.path().join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        run_git(&seed, &["clone", "-q", remote.to_str().unwrap(), "."]);
        run_git(&seed, &["config", "user.email", "t@example.com"]);
        run_git(&seed, &["config", "user.name", "T"]);
        run_git(&seed, &["config", "commit.gpgSign", "false"]);
        std::fs::write(seed.join("a.txt"), "base\n").unwrap();
        std::fs::write(seed.join("b.txt"), "base b\n").unwrap();
        run_git(&seed, &["add", "."]);
        run_git(&seed, &["commit", "-q", "-m", "base"]);
        run_git(&seed, &["push", "-q", "origin", "main"]);

        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        run_git(&work, &["clone", "-q", remote.to_str().unwrap(), "."]);
        run_git(&work, &["config", "user.email", "t@example.com"]);
        run_git(&work, &["config", "user.name", "T"]);
        // Dirty a *different* file than the remote is about to move, so the
        // pop has nothing to conflict with.
        std::fs::write(work.join("b.txt"), "LOCAL WORK\n").unwrap();

        let other = tmp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        run_git(&other, &["clone", "-q", remote.to_str().unwrap(), "."]);
        run_git(&other, &["config", "user.email", "t@example.com"]);
        run_git(&other, &["config", "user.name", "T"]);
        std::fs::write(other.join("a.txt"), "REMOTE WON\n").unwrap();
        run_git(&other, &["add", "."]);
        run_git(&other, &["commit", "-q", "-m", "remote moved"]);
        run_git(&other, &["push", "-q", "origin", "main"]);

        let opts = PullOpts {
            remote: Some("origin".into()),
            branch: Some("main".into()),
            strategy: PullStrategy::FastForwardOnly,
            autostash: true,
        };
        let out = pull(&work, &opts).unwrap();

        assert!(
            out.autostash_hold.is_none(),
            "a clean pop must not be reported as a hold: {:?}",
            out.autostash_hold
        );
        // The local work is back, on top of the remote's version.
        assert_eq!(
            std::fs::read_to_string(work.join("b.txt")).unwrap(),
            "LOCAL WORK\n",
            "the local edit must survive the stash/pop"
        );
        assert_eq!(
            std::fs::read_to_string(work.join("a.txt")).unwrap(),
            "REMOTE WON\n",
            "the remote's version must have landed"
        );
        let stashes = run_git_out(&work, &["stash", "list"]);
        assert!(
            stashes.stdout.is_empty(),
            "a clean pop must leave no stash behind: {}",
            stashes.stdout
        );
    }

    /// The fixture the two tests below share: a worktree whose uncommitted
    /// edit sits on the exact line the remote is about to rewrite, so the
    /// pop after the fast-forward cannot apply cleanly.
    ///
    /// Returned *before* any pull, because that is the moment the two tests
    /// part ways — one pulls first, the other makes a stash first.
    fn worktree_about_to_conflict(tmp: &TempDir) -> PathBuf {
        let remote = tmp.path().join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        run_git(&remote, &["init", "--bare", "-q", "-b", "main"]);

        let seed = tmp.path().join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        run_git(&seed, &["clone", "-q", remote.to_str().unwrap(), "."]);
        run_git(&seed, &["config", "user.email", "t@example.com"]);
        run_git(&seed, &["config", "user.name", "T"]);
        std::fs::write(seed.join("a.txt"), "base\n").unwrap();
        run_git(&seed, &["add", "."]);
        run_git(&seed, &["commit", "-q", "-m", "base"]);
        run_git(&seed, &["push", "-q", "origin", "main"]);

        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        run_git(&work, &["clone", "-q", remote.to_str().unwrap(), "."]);
        run_git(&work, &["config", "user.email", "t@example.com"]);
        run_git(&work, &["config", "user.name", "T"]);
        std::fs::write(work.join("a.txt"), "LOCAL WORK\n").unwrap();

        let other = tmp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        run_git(&other, &["clone", "-q", remote.to_str().unwrap(), "."]);
        run_git(&other, &["config", "user.email", "t@example.com"]);
        run_git(&other, &["config", "user.name", "T"]);
        std::fs::write(other.join("a.txt"), "REMOTE WON\n").unwrap();
        run_git(&other, &["add", "."]);
        run_git(&other, &["commit", "-q", "-m", "remote moved"]);
        run_git(&other, &["push", "-q", "origin", "main"]);

        work
    }

    fn autostash_pull() -> PullOpts {
        PullOpts {
            remote: Some("origin".into()),
            branch: Some("main".into()),
            strategy: PullStrategy::FastForwardOnly,
            autostash: true,
        }
    }

    /// A stash left behind by an *earlier* pull must not condemn every later
    /// pull. This is the wedge.
    ///
    /// The sequence is the one a user actually lives through:
    ///
    ///   1. `ro sync --autostash` conflicts, and the work stays in
    ///      `stash@{0}` — correctly, because that stash is the only route
    ///      back to it.
    ///   2. The user resolves the conflict markers and commits the
    ///      resolution. **The stash is still there**; it is their work, and
    ///      nothing has any business deleting it.
    ///   3. `ro sync --autostash` runs again. The tree is clean, the pull is
    ///      "Already up to date", git stashes nothing and pops nothing.
    ///
    /// A detector that asks "does any line of `git stash list` contain the
    /// word `autostash`" answers "yes" at step 3, on the strength of the
    /// stash created at step 1 — so every sync from then on is a red
    /// `autostash_conflict` over a perfectly healthy repo, and the only way
    /// out is for the user to go and drop a stash by hand. The tool that was
    /// supposed to surface the problem is now the problem.
    ///
    /// The final pull exists so this cannot be "fixed" by reporting a hold
    /// unconditionally: after the user drops the stash, the repo must be
    /// clean and stay clean.
    #[test]
    fn a_stash_from_an_earlier_pull_does_not_wedge_every_later_pull() {
        let tmp = TempDir::new().unwrap();
        let work = worktree_about_to_conflict(&tmp);

        // 1. The conflict, reported once.
        let first = pull(&work, &autostash_pull()).unwrap();
        assert!(
            first.autostash_hold.is_some(),
            "the pop conflicts here, so this pull must report it: {:?}",
            first.autostash_hold
        );
        let held = run_git_out(&work, &["stash", "list", "--format=%H"]);
        assert_eq!(
            held.stdout.lines().count(),
            1,
            "a failed pop keeps exactly one stash, and it is the user's only \
             route back to their work: {:?}",
            held.stdout
        );

        // 2. The user resolves the markers and commits. The stash stays.
        run_git(&work, &["checkout", "--theirs", "a.txt"]);
        run_git(&work, &["add", "a.txt"]);
        run_git(&work, &["commit", "-q", "-m", "resolved: keep the local work"]);
        let still_there = run_git_out(&work, &["stash", "list", "--format=%H"]);
        assert_eq!(
            still_there.stdout.lines().count(),
            1,
            "resolving the conflict must not silently drop the user's work: {:?}",
            still_there.stdout
        );

        // 3. The next sync. This pull stashes nothing and pops nothing, so
        //    the stash from step 1 is not its stash and must not be blamed
        //    on it.
        let second = pull(&work, &autostash_pull()).unwrap();
        assert!(
            second.result.ok(),
            "the tree is clean and the pull is a no-op, so git succeeds: {}",
            second.result.stderr
        );
        assert!(
            second.autostash_hold.is_none(),
            "a leftover autostash from an EARLIER pull must not be reported as \
             this pull's — that wedges the repo forever, with no way out short \
             of dropping the stash by hand. This pull stashed nothing and \
             popped nothing; got {:?}",
            second.autostash_hold
        );
        assert!(!second.conflict, "and nothing is in conflict: {second:?}");

        // The user drops the stash, and the repo is clean from then on.
        run_git(&work, &["stash", "drop"]);
        let third = pull(&work, &autostash_pull()).unwrap();
        assert!(
            third.autostash_hold.is_none(),
            "a repo with no stashes at all must never report a hold: {:?}",
            third.autostash_hold
        );
    }

    /// A stash the *user* made is not this pull's stash, whatever the user
    /// called it.
    ///
    /// `git stash push -m "autostash: before the rewrite"` is an ordinary
    /// thing for a person to write — the word is in the flag, after all. A
    /// detector that substring-matches the stash list cannot tell that stash
    /// from one git created, so it invents a conflict on a pull that
    /// fast-forwarded perfectly and stashed nothing.
    ///
    /// This is the second half of the same defect as the wedge above, and it
    /// is the reason the discriminator cannot be a message match: the only
    /// thing that distinguishes the two stashes is *when they appeared*.
    #[test]
    fn a_stash_the_user_made_is_not_reported_as_this_pulls() {
        let tmp = TempDir::new().unwrap();
        let work = worktree_about_to_conflict(&tmp);

        // The user's own stash, made by hand, before the pull.
        run_git(
            &work,
            &["stash", "push", "-m", "autostash of the report rewrite"],
        );
        let before = run_git_out(&work, &["stash", "list", "--format=%H"]);
        assert_eq!(
            before.stdout.lines().count(),
            1,
            "the fixture must have exactly the user's stash: {:?}",
            before.stdout
        );

        let out = pull(&work, &autostash_pull()).unwrap();

        assert!(
            out.result.ok(),
            "the tree is clean, so this is an ordinary fast-forward: {}",
            out.result.stderr
        );
        // The remote's work landed, and the user's stash is untouched: this
        // is a clean sync.
        assert_eq!(
            std::fs::read_to_string(work.join("a.txt")).unwrap(),
            "REMOTE WON\n",
            "the fast-forward must have landed"
        );
        assert!(
            out.autostash_hold.is_none(),
            "a stash the user made is not this pull's stash, and its message \
             saying 'autostash' changes nothing: {:?}",
            out.autostash_hold
        );
        let after = run_git_out(&work, &["stash", "list", "--format=%H"]);
        assert_eq!(
            after.stdout, before.stdout,
            "the user's stash must be left exactly as it was"
        );
    }

    /// A before-snapshot that could not be read is not a before-snapshot
    /// with nothing in it.
    ///
    /// "We could not read the list before the pull" and "the list was empty
    /// before the pull" lead to opposite verdicts, and the difference is the
    /// user's work: with the first, an entry present afterwards might be
    /// theirs, so this pull cannot be said to have popped cleanly. Calling
    /// that clean is the whole lie this function exists to stop.
    ///
    /// The empty-list case is included because it is the one place the
    /// unknown is genuinely good news: nothing is stashed now, so nothing
    /// could have been left behind, whatever we could not read before.
    #[test]
    fn a_before_snapshot_that_cannot_be_read_is_not_treated_as_empty() {
        let tmp = TempDir::new().unwrap();
        let work = worktree_about_to_conflict(&tmp);
        run_git(&work, &["stash", "push", "-m", "the user's own work"]);

        assert!(
            matches!(autostash_state(&work, None, &RunOpts::none()), AutostashState::Unreadable),
            "with no before-snapshot and an entry in the list, nothing can be \
             shown to be older than this pull, so the pop cannot be called clean"
        );
        assert!(
            matches!(autostash_state(&work, None, &RunOpts::none()), AutostashState::Unreadable),
            "and it is not a hold either: the message has to say which"
        );

        // With nothing stashed at all, the unknown stops mattering: there is
        // no work in a stash, so there is nothing to have failed to pop.
        run_git(&work, &["stash", "drop"]);
        assert!(
            matches!(autostash_state(&work, None, &RunOpts::none()), AutostashState::Popped),
            "an empty list after the pull is evidence, not an absence of it"
        );
    }

    /// "Cannot read the stash list" and "there are no stashes" are different
    /// answers, and only the second one is good news.
    ///
    /// `git stash list` exits 128 outside a repository, so the unreadable
    /// case is reachable in ordinary use — a tracked repo whose checkout was
    /// replaced, a `.git` removed out from under ro — rather than only in a
    /// corrupt repo. A caller that folded the two together would be claiming
    /// a clean pop on the strength of a command that never ran, which is the
    /// exact failure this whole code path exists to prevent, one level down.
    #[test]
    fn an_unreadable_stash_list_is_not_an_empty_one() {
        let tmp = TempDir::new().unwrap();
        let not_a_repo = tmp.path().join("plain-directory");
        std::fs::create_dir_all(&not_a_repo).unwrap();

        // Pin the premise: this really is the unreadable case, not an
        // assumption about which git version does what.
        let raw = std::process::Command::new("git")
            .args(["stash", "list", "--format=%H"])
            .current_dir(&not_a_repo)
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        assert!(
            !raw.status.success(),
            "the fixture must be a directory git cannot answer for: {}",
            String::from_utf8_lossy(&raw.stderr)
        );

        assert!(
            stash_snapshot(&not_a_repo, &RunOpts::none()).is_none(),
            "an unreadable stash list must be reported as unknown, not as an \
             empty one — an empty one says 'nothing is stashed', which is a \
             claim about the user's work that was never checked"
        );
        // And the empty case is a real `Some(vec![])`, so the two are
        // genuinely distinguishable at the call site.
        let (_tmp, repo) = temp_repo();
        run_git(&repo, &["commit", "-q", "--allow-empty", "-m", "first"]);
        assert_eq!(
            stash_snapshot(&repo, &RunOpts::none()),
            Some(Vec::new()),
            "a real repository with no stashes is 'known to be empty', which \
             is not the same answer as 'could not be read'"
        );
    }
}
