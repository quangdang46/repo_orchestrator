//! `ro doctor` — diagnose installation health.
//!
//! Pure detection by default. Mutations only happen when `--fix` is set, and
//! they are **additive**: `--fix` writes missing sections and never deletes
//! what it did not put there. An earlier version of this file claimed every
//! mutation backed up the prior state to `<state_dir>/doctor/runs/<run-id>/`.
//! No such code existed.
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
//! human-readable text or JSON.

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
    /// A token supplied on the command line, for a one-off run.
    ///
    /// Deliberately not stored and not a config layer: a token on argv is
    /// visible to every process on the machine via ps.
    #[allow(dead_code)]
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
    checks.push(check_github_auth());
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

    // Per-repository write access, one row each. This is the check that turns a
    // 403 discovered at the push step — after four other repos have already
    // been pushed — into a warning before any of them were touched.
    if let Ok(conn) = ro_state::open_db(&paths.state_dir.join("state.db")) {
        checks.extend(check_repo_write_access(&conn));
    }

    // Previously `let _ = applied_fix_count; // reserved for future scoring` —
    // a count computed, named, and dropped. It is now reported, because a
    // `--fix` run that silently changed three things and said nothing is the
    // behaviour users are asked to trust with a repair command.
    if applied_fix_count > 0 {
        eprintln!("Applied {applied_fix_count} config fix(es).");
    }

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

/// Probe write access for every tracked repo.
///
/// One [`CheckResult`] per repo rather than a single rolled-up verdict,
/// because the whole point is to say *which* repository will fail. A doctor
/// that stops at the first bad row tells the user one thing when there are
/// three, and the three have nothing to do with each other.
///
/// A repo that cannot be probed is reported, not skipped: "unresolvable" and
/// "resolved but forbidden" both make a push fail and need different fixes, so
/// neither is allowed to disappear.
///
/// No token means no probes at all — there is nothing to probe *with*, and
/// reporting twenty `Unresolvable` rows would bury the single fact that
/// matters.
fn check_repo_write_access(conn: &ro_state::Connection) -> Vec<CheckResult> {
    let Ok(token) = ro_github::auth::discover_token("env") else {
        return vec![CheckResult::warn(
            "github_auth",
            Severity::Optional,
            "skipping the per-repository write check: no token in the environment",
            None,
        )];
    };

    let repos = match ro_sync::manage::list(conn, None) {
        Ok(r) => r,
        Err(e) => {
            return vec![CheckResult::fail(
                "repos",
                Severity::Required,
                format!("could not read the registry: {e}"),
                "run `ro list` to see whether the state database is readable".to_string(),
            )];
        }
    };
    if repos.is_empty() {
        return vec![CheckResult::ok(
            "repos",
            Severity::Optional,
            "no repos tracked yet — run `ro add` first",
        )];
    }

    repos
        .iter()
        .map(|repo| {
            // `owner/name`, not `Display`, which appends " as <alias>" — a
            // string that belongs in a listing, not in a sentence about a URL.
            let slug = format!("{}/{}", repo.owner, repo.name);
            let access =
                ro_github::permissions::probe_write_access(&token, &repo.owner, &repo.name, None);
            let name = format!("repo:{slug}");
            match &access {
                ro_github::permissions::WriteAccess::Granted { .. } => {
                    CheckResult::ok(&name, Severity::Optional, access.render())
                }
                ro_github::permissions::WriteAccess::Forbidden { login, .. } => CheckResult::fail(
                    &name,
                    Severity::Required,
                    format!("{} — a push to this repo will be refused", access.render()),
                    format!(
                        "the credential is {login}; ask a repository admin for write access, \
                             or point this repo at another account with \
                             `ro config set repos.{}.credential_ref env:OTHER_VAR`",
                        slug
                    ),
                ),
                ro_github::permissions::WriteAccess::Unresolvable { reason } => {
                    // A distinct verdict from Forbidden on purpose. "The token
                    // did not work" and "the token worked and was still
                    // refused" look the same from the outside and are fixed by
                    // opposite actions.
                    CheckResult::fail(
                        &name,
                        Severity::Required,
                        format!("{} — could not determine write access", access.render()),
                        format!("{reason}; this is a credential problem, not a permission one"),
                    )
                }
            }
        })
        .collect()
}

