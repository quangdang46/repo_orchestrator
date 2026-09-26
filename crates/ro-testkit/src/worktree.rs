//! Checkouts with a known shape, for the fleet tests.
//!
//! PLAN §12 fixes the shapes: a repo can be clean or dirty, its dirt can be
//! tracked / untracked / inside a brand-new directory, and it can have or
//! not have a remote. The brand-new-directory case is the one that keeps
//! finding bugs — `git add -A` and `git add .` disagree about it, and
//! `ro-sweep/src/commit.rs` hand-rolled exactly that wrong version.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// A git checkout on disk.
pub struct Worktree {
    dir: TempDir,
}

impl Worktree {
    /// An initialised repository on `main`, empty.
    pub fn empty() -> Self {
        let tmp = TempDir::new().expect("the temp dir is creatable");
        let path = tmp.path().to_path_buf();
        run(&path, &["init", "-q", "-b", "main"]);
        run(&path, &["config", "user.email", "test@example.com"]);
        run(&path, &["config", "user.name", "Test"]);
        // gpg signing would make every commit in CI depend on the runner's
        // keyring, which is exactly the "fixture quietly depends on the
        // machine" problem.
        run(&path, &["config", "commit.gpgSign", "false"]);
        Self { dir: tmp }
    }

    /// A repository with one commit.
    pub fn with_one_commit() -> Self {
        let w = Self::empty();
        w.write("README.md", "# test\n");
        w.commit("initial");
        w
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Write a file, creating parent directories.
    pub fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let path = self.dir.path().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the parent dir is creatable");
        }
        std::fs::write(&path, contents).expect("the file is writable");
        path
    }

    /// Write a file **inside a directory that does not exist yet**.
    ///
    /// The case PLAN calls out: `git add .` misses it and `git add -A`
    /// catches it, so a fixture that only ever writes at the top level
    /// never exercises the difference.
    pub fn write_in_new_dir(&self, dir: &str, file: &str, contents: &str) -> PathBuf {
        assert!(
            !self.dir.path().join(dir).exists(),
            "{dir} must not exist yet, or this is not testing the new-directory case"
        );
        self.write(&format!("{dir}/{file}"), contents)
    }

    /// Commit everything currently in the tree.
    pub fn commit(&self, message: &str) {
        run(self.dir.path(), &["add", "-A"]);
        let out = Command::new("git")
            .args(["commit", "-q", "-m", message])
            .current_dir(self.dir.path())
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Add a remote pointing at `url`.
    pub fn add_remote(&self, name: &str, url: &Path) {
        run(
            self.dir.path(),
            &["remote", "add", name, &url.display().to_string()],
        );
    }

    /// The current branch name.
    pub fn current_branch(&self) -> String {
        let out = Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(self.dir.path())
            .output()
            .expect("git runs");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// `git status --porcelain` — the same call `ro-git::read::is_dirty` makes.
    pub fn porcelain(&self) -> String {
        let out = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(self.dir.path())
            .output()
            .expect("git runs");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    pub fn is_dirty(&self) -> bool {
        !self.porcelain().trim().is_empty()
    }
}

/// Run a git command, panicking with its stderr on failure.
///
/// Test-only, so panicking is right: a fixture that quietly swallows a git
/// failure produces a test that passes for the wrong reason.
pub fn run(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} in {} failed:\nstdout: {}\nstderr: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}
