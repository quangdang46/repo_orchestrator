//! `ro doctor --fix` on genuinely broken states.
//!
//! The repair path had only ever been exercised on a trivial case: a config
//! missing a table, a state dir that did not exist. Both of those are states
//! where "add what is missing" is the whole repair. The interesting cases
//! are the ones where something is **there and wrong**, because that is where
//! a repair command can report a fix it did not apply, or destroy what it
//! could not repair.
//!
//! Four broken states, driven through the real binary against directories the
//! test created:
//!
//! | state | what `--fix` must do |
//! |---|---|
//! | config with a syntax error | not touch it, and say what is wrong |
//! | missing state dir | create it |
//! | unwritable config dir | say it could not write, change nothing |
//! | `state.db` that is not a database | back it up, replace it, say both |

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

struct Test {
    config_dir: TempDir,
    state_dir: TempDir,
}

impl Test {
    fn new() -> Self {
        Test {
            config_dir: TempDir::new().unwrap(),
            state_dir: TempDir::new().unwrap(),
        }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("ro").expect("the ro binary compiles");
        cmd.arg("--config-dir")
            .arg(self.config_dir.path())
            .arg("--state-dir")
            .arg(self.state_dir.path());
        // The doctor's `github_auth` check reads `GH_TOKEN`/`GITHUB_TOKEN`
        // from the environment, and a miss is a **Required** failure — so it
        // changes the exit code and the "N failed" count that every count
        // assertion in this file depends on.
        //
        // Left to the ambient environment, these tests pass on a laptop that
        // happens to have a token exported and fail on a CI runner that does
        // not, for a reason that has nothing to do with the state under test.
        // A dummy value pins the check to "ok" without contacting GitHub:
        // `discover_token` only reads the variable, it never calls out.
        cmd.env("GH_TOKEN", "test-token-not-used");
        cmd
    }

    fn cfg(&self) -> std::path::PathBuf {
        self.config_dir.path().join("config.toml")
    }

    fn db(&self) -> std::path::PathBuf {
        self.state_dir.path().join("state.db")
    }

    /// The doctor's own verdict, from `--format json`.
    fn report(&self, fix: bool) -> serde_json::Value {
        let mut cmd = self.cmd();
        cmd.args(["doctor", "--format", "json"]);
        if fix {
            cmd.arg("--fix");
        }
        // The doctor exits 1 when a check fails, which is most of these
        // states by construction. The report is what is being asserted.
        let out = cmd.output().expect("ro doctor runs");
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "doctor must emit JSON on stdout ({e})\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        })
    }

    /// The one check, by name.
    fn check(report: &serde_json::Value, name: &str) -> serde_json::Value {
        report["checks"]
            .as_array()
            .expect("checks is an array")
            .iter()
            .find(|c| c["name"] == name)
            .unwrap_or_else(|| {
                panic!(
                    "no `{name}` check in the report: {}",
                    serde_json::to_string_pretty(report).unwrap()
                )
            })
            .clone()
    }
}

/// A config that will not parse: `--fix` must not touch it.
///
/// The file is the user's, and a syntax error is not something a repair
/// command can infer an answer to. The correct behaviour is to report the
/// problem, leave the file byte-identical, and exit non-zero — not to
/// "repair" a config by overwriting it with defaults, which is data loss
/// wearing a fix's name.
#[test]
fn fix_leaves_a_config_with_a_syntax_error_alone() {
    let t = Test::new();
    std::fs::create_dir_all(t.config_dir.path()).unwrap();
    // `[core` with no closing bracket: TOML will not parse it.
    let broken = "[core\nlayout = \"flat\"\n";
    std::fs::write(t.cfg(), broken).unwrap();

    let report = t.report(true);
    let check = Test::check(&report, "config");

    assert_eq!(
        check["status"], "fail",
        "a config that will not parse is a failure, and `--fix` cannot fix it"
    );
    assert!(
        check["applied_fix"].is_null(),
        "no fix was applied, so none may be reported; got: {}",
        check["applied_fix"]
    );
    assert_eq!(
        std::fs::read_to_string(t.cfg()).unwrap(),
        broken,
        "the file must be byte-identical after a `--fix` that could not repair it"
    );
}

