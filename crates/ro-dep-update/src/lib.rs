//! AI-powered dependency updates.

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

pub struct DetectedDeps {
    pub ecosystem: String,
    pub out_of_date: Vec<String>,
}

pub fn detect_package_managers(repo_path: &Path) -> Vec<String> {
    let mut managers = Vec::new();
    if repo_path.join("Cargo.toml").exists() {
        managers.push("cargo".to_string());
    }
    if repo_path.join("package.json").exists() {
        managers.push("npm".to_string());
    }
    if repo_path.join("go.mod").exists() {
        managers.push("go".to_string());
    }
    if repo_path.join("pyproject.toml").exists()
        || repo_path.join("requirements.txt").exists()
        || repo_path.join("setup.py").exists()
    {
        managers.push("pip".to_string());
    }
    managers
}

pub fn check_outdated(repo_path: &Path, ecosystem: &str) -> Result<Vec<String>> {
    match ecosystem {
        "cargo" => {
            let out = Command::new("cargo")
                .args(["update", "--dry-run", "--color=never"])
                .current_dir(repo_path)
                .env("CARGO_TERM_PROGRESS_WIDTH", "80")
                .output()
                .context("cargo update --dry-run")?;
            let stderr = String::from_utf8_lossy(&out.stderr);
            let mut outdated = Vec::new();
            for line in stderr.lines() {
                if line.contains("Upgrading") || line.contains("Fetching") {
                    continue;
                }
                if line.contains(" ") {
                    let pkg = line.split_whitespace().next().unwrap_or("");
                    if !pkg.is_empty() && !outdated.contains(&pkg.to_string()) {
                        outdated.push(pkg.to_string());
                    }
                }
            }
            Ok(outdated)
        }
        "npm" => {
            let out = Command::new("npm")
                .args(["outdated", "--parseable", "--json"])
                .current_dir(repo_path)
                .output();
            match out {
                Ok(o) if o.status.success() => {
                    let json: serde_json::Value =
                        serde_json::from_slice(&o.stdout).unwrap_or_default();
                    let names: Vec<String> = json
                        .as_object()
                        .map(|obj| {
                            obj.keys()
                                .filter_map(|k| {
                                    if k.starts_with('@') {
                                        None
                                    } else {
                                        Some(k.clone())
                                    }
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    Ok(names)
                }
                _ => Ok(Vec::new()),
            }
        }
        "go" => {
            let out = Command::new("go")
                .args(["list", "-m", "-u", "all"])
                .current_dir(repo_path)
                .output()
                .context("go list -m -u all")?;
            let lines = String::from_utf8_lossy(&out.stdout);
            let outdated: Vec<String> = lines
                .lines()
                .filter_map(|l| {
                    let parts: Vec<&str> = l.split_whitespace().collect();
                    if parts.len() >= 2 && parts[1].contains("upgrade") {
                        Some(parts[0].to_string())
                    } else {
                        None
                    }
                })
                .collect();
            Ok(outdated)
        }
        "pip" => {
            let out = Command::new("pip")
                .args(["list", "--outdated", "--format=json"])
                .current_dir(repo_path)
                .output();
            match out {
                Ok(o) if o.status.success() => {
                    let pkgs: Vec<serde_json::Value> =
                        serde_json::from_slice(&o.stdout).unwrap_or_default();
                    let names: Vec<String> = pkgs
                        .iter()
                        .filter_map(|p| p.get("name").and_then(|n| n.as_str()).map(String::from))
                        .collect();
                    Ok(names)
                }
                _ => Ok(Vec::new()),
            }
        }
        _ => Ok(Vec::new()),
    }
}

pub fn update_single(repo_path: &Path, ecosystem: &str, package: &str) -> Result<bool> {
    match ecosystem {
        "cargo" => {
            let out = Command::new("cargo")
                .args(["update", package])
                .current_dir(repo_path)
                .env("CARGO_TERM_PROGRESS_WIDTH", "80")
                .output()
                .context("cargo update")?;
            Ok(out.status.success())
        }
        "npm" => {
            let out = Command::new("npm")
                .args(["install", package])
                .current_dir(repo_path)
                .output()
                .context("npm install")?;
            Ok(out.status.success())
        }
        "go" => {
            let out = Command::new("go")
                .args(["get", package])
                .current_dir(repo_path)
                .output()
                .context("go get")?;
            Ok(out.status.success())
        }
        "pip" => {
            let out = Command::new("pip")
                .args(["install", "--upgrade", package])
                .current_dir(repo_path)
                .output()
                .context("pip install --upgrade")?;
            Ok(out.status.success())
        }
        _ => Ok(false),
    }
}

pub fn run_tests(repo_path: &Path, ecosystem: &str) -> Result<bool> {
    match ecosystem {
        "cargo" => {
            let out = Command::new("cargo")
                .args(["test", "--no-fail-fast", "--color=never"])
                .current_dir(repo_path)
                .output()
                .context("cargo test")?;
            Ok(out.status.success())
        }
        "npm" => {
            let out = Command::new("npm")
                .args(["test"])
                .current_dir(repo_path)
                .output()
                .context("npm test")?;
            Ok(out.status.success())
        }
        "go" => {
            let out = Command::new("go")
                .args(["test", "./..."])
                .current_dir(repo_path)
                .output()
                .context("go test ./...")?;
            Ok(out.status.success())
        }
        _ => Ok(true),
    }
}

pub fn detect_test_command(repo_path: &Path, ecosystem: &str) -> Option<String> {
    match ecosystem {
        "cargo" => Some("cargo test".to_string()),
        "npm" => {
            if repo_path.join("package.json").is_file() {
                Some("npm test".to_string())
            } else {
                None
            }
        }
        "go" => Some("go test ./...".to_string()),
        _ => None,
    }
}

pub fn commit_update(repo_path: &Path, package: &str, _ecosystem: &str) -> Result<String> {
    let branch = current_branch(repo_path)?;
    let stage = Command::new("git")
        .args(["add", "."])
        .current_dir(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .context("git add")?;
    if !stage.status.success() {
        anyhow::bail!("git add failed");
    }

    let commit_msg = format!("chore(deps): update {package}");
    let commit = Command::new("git")
        .args(["commit", "-m", &commit_msg])
        .current_dir(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .context("git commit")?;
    if !commit.status.success() {
        let stderr = String::from_utf8_lossy(&commit.stderr);
        if stderr.contains("nothing to commit") {
            return Ok("nothing to commit".to_string());
        }
        anyhow::bail!("git commit failed: {stderr}");
    }

    let oid: String = String::from_utf8_lossy(&commit.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(8)
        .collect();

    if let Some(b) = branch {
        let push = Command::new("git")
            .args(["push", "origin", &b])
            .current_dir(repo_path)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .output();
        if push.as_ref().is_ok_and(|o| !o.status.success()) {
            return Ok(format!("committed {} (push failed)", oid));
        }
    }

    Ok(format!("committed {}", oid))
}

fn current_branch(repo_path: &Path) -> Result<Option<String>> {
    let out = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .context("git branch --show-current")?;
    if !out.status.success() {
        return Ok(None);
    }
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if branch.is_empty() {
        Ok(None)
    } else {
        Ok(Some(branch))
    }
}

pub struct UpdateResult {
    pub package: String,
    pub status: String,
    pub oid: String,
}

pub fn update_and_test(
    repo_path: &Path,
    ecosystem: &str,
    package: &str,
    max_retries: u32,
) -> Result<UpdateResult> {
    let mut attempt = 0u32;

    loop {
        attempt += 1;
        if attempt > max_retries {
            return Ok(UpdateResult {
                package: package.to_string(),
                status: "max retries".to_string(),
                oid: String::new(),
            });
        }

        if !update_single(repo_path, ecosystem, package)? {
            return Ok(UpdateResult {
                package: package.to_string(),
                status: "update failed".to_string(),
                oid: String::new(),
            });
        }

        if run_tests(repo_path, ecosystem)? {
            let oid = commit_update(repo_path, package, ecosystem)?;
            return Ok(UpdateResult {
                package: package.to_string(),
                status: "ok".to_string(),
                oid,
            });
        }

        if attempt >= max_retries {
            return Ok(UpdateResult {
                package: package.to_string(),
                status: "tests failed after max retries".to_string(),
                oid: String::new(),
            });
        }

        let reset = Command::new("git")
            .args(["checkout", "--", "."])
            .current_dir(repo_path)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .output();
        if reset.as_ref().is_err() || !reset.unwrap().status.success() {
            return Ok(UpdateResult {
                package: package.to_string(),
                status: "test failed and reset failed".to_string(),
                oid: String::new(),
            });
        }
    }
}
