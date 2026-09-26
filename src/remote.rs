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
//! engine had nowhere to push", which is the same
//! unknown-versus-false confusion ro-dab.4 just removed one layer down.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// A bare git repository that pushes can land in.
pub struct BareRemote {
    dir: TempDir,
}

impl BareRemote {
    /// Create a bare repo at `dir` (the directory is created for you).
    pub fn new(dir: PathBuf) -> Self {
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "remote.git".to_string());
        let parent = dir.parent().unwrap_or(Path::new(".")).to_path_buf();
        std::fs::create_dir_all(&parent).expect("the parent dir is creatable");
        let out = git()
            .args(["init", "--bare", "-q", &name])
            .current_dir(&parent)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git init --bare failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Self {
            dir: TempDir::new_from(parent.join(&name))
                .unwrap_or_else(|_| panic!("{dir:?} should be a directory")),
        }
    }

    /// A bare repo in a fresh temp dir.
    pub fn ephemeral() -> Self {
        let tmp = TempDir::new().expect("the temp dir is creatable");
        let path = tmp.path().join("remote.git");
        std::fs::create_dir_all(&path).expect("the remote dir is creatable");
        let out = git()
            .args(["init", "--bare", "-q", "."])
            .current_dir(&path)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git init --bare failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // The remote owns its own directory, so the outer temp dir is kept
        // alive by leaking it into the returned path rather than dropped.
        // `TempDir::into_path` would delete on drop, which is what we want
        // for the *outer* dir but not the inner one.
        std::mem::forget(tmp);
        Self {
            dir: TempDir::new_from(path).expect("the remote is a directory"),
        }
    }

    /// The path to push to / read from.
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Branch names this remote currently has a ref for.
    ///
    /// Read from the ref store rather than the reflog: the reflog records
    /// pushes and the ref records what is *there now*, and "did the commit
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

    /// Does this remote have `branch`, with at least one commit?
    pub fn has_branch(&self, branch: &str) -> bool {
        self.branches().iter().any(|b| b == branch)
    }

    /// The commit messages on `branch`, newest first.
    ///
    /// Message-level rather than count-level: "a commit arrived" and "the
    /// *right* commit arrived" are different claims, and a test that only
    /// counts commits passes when the wrong one lands.
    pub fn commit_messages(&self, branch: &str) -> Vec<String> {
        let spec = format!("{branch}");
        let out = git()
            .args(["log", "--format=%s", &spec])
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
    pub fn new() -> Self {
        let tmp = TempDir::new().expect("the temp dir is creatable");
        let ro = BareRemote::new(tmp.path().join("ro.git"));
        let denied = BareRemote::new(tmp.path().join("engine-denied.git"));
        // The pair's lifetime is the two remotes' lifetime; the outer temp
        // dir is only their common parent and is deliberately leaked.
        std::mem::forget(tmp);
        Self {
            ro,
            engine_denied: denied,
        }
    }
}

fn git() -> Command {
    let mut c = Command::new("git");
    c.env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .env("LC_ALL", "C");
    c
}
