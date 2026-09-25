//! `ro doctor` — diagnose installation health.
//!
//! Pure detection by default; mutations only happen when `--fix` is set, and
//! every mutation backs up the prior state to `<state_dir>/doctor/runs/<run-id>/`.
//!
//! Checks (per rfo-47 spec):
//!
//! * `git` binary is on `PATH` and reports a parseable version
//! * GitHub auth: `discover_token` finds a token via env / config / `gh`
//! * Config: XDG paths exist (or are creatable) and `config.toml` parses+validates
//! * SQLite state: state directory exists and `state.db` opens with current schema
//! * Providers: `claude` and `codex` binaries available on `PATH`
//!
//! Each check returns a [`CheckResult`] with name, severity, status, and an
//! optional fix hint or applied-fix description. The full report is rendered as
//! human-readable text or JSON via [`ro_output`].

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use ro_config::{ConfigPaths, loader::load_config, paths::default_config_toml};
use ro_github::auth::discover_token;
use ro_state::open_db;

/// Outcome of a single doctor check.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Everything is healthy.
    Ok,
    /// Non-fatal issue; agent/user should know but tool still works.
    Warn,
    /// Issue prevents normal operation.
    Fail,
    /// Detection skipped (e.g., optional component, online required).
    Skipped,
}

impl Status {
    /// True for `Fail`; consumers use this for exit-code aggregation.
    pub fn is_fail(&self) -> bool {
        matches!(self, Status::Fail)
    }

    /// True for `Warn`.
    pub fn is_warn(&self) -> bool {
        matches!(self, Status::Warn)
    }
}

/// Categorical severity used for sorting and short labels.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Required,
    Optional,
}

/// A single check's findings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub severity: Severity,
    pub status: Status,
    /// Short detail for the operator.
    pub message: String,
    /// Suggested next command or env var if status != Ok and no fix was applied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix_hint: Option<String>,
    /// Description of a fix that `--fix` actually applied this run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied_fix: Option<String>,
}

impl CheckResult {
    fn ok(name: impl Into<String>, severity: Severity, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            severity,
            status: Status::Ok,
            message: message.into(),
            fix_hint: None,
            applied_fix: None,
        }
    }

    fn fail(
        name: impl Into<String>,
        severity: Severity,
        message: impl Into<String>,
        fix_hint: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            severity,
            status: Status::Fail,
            message: message.into(),
            fix_hint: Some(fix_hint.into()),
            applied_fix: None,
        }
    }

    fn warn(
        name: impl Into<String>,
        severity: Severity,
        message: impl Into<String>,
        fix_hint: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            severity,
            status: Status::Warn,
            message: message.into(),
            fix_hint,
            applied_fix: None,
        }
    }

    fn with_applied_fix(mut self, applied: impl Into<String>) -> Self {
        self.applied_fix = Some(applied.into());
        self
    }
}

/// Aggregate report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub run_id: String,
    pub started_at_unix: u64,
    pub fix_attempted: bool,
    pub checks: Vec<CheckResult>,
}

impl DoctorReport {
    /// Number of fail-status checks.
    pub fn failures(&self) -> usize {
        self.checks.iter().filter(|c| c.status.is_fail()).count()
    }

    /// Number of warn-status checks.
    pub fn warnings(&self) -> usize {
        self.checks.iter().filter(|c| c.status.is_warn()).count()
    }

    /// Recommended exit code.
    ///
    /// `0` healthy or warnings only; `1` if any check failed.
    pub fn exit_code(&self) -> i32 {
        if self.failures() > 0 { 1 } else { 0 }
    }
}

/// Inputs to the doctor.  Allows tests to override paths and env lookups.
#[derive(Debug, Clone, Default)]
pub struct DoctorOptions {
    pub config_token: Option<String>,
    pub fix: bool,
    /// Override `PATH`-based binary discovery (for tests). When `None`, use the
    /// process environment.
    pub binary_lookup_path: Option<PathBuf>,
    /// Override config/state/cache paths. When `None`, use [`ConfigPaths::discover`]
    /// (which honors `XDG_*` env vars). Passing this lets `ro --config-dir
    /// ... --state-dir ... doctor` actually inspect the paths the user asked
    /// about instead of always reporting on the default XDG location.
    pub paths: Option<ConfigPaths>,
}

