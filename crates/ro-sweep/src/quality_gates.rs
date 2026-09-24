//! Quality gate auto-detection and execution.
//!
//! Rust: cargo fmt --check, cargo test, cargo clippy
//! Node: npm test, npm run lint, npm run typecheck
//! Python: pytest, ruff, mypy
//! Go: go test ./..., go fmt
//! Shell: shellcheck

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;
use std::time::Instant;

/// Ecosystem detected for a repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ecosystem {
    Rust,
    Node,
    Python,
    Go,
    Shell,
}

impl Ecosystem {
    /// Default gates for this ecosystem.
    pub fn gates(self) -> &'static [Gate] {
        match self {
            Self::Rust => &[
                Gate {
                    name: "cargo fmt --check",
                    program: "cargo",
                    args: &["fmt", "--all", "--check"],
                },
                Gate {
                    name: "cargo clippy",
                    program: "cargo",
                    args: &[
                        "clippy",
                        "--workspace",
                        "--all-targets",
                        "--",
                        "-D",
                        "warnings",
                    ],
                },
                Gate {
                    name: "cargo test",
                    program: "cargo",
                    args: &["test", "--workspace"],
                },
            ],
            Self::Node => &[
                Gate {
                    name: "npm run lint",
                    program: "npm",
                    args: &["run", "lint"],
                },
                Gate {
                    name: "npm run typecheck",
                    program: "npm",
                    args: &["run", "typecheck"],
                },
                Gate {
                    name: "npm test",
                    program: "npm",
                    args: &["test"],
                },
            ],
            Self::Python => &[
                Gate {
                    name: "ruff",
                    program: "ruff",
                    args: &["check", "."],
                },
                Gate {
                    name: "mypy",
                    program: "mypy",
                    args: &["."],
                },
                Gate {
                    name: "pytest",
                    program: "pytest",
                    args: &["-q"],
                },
            ],
            Self::Go => &[
                Gate {
                    name: "go fmt",
                    program: "go",
                    args: &["fmt", "./..."],
                },
                Gate {
                    name: "go test",
                    program: "go",
                    args: &["test", "./..."],
                },
            ],
            Self::Shell => &[Gate {
                name: "shellcheck",
                program: "shellcheck",
                args: &["-x", "."],
            }],
        }
    }
}

impl std::fmt::Display for Ecosystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Rust => "rust",
            Self::Node => "node",
            Self::Python => "python",
            Self::Go => "go",
            Self::Shell => "shell",
        })
    }
}

/// Static description of a gate to run.
#[derive(Debug, Clone, Copy)]
pub struct Gate {
    pub name: &'static str,
    pub program: &'static str,
    pub args: &'static [&'static str],
}

/// Outcome of a single gate run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GateResult {
    pub name: String,
    pub status: GateStatus,
    pub duration_ms: u128,
    pub stdout: String,
    pub stderr: String,
    /// Process exit code (None if the binary was not found).
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GateStatus {
    Passed,
    Failed,
    Skipped,
}

/// Detect ecosystems present in `repo_path`. Multiple ecosystems may
/// coexist in monorepos, so a Vec is returned.
pub fn detect(repo_path: &Path) -> Vec<Ecosystem> {
    let mut found = Vec::new();
    if repo_path.join("Cargo.toml").exists() {
        found.push(Ecosystem::Rust);
    }
    if repo_path.join("package.json").exists() {
        found.push(Ecosystem::Node);
    }
    if repo_path.join("pyproject.toml").exists()
        || repo_path.join("requirements.txt").exists()
        || repo_path.join("setup.py").exists()
    {
        found.push(Ecosystem::Python);
    }
    if repo_path.join("go.mod").exists() {
        found.push(Ecosystem::Go);
    }
    found
}

/// Run a single gate in `repo_path` and capture the result.
///
/// If the gate's binary cannot be located, the result is `Skipped`
/// rather than `Failed`.
pub fn run_gate(repo_path: &Path, gate: &Gate) -> GateResult {
    let start = Instant::now();
    let mut cmd = Command::new(gate.program);
    cmd.args(gate.args)
        .current_dir(repo_path)
        .env("CI", "true")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .env("CARGO_TERM_COLOR", "never");
    let output = cmd.output();
    let duration_ms = start.elapsed().as_millis();
    match output {
        Ok(out) => {
            let status = if out.status.success() {
                GateStatus::Passed
            } else {
                GateStatus::Failed
            };
            GateResult {
                name: gate.name.to_string(),
                status,
                duration_ms,
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                exit_code: out.status.code(),
            }
        }
        Err(e) => GateResult {
            name: gate.name.to_string(),
            status: GateStatus::Skipped,
            duration_ms,
            stdout: String::new(),
            stderr: format!("could not run {}: {e}", gate.program),
            exit_code: None,
        },
    }
}

