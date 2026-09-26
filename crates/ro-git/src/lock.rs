//! Repo-level file locks using fs4.
//!
//! Before any git mutation, acquire a lock on the repo. Released automatically
//! when the guard drops, or when the process ends — the kernel releases an
//! `flock` on process exit, which is the property that makes a crashed run
//! recoverable without a janitor.
//!
//! ## Where the lock file lives, and why it is not in the worktree
//!
//! It used to be `<repo_path>/.ro.lock`, and that was a bug with two faces.
//!
//! **The repo looked dirty.** `git status --porcelain` reports `?? .ro.lock`,
//! so `is_dirty` returned true and a checkpoint saw *its own lock* as
//! uncommitted work. A tool that dirties the tree it is inspecting cannot
//! reliably inspect it.
//!
//! **A typo'd path got created.** `acquire` called `create_dir_all(repo_path)`,
//! so pointing it at a path that did not exist silently produced that
//! directory. There is no `create_dir_all` here now, and no worktree is created
//! as a side effect of locking one.
//!
//! The lock now lives at `<state_dir>/locks/<hex>.lock`, where `<hex>` is a
//! hash of the canonical repo path — stable across processes, and independent
//! of where the worktree happens to be.
//!
//! ## Why Drop does not unlink
//!
//! Unlinking on drop opens a window: process B creates the path after A's
//! unlink and before A's unlock, and both then believe they hold the lock on
//! *different files* while the worktree is mutated by both. The lock becomes
//! decorative exactly when two processes are contending, which is the only
//! time it matters.
//!
//! So drop only unlocks. The file stays, holding no lock, and the next acquire
//! takes it. An empty lock file in ro's own state directory is not litter; a
//! lock that two processes both believe they hold is corruption.
//!
//! ## Why `state_dir` is a parameter
//!
//! The other option was giving ro-git a dependency on ro-config or ro-state to
//! discover the directory itself. ro-git currently depends on **no** workspace
//! crate, and that is what makes it the leaf everything else can point at —
//! the same layering that `ro-dab.7` just consolidated for the auth vocabulary.
//! Inverting that to save one parameter is a bad trade, so the caller threads
//! it instead. The parameter is explicit, and a caller that forgets it is a
//! compile error rather than a lock in the wrong tree.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fs4::fs_std::FileExt;

/// Default lock timeout in seconds.
const DEFAULT_LOCK_TIMEOUT_SECS: u64 = 30;

/// A RAII guard that holds an exclusive file lock on a repository.
#[derive(Debug)]
pub struct RepoLock {
    file: std::fs::File,
    lock_path: PathBuf,
    repo_path: PathBuf,
}

impl RepoLock {
    /// Acquire an exclusive lock on `repo_path`, storing the lock file under
    /// `state_dir/locks`.
    ///
    /// `repo_path` is used only to *identify* the repo. It is never created
    /// and never written to, so a path that does not exist is a path that
    /// locks to an identifier nothing will ever match — a visible mistake,
    /// rather than a directory that appears from nowhere.
    pub fn acquire(state_dir: &Path, repo_path: &Path, timeout_secs: u64) -> Result<Self> {
        let lock_dir = state_dir.join("locks");
        std::fs::create_dir_all(&lock_dir)
            .with_context(|| format!("creating lock dir {}", lock_dir.display()))?;
        let lock_path = lock_dir.join(format!("{}.lock", lock_key(repo_path)));

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("opening lock file {}", lock_path.display()))?;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

        loop {
            match file.try_lock_exclusive() {
                Ok(true) => {
                    return Ok(Self {
                        file,
                        lock_path,
                        repo_path: repo_path.to_path_buf(),
                    });
                }
                Ok(false) => {
                    if std::time::Instant::now() >= deadline {
                        anyhow::bail!(
                            "timed out after {timeout_secs}s waiting for the ro lock on {}. \
                             Another ro process may be operating on this repo. The lock file is \
                             {}; if no ro process is running, that file is stale and can be \
                             deleted — it holds no lock of its own.",
                            repo_path.display(),
                            lock_path.display()
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("acquiring exclusive lock on {}", lock_path.display())
                    });
                }
            }
        }
    }

    /// Acquire a lock with the default timeout (30 seconds).
    pub fn acquire_default(state_dir: &Path, repo_path: &Path) -> Result<Self> {
        Self::acquire(state_dir, repo_path, DEFAULT_LOCK_TIMEOUT_SECS)
    }

    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    pub fn repo_path(&self) -> &Path {
        &self.repo_path
    }
}

