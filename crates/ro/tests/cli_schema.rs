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
/// The command surface, transcribed from PLAN.md.
///
/// Kept here rather than derived from the binary on purpose: a test that
/// reads the code it is testing proves only that the code equals itself.
/// The point of this list is that it is a *second* statement of the same
/// specification, and a disagreement between the two is information.
///
/// It is not a copy of any one place in PLAN.md — the plan is not
/// self-consistent here. §"Done looks like" lists eleven names and omits
/// `ro remove`, which §3 explicitly keeps (`ro add` / `ro list` / `ro
/// remove`), and the "Removed | Replacement" table folds `ro status` into
/// `ro list` while the same document keeps it three other times, the last
/// saying "stays `ro status` … Kept as its own command". Where they
/// disagree the more specific statement wins, so `status` and `remove` both
/// stay.
///
/// `prune` and `run` were here and are not any more: the plan cuts them
/// ("`ro run` / … / `prune` | **cut.** No audit trail"), and the help text
/// for every other command says the per-repo summary line is the audit
/// trail instead.
const EXPECTED_COMMANDS: &[&str] = &[
    "init", "add", "remove", "list", "sync", "status", "commit", "push", "ship", "doctor",
    "config", "schema", "tag", "untag", "tags",
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

/// The root's own args are part of the machine-readable surface.
///
/// `--config-dir`, `--state-dir` and `--non-interactive` are `global = true`,
/// so clap stores them on the root command and accepts them on every
/// subcommand. A consumer that only read `commands[]` could not discover a
/// single one of them — which is exactly why they went undocumented for so
/// long. This asserts the root's args are present and named, so the omission
/// cannot come back silently.
#[test]
fn schema_exposes_the_root_commands_own_args() {
    let s = schema();
    let root_args = s["args"]
        .as_array()
        .expect("schema should carry the root command's own args");

    let longs: Vec<&str> = root_args
        .iter()
        .filter_map(|a| a["long"].as_str())
        .collect();

    for flag in ["--config-dir", "--state-dir", "--non-interactive"] {
        assert!(
            longs.contains(&flag),
            "the schema's root args should include {flag}. It is `global = true`, \
             so it is accepted on every subcommand but lives on the root — a \
             consumer reading only `commands[]` cannot see it.\n\
             root args: {longs:?}"
        );
    }
}

/// `values` is published only for an arg that actually takes a value.
///
/// clap's model gives a `SetTrue` flag possible values — `--non-interactive`
/// reports `["true", "false"]` — but the flag consumes no operand. Publishing
/// that list told a consumer to build `--non-interactive=true`, which the
/// binary rejects with "unexpected value ... no more were expected". A
/// machine-readable reference that advertises an invocation the binary refuses
/// is the same defect class as a flag with no help text.
#[test]
fn schema_publishes_values_only_for_args_that_take_a_value() {
    let s = schema();

    fn check(command: &Value, path: &str, problems: &mut Vec<String>) {
        for a in command["args"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
            let name = a["long"].as_str().unwrap_or_else(|| {
                a["name"].as_str().unwrap_or("<unnamed>")
            });
            // The schema publishes `takes_value`, so the assertion can be
            // made about the RIGHT thing rather than inferred.
            //
            // The test previously asserted the opposite — "nothing publishes
            // values" — which is false for every `--format` and `--strategy`
            // in the tool, and would have failed the moment the traversal was
            // fixed to actually reach them. `--format` genuinely takes a
            // value: `ro list --format` with no operand is a usage error
            // naming the three permitted values, so publishing them is
            // exactly what a consumer needs.
            let takes = a["takes_value"].as_bool().unwrap_or(false);
            if takes && a.get("values").is_none() && a.get("enumerated").is_some() {
                problems.push(format!("{path} {name} takes a value but publishes none"));
            }
            if !takes && a.get("values").is_some() {
                problems.push(format!(
                    "{path} {name} publishes a values list but takes no operand, so a \
                     consumer would build `--{name}=…`, which the binary rejects"
                ));
            }
        }
        for sub in command["subcommands"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
            let name = sub["name"].as_str().unwrap_or("?");
            check(sub, &format!("{path} {name}"), problems);
        }
    }

    let mut problems = Vec::new();
    // The root hangs its children off `commands`, not `subcommands` — and
    // this used to call `check` on the root, which reads `subcommands`, finds
    // nothing, and so visited only the three global args. The test passed
    // while covering 3 of 89: a green suite that is not running the test it
    // claims to run, which is worse than no test because it counts as
    // coverage it did not provide.
    //
    // So the root's own args are checked directly, and each child is walked
    // through `check`, which descends `subcommands` from there on.
    check(&s, "ro", &mut problems);
    for sub in s["commands"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let name = sub["name"].as_str().unwrap_or("?");
        check(sub, &format!("ro {name}"), &mut problems);
    }

    assert!(
        problems.is_empty(),
        "a value list and a takes_value flag that disagree:\n  {}\n\
         A `SetTrue` flag has possible values in clap's model, but it consumes \
         no value — `--non-interactive=true` is a usage error. An arg that \
         does consume a value must publish the values it accepts, or a \
         consumer cannot tell what to pass it.",
        problems.join("\n  ")
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
