//! Real bare git remotes, for tests that must assert where a commit landed.
//!
//! # Why a real remote and not a mock
//!
//! The failure the credential design exists to prevent is *"the push went
//! out over the wrong identity"*. A mocked `git push` cannot testify to
//! that: it can only testify to that it was called. So these are
//! `git init --bare` directories and the assertions read the refs back.
//!
//! # Why **two**
//!
//! One remote can only prove a push *reached* something. The boundary that
//! matters is the opposite one: the agent engine must not be able to push
//! at all. That needs a second remote the engine could have reached and did
//! not — otherwise "the engine did not push" is indistinguishable from "the
//! engine had nowhere to push", which is the same unknown-versus-false
//! confusion ro-dab.4 just removed one layer down.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// A bare git repository that pushes can land in.
pub struct BareRemote {
    /// Owns the whole temp dir, so cleanup is automatic.
    ///
    /// The repository is at `root` — **not** at `dir.path()`. Keeping only
    /// the `TempDir` made `path()` return the empty parent, and a worktree
    /// pushed to a directory that contained no repository at all. The
    /// remote's own path is stored rather than recomputed, so the two can
    /// never drift.
    ///
    /// Never read: it exists purely so the temp dir is removed when the
    /// fixture drops.
    #[allow(dead_code)]
    dir: TempDir,
    root: PathBuf,
}

impl BareRemote {
    /// A bare repo at `path`, creating the directory if needed.
    ///
    /// The `TempDir` is created *here* and the repo is initialised inside
    /// it, so cleanup is automatic and nothing is leaked. `path` supplies
    /// only the **file name**: an earlier draft ignored the caller's
    /// directory entirely and initialised the repo somewhere else, so a
    /// worktree given `remote.path()` pushed to a path that was not a
    /// repository at all. The name is taken from `path` and the parent is
    /// this fixture's own temp dir, which is the only directory it is
    /// entitled to own.
    pub fn new(path: PathBuf) -> Self {
        let dir = TempDir::new().expect("a temp dir is creatable");
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "remote.git".to_string());
        let target = dir.path().join(name);
        std::fs::create_dir_all(&target).expect("the remote dir is creatable");
        let out = git()
            .args(["init", "--bare", "-q", "."])
            .current_dir(&target)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git init --bare at {} failed: {}",
            target.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        Self { dir, root: target }
    }

    /// A bare repo in a fresh temp dir.
    pub fn ephemeral() -> Self {
        Self::new(PathBuf::from("remote.git"))
    }

    /// The path to push to / read from — the repository itself.
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Branch names this remote currently has a ref for.
    ///
    /// Read from the ref store rather than the reflog: the reflog records
    /// pushes, the ref records what is *there now*, and "did the commit
    /// land" is the second question.
    pub fn branches(&self) -> Vec<String> {
        let out = git()
            .args(["for-each-ref", "--format=%(refname:short)", "refs/heads"])
            .current_dir(self.path())
            .output()
            .expect("git runs");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Does this remote have a ref for `branch`?
    pub fn has_branch(&self, branch: &str) -> bool {
        self.branches().iter().any(|b| b == branch)
    }

    /// The commit subjects on `branch`, newest first.
    ///
    /// Message-level rather than count-level: "a commit arrived" and "the
    /// *right* commit arrived" are different claims, and a test that only
    /// counts commits passes when the wrong one lands.
    pub fn commit_messages(&self, branch: &str) -> Vec<String> {
        let out = git()
            .args(["log", "--format=%s", branch])
            .current_dir(self.path())
            .output()
            .expect("git runs");
        if !out.status.success() {
            return Vec::new();
        }
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    }
}

/// Two bare remotes: one ro is meant to push to, one the engine must not.
///
/// The pair is the fixture. One remote alone cannot distinguish "the engine
/// did not push" from "the engine had nowhere to push"; with a second
/// remote the engine *could* have reached, the assertion is real.
pub struct RemotePair {
    /// Where ro's own push is expected to land.
    pub ro: BareRemote,
    /// Where the engine's push must **not** land.
    pub engine_denied: BareRemote,
}

impl RemotePair {
    /// Two fresh bare remotes, each owning its own directory.
    ///
    /// Separate owners rather than two paths under one temp dir: the pair
    /// has to outlive any single assertion, and a shared parent dropped at
    /// the end of `new` would take both repos with it.
    pub fn new() -> Self {
        Self {
            ro: BareRemote::new(PathBuf::from("ro.git")),
            engine_denied: BareRemote::new(PathBuf::from("engine-denied.git")),
        }
    }
}

impl Default for RemotePair {
    fn default() -> Self {
        Self::new()
    }
}

fn git() -> Command {
    let mut c = Command::new("git");
    c.env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .env("LC_ALL", "C");
    c
}
