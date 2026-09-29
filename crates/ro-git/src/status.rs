//! Repo status detection.
//!
//! States: Current, Behind { n }, Ahead { n }, Diverged { ahead, behind },
//! Conflict, Dirty, Missing, Unknown. Quantitative variants carry the
//! commit counts so callers can render "behind by 3 commits".
//!
//! [`remote_ref_updated_at`] lives here for the same reason [`AheadBehind`]
//! does: both answer questions about *how far along* a repo is, and both
//! need the `origin/<branch>` ref to be named rather than assumed.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Repository status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum RepoStatus {
    Current,
    Behind { n: u32 },
    Ahead { n: u32 },
    Diverged { ahead: u32, behind: u32 },
    Conflict,
    Dirty,
    Missing,
    Unknown,
}

impl std::fmt::Display for RepoStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepoStatus::Current => write!(f, "Current"),
            RepoStatus::Behind { n } => write!(f, "Behind by {n}"),
            RepoStatus::Ahead { n } => write!(f, "Ahead by {n}"),
            RepoStatus::Diverged { ahead, behind } => {
                write!(f, "Diverged ({ahead} ahead, {behind} behind)")
            }
            RepoStatus::Conflict => write!(f, "Conflict"),
            RepoStatus::Dirty => write!(f, "Dirty"),
            RepoStatus::Missing => write!(f, "Missing"),
            RepoStatus::Unknown => write!(f, "Unknown"),
        }
    }
}

/// Ahead/behind counts relative to a reference (typically upstream).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AheadBehind {
    pub ahead: u32,
    pub behind: u32,
}

impl AheadBehind {
    pub const ZERO: Self = Self {
        ahead: 0,
        behind: 0,
    };

    /// Convert ahead/behind counts into a `RepoStatus`.
    pub fn to_status(self) -> RepoStatus {
        match (self.ahead, self.behind) {
            (0, 0) => RepoStatus::Current,
            (a, 0) => RepoStatus::Ahead { n: a },
            (0, b) => RepoStatus::Behind { n: b },
            (a, b) => RepoStatus::Diverged {
                ahead: a,
                behind: b,
            },
        }
    }
}

/// When `refs/remotes/<remote>/<branch>` was last updated, as a Unix
/// timestamp, and whether that is a real answer.
///
/// The reflog is the only record of *when* a remote-tracking ref moved.
/// `git for-each-ref` will happily report a ref that has not been touched
/// since the last fetch, and a `behind` count measured against it is a count
/// measured against a base nobody has checked — which is exactly the state
/// `ro status` is in during the documented daily loop (`ro status`, *then*
/// `ro sync`): the ref is whatever the previous fetch left behind.
///
/// `None` means the reflog could not be read at all, which is **not** the
/// same as "never updated". A repo whose reflog is disabled, or whose
/// remote-tracking ref was written by something other than a fetch, has no
/// timestamp to report, and answering `Some(0)` there would put a number on
/// a question nobody asked.
pub fn remote_ref_updated_at(
    repo_path: &Path,
    remote: &str,
    branch: &str,
) -> Result<Option<i64>> {
    let refname = format!("refs/remotes/{remote}/{branch}");
    let path = repo_path.join(".git").join("logs").join(&refname);
    let path = if path.is_file() {
        path
    } else {
        // A worktree's `.git` is a *file* pointing at the real gitdir, and a
        // submodule's points at `modules/<name>`. In both cases the reflog
        // lives beside the refs, not under the path that was joined above.
        // `git rev-parse --git-path` is the one command that knows where
        // either layout keeps it, so the layout is asked about rather than
        // guessed at.
        let git_path = git_path(repo_path, &format!("logs/{refname}"))?;
        if git_path.is_file() {
            git_path
        } else {
            // No reflog file: the ref has never been updated by a fetch, or
            // reflogs are off. Both are "there is no timestamp", and neither
            // is a failure.
            return Ok(None);
        }
    };
    let contents = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "reading the reflog for {refname} at {}",
            path.display()
        )
    })?;
    // The reflog is append-only, so the last line is the newest entry, and
    // its shape is
    //
    //     <old> <new> <name> <email> <ts> <tz>\t<message>
    //
    // Two things about that shape decide how it is read. The **tab** is the
    // boundary: the message after it is ordinary prose with spaces in it, so
    // a field counted from the far end of the whole line lands in the words
    // of the message rather than on a number — which is the version of this
    // that parsed `update by push` and got `by`. And within the head, the
    // timestamp is second from the right: the timezone is last, and the
    // committer identity before it may itself contain spaces.
    let newest = contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .next_back()
        .and_then(|line| line.split('\t').next())
        .and_then(|head| head.split_whitespace().rev().nth(1))
        .and_then(|ts| ts.parse::<i64>().ok());
    Ok(newest)
}