/// Run every default gate for every detected ecosystem.
pub fn run_all(repo_path: &Path) -> Result<Vec<GateResult>> {
    let mut results = Vec::new();
    for eco in detect(repo_path) {
        for gate in eco.gates() {
            results.push(run_gate(repo_path, gate));
        }
    }
    Ok(results)
}

/// Return true if any gate failed.
pub fn any_failed(results: &[GateResult]) -> bool {
    results.iter().any(|r| r.status == GateStatus::Failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn detect_rust_repo() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
        assert_eq!(detect(tmp.path()), vec![Ecosystem::Rust]);
    }

    #[test]
    fn detect_node_repo() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("package.json"), "{}").unwrap();
        assert_eq!(detect(tmp.path()), vec![Ecosystem::Node]);
    }

    #[test]
    fn detect_python_repo_pyproject() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("pyproject.toml"), "").unwrap();
        assert_eq!(detect(tmp.path()), vec![Ecosystem::Python]);
    }

    #[test]
    fn detect_python_repo_requirements() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("requirements.txt"), "").unwrap();
        assert_eq!(detect(tmp.path()), vec![Ecosystem::Python]);
    }

    #[test]
    fn detect_go_repo() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("go.mod"), "module x").unwrap();
        assert_eq!(detect(tmp.path()), vec![Ecosystem::Go]);
    }

    #[test]
    fn detect_polyglot_repo() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(tmp.path().join("package.json"), "{}").unwrap();
        let r = detect(tmp.path());
        assert!(r.contains(&Ecosystem::Rust) && r.contains(&Ecosystem::Node));
    }

    #[test]
    fn detect_empty_dir() {
        let tmp = TempDir::new().unwrap();
        assert!(detect(tmp.path()).is_empty());
    }

    #[test]
    fn rust_gates_have_three_steps() {
        let g = Ecosystem::Rust.gates();
        assert_eq!(g.len(), 3);
        assert!(g.iter().any(|x| x.name.contains("fmt")));
        assert!(g.iter().any(|x| x.name.contains("clippy")));
        assert!(g.iter().any(|x| x.name.contains("test")));
    }

    #[test]
    fn run_gate_skips_when_binary_missing() {
        let tmp = TempDir::new().unwrap();
        let gate = Gate {
            name: "nonexistent",
            program: "definitely-not-a-real-binary-ro-test-9999",
            args: &[],
        };
        let r = run_gate(tmp.path(), &gate);
        assert_eq!(r.status, GateStatus::Skipped);
        assert_eq!(r.exit_code, None);
    }

    #[test]
    fn run_gate_passed_status() {
        let tmp = TempDir::new().unwrap();
        #[cfg(unix)]
        let gate = Gate {
            name: "true",
            program: "true",
            args: &[],
        };
        #[cfg(windows)]
        let gate = Gate {
            name: "true",
            program: "cmd",
            args: &["/C", "exit", "0"],
        };
        let r = run_gate(tmp.path(), &gate);
        assert_eq!(r.status, GateStatus::Passed);
        assert_eq!(r.exit_code, Some(0));
    }

    #[test]
    fn run_gate_failed_status() {
        let tmp = TempDir::new().unwrap();
        #[cfg(unix)]
        let gate = Gate {
            name: "false",
            program: "false",
            args: &[],
        };
        #[cfg(windows)]
        let gate = Gate {
            name: "false",
            program: "cmd",
            args: &["/C", "exit", "1"],
        };
        let r = run_gate(tmp.path(), &gate);
        assert_eq!(r.status, GateStatus::Failed);
        assert_eq!(r.exit_code, Some(1));
    }

    #[test]
    fn any_failed_detects_failure() {
        let pass = GateResult {
            name: "x".into(),
            status: GateStatus::Passed,
            duration_ms: 0,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
        };
        let fail = GateResult {
            name: "y".into(),
            status: GateStatus::Failed,
            duration_ms: 0,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(1),
        };
        assert!(!any_failed(std::slice::from_ref(&pass)));
        assert!(any_failed(&[pass, fail]));
    }

    #[test]
    fn json_round_trip() {
        let r = GateResult {
            name: "cargo test".into(),
            status: GateStatus::Passed,
            duration_ms: 1234,
            stdout: "ok".into(),
            stderr: String::new(),
            exit_code: Some(0),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"status\":\"passed\""));
        let back: GateResult = serde_json::from_str(&s).unwrap();
        assert_eq!(r, back);
    }
}