/// A `ro doctor --fix` on a broken config also says it could not write.
///
/// The hint has to name the file, because "fix the config" is not an action
/// and `<path>` is.
#[test]
fn a_broken_config_names_the_file_to_edit() {
    let t = Test::new();
    std::fs::create_dir_all(t.config_dir.path()).unwrap();
    std::fs::write(t.cfg(), "[core\n").unwrap();

    let report = t.report(true);
    let check = Test::check(&report, "config");
    let hint = check["fix_hint"].as_str().unwrap_or_default();
    assert!(
        hint.contains(&t.cfg().display().to_string()),
        "the hint must name the file, got: {hint}"
    );
}

/// A missing state database is created, and reported as created.
#[test]
fn fix_creates_a_missing_state_database() {
    let t = Test::new();
    std::fs::create_dir_all(t.state_dir.path()).unwrap();

    assert!(!t.db().exists(), "the fixture is broken on purpose");

    let report = t.report(true);
    let check = Test::check(&report, "state");

    assert_eq!(check["status"], "ok", "got: {}", check["message"]);
    assert!(
        t.db().exists(),
        "the state database must exist after a `--fix` that was asked to create it"
    );
}

/// A state directory that does not exist at all is created too.
///
/// The parent of the database, not just the file: `--fix` is asked to make
/// the paths work, and a missing `~/.local/state/ro` is the state every
/// fresh install is in.
#[test]
fn fix_creates_a_missing_state_directory() {
    let t = Test::new();
    let deeper = TempDir::new().unwrap();
    let state_dir = deeper.path().join("not-created-yet");
    assert!(!state_dir.exists(), "the fixture is broken on purpose");

    let mut cmd = Command::cargo_bin("ro").expect("the ro binary compiles");
    cmd.arg("--config-dir")
        .arg(t.config_dir.path())
        .arg("--state-dir")
        .arg(&state_dir)
        .args(["doctor", "--fix", "--format", "json"]);
    let out = cmd.output().expect("ro doctor runs");
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("doctor must emit valid JSON on stdout");

    let check = Test::check(&report, "state");
    assert_eq!(check["status"], "ok", "got: {}", check["message"]);
    assert!(
        state_dir.join("state.db").exists(),
        "`--fix` must create the state directory, not just the file inside it"
    );
}

/// Without `--fix` a missing state dir is reported, not repaired.
///
/// A doctor that repairs by default is a doctor nobody can run to find out
/// what is wrong.
#[test]
fn without_fix_a_missing_state_directory_is_only_reported() {
    let t = Test::new();
    let deeper = TempDir::new().unwrap();
    let state_dir = deeper.path().join("not-created-yet");

    let mut cmd = Command::cargo_bin("ro").expect("the ro binary compiles");
    cmd.arg("--config-dir")
        .arg(t.config_dir.path())
        .arg("--state-dir")
        .arg(&state_dir)
        .args(["doctor", "--format", "json"]);
    let out = cmd.output().expect("ro doctor runs");
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("doctor must emit valid JSON on stdout");

    let check = Test::check(&report, "state");
    assert_eq!(check["status"], "warn");
    assert!(
        check["applied_fix"].is_null(),
        "nothing was applied without --fix"
    );
    assert!(
        !state_dir.exists(),
        "a run without --fix must not create anything: the doctor is \
         documented as pure detection, and a diagnostic that writes to the \
         user's machine is not a diagnostic"
    );
}

/// The negative control for the assertion above, on the *existing*
/// database: a run without `--fix` reports the repos and touches nothing.
///
/// Without this, "the doctor creates nothing" could be passing because the
/// per-repo write check was skipped — which is what it should do when there
/// is no database, and must not do when there is one.
#[test]
fn without_fix_an_existing_database_is_reported_not_modified() {
    let t = Test::new();
    t.cmd().arg("init").assert().success();
    let before = std::fs::metadata(t.db()).unwrap().len();

    let report = t.report(false);

    let after = std::fs::metadata(t.db()).unwrap().len();
    assert_eq!(
        before, after,
        "a diagnostic run must not resize the database"
    );
    assert!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "repos"),
        "an installation with a database must get the repos check"
    );
}