/// Report whether a GitHub token is available, and where it came from.
///
/// Both sources are probed and named rather than one being tried until
/// something works. `auto` used to do exactly that, with every error
/// swallowed — so a run could push with a credential nobody chose and the only
/// evidence was that it worked. Naming the source is what turns "it found a
/// token" into something a user can check against what they expected.
fn check_github_auth() -> CheckResult {
    let env_result = discover_token("env");
    let gh_result = if env_result.is_err() {
        discover_token("gh").err()
    } else {
        None
    };

    match (env_result, gh_result) {
        (Ok(_), _) => CheckResult::ok(
            "github_auth",
            Severity::Required,
            "GitHub token found: GH_TOKEN or GITHUB_TOKEN in the environment",
        ),
        (Err(_), None) => CheckResult::ok(
            "github_auth",
            Severity::Optional,
            "no token in the environment, but `gh auth token` works — ro will use it",
        ),
        (Err(e), Some(_)) => CheckResult::fail(
            "github_auth",
            Severity::Required,
            format!("no GitHub token available: {e}"),
            "set GH_TOKEN (preferred) or GITHUB_TOKEN, or run `gh auth login`. \
             For a per-repo credential, set `credential_ref` on the row to \
             'env:VAR_NAME' so the variable is named for that repo specifically.",
        ),
    }
}

/// Sections the current schema expects. `--fix` adds whichever are missing.
///
/// A *section*, not a key: every field inside already defaults, so creating an
/// empty `[auth]` is enough for the schema to see the table and for the user to
/// discover it exists by opening their own file.
const EXPECTED_SECTIONS: &[&str] = &["core", "auth", "git", "safety"];

/// Add the sections a config predating them is missing, leaving everything
/// else alone.
///
/// The asymmetry is deliberate and is the whole point: `--fix` **adds** and
/// never **removes**. An unknown table is a setting from a newer ro, a leftover
/// from an older one, or a comment-adjacent note the user put there. Deleting
/// a user's file contents is not a repair, and a repair command that loses data
/// is worse than no repair command — so legacy tables stay, and the user is
/// told which ones were left alone so they can decide.
fn upgrade_existing_config(cfg_path: &Path) -> (CheckResult, usize) {
    let Ok(raw) = std::fs::read_to_string(cfg_path) else {
        return (
            CheckResult::fail(
                "config",
                Severity::Required,
                format!("cannot read {}", cfg_path.display()),
                "check the file's permissions".to_string(),
            ),
            0,
        );
    };
    let Ok(mut doc) = raw.parse::<toml_edit::DocumentMut>() else {
        // load_config already rejected it, so this is unreachable in practice;
        // report the parse problem rather than panicking on it.
        return (
            CheckResult::fail(
                "config",
                Severity::Required,
                format!("cannot parse {}", cfg_path.display()),
                "fix the TOML syntax by hand".to_string(),
            ),
            0,
        );
    };

    let mut added = 0usize;
    for section in EXPECTED_SECTIONS {
        if doc.get(section).is_none() {
            let mut table = toml_edit::Table::new();
            table.set_implicit(false);
            doc[*section] = toml_edit::Item::Table(table);
            added += 1;
        }
    }

    if added > 0 {
        if let Err(e) = std::fs::write(cfg_path, doc.to_string()) {
            return (
                CheckResult::fail(
                    "config",
                    Severity::Required,
                    format!("could not write {}: {e}", cfg_path.display()),
                    "check the file's permissions".to_string(),
                ),
                0,
            );
        }
    }

    // Name the tables left alone. Silent divergence between what is in the file
    // and what ro reads is the thing a user cannot debug from ro's side.
    let untouched: Vec<&str> = doc
        .as_table()
        .iter()
        .map(|(k, _)| k)
        .filter(|k| !EXPECTED_SECTIONS.contains(k))
        .collect();
    let mut detail = format!("config valid at {}", cfg_path.display());
    if added > 0 {
        detail.push_str(&format!("; added {added} missing section(s)"));
    }
    if !untouched.is_empty() {
        detail.push_str(&format!(
            "; left unrecognised table(s) in place: {}",
            untouched.join(", ")
        ));
    }

    (CheckResult::ok("config", Severity::Required, detail), added)
}

