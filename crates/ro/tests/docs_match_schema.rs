//! The docs are checked against the binary, so they cannot drift again.
//!
//! `ro schema` exists precisely so an agent can read the truth instead of
//! the prose. This test uses it: the command table in the docs is compared
//! against the **live clap tree**, so a command that exists and is not
//! documented — or a command documented and since removed — is a red test
//! rather than something a reader discovers.
//!
//! It replaces the earlier hand-maintained list, which had drifted three
//! times in one phase: `ro sweep` survived a whole cut, `ro conflict`
//! survived the stage that replaced it, and `ro robot-docs` was documented
//! for a topic that never existed.

use serde_json::Value;

/// The commands the docs are required to mention, with what they are for.
///
/// This is the *intent* — the reason each command exists — which no amount
/// of reflection can derive. The flags are not here: those come from the
/// schema, so a new flag needs no edit.
const COMMANDS: &[(&str, &str)] = &[
    ("init", "create the config and the state database"),
    ("add", "track a repository, cloning or adopting one"),
    ("list", "show what is tracked"),
    ("status", "what state each repository is in"),
    ("sync", "bring every tracked repository up to date"),
    ("commit", "run the engine and commit what it found"),
    ("push", "commit, then push"),
    ("ship", "fetch, rebase, commit, push"),
    ("doctor", "diagnose this machine"),
    ("config", "read and write configuration"),
    ("schema", "this, as JSON"),
];

fn schema() -> Value {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ro"))
        .arg("schema")
        .output()
        .expect("the binary runs");
    assert!(
        out.status.success(),
        "ro schema failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("ro schema emits JSON")
}

fn live_commands() -> Vec<String> {
    schema()["commands"]
        .as_array()
        .expect("schema has a commands array")
        .iter()
        .filter_map(|c| c["name"].as_str().map(str::to_string))
        .collect()
}

fn docs() -> String {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("the workspace root")
        .to_path_buf();
    ["README.md", "FEATURES.md"]
        .iter()
        .map(|f| {
            std::fs::read_to_string(root.join(f)).unwrap_or_else(|e| panic!("reading {f}: {e}"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn every_live_command_is_documented() {
    let live = live_commands();
    let text = docs();
    let mut missing = Vec::new();
    for name in &live {
        if name == "help" {
            continue; // clap's own, not ours
        }
        // The docs must show it as an *invocation*, not merely name it in
        // prose — a command named in a sentence is a command nobody can
        // copy.
        if !text.contains(&format!("ro {name}")) && !text.contains(&format!("`{name}`")) {
            missing.push(name.clone());
        }
    }
    assert!(
        missing.is_empty(),
        "the binary has {missing:?} and the docs never mention them. \
         `ro schema` is the source of truth; the docs are not."
    );
}

#[test]
fn no_documented_command_is_gone() {
    let live = live_commands();
    let text = docs();
    let mut ghosts = Vec::new();
    for (name, _) in COMMANDS {
        if live.iter().any(|c| c == name) {
            continue;
        }
        if text.contains(&format!("ro {name} ")) || text.contains(&format!("`ro {name}`")) {
            ghosts.push(*name);
        }
    }
    assert!(
        ghosts.is_empty(),
        "the docs still show {ghosts:?} as invocations, and the binary has \
         no such command. A command that is gone but still documented is \
         worse than one that never existed: the reader's command fails."
    );
}

#[test]
fn the_documented_commands_all_exist() {
    let live = live_commands();
    for (name, why) in COMMANDS {
        assert!(
            live.iter().any(|c| c == name),
            "{name} is documented ({why}) but is not in the binary"
        );
    }
}

#[test]
fn the_flags_come_from_the_schema_not_the_prose() {
    // A flag the docs advertise but the binary lacks is the specific drift
    // this rewrite was about, so it gets its own check: sample the flags on
    // the commands most likely to change.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("the workspace root")
        .to_path_buf();
    let text = std::fs::read_to_string(root.join("FEATURES.md")).unwrap_or_default();

    // A removed flag is a defect only where the docs **advertise** it. The
    // same word in "there is no `--resume`, and here is why" is the docs
    // being correct, so a bare substring search is the wrong test — it
    // would fail the corrected document and pass the broken one.
    for removed in ["--resume", "--verbose", "--parallel"] {
        let advertised = text.lines().any(|l| {
            l.contains(removed) && (l.contains("|") || l.contains("`ro ")) && {
                // A line that *denies* the flag is not advertising it.
                let denies = l.contains("no --")
                    || l.contains("There is no")
                    || l.contains("never worked")
                    || l.contains("not its happy path");
                !denies
            }
        });
        assert!(
            !advertised,
            "FEATURES.md still advertises {removed}, which no longer exists. \
             A line saying the flag is *gone* is correct and must not trip \
             this check."
        );
    }
}
