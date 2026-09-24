//! Test fixtures.
//!
//! Shared fixtures for tests across all crates.

use tempfile::TempDir;

/// Create a temporary directory with a minimal git repo initialized.
pub fn git_repo() -> TempDir {
    let tmp = TempDir::new().unwrap();
    std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&tmp)
        .output()
        .expect("git init");
    std::process::Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(&tmp)
        .output()
        .expect("git config");
    std::process::Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(&tmp)
        .output()
        .expect("git config");
    tmp
}

/// Create a temporary directory with a Cargo.toml for a Rust project.
pub fn rust_project() -> TempDir {
    let tmp = git_repo();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        b"[package]\nname = \"test\"\n",
    )
    .unwrap();
    tmp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_repo_creates_valid_repo() {
        let tmp = git_repo();
        let dot_git = tmp.path().join(".git");
        assert!(dot_git.exists());
    }

    #[test]
    fn rust_project_has_cargo_toml() {
        let tmp = rust_project();
        assert!(tmp.path().join("Cargo.toml").exists());
    }
}
