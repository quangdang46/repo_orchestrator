//! Exit codes, and the one error that is not about a repo.
//!
//! | Code | Meaning |
//! |---|---|
//! | **0** | every repo succeeded — and `--all` on an empty inventory is a legitimate 0 |
//! | **1** | partial: some repos succeeded, some did not |
//! | **2** | all repos failed |
//! | **64** | bad usage: unknown flag, unknown repo name, a name matching nothing, an invalid glob |
//! | **70** | fatal: bad config, `open_db` failure, a duplicate add |
//!
//! # `2` used to mean something else
//!
//! clap exits 2 on a usage error, and the fleet table wants `2 = all
//! failed`. A CI script keying on the old meaning silently reinterprets
//! it, so this is a **breaking change** and belongs in the release note.
//!
//! # Why 70 and not 1 for fatal
//!
//! `main()` used to do `eprintln!` + `exit(1)` on any escaped error, so a
//! config file that will not parse was reported as *"some repos
//! succeeded"*. That is worse than a wrong number: it invites a retry that
//! cannot possibly work. `EX_FATAL` is `sysexits.h`'s `EX_SOFTWARE` —
//! the closest portable convention, and a number nothing else in the
//! table can be confused with.
//!
//! # `ro doctor` is not a fleet command
//!
//! It keeps its own 0/1, and its `Severity::Optional` probes can never
//! move the exit code. `ro prune` hard-exits 3. Applying the fleet table
//! to every command would read a failed environment check as a partial
//! run.

/// Bad usage. The clap-adjacent convention (`EX_USAGE`).
pub const EX_USAGE: u8 = 64;

/// Fatal: not about any repo.
pub const EX_FATAL: u8 = 70;

/// Every repo failed.
pub const EX_ALL_FAILED: u8 = 2;

/// Some repos failed and some did not.
pub const EX_PARTIAL: u8 = 1;

/// Everything worked.
pub const EX_OK: u8 = 0;

/// `ro doctor`'s own code: an environment check failed.
///
/// Its own number, not the fleet table's — a failed environment check is
/// not a partial fleet run, and reading it as one is how a green run gets
/// retried for the wrong reason.
pub const EX_DOCTOR_FAILED: u8 = 1;

/// What a fleet run produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunExit {
    Ok,
    Partial,
    AllFailed,
}

impl RunExit {
    /// Derive the code from the counts.
    ///
    /// `--all` on an empty inventory is `Ok`, not a failure: asking for
    /// everything when there is nothing is a legitimate question with a
    /// legitimate answer, and a `ro ship --all` on a fresh install exiting
    /// 2 would read as "everything broke".
    pub fn from_counts(succeeded: usize, failed: usize) -> Self {
        match (failed, succeeded) {
            (0, _) => RunExit::Ok,
            (_, 0) => RunExit::AllFailed,
            _ => RunExit::Partial,
        }
    }

    pub fn code(self) -> u8 {
        match self {
            RunExit::Ok => EX_OK,
            RunExit::Partial => EX_PARTIAL,
            RunExit::AllFailed => EX_ALL_FAILED,
        }
    }
}

/// An error that is not about a repo: bad config, an unopenable database,
/// a duplicate add.
///
/// A distinct type rather than a marker in a string, so it cannot be
/// produced by accident and so the compiler forces the two paths apart.
#[derive(Debug, Clone)]
pub struct FatalError {
    pub message: String,
    pub code: u8,
}

impl FatalError {
    /// A fatal error with the standard code.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: EX_FATAL,
        }
    }

    /// The code, for the top-level handler and for tests.
    ///
    /// A method rather than a bare field so every exit goes through one
    /// named place, and a test can assert on it without reaching into the
    /// struct.
    pub fn code(&self) -> u8 {
        self.code
    }
}

impl std::fmt::Display for FatalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for FatalError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_what_the_doc_says() {
        assert_eq!(RunExit::from_counts(3, 0), RunExit::Ok);
        assert_eq!(RunExit::from_counts(2, 1), RunExit::Partial);
        assert_eq!(RunExit::from_counts(0, 3), RunExit::AllFailed);

        assert_eq!(RunExit::Ok.code(), 0);
        assert_eq!(RunExit::Partial.code(), 1);
        assert_eq!(RunExit::AllFailed.code(), 2);
        assert_eq!(EX_USAGE, 64);
        assert_eq!(EX_FATAL, 70);
    }

    /// `--all` on an empty inventory is a legitimate 0.
    ///
    /// Exiting 2 would read as "everything broke" on a fresh install.
    #[test]
    fn an_empty_fleet_is_a_success() {
        assert_eq!(RunExit::from_counts(0, 0), RunExit::Ok);
    }

    /// And "all failed" really means all, not "some".
    #[test]
    fn one_success_among_failures_is_partial_not_total() {
        assert_eq!(RunExit::from_counts(1, 4), RunExit::Partial);
    }

    /// The codes must not collide.
    ///
    /// The bead calls out that `2` changed meaning: a script keying on
    /// clap's old 2 would silently re-read it.
    #[test]
    fn no_two_meanings_share_a_code() {
        let all = [EX_OK, EX_PARTIAL, EX_ALL_FAILED, EX_USAGE, EX_FATAL];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b, "two meanings share code {a}");
            }
        }
    }

    /// A fatal error is not 1, so it cannot be read as a partial run.
    #[test]
    fn a_fatal_error_is_not_the_partial_code() {
        let e = FatalError::new("config will not parse");
        assert_ne!(
            e.code, EX_PARTIAL,
            "a config that will not parse must not read as 'some repos \\
             succeeded' — that invites a retry that cannot work"
        );
        assert_eq!(e.code, EX_FATAL);
    }

    /// A duplicate `ro add` is a usage problem, not a crash — so the
    /// constructor that says so exists, and a test says the code is the
    /// usage one.
    #[test]
    fn a_duplicate_add_is_a_usage_problem_not_a_crash() {
        let e = FatalError {
            message: "already tracked".into(),
            code: EX_USAGE,
        };
        assert_ne!(e.code, EX_FATAL, "a duplicate add must not read as a crash");
    }
}
