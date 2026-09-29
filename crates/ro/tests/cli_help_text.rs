//! Every flag on every verb has help text.
//!
//! `--all`, `--engine` and `--engine-bin` shipped with empty descriptions for
//! releases: the flags worked, and nobody could find out what they did without
//! reading the source. That is the same "promise the tool does not keep"
//! category FEATURES.md uses to justify removing other flags — a flag nobody
//! can discover is a flag that does not exist for the person typing the
//! command line.
//!
//! The check walks the live clap tree rather than the help text, because the
//! tree is where the promise is made: a flag with no `help` attribute is a
//! flag the author did not write one for, and that is the fact worth
//! asserting on. `ro schema` already flattens the same tree for consumers;
//! this asserts the property the flattening exists to serve.
//!
//! Hidden flags are exempt. They are compatibility spellings — `ro sweep
//! commit-sweep`, `ro health` — kept for one release so a script does not
//! break, and documenting them in `--help` would advertise the very spelling
//! the release is retiring.

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

fn schema() -> Value {
    let config_dir = TempDir::new().unwrap();
    let state_dir = TempDir::new().unwrap();
    let mut cmd = Command::cargo_bin("ro").expect("ro binary should compile");
    cmd.arg("--config-dir")
        .arg(config_dir.path())
        .arg("--state-dir")
        .arg(state_dir.path())
        .arg("schema");
    cmd.assert().success();
    let out = cmd.output().expect("ro schema should run");
    serde_json::from_slice(&out.stdout).expect("ro schema must emit valid JSON")
}

/// Every argument of one command, as `(name, help)` pairs.
///
/// The bare arg name, not the long form. `ro schema` emits both — `name` is
/// the id, `long` is the invocable `--name` — and keying on `long` means
/// every comparison below has to remember the dashes, which is the mistake
/// that made the first version of this file report `push ----all`.
fn args_of(command: &Value) -> Vec<(String, Option<String>)> {
    command["args"]
        .as_array()
        .map(|args| {
            args.iter()
                .map(|a| {
                    let name = a["name"].as_str().unwrap_or_default().to_string();
                    let help = a["help"].as_str().map(|h| h.to_string());
                    (name, help)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Recursively: this command, then every subcommand of it.
fn walk(command: &Value, path: &str, out: &mut Vec<(String, String, Option<String>)>) {
    let name = command["name"].as_str().unwrap_or("?");
    let here = if path.is_empty() {
        name.to_string()
    } else {
        format!("{path} {name}")
    };
    for (arg, help) in args_of(command) {
        out.push((here.clone(), arg, help));
    }
    if let Some(subs) = command["subcommands"].as_array() {
        for sub in subs {
            walk(sub, &here, out);
        }
    }
}

#[test]
fn every_flag_on_every_verb_has_help_text() {
    let s = schema();
    let mut all = Vec::new();
    for command in s["commands"].as_array().expect("commands is an array") {
        walk(command, "", &mut all);
    }

    let mut missing: Vec<String> = all
        .iter()
        .filter(|(_, arg, help)| {
            arg.is_empty() || help.as_deref().is_none_or(|h| h.trim().is_empty())
        })
        .map(|(cmd, arg, _)| format!("{cmd} {arg}"))
        .collect();
    missing.sort();
    missing.dedup();

    assert!(
        missing.is_empty(),
        "these flags have no help text, so nobody can find out what they do:\n  {}\n\
         Write a real description for each — what it does, and when you would \
         reach for it. A flag nobody can discover is a flag that does not exist \
         for the person typing the command line.",
        missing.join("\n  ")
    );
}

/// The three that were found empty, named.
///
/// The walk above is the guard against the class returning; this is the guard
/// against the three known instances being quietly dropped from the tree
/// rather than documented.
#[test]
fn the_three_flags_that_shipped_empty_are_documented() {
    let s = schema();
    let mut all = Vec::new();
    for command in s["commands"].as_array().expect("commands is an array") {
        walk(command, "", &mut all);
    }

    // `engine_bin` is the arg id; the flag a user types is `--engine-bin`.
    // Matching on the id is what makes this test independent of clap's
    // naming convention between the two.
    for flag in ["all", "engine", "engine_bin"] {
        let documented: Vec<_> = all
            .iter()
            .filter(|(_, arg, _)| arg == flag)
            .filter(|(_, _, help)| help.as_deref().is_some_and(|h| h.trim().len() >= 20))
            .map(|(cmd, _, _)| cmd.clone())
            .collect();
        assert!(
            !documented.is_empty(),
            "--{flag} has no help text on any verb. It works; nobody can find out \
             what it does without reading the source."
        );
    }
}

/// A verb's own description is part of the same promise.
///
/// `ro config` with no subcommand prints its help, and a command whose `about`
/// is empty is a command the reader cannot tell from the one below it.
#[test]
fn every_verb_has_an_about() {
    let s = schema();
    let mut missing = Vec::new();
    for command in s["commands"].as_array().expect("commands is an array") {
        let name = command["name"].as_str().unwrap_or("?");
        let about = command["about"].as_str().unwrap_or("").trim();
        if about.is_empty() {
            missing.push(name.to_string());
        }
    }
    assert!(
        missing.is_empty(),
        "these commands have no description: {missing:?}"
    );
}
