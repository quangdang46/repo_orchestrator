//! `ro doctor` — diagnose installation health.
//!
//! Pure detection by default. Mutations only happen when `--fix` is set, and
//! they are **additive**: `--fix` writes missing sections and never deletes
//! what it did not put there. An earlier version of this file claimed every
//! mutation backed up the prior state to `<state_dir>/doctor/runs/<run-id>/`.
//! No such code existed.
//!
//! There is exactly one exception, and it is a copy rather than a deletion:
//! a `state.db` that is not a database is saved to `state.db.bak` and
//! replaced with an empty one. That is the only repair here that throws
//! anything away, so it is the only one that takes a backup first — and the
//! only one that refuses when the backup could not be written. See
//! [`repair_state_db`].
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
    /// The doctor's own 0/1, deliberately not the fleet table.
    ///
    /// A failed environment check is not a partial run, and reading it as
    /// one is how a green run gets retried for the wrong reason. The
    ///  probes can never move it.
    pub fn exit_code(&self) -> u8 {
        if self.failures() > 0 {
            crate::exit::EX_DOCTOR_FAILED
        } else {
            crate::exit::EX_OK
        }
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
    /// Where `--fix` writes a repaired state database.
    ///
    /// `None` means "the path `paths` names", which is what the CLI passes
    /// and what every test that drives `run()` wants. A test that needs to
    /// assert on the *file* — that a backup was written, that a corrupt
    /// database was replaced rather than edited in place — passes the path
    /// it already holds.
    pub state_db_path: Option<PathBuf>,
}