impl Drop for RepoLock {
    /// Unlock only. See the module doc for why unlinking here is a correctness
    /// bug and not a tidiness preference.
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// A stable filename for a repo path.
///
/// Canonicalize first so two processes that name the same repo differently
/// (`/tmp/x` and `/private/tmp/x`, or a symlink and its target) contend on one
/// lock rather than two. Canonicalize can fail for a path that does not exist,
/// which is fine: the raw path is still a consistent key, and the alternative
/// is failing to lock a repo that is merely unreadable.
///
/// A hash, not the path itself: a repository path can contain characters that
/// are not legal in a filename, and the lock directory should not need sanitising
/// rules that differ from the filesystem's.
fn lock_key(repo_path: &Path) -> String {
    let canonical = repo_path
        .canonicalize()
        .unwrap_or_else(|_| repo_path.to_path_buf());
    let as_str = canonical.to_string_lossy();
    // FNV-1a: no dependency, and this only has to avoid collisions between the
    // handful of repos one user runs, not resist an adversary.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in as_str.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    /// The TOCTOU fix, stated as the behaviour it produces.
    ///
    /// The old version unlinked on drop, so the window between unlink and
    /// unlock was real: another process could create the path and both would
    /// believe they held the lock. Drop now only unlocks, so the file is still
    /// there for the next acquirer and there is no window.
    #[test]
    fn a_second_acquire_succeeds_after_the_first_is_dropped() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let state = tmp.path().join("state");

        let path = {
            let lock = RepoLock::acquire_default(&state, &repo).unwrap();
            lock.lock_path().to_path_buf()
        };
        // The file survives, holding no lock. It is not litter: it is the thing
        // the next acquirer locks, and deleting it is what opened the race.
        assert!(
            path.exists(),
            "drop must not unlink; the file is what closes the TOCTOU window"
        );
        let _reacquired = RepoLock::acquire(&state, &repo, 1)
            .expect("a second acquire must succeed once the first is dropped");
    }

    /// A contended acquire still fails, and the message says where the lock
    /// file is — otherwise a user whose process died is left with a lock that
    /// looks permanent and no way to tell.
    #[test]
    fn lock_is_exclusive_while_held() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let state = tmp.path().join("state");

        let _held = RepoLock::acquire_default(&state, &repo).unwrap();
        let result = RepoLock::acquire(&state, &repo, 1);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("timed out"), "error was: {msg}");
        assert!(
            msg.contains(".lock"),
            "the message must point at the lock file so a stale one is recoverable: {msg}"
        );
    }

    /// The reason the lock left the worktree. A lock inside the repo makes the
    /// repo look dirty, and a tool that dirties the tree it is inspecting
    /// cannot reliably inspect it.
    #[test]
    fn the_lock_never_appears_in_the_worktree() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let state = tmp.path().join("state");

        // Initialise first, so `status` means something at all.
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .output()
            .expect("git runs");

        let status = |dir: &Path| -> String {
            let out = Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(dir)
                .output()
                .expect("git runs");
            String::from_utf8_lossy(&out.stdout).into_owned()
        };

        {
            let _lock = RepoLock::acquire_default(&state, &repo).unwrap();
        }

        assert!(
            !repo.join(".ro.lock").exists(),
            "the lock must not be written into the worktree"
        );
        let after = status(&repo);
        assert!(
            after.trim().is_empty(),
            "the repo must read as clean after a run, got: {after:?}"
        );
    }

    /// The dropped `create_dir_all` side effect: locking a path that does not
    /// exist must not bring it into being.
    #[test]
    fn locking_a_nonexistent_path_does_not_create_it() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("not-a-repo");
        let state = tmp.path().join("state");

        // The key is derived from the raw path, so this succeeds...
        let _lock = RepoLock::acquire_default(&state, &missing).unwrap();
        // ...without conjuring the directory.
        assert!(
            !missing.exists(),
            "acquiring a lock must not create the worktree it names"
        );
    }

    /// A timeout leaves the lock file, which is correct — but it must not leave
    /// a *held* lock, or every later acquire fails.
    #[test]
    fn a_timed_out_acquire_leaves_nothing_held() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let state = tmp.path().join("state");

        let _held = RepoLock::acquire_default(&state, &repo).unwrap();
        assert!(RepoLock::acquire(&state, &repo, 1).is_err());
        drop(_held);
        // Immediately acquirable again, which it would not be if the failed
        // attempt had left a lock behind.
        RepoLock::acquire(&state, &repo, 1).expect("a failed acquire must not leave a lock held");
    }

    /// Two names for one repo must contend on one lock. Canonicalize is what
    /// makes `/tmp/x` and a symlink to it the same repo; without it, two
    /// processes can each hold "the lock" and mutate one worktree.
    #[test]
    fn two_names_for_one_repo_contend_on_one_lock() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let link = tmp.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo, &link).expect("a symlink is creatable");

        let state = tmp.path().join("state");
        let _held = RepoLock::acquire_default(&state, &repo).unwrap();

        #[cfg(unix)]
        assert!(
            RepoLock::acquire(&state, &link, 1).is_err(),
            "a symlink to a locked repo must not get a second lock"
        );
    }
}
