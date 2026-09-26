//! The engine trait, and the types that bound it.
//!
//! # Why `EngineOutcome` is a value and not a `Result`
//!
//! This is the single most load-bearing type decision in the crate.
//!
//! A `Result` invites `?` at some call site inside a worker, and **one `?`
//! that escapes converts a per-repo failure into a fleet abort.** Twenty
//! repos, one has a bad credential, and the other nineteen are never
//! touched — with the reason on stderr rather than on the one row that
//! needed it.
//!
//! Returning an enum makes that unrepresentable: there is no error channel
//! to write `?` into, so a worker has to decide what to do with an
//! unexpected outcome, and the only thing it can do is record it against
//! that repo. Every outcome is a *fact about one repo*, including the
//! boring ones.

pub mod git_engine;

use std::path::PathBuf;
use std::time::Duration;

use ro_core::CommitIdentity;
use ro_core::FailureClass;

/// Exactly three built-ins, no plugin registry.
///
/// `Git` is the raw backend and the explicit fallback. It cannot read a
/// diff, split commits, or resolve conflicts — which is exactly why the
/// agent engines exist — and it is **not** the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EngineKind {
    Claude,
    Codex,
    Git,
}

impl EngineKind {
    /// The name shown to a user, and the key a config uses.
    pub fn as_str(&self) -> &'static str {
        match self {
            EngineKind::Claude => "claude",
            EngineKind::Codex => "codex",
            EngineKind::Git => "git",
        }
    }

    /// Parse a config value. Unknown names are an error rather than a
    /// silent default, because falling back to `Git` for a typo would
    /// mean committing with the raw backend while the user believed an
    /// agent was reading the diff.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(EngineKind::Claude),
            "codex" => Some(EngineKind::Codex),
            "git" => Some(EngineKind::Git),
            _ => None,
        }
    }
}

impl std::fmt::Display for EngineKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Is the engine's binary actually on `PATH`?
///
/// Present carries the resolved path so the error message can name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Present(PathBuf),
    Missing,
}

impl Availability {
    pub fn is_present(&self) -> bool {
        matches!(self, Availability::Present(_))
    }
}

/// One commit the engine produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRecord {
    /// The commit subject, as written.
    pub message: String,
    /// The resulting commit id.
    pub oid: String,
    /// Files the commit touched, repo-relative.
    pub files: Vec<String>,
}

/// What an engine did, or why it did not.
///
/// No variant is a "the run failed, stop" — every one of them is scoped to
/// a single repo, because that is the property the orchestrator needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineOutcome {
    Committed {
        commits: Vec<CommitRecord>,
    },
    /// Nothing to do. Distinct from a failure with no commits: this is a
    /// clean tree, and a user reading a status board needs to tell those
    /// apart.
    NothingToCommit,
    Failed {
        error: String,
        class: FailureClass,
    },
    /// The binary is not installed. A setup problem, not a repo problem —
    /// and the hint is what turns it from a dead end into a fix.
    Unavailable {
        binary: String,
        hint: String,
    },
    /// A kill is not a refusal.
    ///
    /// A timeout means "try again"; a `Failed` can mean "this will never
    /// work" or "you are being rate limited", which does deserve a retry.
    /// Collapsing them removes the caller's ability to choose.
    TimedOut {
        after: Duration,
    },
}

impl EngineOutcome {
    /// Did this engine write anything?
    pub fn committed(&self) -> bool {
        matches!(self, EngineOutcome::Committed { .. })
    }

    /// Is this a problem with the *setup* rather than the repo?
    pub fn is_unavailable(&self) -> bool {
        matches!(self, EngineOutcome::Unavailable { .. })
    }

    /// One line for a status table.
    pub fn render(&self) -> String {
        match self {
            EngineOutcome::Committed { commits } => match commits.len() {
                0 => "committed nothing".to_string(),
                1 => format!("committed 1 ({})", commits[0].oid),
                n => format!("committed {n} commits"),
            },
            EngineOutcome::NothingToCommit => "nothing to commit".to_string(),
            EngineOutcome::Failed { error, class } => format!("{class}: {error}"),
            EngineOutcome::Unavailable { binary, .. } => format!("{binary} is not installed"),
            EngineOutcome::TimedOut { after } => format!("timed out after {after:?}"),
        }
    }
}

/// Everything an engine is given, and nothing else.
///
/// # This is the security boundary, expressed as a type
///
/// `env` is **merged into the child environment after subtraction**, never
/// instead of it. A caller that wanted to *add* its way to a credential
/// would have no way to express that here: the field is additions to an
/// environment that has already had everything sensitive removed.
///
/// `base_branch` is a snapshot taken in preflight, *before* any branch is
/// created and before the engine runs — because an engine may switch
/// branches itself, and a value read afterwards would be whatever the
/// engine left behind rather than what it was asked to work from.
#[derive(Debug, Clone)]
pub struct EngineContext<'a> {
    pub repo_root: &'a std::path::Path,
    pub base_branch: String,
    pub identity: Option<&'a CommitIdentity>,
    pub timeout: Duration,
    pub message_override: Option<&'a str>,
    /// Merged into the child env — AFTER subtraction, not instead of it.
    pub env: &'a [(String, String)],
}

impl<'a> EngineContext<'a> {
    /// A context with the defaults every engine can rely on: a 10-minute
    /// deadline, no overrides, no extra environment.
    ///
    /// The timeout is a floor rather than a default the caller can forget:
    /// an engine with no deadline is a hung fleet run, and the one thing a
    /// context that is easy to construct should never permit is "no
    /// timeout".
    pub fn new(repo_root: &'a std::path::Path, base_branch: impl Into<String>) -> Self {
        Self {
            repo_root,
            base_branch: base_branch.into(),
            identity: None,
            timeout: Duration::from_secs(600),
            message_override: None,
            env: &[],
        }
    }

    pub fn with_identity(mut self, identity: Option<&'a CommitIdentity>) -> Self {
        self.identity = identity;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_message(mut self, message: Option<&'a str>) -> Self {
        self.message_override = message;
        self
    }

    pub fn with_env(mut self, env: &'a [(String, String)]) -> Self {
        self.env = env;
        self
    }
}

/// An engine that can write commits for one repo.
///
/// `Sync` because `&Engine` is shared read-only across
/// `std::thread::scope` workers: a fleet run holds one engine and points
/// it at twenty repos, and without `Sync` that would need a lock around
/// every call.
pub trait Engine: Send + Sync {
    fn kind(&self) -> EngineKind;

    /// The binary this engine spawns.
    fn bin(&self) -> &str;

    /// Arguments passed before the prompt, if the engine takes one.
    fn default_args(&self) -> &[String];

    /// A cheap `PATH` probe. Called at **dispatch time only** — never per
    /// repo, because a `PATH` walk across a fleet is work the answer does
    /// not depend on.
    fn availability(&self) -> Availability;

    /// Returns a **value**, never a `Result`. See the module docs.
    fn checkpoint(&self, ctx: &EngineContext<'_>) -> EngineOutcome;
}

pub use git_engine::GitEngine;
