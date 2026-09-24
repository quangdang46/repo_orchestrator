//! End-to-end CLI tests for ro commands.
//! Uses --config-dir and --state-dir overrides for isolation.

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

struct RfoTest {
    config_dir: TempDir,
    state_dir: TempDir,
}

impl RfoTest {
    fn new() -> Self {
        Self {
            config_dir: TempDir::new().unwrap(),
            state_dir: TempDir::new().unwrap(),
        }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("ro").expect("ro binary should compile");
        cmd.arg("--config-dir").arg(self.config_dir.path());
        cmd.arg("--state-dir").arg(self.state_dir.path());
        cmd
    }
}

#[test]
fn init_creates_config_and_state() {
    let test = RfoTest::new();
    let mut cmd = test.cmd();
    cmd.arg("init");
    cmd.assert()
        .success()
        .stderr(predicate::str::contains("Initialized"));

    assert!(test.state_dir.path().join("state.db").exists());
}

#[test]
fn init_is_idempotent() {
    let test = RfoTest::new();
    let mut cmd = test.cmd();
    cmd.arg("init");
    cmd.assert().success();

    let mut cmd2 = test.cmd();
    cmd2.arg("init");
    cmd2.assert()
        .success()
        .stderr(predicate::str::contains("Already initialized"));
}

#[test]
fn add_and_list_roundtrip() {
    let test = RfoTest::new();

    let mut cmd = test.cmd();
    cmd.arg("init");
    cmd.assert().success();

    let mut cmd2 = test.cmd();
    cmd2.args(["add", "quangdang46/repo_orchestrator"]);
    cmd2.assert().success();

    let mut cmd3 = test.cmd();
    cmd3.args(["list", "--format", "json"]);
    cmd3.assert()
        .success()
        .stdout(predicate::str::contains("quangdang46/repo_orchestrator"));
}

#[test]
fn remove_repo() {
    let test = RfoTest::new();

    let mut cmd = test.cmd();
    cmd.arg("init");
    cmd.assert().success();

    let mut cmd2 = test.cmd();
    cmd2.args(["add", "quangdang46/repo_orchestrator"]);
    cmd2.assert().success();

    let mut cmd3 = test.cmd();
    cmd3.args(["remove", "quangdang46/repo_orchestrator"]);
    cmd3.assert()
        .success()
        .stderr(predicate::str::contains("Removed"));
}

#[test]
fn import_from_file() {
    let test = RfoTest::new();

    let mut cmd = test.cmd();
    cmd.arg("init");
    cmd.assert().success();

    let list_file = test.config_dir.path().join("repos.list");
    std::fs::write(&list_file, b"quangdang46/repo_orchestrator\nalice/other-repo\n").unwrap();

    let mut cmd2 = test.cmd();
    cmd2.args(["import", list_file.to_str().unwrap()]);
    cmd2.assert().success();

    let mut cmd3 = test.cmd();
    cmd3.args(["list"]);
    cmd3.assert()
        .success()
        .stdout(predicate::str::contains("quangdang46/repo_orchestrator"))
        .stdout(predicate::str::contains("alice/other-repo"));
}
