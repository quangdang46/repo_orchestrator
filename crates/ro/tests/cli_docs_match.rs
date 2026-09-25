//! Guard against the docs advertising commands the binary does not have.
//!
//! Three command removals in a row shipped with the documentation still
//! describing them: `ro import` survived in four places, `ro self-update` in
//! five, `ro fork` in four. None was catchable by the compiler or the test
//! suite — a command name in a markdown table, or inside the hand-written
//! `json!` literal `ro robot-docs` emitted, is neither compiled nor
//! executed. Every one was found by grep, or by a reviewer reading a diff,
//! and each cost a round trip.
//!
//! This is the mechanical version of that grep.
//!
//! **One direction only.** Docs to tree: a documented command that no longer
//! exists is a broken promise, and that is a correctness problem. Tree to
//! docs: a command the docs happen not to mention is a style opinion, and
//! asserting it would fail on taste rather than on fact. So the rule is
//! "every command ro tells you about must be a command ro has", and a new
//! command does not have to be documented in order to pass.

use assert_cmd::Command;

/// Files whose `ro <word>` mentions are promises ro makes to a reader.
const DOCS: &[&str] = &["README.md", "FEATURES.md"];

/// Read the top-level command names out of `ro --help`.
///
/// Parsing the help text rather than importing the clap tree is deliberate:
/// `ro` is a binary crate with no lib target, so an integration test has no
/// way to call `Cli::command()` directly. The help text *is* what a reader
/// sees, so it is the right authority for this assertion.
fn live_commands() -> Vec<String> {
    let mut cmd = Command::cargo_bin("ro").expect("ro binary should compile");
    cmd.arg("--help");
    cmd.assert().success();
    let out = cmd.output().expect("ro --help should run");
    let help = String::from_utf8(out.stdout).expect("help should be UTF-8");

    let mut names = Vec::new();
    let mut in_commands = false;
    for line in help.lines() {
        let trimmed = line.trim();
        if trimmed == "Commands:" {
            in_commands = true;
            continue;
        }
        if in_commands {
            // The section ends at the next heading ("Options:", "Arguments:", …).
            if trimmed.ends_with(':') && !trimmed.is_empty() {
                break;
            }
            // Command lines are indented and start with the bare name.
            if line.starts_with("  ") && !trimmed.is_empty() {
                let name: String = trimmed
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                    .collect();
                if !name.is_empty() {
                    names.push(name);
                }
            }
        }
    }
    assert!(
        !names.is_empty(),
        "could not parse any commands out of `ro --help`:\n{help}"
    );
    names
}

fn doc_text() -> String {
    let mut out = String::new();
    for f in DOCS {
        let p = format!("{}/../../{f}", env!("CARGO_MANIFEST_DIR"));
        match std::fs::read_to_string(&p) {
            Ok(t) => {
                out.push_str(&t);
                out.push('\n');
            }
            Err(e) => panic!("could not read {f} at {p}: {e}"),
        }
    }
    out
}

#[test]
fn every_command_named_in_the_docs_exists() {
    let live = live_commands();
    let text = doc_text();

    let mut claimed: Vec<String> = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix("ro ") else {
            continue;
        };
        let word: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        // A bare flag, or a trailing period from prose, is not a command.
        if word.is_empty() || word.starts_with('-') {
            continue;
        }
        let word = word.trim_end_matches('.').to_string();
        if !word.is_empty() && !claimed.contains(&word) {
            claimed.push(word);
        }
    }

    let mut missing: Vec<String> = claimed
        .into_iter()
        .filter(|w| !live.contains(w))
        .collect();
    missing.sort();

    assert!(
        missing.is_empty(),
        "the docs name command(s) that `ro` no longer has: {missing:?}\n\
         live commands: {live:?}\n\
         Either the command came back, or the docs still describe one that was cut."
    );
}