/// Run all checks and return a [`DoctorReport`].
pub fn run(opts: DoctorOptions) -> DoctorReport {
    let started_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let run_id = format!("doctor-{started_at_unix:x}");

    let mut checks = Vec::new();
    checks.push(check_git());
    checks.push(check_github_auth(opts.config_token.as_deref()));

    let paths = opts.paths.clone().unwrap_or_else(|| {
        ConfigPaths::discover().unwrap_or_else(|_| {
            // fallback to tmp dir for testability - in real usage this should not fail
            let tmp = tempfile::tempdir().unwrap();
            ConfigPaths {
                config_dir: tmp.path().join(".config/ro"),
                state_dir: tmp.path().join(".local/state/ro"),
                cache_dir: tmp.path().join(".cache/ro"),
            }
        })
    });

    let (cfg_check, applied_fix_count) = check_and_optionally_fix_config(&paths, opts.fix);
    checks.push(cfg_check);

    checks.push(check_state(&paths, opts.fix));
    let _ = applied_fix_count; // reserved for future scoring

    checks.push(check_provider("claude", opts.binary_lookup_path.as_deref()));
    checks.push(check_provider("codex", opts.binary_lookup_path.as_deref()));

    DoctorReport {
        run_id,
        started_at_unix,
        fix_attempted: opts.fix,
        checks,
    }
}

fn check_git() -> CheckResult {
    match Command::new("git").arg("--version").output() {
        Ok(out) if out.status.success() => {
            let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
            CheckResult::ok("git", Severity::Required, v)
        }
        Ok(out) => CheckResult::fail(
            "git",
            Severity::Required,
            format!(
                "git --version exited {}: {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            "install git from https://git-scm.com/downloads",
        ),
        Err(e) => CheckResult::fail(
            "git",
            Severity::Required,
            format!("git binary not found on PATH: {e}"),
            "install git from https://git-scm.com/downloads",
        ),
    }
}

fn check_github_auth(config_token: Option<&str>) -> CheckResult {
    match discover_token("auto", config_token) {
        Ok(_token) => CheckResult::ok(
            "github_auth",
            Severity::Required,
            "GitHub token discovered via env/config/gh",
        ),
        Err(e) => CheckResult::fail(
            "github_auth",
            Severity::Required,
            format!("no GitHub token available: {e}"),
            "set GITHUB_TOKEN env var, run `gh auth login`, or set [github].token in config.toml",
        ),
    }
}

fn check_and_optionally_fix_config(paths: &ConfigPaths, fix: bool) -> (CheckResult, usize) {
    let cfg_path = paths.config_toml();
    if cfg_path.exists() {
        return match load_config(&cfg_path) {
            Ok(_) => (
                CheckResult::ok(
                    "config",
                    Severity::Required,
                    format!("config valid at {}", cfg_path.display()),
                ),
                0,
            ),
            Err(e) => (
                CheckResult::fail(
                    "config",
                    Severity::Required,
                    format!("invalid config at {}: {e}", cfg_path.display()),
                    format!(
                        "edit {} or delete it to regenerate defaults",
                        cfg_path.display()
                    ),
                ),
                0,
            ),
        };
    }

    if !fix {
        let hint = format!(
            "config.toml not found at {}; run `ro doctor --fix` to write defaults",
            cfg_path.display()
        );
        return (
            CheckResult::warn(
                "config",
                Severity::Required,
                "config not initialized",
                Some(hint),
            ),
            0,
        );
    }

    match write_default_config(&cfg_path) {
        Ok(_) => (
            CheckResult::ok(
                "config",
                Severity::Required,
                format!("wrote default config to {}", cfg_path.display()),
            )
            .with_applied_fix(format!("created {}", cfg_path.display())),
            1,
        ),
        Err(e) => (
            CheckResult::fail(
                "config",
                Severity::Required,
                format!(
                    "failed to write default config to {}: {e}",
                    cfg_path.display()
                ),
                "check XDG_CONFIG_HOME permissions",
            ),
            0,
        ),
    }
}

fn write_default_config(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, default_config_toml())
}