fn check_and_optionally_fix_config(paths: &ConfigPaths, fix: bool) -> (CheckResult, usize) {
    let cfg_path = paths.config_toml();
    if cfg_path.exists() {
        let loaded = load_config(&cfg_path);
        if loaded.is_ok() {
            // A file that already parses is a candidate for an *upgrade*, not
            // just for a verdict. Without this, every user keeps dead
            // `[mcp]`/`[jobs]`/`[review]` config indefinitely and `validate`
            // keeps checking keys the schema no longer models.
            if !fix {
                return (
                    CheckResult::ok(
                        "config",
                        Severity::Required,
                        format!("config valid at {}", cfg_path.display()),
                    ),
                    0,
                );
            }
            return upgrade_existing_config(&cfg_path);
        }
        let Err(e) = loaded else {
            unreachable!("the Ok arm returns above")
        };
        return (
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
        );
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
    if ro_git::which_in(name, lookup_path).is_some() {
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

    /// `--fix` on a config that predates the current schema adds what is
    /// missing. Without this, every existing user keeps dead `[mcp]`/`[jobs]`/
    /// `[review]` config indefinitely and `validate` keeps checking keys the
    /// schema no longer models.
    #[test]
    fn fix_adds_a_missing_section_to_an_existing_config() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(&tmp);
        paths.ensure_all().unwrap();
        let cfg_path = paths.config_toml();
        fs::write(&cfg_path, "[core]\nlayout = \"flat\"\n").unwrap();

        let (result, added) = check_and_optionally_fix_config(&paths, true);
        assert_eq!(
            added, 3,
            "the fixture has [core] only, so auth, git, safety"
        );
        assert_eq!(
            result.status,
            Status::Ok,
            "an upgrade must not report a failure"
        );

        let after = fs::read_to_string(&cfg_path).unwrap();
        for section in ["[auth]", "[git]", "[safety]"] {
            assert!(
                after.contains(section),
                "{section} should have been added, got:\n{after}"
            );
        }
        assert!(after.contains("layout = \"flat\""), "got:\n{after}");
    }

    /// The asymmetry that matters. `--fix` adds; it never removes. An unknown
    /// table is a setting from a newer ro, a leftover from an older one, or
    /// something the user put there — and a repair command that deletes a
    /// user's file contents is worse than no repair command.
    #[test]
    fn fix_leaves_a_legacy_table_in_place_and_names_it() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(&tmp);
        paths.ensure_all().unwrap();
        let cfg_path = paths.config_toml();
        fs::write(
            &cfg_path,
            "[core]\nlayout = \"flat\"\n\n[review]\nauto_approve = \"low\"\n",
        )
        .unwrap();

        let (result, _) = check_and_optionally_fix_config(&paths, true);

        let after = fs::read_to_string(&cfg_path).unwrap();
        assert!(
            after.contains("[review]") && after.contains("auto_approve"),
            "a legacy table must survive --fix, got:\n{after}"
        );
        // And it is reported, so the user knows it is being ignored rather
        // than quietly honoured.
        assert!(
            result.message.contains("review"),
            "the check must name the table it left alone, got: {}",
            result.message
        );
    }

    /// Without `--fix` the file is only judged, never written. A doctor that
    /// repairs by default is a doctor nobody can run to find out what is wrong.
    #[test]
    fn no_fix_never_writes() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(&tmp);
        paths.ensure_all().unwrap();
        let cfg_path = paths.config_toml();
        let original = "[core]\nlayout = \"flat\"\n";
        fs::write(&cfg_path, original).unwrap();

        let (result, added) = check_and_optionally_fix_config(&paths, false);
        assert_eq!(result.status, Status::Ok);
        assert_eq!(added, 0);
        assert_eq!(fs::read_to_string(&cfg_path).unwrap(), original);
    }

    /// A file that already has every expected section is a no-op, and reports
    /// a count of zero rather than a fix that changed nothing.
    #[test]
    fn fix_on_a_current_config_changes_nothing() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(&tmp);
        paths.ensure_all().unwrap();
        let cfg_path = paths.config_toml();
        fs::write(&cfg_path, ro_config::paths::default_config_toml()).unwrap();
        let before = fs::read_to_string(&cfg_path).unwrap();

        let (_, added) = check_and_optionally_fix_config(&paths, true);
        assert_eq!(added, 0);
        assert_eq!(fs::read_to_string(&cfg_path).unwrap(), before);
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
