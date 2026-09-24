//! Repo-level file locks using fs4.
//!
//! Before any git mutation, acquire a lock on the repo path.
//! Lock is released after mutation completes or fails.
//! Lock timeout with clear error message.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fs4::fs_std::FileExt;

/// Default lock timeout in seconds.
const DEFAULT_LOCK_TIMEOUT_SECS: u64 = 30;

/// A RAII guard that holds an exclusive file lock on a repository.
///
/// The lock file lives next to the repo worktree at `<repo_path>/.ro.lock`.
/// When the guard is dropped, the lock is released automatically.
#[derive(Debug)]
pub struct RepoLock {
    file: std::fs::File,
    lock_path: PathBuf,
}

impl RepoLock {
    /// Acquire an exclusive lock on the given repo path.
    ///
    /// The lock file is created at `<repo_path>/.ro.lock`. If the lock
    /// cannot be acquired within `timeout_secs`, an error is returned with
    /// a clear message indicating which repo is locked.
    pub fn acquire(repo_path: &Path, timeout_secs: u64) -> Result<Self> {
        std::fs::create_dir_all(repo_path)
            .with_context(|| format!("creating lock parent dir {}", repo_path.display()))?;
        let lock_path = repo_path.join(".ro.lock");
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
                    return Ok(Self { file, lock_path });
                }
                Ok(false) => {
                    if std::time::Instant::now() >= deadline {
                        anyhow::bail!(
                            "timed out after {timeout_secs}s waiting for repo lock at {}. \
                             Another ro process may be operating on this repo.",
                            repo_path.display()
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
    pub fn acquire_default(repo_path: &Path) -> Result<Self> {
        Self::acquire(repo_path, DEFAULT_LOCK_TIMEOUT_SECS)
    }

    /// Return the path to the lock file.
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
        // Best-effort cleanup of the lock file.
        let _ = std::fs::remove_file(&self.lock_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn lock_acquire_and_release() {
        let tmp = TempDir::new().unwrap();
        let lock = RepoLock::acquire_default(tmp.path()).unwrap();
        assert!(lock.lock_path().exists());
        let path = lock.lock_path().to_path_buf();
        drop(lock);
        // Lock file is cleaned up on drop.
        assert!(!path.exists());
    }

    #[test]
    fn lock_is_exclusive() {
        let tmp = TempDir::new().unwrap();
        let _lock1 = RepoLock::acquire_default(tmp.path()).unwrap();
        // Second acquire with a short timeout should fail.
        let result = RepoLock::acquire(tmp.path(), 1);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("timed out"), "error was: {msg}");
    }

    #[test]
    fn lock_releases_on_drop() {
        let tmp = TempDir::new().unwrap();
        {
            let _lock = RepoLock::acquire_default(tmp.path()).unwrap();
        }
        // After drop, we can re-acquire immediately.
        let _lock2 = RepoLock::acquire(tmp.path(), 1).unwrap();
    }
}
