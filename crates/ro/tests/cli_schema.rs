//! Tests for `ro schema`, the machine-readable CLI reference.
//!
//! The point of `ro schema` is that it is generated from the live clap tree
//! rather than maintained by hand, which means a command cannot "forget the
//! schema" the way a hand-written JSON literal could. That property is only
//! worth anything if something checks it.
//!
//! So these tests assert against a **hand-written list of commands** rather
//! than against `Cli::command()`. Comparing the output to the tree it is
//! generated from would be circular and would pass unconditionally. The list
//! below is the specification: when a command is added or removed, this file
//! has to be edited deliberately, and a removal that nobody intended shows up
//! here as a failure rather than as a consumer discovering a 404 at runtime.

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use tempfile::TempDir;

/// Every top-level command `ro` is expected to expose right now.
///
/// Update this list **in the same commit** as any change to the `Commands`
/// enum. That is the whole mechanism: the compiler will not notice a missing
/// entry, and that is deliberate — the failure is supposed to be a red test a
/// human reads, not a type error.
const EXPECTED_COMMANDS: &[&str] = &[
    "init", "add", "remove", "list", "sync", "status", "prune", "run", "commit", "push", "ship",
    "doctor", "config", "schema",
];

fn schema() -> Value {
    let config_dir = TempDir::new().unwrap();
    let state_dir = TempDir::new().unwrap();

    let mut cmd = Command::cargo_bin("ro").expect("ro binary should compile");
    cmd.arg("--config-dir")
        .arg(config_dir.path())
        .arg("--state-dir")
        .arg(state_dir.path())
        .arg("schema");
    cmd.assert()
        .success()
        .stdout(predicate::str::is_empty().not());

    let out = cmd.output().expect("ro schema should run");
    serde_json::from_slice(&out.stdout).expect("ro schema must emit valid JSON")
}

#[test]
fn schema_emits_valid_json_with_an_identity() {
    let s = schema();
    assert_eq!(s["name"], "ro", "schema should name the program");
    assert!(
        !s["version"].as_str().unwrap_or_default().is_empty(),
        "schema should carry a version, so a consumer can tell what it is reading"
    );
    assert!(
        s.get("commands").is_some(),
        "schema should have a commands array"
    );
}

#[test]
fn schema_contains_every_expected_command() {
    let s = schema();
    let reported: Vec<&str> = s["commands"]
        .as_array()
        .expect("commands should be an array")
        .iter()
        .map(|c| c["name"].as_str().expect("command should be named"))
        .collect();

    for want in EXPECTED_COMMANDS {
        assert!(
            reported.contains(want),
            "`{want}` is in the specification but missing from `ro schema`.\n\
             reported: {reported:?}\n\
             If this removal was intended, update EXPECTED_COMMANDS in this file \
             in the same commit."
        );
    }
}

#[test]
fn schema_reports_no_command_the_specification_does_not_know_about() {
    let s = schema();
    let reported: Vec<&str> = s["commands"]
        .as_array()
        .expect("commands should be an array")
        .iter()
        .map(|c| c["name"].as_str().expect("command should be named"))
        .collect();

    for got in &reported {
        assert!(
            EXPECTED_COMMANDS.contains(got),
            "`{got}` is in `ro schema` but not in the specification.\n\
             Add it to EXPECTED_COMMANDS in this file — a new command should be a \
             deliberate addition, not an accident."
        );
    }
}

#[test]
fn schema_arguments_use_the_flat_stable_shape() {
    let s = schema();
    let add = s["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "add")
        .expect("add should exist");
    let spec = &add["args"].as_array().expect("add should take arguments")[0];

    // The point of flattening: a consumer reads these keys without knowing
    // anything about clap's Arg type.
    for key in ["name", "required"] {
        assert!(spec.get(key).is_some(), "arg should expose `{key}`");
    }
    assert_eq!(spec["name"], "spec");
    assert_eq!(spec["required"], true);

    // Flags carry their invocable form, so a consumer can build a command
    // line without knowing clap's convention of storing names without dashes.
    let sync = s["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "sync")
        .expect("sync should exist");
    let longs: Vec<&str> = sync["args"]
        .as_array()
        .expect("sync should take arguments")
        .iter()
        .filter_map(|a| a["long"].as_str())
        .collect();
    assert!(
        longs.contains(&"--strategy"),
        "sync should report --strategy, got: {longs:?}"
    );
    assert!(
        longs.contains(&"--dry-run"),
        "sync should report --dry-run, got: {longs:?}"
    );
    assert!(
        longs.iter().all(|l| l.starts_with("--")),
        "every long form should be invocable, got: {longs:?}"
    );
}

#[test]
fn schema_nests_subcommands() {
    let s = schema();
    let config = s["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "config")
        .expect("config should exist");
    let subs: Vec<&str> = config["subcommands"]
        .as_array()
        .expect("config should have subcommands")
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(subs, vec!["print", "set"], "config has print and set");
}

#[test]
fn schema_documents_nothing_that_was_removed() {
    let s = schema();
    let reported: Vec<&str> = s["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();

    // Each of these was cut while `ro schema` was being introduced. The point of
    // the test is that the machine-readable surface cannot keep advertising a
    // command the binary no longer has — the failure that took three commits
    // and a hand-written JSON literal to notice.
    for gone in [
        "import",
        "fork",
        "self-update",
        "robot-docs",
        "health",
        "review",
    ] {
        assert!(
            !reported.contains(&gone),
            "`{gone}` was removed but `ro schema` still advertises it"
        );
    }
}
