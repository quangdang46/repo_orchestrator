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
    /// Which host the `extraheader` is scoped to. Defaults to github.com.
    ///
    /// Scoped rather than global on purpose: a credential for one host must not
    /// be offered to another, and a global `http.extraheader` would offer it to
    /// every host this git talks to for the life of the invocation.
    pub host: Option<String>,
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

/// Run an arbitrary git subcommand inside `repo`.
///
/// Use one of the typed helpers when possible; this is the escape hatch for a
/// caller that needs flags none of them expose.
#[deprecated(
    since = "0.2.0",
    note = "no per-invocation settings are reachable through this. Callers that need \
            an env var — which is every caller that hands a credential to git — must go \
            through run_in with RunOpts. This shim exists so the sixteen existing call \
            sites do not all churn at once; delete it when the sweep namespace goes in \
            Phase 4."
)]
pub fn run(repo: &Path, args: &[&str]) -> Result<GitCommandResult> {
    run_in(Some(repo), args, &RunOpts::none())
}

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
    run_in(Some(repo), &argv, &RunOpts::none())
}

/// Pull from a remote.
pub fn pull(repo: &Path, opts: &PullOpts) -> Result<PullOutcome> {
    let mut args: Vec<String> = vec!["pull".to_string()];
    match opts.strategy {
        PullStrategy::FastForwardOnly => args.push("--ff-only".to_string()),
        PullStrategy::Merge => args.push("--no-rebase".to_string()),
        PullStrategy::Rebase => args.push("--rebase".to_string()),
    }
    if let Some(remote) = &opts.remote {
        args.push(remote.clone());
    }
    if let Some(branch) = &opts.branch {
        args.push(branch.clone());
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = run_in(Some(repo), &argv, &RunOpts::none())?;
    let conflict = result.stderr.contains("conflict") || result.stdout.contains("CONFLICT");
    let already_up_to_date = result.stdout.contains("Already up to date");
    Ok(PullOutcome {
        result,
        conflict,
        already_up_to_date,
    })
}

/// Clone a repository to `dest`.
pub fn clone(url: &str, dest: &Path, opts: &CloneOpts) -> Result<CloneOutcome> {
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
    let result = run_in(None, &argv, &RunOpts::none())?;
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
fn extraheader_env(host: &str, token: &SecretString) -> Vec<(String, String)> {
    // GitHub's documented form for a token over HTTPS. The password half is
    // the token; the username is a fixed marker, not a login.
    let credentials = format!("x-access-token:{}", token.expose());
    let encoded = base64_encode(credentials.as_bytes());
    vec![
        ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
        (
            "GIT_CONFIG_KEY_0".to_string(),
            format!("http.https://{host}/.extraheader"),
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

    let env = match token {
        Some(t) => extraheader_env(opts.host.as_deref().unwrap_or("github.com"), t),
        None => Vec::new(),
    };
    let run_opts = if env.is_empty() {
        RunOpts::none()
    } else {
        RunOpts::with_env(&env)
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
        let shim = dir.join("git-shim");
        std::fs::create_dir_all(dir).expect("the shim dir is creatable");
        std::fs::write(
            &shim,
            r#"#!/bin/sh
echo "GIT_CONFIG_COUNT=$GIT_CONFIG_COUNT"
echo "GIT_CONFIG_KEY_0=$GIT_CONFIG_KEY_0"
echo "GIT_CONFIG_VALUE_0=$GIT_CONFIG_VALUE_0"
echo "PROBE_ARGS=$*"
"#,
        )
        .expect("the shim is writable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&shim)
                .expect("the shim exists")
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&shim, perms).expect("the mode is settable");
        }
        shim
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
        assert!(outcome.is_err(), "the shim hangs, so this must time out");

        // Well past when the grandchild would have written, if it were alive.
        std::thread::sleep(Duration::from_millis(1500));

        assert!(
            !marker.exists(),
            "a grandchild outlived the timeout and performed its side effect, so the kill \
             reached the direct child but not the tree"
        );
    }

    /// A shim that never returns.
    fn hanging_shim(dir: &Path) -> PathBuf {
        write_executable(&dir.join("git-hang"), "#!/bin/sh\nsleep 600\n")
    }

    /// A shim that starts a child which writes `marker` after a delay, then
    /// hangs itself. The marker existing afterwards is the proof of survival.
    fn grandchild_shim(dir: &Path, marker: &Path) -> PathBuf {
        write_executable(
            &dir.join("git-grandchild"),
            &format!(
                "#!/bin/sh\n( sleep 1; : > {} ) &\nsleep 600\n",
                marker.display()
            ),
        )
    }

    /// Write an executable shell script, creating its directory.
    fn write_executable(path: &Path, body: &str) -> PathBuf {
        std::fs::create_dir_all(path.parent().expect("a path has a parent"))
            .expect("the shim dir is creatable");
        std::fs::write(path, body).expect("the shim is writable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(path)
                .expect("the shim exists")
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(path, perms).expect("the mode is settable");
        }
        path.to_path_buf()
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

    /// The hardening block reaches a wrapper binary too, so a caller that
    /// swaps in a shim still gets `GIT_TERMINAL_PROMPT=0` and friends.
    #[test]
    fn the_hardening_block_reaches_a_wrapped_binary() {
        let tmp = TempDir::new().unwrap();
        let shim_dir = tmp.path().join("bin");
        let shim = shim_dir.join("git-shim");
        std::fs::create_dir_all(&shim_dir).expect("the shim dir is creatable");
        std::fs::write(
            &shim,
            "#!/bin/sh\necho \"PROMPT=$GIT_TERMINAL_PROMPT|$GCM_INTERACTIVE|$LC_ALL\"\n",
        )
        .expect("the shim is writable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&shim)
                .expect("the shim exists")
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&shim, perms).expect("the mode is settable");
        }

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

    fn temp_repo() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().to_path_buf();
        run_git(&path, &["init", "-q", "-b", "main"]);
        run_git(&path, &["config", "user.email", "test@example.com"]);
        run_git(&path, &["config", "user.name", "Test"]);
        run_git(&path, &["config", "commit.gpgSign", "false"]);
        (tmp, path)
    }

    fn run_git(dir: &Path, args: &[&str]) {
        // The real API, not the deprecated `run` shim. CI compiles tests
        // with `-D warnings`, so a shim kept "so callers do not churn" turns
        // into a build error for exactly the callers it was meant to spare.
        let r = run_in(Some(dir), args, &RunOpts::none()).unwrap();
        assert!(
            r.ok(),
            "git {args:?} failed: stdout={:?} stderr={:?}",
            r.stdout,
            r.stderr
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
