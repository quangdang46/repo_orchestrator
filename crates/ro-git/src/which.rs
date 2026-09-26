//! Finding a binary on `PATH`.
//!
//! It lives here rather than in the `ro` binary because both callers need
//! it and only one of them is a binary: `ro doctor` checks that a provider
//! is installed, and the engines check the same thing at dispatch. A
//! `which_in` private to the binary crate is a `which_in` a library cannot
//! reach, which is how the doctor had to grow its own copy.
//!
//! It is a *probe*, and nothing more. It answers "is there a file with
//! this name somewhere on `PATH`" — not "does this work", not "is this the
//! right version". Availability is checked at dispatch time only, never at
//! `ro init` and never in a way that changes doctor's exit code.

use std::path::{Path, PathBuf};

/// The first `bin` found on `PATH`, or on `lookup_path` when given.
///
/// `lookup_path` exists so a test can point at a directory it controls
/// instead of whatever the machine happens to have installed.
pub fn which_in(bin: &str, lookup_path: Option<&Path>) -> Option<PathBuf> {
    let path_var = match lookup_path {
        Some(p) => p.to_string_lossy().into_owned(),
        None => std::env::var("PATH").ok()?,
    };
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
        // Windows resolves `.exe` through PATHEXT, but a caller asking
        // for `claude` by name should not have to know that.
        let candidate_exe = dir.join(format!("{bin}.exe"));
        if candidate_exe.is_file() {
            return Some(candidate_exe);
        }
        let candidate_cmd = dir.join(format!("{bin}.cmd"));
        if candidate_cmd.is_file() {
            return Some(candidate_cmd);
        }
    }
    None
}

/// The first `bin` on the process's own `PATH`.
pub fn which(bin: &str) -> Option<PathBuf> {
    which_in(bin, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn it_finds_a_file_that_is_there() {
        let dir = TempDir::new().unwrap();
        let script = dir.path().join("ro-test-probe");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        assert_eq!(
            which_in("ro-test-probe", Some(dir.path())),
            Some(script),
            "a file that exists must be found"
        );
    }

    /// The negative control. Without it, the test above would also pass on
    /// a probe that returned the first candidate without checking it.
    #[test]
    fn it_reports_absence_for_a_missing_name() {
        let dir = TempDir::new().unwrap();
        assert_eq!(
            which_in("definitely-not-installed-xyz", Some(dir.path())),
            None
        );
    }

    /// A directory is not a binary. Without the `is_file` check, a
    /// directory named `claude` on `PATH` would read as "installed" and
    /// the failure would surface per-repo instead of as a setup problem.
    #[test]
    fn a_directory_is_not_a_binary() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("claude")).unwrap();
        assert_eq!(
            which_in("claude", Some(dir.path())),
            None,
            "a directory must not read as an installed binary"
        );
    }

    #[test]
    fn an_empty_lookup_path_finds_nothing() {
        let dir = TempDir::new().unwrap();
        assert_eq!(which_in("anything", Some(dir.path())), None);
    }
}