/// Ask git where a path inside the repository lives.
///
/// Used for the reflog, whose location depends on the layout of the checkout
/// (a plain repo, a worktree, a submodule) in ways that are not worth
/// hard-coding here.
fn git_path(repo_path: &Path, suffix: &str) -> Result<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--git-path", suffix])
        .current_dir(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .with_context(|| format!("asking git for a path inside {}", repo_path.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "git rev-parse --git-path {suffix} failed in {}: {}",
            repo_path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let reported = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if reported.is_empty() {
        anyhow::bail!("git rev-parse --git-path {suffix} returned nothing");
    }
    // `--git-path` answers with a path relative to the working directory
    // when the repository is a plain one, and absolute when it is not.
    // Resolving against `repo_path` covers both without having to know which
    // one this checkout is.
    Ok(repo_path.join(&reported))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ahead_behind_zero_is_current() {
        assert_eq!(AheadBehind::ZERO.to_status(), RepoStatus::Current);
    }

    #[test]
    fn ahead_only() {
        let ab = AheadBehind {
            ahead: 3,
            behind: 0,
        };
        assert_eq!(ab.to_status(), RepoStatus::Ahead { n: 3 });
    }

    #[test]
    fn behind_only() {
        let ab = AheadBehind {
            ahead: 0,
            behind: 5,
        };
        assert_eq!(ab.to_status(), RepoStatus::Behind { n: 5 });
    }

    #[test]
    fn diverged() {
        let ab = AheadBehind {
            ahead: 1,
            behind: 2,
        };
        assert_eq!(
            ab.to_status(),
            RepoStatus::Diverged {
                ahead: 1,
                behind: 2
            }
        );
    }

    #[test]
    fn display_formats() {
        assert_eq!(RepoStatus::Current.to_string(), "Current");
        assert_eq!(RepoStatus::Behind { n: 3 }.to_string(), "Behind by 3");
        assert_eq!(RepoStatus::Ahead { n: 1 }.to_string(), "Ahead by 1");
        assert_eq!(
            RepoStatus::Diverged {
                ahead: 2,
                behind: 3
            }
            .to_string(),
            "Diverged (2 ahead, 3 behind)"
        );
    }

    // ── When the remote-tracking ref last moved ──

    fn run_git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git runs");
        if !out.status.success() {
            panic!(
                "git {args:?} failed:\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    fn bare(dir: &Path) -> PathBuf {
        let remote = dir.join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        run_git(&remote, &["init", "--bare", "-q", "-b", "main"]);
        remote
    }

    /// A checkout with an `origin` and one push behind it: the shape every
    /// repo in a fleet actually has.
    fn seeded_worktree(tmp: &Path, remote: &Path) -> PathBuf {
        let work = tmp.join("work");
        std::fs::create_dir_all(&work).unwrap();
        run_git(&work, &["init", "-q", "-b", "main"]);
        run_git(&work, &["remote", "add", "origin", &remote.to_string_lossy()]);
        std::fs::write(work.join("a.txt"), "one\n").unwrap();
        run_git(&work, &["add", "."]);
        run_git(&work, &["commit", "-q", "-m", "one"]);
        run_git(&work, &["push", "-q", "origin", "main"]);
        work
    }

    /// A commit pushed by a second clone, which the first clone cannot know
    /// about until it fetches.
    fn push_one_more_commit(tmp: &Path, remote: &Path) {
        let other = tmp.join("other");
        run_git(tmp, &["clone", "-q", &remote.to_string_lossy(), "other"]);
        run_git(&other, &["config", "user.email", "test@example.com"]);
        run_git(&other, &["config", "user.name", "Test"]);
        std::fs::write(other.join("b.txt"), "two\n").unwrap();
        run_git(&other, &["add", "."]);
        run_git(&other, &["commit", "-q", "-m", "two"]);
        run_git(&other, &["push", "-q", "origin", "main"]);
    }

    /// The timestamp is git's, not an approximation.
    ///
    /// This is the whole reason the function exists: `behind=0` is only
    /// meaningful against a base someone has checked, and the reflog is the
    /// only record of when that check last happened.
    #[test]
    fn a_fetched_ref_reports_when_it_moved() {
        let tmp = tempfile::TempDir::new().unwrap();
        let remote = bare(tmp.path());
        let work = seeded_worktree(tmp.path(), &remote);

        let before = remote_ref_updated_at(&work, "origin", "main")
            .unwrap()
            .expect("a pushed remote-tracking ref has a reflog");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(
            (now - before).abs() < 120,
            "the reflog timestamp must be the push that just happened, got {before} \
             against a now of {now}"
        );
    }

    /// The newest entry wins. A ref fetched twice reports the second fetch,
    /// because that is the base the next `behind` count will be measured
    /// against — so a stale base cannot be reported as a fresh one.
    #[test]
    fn the_newest_reflog_entry_is_the_one_reported() {
        let tmp = tempfile::TempDir::new().unwrap();
        let remote = bare(tmp.path());
        let work = seeded_worktree(tmp.path(), &remote);
        let first = remote_ref_updated_at(&work, "origin", "main")
            .unwrap()
            .expect("a pushed remote-tracking ref has a reflog");

        // The remote moves and the checkout learns about it. Sleep is not
        // how the two timestamps are told apart: the second fetch is a
        // strictly later *event*, and the reflog records the second of the
        // two, so a fixture that only got slower under load still separates
        // them.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        push_one_more_commit(tmp.path(), &remote);
        run_git(&work, &["fetch", "-q", "origin"]);

        let second = remote_ref_updated_at(&work, "origin", "main")
            .unwrap()
            .expect("a fetched ref has a reflog");
        assert!(
            second > first,
            "the newest entry must be the one reported: first={first} second={second}"
        );
    }

    /// A ref that has never been fetched has no reflog, and that is a fact
    /// rather than a failure — `None`, never `Some(0)`.
    #[test]
    fn an_unfetched_ref_has_no_timestamp() {
        let tmp = tempfile::TempDir::new().unwrap();
        let remote = bare(tmp.path());

        // A checkout that has a remote and has never talked to it: the
        // remote-tracking ref does not exist locally, so there is no
        // reflog to read and no timestamp to report.
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        run_git(&work, &["init", "-q", "-b", "main"]);
        run_git(&work, &["remote", "add", "origin", &remote.to_string_lossy()]);
        std::fs::write(work.join("a.txt"), "one\n").unwrap();
        run_git(&work, &["add", "."]);
        run_git(&work, &["commit", "-q", "-m", "one"]);

        assert_eq!(
            remote_ref_updated_at(&work, "origin", "main").unwrap(),
            None,
            "a ref that has never been updated has no timestamp, and \
             inventing one would be a number about a question nobody asked"
        );
    }

    /// A path that is not a repository is an error, not a silent `None`.
    ///
    /// The two are different answers to different questions, and collapsing
    /// them is how a broken checkout comes to look like a repo that has
    /// simply never been fetched.
    #[test]
    fn a_directory_that_is_not_a_repo_is_an_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let not_a_repo = tmp.path().join("plain-directory");
        std::fs::create_dir_all(&not_a_repo).unwrap();

        let err = remote_ref_updated_at(&not_a_repo, "origin", "main")
            .expect_err("a non-repo has no reflog and no git to ask about one");
        assert!(
            format!("{err:#}").contains("rev-parse"),
            "the message must name the git call that failed, got {err:#}"
        );
    }
}