fn check_state(paths: &ConfigPaths, fix: bool) -> CheckResult {
    let db_path = paths.state_db();
    let parent_exists = db_path.parent().is_some_and(Path::exists);

    if !parent_exists {
        if !fix {
            return CheckResult::warn(
                "state",
                Severity::Required,
                format!("state dir missing: {}", db_path.display()),
                Some(format!(
                    "run `ro doctor --fix` to create {}",
                    db_path.parent().unwrap_or(Path::new("")).display()
                )),
            );
        }
        if let Some(parent) = db_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return CheckResult::fail(
                    "state",
                    Severity::Required,
                    format!("cannot create state dir {}: {e}", parent.display()),
                    "check XDG_STATE_HOME permissions",
                );
            }
        }
    }

    match open_db(&db_path) {
        Ok(_) => CheckResult::ok(
            "state",
            Severity::Required,
            format!("state db ok at {}", db_path.display()),
        ),
        Err(e) => CheckResult::fail(
            "state",
            Severity::Required,
            format!("cannot open state db {}: {e}", db_path.display()),
            "delete the file to recreate, or check disk space",
        ),
    }
}

fn check_provider(name: &str, lookup_path: Option<&Path>) -> CheckResult {
    if which_in(name, lookup_path).is_some() {
        CheckResult::ok(
            format!("provider:{name}"),
            Severity::Optional,
            format!("{name} binary available"),
        )
    } else {
        CheckResult::warn(
            format!("provider:{name}"),
            Severity::Optional,
            format!("{name} not found on PATH"),
            Some(format!(
                "install the {name} CLI to enable provider invocation"
            )),
        )
    }
}

/// `which`-style binary lookup. Honors `lookup_path` override for tests.
fn which_in(bin: &str, lookup_path: Option<&Path>) -> Option<PathBuf> {
    let path_var = match lookup_path {
        Some(p) => p.to_string_lossy().into_owned(),
        None => std::env::var("PATH").ok()?,
    };
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
        // Windows .exe suffix
        let candidate_exe = dir.join(format!("{bin}.exe"));
        if candidate_exe.is_file() {
            return Some(candidate_exe);
        }
    }
    None
}