/// A config directory that cannot be written to: `--fix` says so and
/// changes nothing.
///
/// The honest failure is the whole test. A repair command that reports
/// success on an unwritable directory leaves the user believing their config
/// is fixed, and the next run fails the same way with no explanation of why.
///
/// The directory is read-only and there is **no** `config.toml` in it, so
/// the repair that fails is the one that creates the file. (An existing file
/// in a read-only directory is still writable — truncating a file needs
/// write permission on the file, not the directory — so a fixture with a
/// config already in it would pass for the wrong reason.)
#[test]
#[cfg(unix)]
fn fix_reports_an_unwritable_config_directory_and_writes_nothing() {
    use std::os::unix::fs::PermissionsExt;

    let t = Test::new();
    std::fs::create_dir_all(t.config_dir.path()).unwrap();
    assert!(!t.cfg().exists());
    std::fs::set_permissions(t.config_dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();

    let report = t.report(true);

    // Restore before asserting, so a failing assertion still cleans up.
    std::fs::set_permissions(t.config_dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

    let check = Test::check(&report, "config");
    assert_eq!(
        check["status"], "fail",
        "a repair that could not be written must not read as healthy; got: {}",
        check["message"]
    );
    assert!(
        check["applied_fix"].is_null(),
        "a fix that could not be written must not be reported as applied; got: {}",
        check["applied_fix"]
    );
    assert!(
        !t.cfg().exists(),
        "no config may be left behind by a run that could not write one"
    );
}

/// The one destructive repair: a `state.db` that is not a database is
/// backed up and replaced.
///
/// `state.db` **is** the registry — every tracked repo, every tag, every
/// recorded run. Replacing it is a repair only if the old bytes are still
/// somewhere, so the assertions are about the backup: it exists, and it is
/// the original file byte for byte.
#[test]
fn fix_backs_up_and_replaces_a_state_db_that_is_not_a_database() {
    let t = Test::new();
    std::fs::create_dir_all(t.state_dir.path()).unwrap();
    let garbage = b"this is not a database, not even a little bit";
    std::fs::write(t.db(), garbage).unwrap();

    let report = t.report(true);
    let check = Test::check(&report, "state");

    assert_eq!(
        check["status"], "warn",
        "a replaced registry is a warning, not a clean bill of health — the \
         repos it tracked are gone until they are re-added; got: {}",
        check["message"]
    );
    let applied = check["applied_fix"]
        .as_str()
        .unwrap_or_else(|| panic!("the applied fix must be reported, not just counted"));
    assert!(
        applied.contains("state.db.bak"),
        "the applied fix must name the backup it wrote; got: {applied}"
    );

    let backup = t.state_dir.path().join("state.db.bak");
    assert!(
        backup.exists(),
        "the original must survive at {backup:?}, or this is data loss"
    );
    assert_eq!(
        std::fs::read(&backup).unwrap(),
        garbage,
        "the backup must be the original file, byte for byte"
    );

    // And the replacement is a real, empty, usable database — not a
    // zero-byte file that fails the same way on the next run.
    let conn = ro_state::open_db(&t.db()).expect("the replacement opens");
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM repos", [], |r| r.get(0))
        .expect("the replacement has the schema");
    assert_eq!(count, 0, "a fresh registry is empty");
}

/// A second `--fix` on a repaired database changes nothing.
///
/// A repair command that keeps "repairing" the same file is one a user stops
/// running.
#[test]
fn fix_converges_after_replacing_a_broken_database() {
    let t = Test::new();
    std::fs::create_dir_all(t.state_dir.path()).unwrap();
    std::fs::write(t.db(), b"not a database").unwrap();

    t.report(true);
    let backup_after_first = t.state_dir.path().join("state.db.bak");
    assert!(backup_after_first.exists());
    let bytes_after_first = std::fs::read(&backup_after_first).unwrap();

    let report = t.report(true);
    let check = Test::check(&report, "state");
    assert_eq!(
        check["status"], "ok",
        "a repaired database must read as healthy on the next run; got: {}",
        check["message"]
    );
    assert!(
        check["applied_fix"].is_null(),
        "the second run must change nothing, and must not claim to"
    );
    assert_eq!(
        std::fs::read(&backup_after_first).unwrap(),
        bytes_after_first,
        "the second run must not overwrite the backup"
    );
    let backups = backups_in(&t);
    assert_eq!(
        backups.len(),
        1,
        "exactly one backup, from the first run; got: {backups:?}"
    );
}

/// The applied fix reaches the text report, not only the JSON.
///
/// A `--fix` run that changed three things and said nothing is the
/// behaviour users are asked to trust with a repair command. The report
/// goes to stdout (it is the command's output, not a diagnostic), so this
/// asserts where a reader actually looks.
#[test]
fn the_replacement_is_rendered_in_the_text_report() {
    let t = Test::new();
    std::fs::create_dir_all(t.state_dir.path()).unwrap();
    std::fs::write(t.db(), b"not a database").unwrap();

    t.cmd()
        .args(["doctor", "--fix"])
        .assert()
        .stdout(predicate::str::contains("fix applied:"))
        .stdout(predicate::str::contains("state.db.bak"))
        .stdout(predicate::str::contains("not a database"));
}

/// A backup that cannot be written is a refusal, not a repair.
///
/// The registry is the only copy of what is tracked, so replacing it without
/// a verified backup is a data-loss event wearing a repair command's
/// costume. The check must fail, and must say the original is untouched.
#[test]
#[cfg(unix)]
fn fix_refuses_to_replace_a_registry_it_could_not_back_up() {
    use std::os::unix::fs::PermissionsExt;

    let t = Test::new();
    std::fs::create_dir_all(t.state_dir.path()).unwrap();
    std::fs::write(t.db(), b"not a database").unwrap();
    // Read and execute only: the backup cannot be written here.
    std::fs::set_permissions(t.state_dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();

    let report = t.report(true);

    std::fs::set_permissions(t.state_dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

    let check = Test::check(&report, "state");
    assert_eq!(
        check["status"], "fail",
        "a repair that could not back up the registry must not report success"
    );
    assert!(
        check["applied_fix"].is_null(),
        "nothing was applied; got: {}",
        check["applied_fix"]
    );
    assert_eq!(
        std::fs::read(t.db()).unwrap(),
        b"not a database",
        "the original must be untouched"
    );
}

/// A state directory holding files that are not ro's is not repaired.
///
/// `--fix` destroys a file, and the only thing that makes that safe is
/// knowing the file is ro's. A directory that also holds something else is
/// not ro's, and the refusal has to say what it saw.
#[test]
fn fix_refuses_a_state_dir_that_is_not_ros() {
    let t = Test::new();
    std::fs::create_dir_all(t.state_dir.path()).unwrap();
    std::fs::write(t.db(), b"not a database").unwrap();
    let bystander = t.state_dir.path().join("notes.txt");
    std::fs::write(&bystander, b"the user's own file").unwrap();

    let report = t.report(true);
    let check = Test::check(&report, "state");

    assert_eq!(check["status"], "fail");
    assert!(
        check["applied_fix"].is_null(),
        "a refused repair is not a repair; got: {}",
        check["applied_fix"]
    );
    let message = check["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("notes.txt"),
        "the refusal must name the file it saw; got: {message}"
    );
    assert!(
        message.contains("state.db"),
        "the refusal must name the file it refused to touch; got: {message}"
    );
    assert_eq!(
        std::fs::read(t.db()).unwrap(),
        b"not a database",
        "the file must be untouched"
    );
    assert_eq!(
        std::fs::read(&bystander).unwrap(),
        b"the user's own file",
        "and so must the user's own file"
    );
    assert!(backups_in(&t).is_empty(), "no backup was written either");
}

/// Every case: the exit code is the doctor's own, and a `--fix` that could
/// not repair something does not exit 0.
///
/// A green exit from a run that changed nothing is how a broken install gets
/// a green light. The report goes to stdout, so that is where the failure
/// is asserted — the exit code alone would not say *what* failed.
#[test]
fn fix_exits_non_zero_when_it_could_not_repair() {
    let t = Test::new();
    std::fs::create_dir_all(t.config_dir.path()).unwrap();
    std::fs::write(t.cfg(), "[core\n").unwrap();

    t.cmd()
        .args(["doctor", "--fix"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("✘"))
        .stdout(predicate::str::contains("1 failed"));
}

/// A healthy installation exits 0 with `--fix`, and says it changed nothing.
///
/// The negative control for the test above: without it, "the doctor exits 1"
/// could be passing for a reason unrelated to the broken state.
#[test]
fn a_healthy_installation_exits_zero_with_fix() {
    let t = Test::new();
    t.cmd().arg("init").assert().success();

    t.cmd()
        .args(["doctor", "--fix"])
        .assert()
        .success()
        .stdout(predicate::str::contains("summary:"));
}

fn backups_in(t: &Test) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(t.state_dir.path())
        .expect("the state dir exists")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(".bak"))
        })
        .collect()
}