/// The state database this run is acting on.
///
/// One place, so the check and the repair cannot disagree about which file
/// they are talking about — a check that judges one path and a fix that
/// writes another is a repair command that reports on work it did not do.
fn state_db_path(paths: &ConfigPaths, opts: &DoctorOptions) -> PathBuf {
    opts.state_db_path
        .clone()
        .unwrap_or_else(|| paths.state_db())
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

    checks.push(check_github_auth(&paths));
    let (cfg_check, applied_fix_count) = check_and_optionally_fix_config(&paths, opts.fix);
    checks.push(cfg_check);

    let (state_check, state_fixes) = check_and_optionally_fix_state(&paths, &opts);
    checks.push(state_check);
    let applied_fix_count = applied_fix_count + state_fixes;

    // Per-repository write access, one row each. This is the check that turns a
    // 403 discovered at the push step — after four other repos have already
    // been pushed — into a warning before any of them were touched.
    //
    // Only when the database is already there. `open_db` **creates** it —
    // `ro_state::open_db` runs `create_dir_all` on the parent and then
    // `Connection::open` — so probing a missing database with it silently
    // manufactures a 160KB `state.db` and then reports "state db ok" about
    // the file it just made. A diagnostic run that writes to the user's
    // machine is not a diagnostic run, and the "ok" is worse than the write:
    // it tells the user their state is healthy when the only thing that
    // happened is that ro created it.
    if paths.state_db().exists() {
        if let Ok(conn) = ro_state::open_db(&paths.state_db()) {
            // `github.host` is read here. The `base_uri` was a hardcoded
            // `None`, so every probe went to `github.com` whatever the
            // config said — a workspace-wide grep for `github.host` found it
            // only in comments and tests. A user on GitHub Enterprise was
            // diagnosed against the public API and told their token was
            // rejected (HTTP 401) when it had never been offered to the
            // right host. `ro_github::auth::build_client` already takes
            // `Option<&str>` and builds `https://{host}/api/v3` for a host
            // that is not `github.com`; it was simply never given one.
            let host = github_host(&paths);
            checks.extend(check_repo_write_access(&conn, host.as_deref()));
        }
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

/// The credential one row will actually be pushed with, and a name for it.
///
/// A row may carry its own `credential_ref` — `env:VAR` or `keychain:NAME` —
/// and the push path in `ship` honours it. The write check has to probe with
/// the *same* credential, or it answers a question nobody asked: a row with a
/// perfectly good per-repo token, probed with the machine's ambient token,
/// reports `write: NO` forever. That is a permanent false alarm, and it is
/// worse than no check because it is believed.
///
/// The name travels with the token for the same reason. "my token has no
/// access" and "this repo points at a credential I do not have" look
/// identical in a `write: NO` row and are fixed by opposite actions, so the
/// message has to say which credential it asked.
struct RowCredential {
    token: ro_github::AuthToken,
    /// How to refer to this credential in a message. The literal
    /// `credential_ref` for a row that names one, and a description of the
    /// ambient fallback for a row that does not.
    label: String,
}

/// The token used for a row that names no credential: whatever the machine's
/// environment offers.
const AMBIENT_LABEL: &str = "the machine's own token (GH_TOKEN/GITHUB_TOKEN)";

/// Resolve the credential for one row, or explain why it cannot be resolved.
///
/// A malformed reference and an unset variable are both failures, and both are
/// reported *without* falling back to the ambient token — the same
/// no-silent-fallback rule the push path follows. A repo whose row says
/// `env:WORK_TOKEN` and whose `WORK_TOKEN` is unset must not be probed as
/// somebody else, or the check reports a verdict for a push that will never
/// happen that way.
fn resolve_row_credential(
    repo: &ro_sync::manage::TrackedRepo,
    ambient: Option<&ro_github::AuthToken>,
) -> Result<RowCredential, String> {
    let Some(reference) = repo
        .credential_ref
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    else {
        // No reference configured is not an error: a repo whose SSH key is
        // already correct needs no configuration, and the push path treats
        // `None` the same way.
        return match ambient {
            Some(token) => Ok(RowCredential {
                token: ro_github::AuthToken::new(token.as_str().to_string()),
                label: AMBIENT_LABEL.to_string(),
            }),
            None => Err(format!(
                "this row names no credential ({AMBIENT_LABEL}) and neither is available"
            )),
        };
    };

    // Parsed first, so a malformed reference is a clear error rather than a
    // fall-through to the machine's own credential. The `CredentialRef` error
    // already explains the two accepted forms, so this only attributes it to
    // the row rather than restating "invalid reference".
    let parsed: ro_core::CredentialRef = reference
        .parse()
        .map_err(|e| format!("this row's credential reference is invalid — {e}"))?;
    let resolved = ro_core::credential_resolve::resolve(&parsed)
        .map_err(|e| format!("credential reference {reference:?}: {e}"))?;

    Ok(RowCredential {
        token: ro_github::AuthToken::new(resolved.expose().to_string()),
        label: reference.to_string(),
    })
}

/// Is this remote one the permissions probe can actually ask?
///
/// `false` for a host that is neither the public GitHub nor the configured
/// `github.host`. A row on such a remote is not probed, because the check
/// cannot validate a credential for a remote it does not talk to — see the
/// call site.
///
/// The host is read out of the **port** as well as the name, because a test
/// and a GitHub Enterprise both live on `example.com:PORT` and a name-only
/// comparison would probe every one of them.
fn probes_github_api(remote: &str, base_uri: Option<&str>) -> bool {
    let Some((_, host)) = remote.split_once("://") else {
        return true;
    };
    // The public API is always one of these two; `github.host` adds the
    // Enterprise case on top.
    if matches!(host, "github.com" | "api.github.com") {
        return true;
    }
    match base_uri {
        Some(h) => {
            let configured = h.split("://").last().unwrap_or(h).trim_end_matches('/');
            host == configured
        }
        None => false,
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
/// The ambient token is only a *fallback*. A row that names its own
/// credential is probed with that, and a row whose reference cannot be
/// resolved is a failure naming the reference — never quietly re-probed with
/// the machine's token. The "no token at all" collapse to a single warning is
/// kept for the genuinely-empty case (no ambient token and no row naming one),
/// so a run with nothing to probe with reports one fact rather than twenty
/// `Unresolvable` rows; rows that *do* carry a credential are still probed.
fn check_repo_write_access(
    conn: &ro_state::Connection,
    base_uri: Option<&str>,
) -> Vec<CheckResult> {
    let ambient = ro_github::auth::discover_token("env").ok();

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

    // Nothing to probe with, and no row that could supply its own credential.
    // One warning, not twenty `Unresolvable` rows: the single fact that matters
    // is that there is no credential, and a fleet of identical failures buries
    // it. Rows that name a credential are still probed below — a per-repo
    // credential is exactly the case where the machine's own token is absent.
    if ambient.is_none() && repos.iter().all(|r| r.credential_ref.is_none()) {
        return vec![CheckResult::warn(
            "github_auth",
            Severity::Optional,
            "skipping the per-repository write check: no token in the environment",
            None,
        )];
    }

    let mut results = Vec::new();
    let mut skipped_without_credential = 0usize;

    for repo in &repos {
        // `owner/name`, not `Display`, which appends " as <alias>" — a
        // string that belongs in a listing, not in a sentence about a URL.
        let slug = format!("{}/{}", repo.owner, repo.name);
        let name = format!("repo:{slug}");

        // A row whose remote is **not** the GitHub API this check talks to is
        // not probed at all.
        //
        // The check asks "will a push to this repo be refused?", and the only
        // thing it can ask is the GitHub permissions API. For a repo on a
        // self-hosted HTTP remote the row's credential is perfectly valid for
        // that remote — `ro ship` over the same row authenticates and pushes —
        // and completely meaningless to the API, which answers 401. So the
        // check reported a **permanent** "token rejected by GitHub (HTTP 401)"
        // for a working token, on every run, with no way for the user to
        // clear it. The git server saw zero requests from doctor.
        //
        // Skipped, not failed, and it says why: the honest answer is "this
        // check does not apply to this row", which is a fact about the check
        // rather than about the credential.
        if let Some(remote) = ro_sync::manage::extraheader_host(&repo.clone_url)
            && !probes_github_api(&remote, base_uri)
        {
            results.push(CheckResult::ok(
                &name,
                Severity::Optional,
                format!(
                    "write: not checked — this repo's remote is {remote}, not the GitHub API, \
                     so its credential cannot be validated here",
                ),
            ));
            continue;
        }

        // The credential this row will actually be pushed with. A row that
        // names one is probed with it; a row that does not falls back to
        // the machine's own. A reference that cannot be resolved is a
        // failure, not a fall-back.
        let credential = match resolve_row_credential(repo, ambient.as_ref()) {
            Ok(c) => c,
            Err(why) => {
                // A row that names a credential which cannot be resolved is a
                // real failure: the push will fail the same way, and the fix
                // is to set the variable or repair the reference.
                if repo.credential_ref.is_some() {
                    results.push(CheckResult::fail(
                        &name,
                        Severity::Required,
                        format!("{why} — could not determine write access"),
                        format!(
                            "{why}; this is a credential problem, not a permission one. \
                             Fix the reference, or set the variable, before pushing {slug}."
                        ),
                    ));
                } else {
                    // No reference and no ambient token: this row has nothing
                    // to probe with. Counted and reported once below, so a
                    // fleet of them does not become a wall of identical
                    // failures.
                    skipped_without_credential += 1;
                }
                continue;
            }
        };

        let access = ro_github::permissions::probe_write_access(
            &credential.token,
            &repo.owner,
            &repo.name,
            base_uri,
        );
        results.push(match &access {
            ro_github::permissions::WriteAccess::Granted { .. } => CheckResult::ok(
                &name,
                Severity::Optional,
                format!("{} — credential: {}", access.render(), credential.label),
            ),
            ro_github::permissions::WriteAccess::Forbidden { login, .. } => CheckResult::fail(
                &name,
                Severity::Required,
                format!(
                    "{} — a push to this repo will be refused — credential: {}",
                    access.render(),
                    credential.label
                ),
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
                    format!(
                        "{} — could not determine write access — credential: {}",
                        access.render(),
                        credential.label
                    ),
                    format!("{reason}; this is a credential problem, not a permission one"),
                )
            }
        });
    }

    if skipped_without_credential > 0 {
        results.push(CheckResult::warn(
            "github_auth",
            Severity::Optional,
            format!(
                "skipped the write check for {skipped_without_credential} repo(s) with no \
                 credential_ref and no token in the environment"
            ),
            None,
        ));
    }

    results
}

/// Report whether a GitHub token is available, and where it came from.
///
/// Both sources are probed and named rather than one being tried until
/// something works. `auto` used to do exactly that, with every error
/// swallowed — so a run could push with a credential nobody chose and the only
/// evidence was that it worked. Naming the source is what turns "it found a
/// token" into something a user can check against what they expected.
/// The `github.host` the doctor should probe, or `None` for github.com.
///
/// `None` is what `build_client` wants for the public API, so a default
/// install takes the same path it always did.
fn github_host(paths: &ro_config::ConfigPaths) -> Option<String> {
    let cfg = ro_config::load_config(&paths.config_toml()).unwrap_or_default();
    match cfg.github.host.as_str() {
        "" | "github.com" | "api.github.com" => None,
        other => Some(other.to_string()),
    }
}

/// The `github.auth` strategy, mapped onto what `discover_token` implements.
///
/// `discover_token` accepts `env` and `gh` and **rejects** `auto` and
/// `config-token` with the sentence "a strategy that falls back silently can
/// push with a credential nobody chose". `validate` accepts all four. So
/// the two the validator permits and the discoverer refuses are mapped here
/// rather than passed through naively: `auto` is genuinely "try, and say
/// which one answered" and `config-token` is the per-row `credential_ref`
/// path, which the per-repo probe below already covers.
fn github_auth_strategy(configured: &str) -> &'static str {
    match configured {
        "gh" => "gh",
        // `auto` and `config-token` both mean "more than one place" or
        // "not the ambient env"; neither is a single source, so both are
        // probed in order and the source is named.
        _ => "env",
    }
}

fn check_github_auth(paths: &ro_config::ConfigPaths) -> CheckResult {
    // `github.auth` is read here. It was hardcoded to `env` then `gh`, so
    // all four settings produced byte-identical output — and a user who
    // wrote `github.auth = "env"` **specifically to pin the credential
    // source** got a `gh` credential anyway, which is exactly the silent
    // fallback `discover_token`'s own error text says ro forbids.
    let configured = ro_config::load_config(&paths.config_toml())
        .unwrap_or_default()
        .github
        .auth;
    let pinned = matches!(configured.as_str(), "env" | "gh");
    let env_result = discover_token(github_auth_strategy(&configured));
    // A **pinned** strategy is not allowed to fall through. An unpinned one
    // still gets the second source, and the result says which answered.
    let gh_result = if env_result.is_err() && !pinned {
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
/// The sections a config is expected to carry.
///
/// This list is the set of tables something **reads**, and it has to be
/// maintained as one: a section here that nothing reads makes `--fix` write
/// an empty table into the user's file that they then set values into, and
/// those values do nothing. A section missing here means `--fix` does not
/// add a table that would have been used.
///
/// It was both wrong at once until now: it listed `git` and `safety` (read by
/// nothing) and omitted `github`, `agent` and `identity` (all read).
const EXPECTED_SECTIONS: &[&str] = &["core", "auth", "github", "agent", "identity"];

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
        Ok(_) => {
            // Writing the template is not the same as writing a *complete*
            // config. The template leaves `[identity]` commented out because
            // most installs never need it, so a fresh `--fix` left a file
            // that the very next `--fix` wanted to change again. A repair
            // command that needs a second run to finish is one a user stops
            // running, and the check then reports a config it would still
            // "fix" on the next invocation.
            let (_, added) = upgrade_existing_config(&cfg_path);
            (
                CheckResult::ok(
                    "config",
                    Severity::Required,
                    format!("wrote default config to {}", cfg_path.display()),
                )
                .with_applied_fix(format!("created {}", cfg_path.display())),
                1 + added,
            )
        }
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

/// Is this a directory of files that is not a database?
///
/// Walked rather than string-matched, because the only things `ro` itself
/// ever writes here are `state.db`, SQLite's `state.db-wal` / `state.db-shm`
/// sidecars, and the `state.db.bak` / `state.db.N.bak` backups
/// [`back_up_state_db`] writes — all of them ro's files in ro's directory.
/// A state directory containing anything else (a checkouted tree someone
/// pointed `--state-dir` at, a `notes.txt`) is not ro's, and neither its
/// contents nor anything above it is touched on the strength of a `--fix`.
///
/// The backups are the reason this list is not just the three SQLite names:
/// the second `--fix` on a still-broken database finds `state.db.bak` in
/// the directory, and a check that called that a stranger would refuse to
/// write the numbered backup it is about to need — the check would get in
/// the way of the repair it exists to guard.
///
/// The emptiness is a precondition rather than a conclusion: the "refused"
/// message is a claim about what was left alone, and it can only be made
/// for a directory whose entire contents were listed.
fn looks_like_ro_state_dir(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(Result::ok).all(|e| {
        e.file_name()
            .to_str()
            .is_some_and(|n| n == "state.db" || n.starts_with("state.db-") || is_state_db_backup(n))
    })
}

/// Is this the name of a backup [`back_up_state_db`] would have written?
///
/// `state.db.bak` and `state.db.7.bak`, and nothing else. Both come from
/// `Path::with_extension`, which replaces the existing `.db` — so the
/// generated names are `state.db` + `bak` and `state.db` + `7.bak`, and the
/// match is on the suffix first and on what precedes it second.
///
/// Deliberately not a `starts_with`: `state.db.important-things` is a file a
/// user made, and treating it as ro's would be the exact mistake this
/// function exists to prevent.
fn is_state_db_backup(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".bak") else {
        return false;
    };
    match stem.strip_prefix("state.db") {
        Some("") => true,
        Some(number) => {
            !number.is_empty()
                && number.starts_with('.')
                && number[1..].bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
    }
}

/// The one operation in `ro doctor --fix` that destroys anything, and what
/// it does about it.
///
/// `state.db` **is** the registry: every tracked repo, every tag, every
/// recorded run. Recreating a corrupt one is a repair only if the old
/// bytes are still somewhere to be recovered from, so the old file is
/// copied to `state.db.bak` before anything else happens — `copy`, not
/// `rename`, because the original stays exactly where SQLite expects to
/// find it and a half-applied repair is worse than an unapplied one. The
/// copy is verified before it is called a backup: a backup that silently
/// truncated is worse than no backup, because it is reported.
///
/// The directory holding the file is not required to be empty — see
/// [`looks_like_ro_state_dir`], which scopes the check rather than letting
/// a `--fix` run destroy a directory that was never ro's.
fn back_up_state_db(db_path: &Path) -> Result<(PathBuf, u64), String> {
    let bytes =
        std::fs::read(db_path).map_err(|e| format!("reading {}: {e}", db_path.display()))?;
    let mut dest = db_path.with_extension("db.bak");
    // Never overwrite an existing backup. Each attempt is numbered, and a
    // backup that already exists is a user decision this run does not get
    // to make again.
    let mut n = 1u32;
    while dest.exists() {
        dest = db_path.with_extension(format!("db.{n}.bak"));
        n += 1;
    }
    std::fs::copy(db_path, &dest).map_err(|e| {
        format!(
            "backing up {} to {}: {e}",
            db_path.display(),
            dest.display()
        )
    })?;
    let written = std::fs::metadata(&dest)
        .map_err(|e| format!("checking the backup {}: {e}", dest.display()))?
        .len();
    if written != bytes.len() as u64 {
        return Err(format!(
            "the backup {} is {written} bytes but the original was {}",
            dest.display(),
            bytes.len()
        ));
    }
    Ok((dest, written))
}

/// Judge the state database, and repair it when `--fix` was asked for.
///
/// Four states, and each one gets a different answer:
///
/// | state | without `--fix` | with `--fix` |
/// |---|---|---|
/// | missing | warn, "run `--fix`" | created |
/// | opens | ok | ok (an existing file is never rewritten) |
/// | a file that is not a database | fail, and what to do | **backed up, replaced** |
/// | anything else that will not open | fail, left alone | fail, left alone |
///
/// The last row is the one that makes the third safe. "Could not open" is
/// not "corrupt": a full disk, a directory where the file should be, a
/// permission problem and a truncated page header all surface as an error
/// from `open_db`, and only the second is a thing the repair path is
/// allowed to throw away. The reason is therefore looked for in the
/// SQLite error and nowhere else.
fn check_and_optionally_fix_state(
    paths: &ConfigPaths,
    opts: &DoctorOptions,
) -> (CheckResult, usize) {
    let db_path = state_db_path(paths, opts);
    let fix = opts.fix;
    let parent_exists = db_path.parent().is_some_and(Path::exists);

    if !parent_exists {
        if !fix {
            return (
                CheckResult::warn(
                    "state",
                    Severity::Required,
                    format!("state dir missing: {}", db_path.display()),
                    Some(format!(
                        "run `ro doctor --fix` to create {}",
                        db_path.parent().unwrap_or(Path::new("")).display()
                    )),
                ),
                0,
            );
        }
        if let Some(parent) = db_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return (
                    CheckResult::fail(
                        "state",
                        Severity::Required,
                        format!("cannot create state dir {}: {e}", parent.display()),
                        "check XDG_STATE_HOME permissions",
                    ),
                    0,
                );
            }
        }
    }

    match open_db(&db_path) {
        Ok(_) => (
            CheckResult::ok(
                "state",
                Severity::Required,
                format!("state db ok at {}", db_path.display()),
            ),
            0,
        ),
        Err(e) => {
            if !fix {
                return (
                    CheckResult::fail(
                        "state",
                        Severity::Required,
                        format!("cannot open state db {}: {e}", db_path.display()),
                        "delete the file to recreate, or check disk space",
                    ),
                    0,
                );
            }
            repair_state_db(&db_path, &e)
        }
    }
}

/// The one destructive repair `--fix` performs, and everything it refuses to.
///
/// An error that is not "this file is not a database" is never repaired,
/// because the repair throws the file away and the reason is the only
/// evidence of what was wrong with it.
fn repair_state_db(db_path: &Path, error: &anyhow::Error) -> (CheckResult, usize) {
    let not_a_database = error
        .downcast_ref::<ro_state::rusqlite::Error>()
        .and_then(|e| match e {
            ro_state::rusqlite::Error::SqliteFailure(f, _) => Some(f.code),
            _ => None,
        })
        .is_some_and(|c| c == ro_state::rusqlite::ErrorCode::NotADatabase);

    if !not_a_database {
        return (
            CheckResult::fail(
                "state",
                Severity::Required,
                format!(
                    "cannot open state db {}: {error} — `--fix` did not touch it, \
                     because this is not a database that is merely unreadable",
                    db_path.display()
                ),
                "check disk space and permissions; the file was left exactly as it was".to_string(),
            ),
            0,
        );
    }

    // A file that is not a database is a file that is not ours, and a
    // directory that has ever contained anything but `state.db*` is a
    // directory that is not ours. Neither is repaired, and both say so —
    // the refusal names what it saw so the user can tell this from "ro is
    // broken".
    let not_a_repo_state_dir = match db_path.parent() {
        Some(dir) => !looks_like_ro_state_dir(dir),
        None => false,
    };
    if not_a_repo_state_dir {
        // Named, not just counted. A refusal that says "this directory holds
        // files that are not ro's" tells the user a fact they already know
        // (something is wrong); naming the file tells them which one, and
        // that is the difference between a message and a riddle.
        let foreign: Vec<String> = std::fs::read_dir(db_path.parent().unwrap_or(Path::new("")))
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| {
                        n != "state.db" && !n.starts_with("state.db-") && !is_state_db_backup(n)
                    })
                    .collect()
            })
            .unwrap_or_default();
        return (
            CheckResult::fail(
                "state",
                Severity::Required,
                format!(
                    "{} is not a database, and {} holds files that are not ro's \
                     state ({}) — `--fix` refused to replace it, because a \
                     directory that is not ro's is not ro's to destroy",
                    db_path.display(),
                    db_path.parent().unwrap_or(Path::new("")).display(),
                    foreign.join(", ")
                ),
                "point --state-dir at the directory that holds ro's state.db, \
                 or move the offending files out of it yourself"
                    .to_string(),
            ),
            0,
        );
    }

    match back_up_state_db(db_path) {
        Err(why) => (
            CheckResult::fail(
                "state",
                Severity::Required,
                format!(
                    "{} is not a database ({why}); nothing was changed — \
                     ro will not replace a registry whose backup it could not write",
                    db_path.display()
                ),
                "fix the filesystem problem (space, permissions) and re-run \
                 `ro doctor --fix`; the original file is untouched"
                    .to_string(),
            ),
            0,
        ),
        Ok((backup, bytes)) => {
            // Truncate rather than unlink. A `recover` step in between — say
            // a reader that opened the file while this ran — writes into the
            // same inode, so removing the name would leave those writes in
            // a file with no name, and an operation that can lose writes is
            // not one to take a lock-free.
            if let Err(e) = std::fs::File::create(db_path) {
                return (
                    CheckResult::fail(
                        "state",
                        Severity::Required,
                        format!(
                            "{} is not a database, and it could not be replaced: {e} — \
                             the backup at {} is intact, and the original is untouched",
                            db_path.display(),
                            backup.display()
                        ),
                        "check the file's permissions and re-run `ro doctor --fix`".to_string(),
                    ),
                    0,
                );
            }
            match open_db(db_path) {
                Ok(_) => (
                    CheckResult::warn(
                        "state",
                        Severity::Required,
                        format!(
                            "{} was not a database; it was replaced with an empty one",
                            db_path.display()
                        ),
                        Some(format!(
                            "the previous registry is at {}; re-add each repo with `ro add`",
                            backup.display()
                        )),
                    )
                    .with_applied_fix(format!(
                        "backed up the unreadable registry to {} ({bytes} bytes) and \
                         created a fresh state.db — every tracked repo is in that backup, \
                         and `ro add` is how they come back",
                        backup.display()
                    )),
                    2,
                ),
                Err(e) => (
                    CheckResult::fail(
                        "state",
                        Severity::Required,
                        format!(
                            "replacing {} failed: {e} — the original is at {}; \
                             restore it with `mv {} {}` and re-run `ro doctor --fix`",
                            db_path.display(),
                            backup.display(),
                            backup.display(),
                            db_path.display()
                        ),
                        "the backup is intact; move it back over the file and re-run \
                         `ro doctor --fix`"
                            .to_string(),
                    ),
                    0,
                ),
            }
        }
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
    use ro_state::rusqlite::params;
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
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
            added,
            EXPECTED_SECTIONS.len() - 1,
            "the fixture has [core] only, so every other live section is added"
        );
        assert_eq!(
            result.status,
            Status::Ok,
            "an upgrade must not report a failure"
        );

        let after = fs::read_to_string(&cfg_path).unwrap();
        for section in EXPECTED_SECTIONS.iter().filter(|s| **s != "core") {
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

    /// A second `--fix` on an already-repaired config changes nothing.
    ///
    /// The first run writes the shipped template, which leaves `[identity]`
    /// commented out; the second adds that one empty table and stops. What
    /// matters is that the third is a no-op — a repair command that keeps
    /// "repairing" the same file is one a user stops running.
    #[test]
    fn fix_converges_after_one_pass() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(&tmp);
        paths.ensure_all().unwrap();
        let cfg_path = paths.config_toml();
        fs::write(&cfg_path, ro_config::paths::default_config_toml()).unwrap();

        let (_, added) = check_and_optionally_fix_config(&paths, true);
        let after_first = fs::read_to_string(&cfg_path).unwrap();

        let (_, added_again) = check_and_optionally_fix_config(&paths, true);
        assert_eq!(
            added_again, 0,
            "a converged config must not be repaired again; first pass added {added}"
        );
        assert_eq!(fs::read_to_string(&cfg_path).unwrap(), after_first);
    }

    /// `--fix` must never add a table nothing reads.
    ///
    /// The list it walked listed `git` and `safety` and omitted `github`,
    /// `agent` and `identity`, so the repair command was writing empty
    /// tables a user would then fill in — and nothing would read what they
    /// wrote. A repair that produces config you cannot use is worse than no
    /// repair.
    #[test]
    fn fix_only_adds_sections_that_are_actually_read() {
        for gone in [
            "git",
            "jobs",
            "mcp",
            "safety",
            "checkpoint",
            "review",
            "providers",
            "engines",
        ] {
            assert!(
                !EXPECTED_SECTIONS.contains(&gone),
                "--fix would add [{gone}], which nothing reads"
            );
        }
        for live in ["core", "auth", "github", "agent", "identity"] {
            assert!(
                EXPECTED_SECTIONS.contains(&live),
                "--fix would not add [{live}], which is read"
            );
        }
    }

    /// And end to end: a config with none of them gets exactly the live set
    /// and nothing more.
    #[test]
    fn fix_on_a_bare_config_adds_only_live_sections() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(&tmp);
        paths.ensure_all().unwrap();
        fs::write(
            paths.config_toml(),
            "[core]
layout = \"flat\"
",
        )
        .unwrap();

        let (_, added) = check_and_optionally_fix_config(&paths, true);
        assert_eq!(added, EXPECTED_SECTIONS.len() - 1, "everything but [core]");

        let doc: toml::Value =
            toml::from_str(&fs::read_to_string(paths.config_toml()).unwrap()).unwrap();
        for gone in ["git", "safety", "jobs", "mcp", "checkpoint"] {
            assert!(
                doc.get(gone).is_none(),
                "--fix wrote [{gone}], which nothing reads"
            );
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
        assert!(
            applied >= 1,
            "writing a missing config is at least one repair action"
        );
        assert!(paths.config_toml().exists());
        // running again should be ok (file already valid)
        let (result2, applied2) = check_and_optionally_fix_config(&paths, true);
        assert_eq!(result2.status, Status::Ok);
        assert_eq!(
            applied2, 0,
            "a second --fix must be a no-op; the list and the file agree"
        );
    }

    #[test]
    fn state_check_warns_when_missing_without_fix() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(&tmp);
        let (result, applied) = check_and_optionally_fix_state(
            &paths,
            &DoctorOptions {
                fix: false,
                ..Default::default()
            },
        );
        assert_eq!(result.status, Status::Warn);
        assert_eq!(
            applied, 0,
            "a check that repairs nothing reports nothing applied"
        );
        assert!(
            !paths.state_db().exists(),
            "a run without --fix must not create the database"
        );
    }

    #[test]
    fn state_check_creates_db_when_fix_set() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(&tmp);
        let (result, applied) = check_and_optionally_fix_state(
            &paths,
            &DoctorOptions {
                fix: true,
                ..Default::default()
            },
        );
        assert_eq!(result.status, Status::Ok);
        assert_eq!(
            applied, 0,
            "creating a database that is not there is not a count of fixes"
        );
        assert!(paths.state_db().exists());
    }

    // ── `--fix` on a state.db that is not a database ─────────────────────────
    //
    // The one repair in the doctor that destroys anything, and the one that
    // has never been exercised on a real broken file. Every earlier test
    // used a database that was missing or healthy; a file that is present
    // and unreadable is a different state, and the difference is the whole
    // point: the repair throws the file away, so it has to be the one
    // operation that takes a backup first.

    /// A `state.db` that is not a database is backed up and replaced.
    ///
    /// The backup is the assertion. A repair that reports "replaced" while
    /// the old bytes are gone is a lie with a good exit code, and the old
    /// bytes are the only record of what was tracked.
    #[test]
    fn fix_backs_up_and_replaces_a_state_db_that_is_not_a_database() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(&tmp);
        paths.ensure_all().unwrap();
        let db_path = paths.state_db();
        // Not a database: a file SQLite will refuse to open. The header is
        // the first thing SQLite reads, so anything else is "not a database"
        // rather than "corrupt".
        fs::write(&db_path, b"this is not a database, not even a little bit").unwrap();

        let (result, applied) = check_and_optionally_fix_state(
            &paths,
            &DoctorOptions {
                fix: true,
                state_db_path: Some(db_path.clone()),
                ..Default::default()
            },
        );

        assert_eq!(
            result.status,
            Status::Warn,
            "a replaced registry is a warning, not a clean bill of health: \
             the repos it tracked are gone until they are re-added. got: {}",
            result.message
        );
        assert_eq!(applied, 2, "a backup and a replacement are two repairs");
        assert!(
            result.applied_fix.is_some(),
            "the applied fix must be reported, not just counted"
        );

        let backups: Vec<_> = fs::read_dir(paths.state_dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".bak"))
            .collect();
        assert_eq!(backups.len(), 1, "exactly one backup, got: {backups:?}");
        let backup_bytes = fs::read(backups[0].path()).unwrap();
        assert_eq!(
            backup_bytes, b"this is not a database, not even a little bit",
            "the backup must be the original file, byte for byte"
        );

        // And the replacement is a real, empty, usable database — not a
        // zero-byte file that will fail the same way on the next run.
        let conn = ro_state::open_db(&db_path).expect("the replacement opens");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM repos", [], |r| r.get(0))
            .expect("the replacement has the schema");
        assert_eq!(count, 0, "a fresh registry is empty");
    }

    /// The backup is never overwritten.
    ///
    /// A second `--fix` on a still-broken file must not destroy the first
    /// backup: each attempt is numbered, and the oldest copy is the only
    /// one that has never been through this path.
    #[test]
    fn fix_never_overwrites_an_existing_backup() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(&tmp);
        paths.ensure_all().unwrap();
        let db_path = paths.state_db();
        fs::write(&db_path, b"first corruption").unwrap();
        let first = paths.state_dir.join("state.db.bak");
        fs::write(&first, b"the original, from before the first fix").unwrap();

        let (result, _) = check_and_optionally_fix_state(
            &paths,
            &DoctorOptions {
                fix: true,
                state_db_path: Some(db_path.clone()),
                ..Default::default()
            },
        );
        assert_eq!(result.status, Status::Warn);

        assert_eq!(
            fs::read(&first).unwrap(),
            b"the original, from before the first fix",
            "an existing backup is a user decision this run does not get to make again"
        );
        let second = paths.state_dir.join("state.db.1.bak");
        assert!(
            second.exists(),
            "the new backup must be numbered, not written over the old one"
        );
        assert_eq!(fs::read(&second).unwrap(), b"first corruption");
    }

    /// A directory that is not ro's is not repaired.
    ///
    /// The refusal is the point. `--fix` is a command that destroys a file,
    /// and the only thing that makes that safe is knowing the file is ro's.
    /// A state directory holding anything but `state.db*` is not ro's, and
    /// the message has to say what it saw so the user can tell this from
    /// "ro is broken".
    #[test]
    fn fix_refuses_to_touch_a_state_dir_that_is_not_ros() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(&tmp);
        paths.ensure_all().unwrap();
        let db_path = paths.state_db();
        fs::write(&db_path, b"not a database").unwrap();
        // A file that is not ro's, in the directory that is.
        fs::write(paths.state_dir.join("notes.txt"), b"the user's own file").unwrap();

        let (result, applied) = check_and_optionally_fix_state(
            &paths,
            &DoctorOptions {
                fix: true,
                state_db_path: Some(db_path.clone()),
                ..Default::default()
            },
        );

        assert_eq!(result.status, Status::Fail);
        assert_eq!(applied, 0, "a refused repair is not a repair");
        assert!(
            result.message.contains("notes.txt"),
            "the refusal must name what it saw; got: {}",
            result.message
        );
        assert_eq!(
            fs::read(&db_path).unwrap(),
            b"not a database",
            "the file must be untouched"
        );
        assert_eq!(
            fs::read(paths.state_dir.join("notes.txt")).unwrap(),
            b"the user's own file",
            "and so must everything else in the directory"
        );
    }

    /// An error that is not "not a database" is never repaired.
    ///
    /// "Could not open" is not "corrupt". A full disk, a directory where
    /// the file should be, and a permission problem all surface as an error
    /// from `open_db`, and the repair for all of them is to throw the file
    /// away — which is exactly what must not happen.
    #[test]
    fn fix_leaves_a_file_it_cannot_understand_alone() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(&tmp);
        paths.ensure_all().unwrap();
        let db_path = paths.state_db();
        // A directory where the database should be: `open_db` fails, and
        // the reason is not "not a database".
        fs::create_dir_all(&db_path).unwrap();

        let (result, applied) = check_and_optionally_fix_state(
            &paths,
            &DoctorOptions {
                fix: true,
                state_db_path: Some(db_path.clone()),
                ..Default::default()
            },
        );

        assert_eq!(result.status, Status::Fail);
        assert_eq!(applied, 0);
        assert!(
            result.message.contains("did not touch it"),
            "the message must say the file was left alone; got: {}",
            result.message
        );
        assert!(
            db_path.is_dir(),
            "a directory where the database should be is not a database to replace"
        );
    }

    /// A backup that could not be written is a refusal, not a repair.
    ///
    /// The registry is the only copy of what is tracked. Replacing it
    /// without a verified backup is a data-loss event wearing a repair
    /// command's costume, so the check fails and says where the original
    /// is.
    ///
    /// The state directory is made read-only rather than planting a
    /// directory at `state.db.bak`: the backup writer numbers past an
    /// existing backup (`state.db.1.bak`), so a single blocked name is
    /// stepped over and the repair goes ahead — correctly, and which would
    /// make this test pass for the wrong reason.
    #[test]
    #[cfg(unix)]
    fn fix_refuses_to_replace_a_registry_it_could_not_back_up() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let paths = paths_in(&tmp);
        paths.ensure_all().unwrap();
        let db_path = paths.state_db();
        fs::write(&db_path, b"not a database").unwrap();
        fs::set_permissions(paths.state_dir.clone(), fs::Permissions::from_mode(0o500)).unwrap();

        let (result, applied) = check_and_optionally_fix_state(
            &paths,
            &DoctorOptions {
                fix: true,
                state_db_path: Some(db_path.clone()),
                ..Default::default()
            },
        );

        // Restore before asserting, so a failing assertion still cleans up.
        fs::set_permissions(paths.state_dir.clone(), fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(result.status, Status::Fail);
        assert_eq!(applied, 0);
        assert!(
            result.message.contains("backup"),
            "the refusal must name the backup; got: {}",
            result.message
        );
        assert_eq!(
            fs::read(&db_path).unwrap(),
            b"not a database",
            "the original must be untouched"
        );
    }

    /// An existing backup is stepped over, not overwritten — and the refusal
    /// to overwrite it is a property of the *writer*, not of the directory
    /// check.
    ///
    /// A directory at `state.db.bak` makes the first backup name unusable;
    /// the repair must still find somewhere to put the original rather than
    /// concluding it cannot be saved. (The companion test above covers the
    /// case where there is nowhere at all to write.)
    #[test]
    fn an_unusable_backup_name_is_stepped_over_not_treated_as_a_blocker() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(&tmp);
        paths.ensure_all().unwrap();
        let db_path = paths.state_db();
        fs::write(&db_path, b"not a database").unwrap();
        fs::create_dir_all(paths.state_dir.join("state.db.bak")).unwrap();

        let (result, _) = check_and_optionally_fix_state(
            &paths,
            &DoctorOptions {
                fix: true,
                state_db_path: Some(db_path.clone()),
                ..Default::default()
            },
        );

        assert_eq!(
            result.status,
            Status::Warn,
            "the repair must proceed; got: {}",
            result.message
        );
        let numbered = paths.state_dir.join("state.db.1.bak");
        assert!(
            numbered.exists(),
            "the backup went to the numbered name; the directory is still there"
        );
        assert_eq!(
            fs::read(&numbered).unwrap(),
            b"not a database",
            "and it is the original file"
        );
    }

    /// A second `--fix` on a repaired database is a no-op.
    ///
    /// The repair converges: a fresh database opens, so the second run
    /// reports it healthy and changes nothing. A repair command that keeps
    /// "repairing" the same file is one a user stops running.
    #[test]
    fn fix_converges_after_replacing_a_broken_database() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(&tmp);
        paths.ensure_all().unwrap();
        let db_path = paths.state_db();
        fs::write(&db_path, b"not a database").unwrap();

        let opts = || DoctorOptions {
            fix: true,
            state_db_path: Some(db_path.clone()),
            ..Default::default()
        };
        let (first, applied_first) = check_and_optionally_fix_state(&paths, &opts());
        assert_eq!(first.status, Status::Warn);
        assert_eq!(applied_first, 2);

        let (second, applied_second) = check_and_optionally_fix_state(&paths, &opts());
        assert_eq!(
            second.status,
            Status::Ok,
            "a repaired database must read as healthy on the next run"
        );
        assert_eq!(applied_second, 0, "the second run must change nothing");
        let backups: Vec<_> = fs::read_dir(paths.state_dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".bak"))
            .collect();
        assert_eq!(
            backups.len(),
            1,
            "the second run must not back up a healthy database"
        );
    }

    /// The applied fix is rendered, not just counted.
    ///
    /// A `--fix` run that silently changed three things and said nothing
    /// is the behaviour users are asked to trust with a repair command. The
    /// count reaches the exit code; the sentence reaches the person.
    #[test]
    fn the_replacement_is_rendered_in_the_report() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(&tmp);
        paths.ensure_all().unwrap();
        let db_path = paths.state_db();
        fs::write(&db_path, b"not a database").unwrap();

        let report = run(DoctorOptions {
            fix: true,
            state_db_path: Some(db_path.clone()),
            paths: Some(paths.clone()),
            ..Default::default()
        });
        let state = report
            .checks
            .iter()
            .find(|c| c.name == "state")
            .expect("the state check is in the report");
        let applied = state
            .applied_fix
            .as_deref()
            .expect("an applied fix is reported");
        assert!(
            applied.contains("state.db.bak"),
            "the applied fix must name the backup it wrote; got: {applied}"
        );
        assert!(
            applied.contains("ro add"),
            "the applied fix must say how the tracked repos come back; got: {applied}"
        );
        let text = render_text(&report);
        assert!(
            text.contains("fix applied:"),
            "the rendered report carries the applied fix; got:\n{text}"
        );
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
            ..Default::default()
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

    // ── Per-repo credential resolution ─────────────────────────────────────────
    //
    // A row may carry its own `credential_ref` — `env:VAR` or `keychain:NAME`
    // — and the push path honours it. The write check must probe with THAT
    // credential, not the ambient one. A permanent false alarm is the failure
    // mode being guarded against: a row whose token is fine for everything
    // except this one repo, probed with the machine's token, reports NO forever.

    /// A loopback GitHub that answers the two GETs the probe makes and records
    /// every `Authorization` header it sees.
    struct FakeGitHub {
        base_uri: String,
        auths: Arc<Mutex<Vec<String>>>,
    }

    impl FakeGitHub {
        fn start(login: &'static str, grant_push: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
            let addr = listener.local_addr().expect("a bound address");
            let auths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&auths);
            let login = login.to_string();

            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    let recorded = Arc::clone(&recorded);
                    let login = login.clone();
                    std::thread::spawn(move || {
                        let _ = serve(stream, recorded, &login, grant_push);
                    });
                }
            });

            Self {
                base_uri: format!("http://{addr}"),
                auths,
            }
        }

        fn requested_tokens(&self) -> Vec<String> {
            self.auths.lock().expect("the log is not poisoned").clone()
        }
    }

    /// Serve one HTTP request. Records the bearer token it carried, then answers
    /// the two routes the probe reads: `/user` and `/repos/{owner}/{name}`.
    fn serve(
        mut stream: TcpStream,
        auths: Arc<Mutex<Vec<String>>>,
        login: &str,
        grant_push: bool,
    ) -> std::io::Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let path = line
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_string();

        let mut authorization = String::new();
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header)? == 0 {
                break;
            }
            if header.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                if name.trim().eq_ignore_ascii_case("authorization") {
                    authorization = value.trim().to_string();
                }
            }
        }
        auths
            .lock()
            .expect("the log is not poisoned")
            .push(authorization.clone());

        let (status, body) = if path == "/user" {
            ("200 OK", format!(r#"{{"login":"{login}"}}"#))
        } else {
            let push = if grant_push { "true" } else { "false" };
            (
                "200 OK",
                format!(
                    r#"{{"permissions":{{"push":{push},"maintain":false,"admin":false,"triage":false,"pull":true}}}}"#
                ),
            )
        };

        let reason = if status.starts_with("200") {
            "OK"
        } else {
            "Error"
        };
        stream.write_all(
            format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
                 Content-Length: {len}\r\nConnection: close\r\n\r\n{body}",
                len = body.len()
            )
            .as_bytes(),
        )?;
        stream.flush()
    }

    /// Insert a tracked repo row directly into the registry.
    ///
    /// Adopts a real local checkout rather than cloning, so the fixture never
    /// touches the network. The checkout is laid out as `<owner>/<name>` so the
    /// derived row matches the `acme/api` the assertions below look for.
    fn track_repo(conn: &ro_state::Connection, spec: &str) {
        let (owner, name) = spec.split_once('/').expect("spec is owner/name");
        let projects = tempfile::tempdir().expect("a projects dir");
        let checkout = projects.path().join(owner).join(name);
        fs::create_dir_all(&checkout).expect("the checkout directory is creatable");

        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&checkout)
                .output()
                .expect("git runs");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        fs::write(checkout.join("README.md"), "# fixture\n").expect("a file to commit");
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "initial"]);

        let repo = ro_sync::manage::add_from_input(
            conn,
            &checkout.to_string_lossy(),
            projects.path(),
            &ro_sync::manage::AddOptions::default(),
            "flat",
        )
        .expect("the repo is tracked");
        assert_eq!(repo.owner, owner);
        assert_eq!(repo.name, name);
    }

    fn rows_for<'a>(checks: &'a [CheckResult], slug: &str) -> Vec<&'a CheckResult> {
        checks
            .iter()
            .filter(|c| c.name == format!("repo:{slug}"))
            .collect()
    }

    /// A row whose `credential_ref` names a SET variable is probed with THAT
    /// token, not the ambient one.
    ///
    /// The fake GitHub records the bearer token of every request. The named
    /// variable's value must appear and the ambient token must not — proving the
    /// probe used the credential the row points at.
    #[test]
    fn a_row_with_a_set_credential_ref_is_probed_with_that_credential() {
        let _guard = ro_testkit::shim::path_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("GH_TOKEN", "ghp_ambienttoken123456789012345678901234");
            std::env::set_var("GH_TOKEN_WORK", "ghp_worktoken1234567890123456789012345");
        }

        let tmp = TempDir::new().unwrap();
        let paths = paths_in(&tmp);
        paths.ensure_all().unwrap();
        let conn = ro_state::open_db(&paths.state_db()).expect("the db opens");
        track_repo(&conn, "acme/api");
        ro_sync::manage::set_repo_config(&conn, "acme/api", "credential_ref", "env:GH_TOKEN_WORK")
            .expect("the row is updated");

        let fake = FakeGitHub::start("quangdang46", true);
        let checks = check_repo_write_access(&conn, Some(&fake.base_uri));

        let tokens = fake.requested_tokens();
        assert!(
            tokens
                .iter()
                .any(|t| t.contains("ghp_worktoken1234567890123456789012345")),
            "the row's own credential must be the one probed; saw: {tokens:?}"
        );
        assert!(
            !tokens
                .iter()
                .any(|t| t.contains("ghp_ambienttoken123456789012345678901234")),
            "the ambient token must NOT be used for a row with its own credential_ref; saw: {tokens:?}"
        );

        let rows = rows_for(&checks, "acme/api");
        assert_eq!(rows.len(), 1, "one row per repo");
        assert_eq!(rows[0].status, Status::Ok);
        assert!(
            rows[0].message.contains("write: yes"),
            "got: {}",
            rows[0].message
        );

        unsafe {
            std::env::remove_var("GH_TOKEN");
            std::env::remove_var("GH_TOKEN_WORK");
        }
    }

    /// A row whose `credential_ref` names an UNSET variable is a FAIL naming the
    /// variable — never a silent fall-back to the ambient token.
    #[test]
    fn a_row_with_an_unset_credential_ref_fails_and_names_the_variable() {
        let _guard = ro_testkit::shim::path_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("GH_TOKEN", "ghp_ambienttoken123456789012345678901234");
            std::env::remove_var("GH_TOKEN_MISSING");
        }

        let tmp = TempDir::new().unwrap();
        let paths = paths_in(&tmp);
        paths.ensure_all().unwrap();
        let conn = ro_state::open_db(&paths.state_db()).expect("the db opens");
        track_repo(&conn, "acme/api");
        ro_sync::manage::set_repo_config(
            &conn,
            "acme/api",
            "credential_ref",
            "env:GH_TOKEN_MISSING",
        )
        .expect("the row is updated");

        let fake = FakeGitHub::start("quangdang46", true);
        let checks = check_repo_write_access(&conn, Some(&fake.base_uri));

        let rows = rows_for(&checks, "acme/api");
        assert_eq!(rows.len(), 1, "one row per repo");
        assert_eq!(
            rows[0].status,
            Status::Fail,
            "an unresolvable credential is a failure, not a fall-back"
        );
        assert!(
            rows[0].message.contains("GH_TOKEN_MISSING"),
            "the message must name the variable; got: {}",
            rows[0].message
        );
        assert!(
            rows[0].message.contains("env:GH_TOKEN_MISSING"),
            "the message must name the reference; got: {}",
            rows[0].message
        );

        // The ambient token must not have been used as a stand-in.
        assert!(
            !fake
                .requested_tokens()
                .iter()
                .any(|t| t.contains("ghp_ambienttoken123456789012345678901234")),
            "an unset variable must not fall back to the ambient token; saw: {:?}",
            fake.requested_tokens()
        );

        unsafe {
            std::env::remove_var("GH_TOKEN");
        }
    }

    /// A malformed `credential_ref` is a clear failure, not a fall-back.
    #[test]
    fn a_malformed_credential_ref_is_a_clear_failure() {
        let _guard = ro_testkit::shim::path_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("GH_TOKEN", "ghp_ambienttoken123456789012345678901234");
        }

        let tmp = TempDir::new().unwrap();
        let paths = paths_in(&tmp);
        paths.ensure_all().unwrap();
        let conn = ro_state::open_db(&paths.state_db()).expect("the db opens");
        track_repo(&conn, "acme/api");
        // A pasted token is the classic mistake. `set_repo_config` validates the
        // reference, so write the malformed value straight to the column to
        // model a row that predates the validation or was edited by hand.
        conn.execute(
            "UPDATE repos SET credential_ref = ?1 WHERE owner = 'acme' AND name = 'api'",
            params!["ghp_pastedtoken1234567890123456789012345678"],
        )
        .expect("the row is updated");

        let fake = FakeGitHub::start("quangdang46", true);
        let checks = check_repo_write_access(&conn, Some(&fake.base_uri));

        let rows = rows_for(&checks, "acme/api");
        assert_eq!(rows.len(), 1, "one row per repo");
        assert_eq!(
            rows[0].status,
            Status::Fail,
            "a malformed reference must not fall back to the ambient token"
        );
        assert!(
            rows[0].message.contains("credential"),
            "the message must say what is wrong; got: {}",
            rows[0].message
        );

        assert!(
            !fake
                .requested_tokens()
                .iter()
                .any(|t| t.contains("ghp_ambienttoken123456789012345678901234")),
            "a malformed reference must not fall back to the ambient token; saw: {:?}",
            fake.requested_tokens()
        );

        unsafe {
            std::env::remove_var("GH_TOKEN");
        }
    }

    /// A row with no `credential_ref` still uses the ambient token — no
    /// regression on the common case.
    #[test]
    fn a_row_without_a_credential_ref_uses_the_ambient_token() {
        let _guard = ro_testkit::shim::path_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("GH_TOKEN", "ghp_ambienttoken123456789012345678901234");
        }

        let tmp = TempDir::new().unwrap();
        let paths = paths_in(&tmp);
        paths.ensure_all().unwrap();
        let conn = ro_state::open_db(&paths.state_db()).expect("the db opens");
        track_repo(&conn, "acme/api");

        let fake = FakeGitHub::start("quangdang46", true);
        let checks = check_repo_write_access(&conn, Some(&fake.base_uri));

        let tokens = fake.requested_tokens();
        assert!(
            tokens
                .iter()
                .any(|t| t.contains("ghp_ambienttoken123456789012345678901234")),
            "a row with no credential_ref must be probed with the ambient token; saw: {tokens:?}"
        );

        let rows = rows_for(&checks, "acme/api");
        assert_eq!(rows.len(), 1, "one row per repo");
        assert_eq!(rows[0].status, Status::Ok);

        unsafe {
            std::env::remove_var("GH_TOKEN");
        }
    }

    /// The check must say WHICH credential it probed with, so a user can tell
    /// "my token has no access" from "this repo points at a credential I do not
    /// have".
    #[test]
    fn the_message_names_the_credential_it_probed_with() {
        let _guard = ro_testkit::shim::path_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("GH_TOKEN", "ghp_ambienttoken123456789012345678901234");
            std::env::set_var("GH_TOKEN_WORK", "ghp_worktoken1234567890123456789012345");
        }

        let tmp = TempDir::new().unwrap();
        let paths = paths_in(&tmp);
        paths.ensure_all().unwrap();
        let conn = ro_state::open_db(&paths.state_db()).expect("the db opens");
        track_repo(&conn, "acme/api");
        ro_sync::manage::set_repo_config(&conn, "acme/api", "credential_ref", "env:GH_TOKEN_WORK")
            .expect("the row is updated");

        let fake = FakeGitHub::start("quangdang46", true);
        let checks = check_repo_write_access(&conn, Some(&fake.base_uri));

        let rows = rows_for(&checks, "acme/api");
        assert_eq!(rows.len(), 1, "one row per repo");
        assert!(
            rows[0].message.contains("GH_TOKEN_WORK"),
            "the message must name the credential it probed with; got: {}",
            rows[0].message
        );

        unsafe {
            std::env::remove_var("GH_TOKEN");
            std::env::remove_var("GH_TOKEN_WORK");
        }
    }
}