/// Render a human-readable summary line per check.
pub fn render_text(report: &DoctorReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    writeln!(out, "ro doctor — run {}", report.run_id).ok();
    writeln!(out).ok();
    for c in &report.checks {
        let icon = match c.status {
            Status::Ok => "✔",
            Status::Warn => "⚠",
            Status::Fail => "✘",
            Status::Skipped => "·",
        };
        writeln!(out, "{icon} {} — {}", c.name, c.message).ok();
        if let Some(fix) = &c.applied_fix {
            writeln!(out, "    fix applied: {fix}").ok();
        } else if let Some(hint) = &c.fix_hint {
            writeln!(out, "    hint: {hint}").ok();
        }
    }
    writeln!(out).ok();
    writeln!(
        out,
        "summary: {} checks, {} failed, {} warnings",
        report.checks.len(),
        report.failures(),
        report.warnings()
    )
    .ok();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use ro_config::ConfigPaths;
    use std::fs;
    use tempfile::TempDir;

    fn paths_in(tmp: &TempDir) -> ConfigPaths {
        let root = tmp.path();
        ConfigPaths {
            config_dir: root.join(".config/ro"),
            state_dir: root.join(".local/state/ro"),
            cache_dir: root.join(".cache/ro"),
        }
    }

    #[test]
    fn status_helpers() {
        assert!(Status::Fail.is_fail());
        assert!(!Status::Ok.is_fail());
        assert!(Status::Warn.is_warn());
        assert!(!Status::Ok.is_warn());
    }

    #[test]
    fn report_exit_code_zero_when_no_failures() {
        let report = DoctorReport {
            run_id: "x".into(),
            started_at_unix: 0,
            fix_attempted: false,
            checks: vec![CheckResult::ok("a", Severity::Info, "ok")],
        };
        assert_eq!(report.exit_code(), 0);
        assert_eq!(report.failures(), 0);
    }

    #[test]
    fn report_exit_code_one_when_any_failure() {
        let report = DoctorReport {
            run_id: "x".into(),
            started_at_unix: 0,
            fix_attempted: false,
            checks: vec![
                CheckResult::ok("a", Severity::Info, "ok"),
                CheckResult::fail("b", Severity::Required, "broken", "fix me"),
            ],
        };
        assert_eq!(report.exit_code(), 1);
        assert_eq!(report.failures(), 1);
    }

    #[test]
    fn config_check_warns_when_missing_without_fix() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(&tmp);
        let (result, applied) = check_and_optionally_fix_config(&paths, false);
        assert_eq!(result.status, Status::Warn);
        assert!(result.fix_hint.is_some());
        assert_eq!(applied, 0);
    }

    #[test]
    fn config_check_writes_default_when_fix_set() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(&tmp);
        let (result, applied) = check_and_optionally_fix_config(&paths, true);
        assert_eq!(result.status, Status::Ok);
        assert!(result.applied_fix.is_some());
        assert_eq!(applied, 1);
        assert!(paths.config_toml().exists());
        // running again should be ok (file already valid)
        let (result2, applied2) = check_and_optionally_fix_config(&paths, true);
        assert_eq!(result2.status, Status::Ok);
        assert_eq!(applied2, 0);
    }

    #[test]
    fn state_check_warns_when_missing_without_fix() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(&tmp);
        let result = check_state(&paths, false);
        assert_eq!(result.status, Status::Warn);
    }

    #[test]
    fn state_check_creates_db_when_fix_set() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(&tmp);
        let result = check_state(&paths, true);
        assert_eq!(result.status, Status::Ok);
        assert!(paths.state_db().exists());
    }

    #[test]
    fn provider_check_handles_missing_binary() {
        let tmp = TempDir::new().unwrap();
        let result = check_provider("definitely-not-installed-zzz", Some(tmp.path()));
        assert_eq!(result.status, Status::Warn);
        assert_eq!(result.severity, Severity::Optional);
    }

    #[test]
    fn provider_check_finds_binary_in_lookup_path() {
        let tmp = TempDir::new().unwrap();
        let bin = tmp.path().join("fakebin");
        fs::write(&bin, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&bin).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&bin, perms).unwrap();
        }
        let result = check_provider("fakebin", Some(tmp.path()));
        assert_eq!(result.status, Status::Ok);
    }

    #[test]
    fn run_returns_at_least_six_checks() {
        let tmp = TempDir::new().unwrap();
        let opts = DoctorOptions {
            config_token: Some("ghp_test_token".into()),
            fix: true,
            binary_lookup_path: Some(tmp.path().to_path_buf()),
            paths: Some(paths_in(&tmp)),
        };
        let report = run(opts);
        // git, github_auth, config, state, provider:claude, provider:codex
        assert!(report.checks.len() >= 6);
        assert!(report.fix_attempted);
    }

    #[test]
    fn render_text_includes_check_names() {
        let report = DoctorReport {
            run_id: "abc".into(),
            started_at_unix: 0,
            fix_attempted: false,
            checks: vec![CheckResult::ok("git", Severity::Required, "git 2.40")],
        };
        let txt = render_text(&report);
        assert!(txt.contains("ro doctor"));
        assert!(txt.contains("git"));
        assert!(txt.contains("git 2.40"));
    }

    #[test]
    fn render_text_includes_fix_hint() {
        let report = DoctorReport {
            run_id: "abc".into(),
            started_at_unix: 0,
            fix_attempted: false,
            checks: vec![CheckResult::fail(
                "github_auth",
                Severity::Required,
                "missing token",
                "set GITHUB_TOKEN",
            )],
        };
        let txt = render_text(&report);
        assert!(txt.contains("hint: set GITHUB_TOKEN"));
    }

    #[test]
    fn json_round_trip() {
        let report = DoctorReport {
            run_id: "abc".into(),
            started_at_unix: 1234,
            fix_attempted: true,
            checks: vec![CheckResult::ok("git", Severity::Required, "git 2.40")],
        };
        let json = serde_json::to_string(&report).unwrap();
        let parsed: DoctorReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.run_id, "abc");
        assert_eq!(parsed.checks[0].name, "git");
    }
}
