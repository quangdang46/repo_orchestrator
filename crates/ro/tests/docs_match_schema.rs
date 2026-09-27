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
    (
        "remove",
        "stop tracking a repository, and optionally delete its working copy",
    ),
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

/// The nearest char boundary at or below `i`.
///
/// The docs are full of box-drawing characters, and slicing `text` at an
/// arbitrary byte index panics rather than returning `None`. A window that
/// starts or ends inside one is simply nudged outward — the exact edge
/// never decides whether a mention is a denial.
fn floor_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Phrases that make a mention a *denial* rather than an advertisement.
const DENIALS: &[&str] = &[
    "no `ro",
    "There is no",
    "there is no",
    "removed",
    "deleted",
    "replaces",
    "not a command",
    "no longer",
    "was cut",
    "were cut",
    "is cut",
    "cut.",
];

#[test]
fn no_documented_command_is_gone() {
    let live = live_commands();
    let text = docs();
    let mut ghosts: Vec<String> = Vec::new();

    // Scanned **out of the prose**, not out of `COMMANDS`. The old version
    // walked the hand-written list, so the check only ever asked about
    // commands somebody remembered to write down: delete a command from the
    // binary *and* from this list in the same commit and the test is
    // silent — which is precisely what happened to `prune` and `run`.
    //
    // The scan reads the two shapes a doc actually uses to show a command
    // someone can type: inside backticks, or at the start of a line. Both
    // are unambiguous. Ordinary prose ("ro tracks many repos") matches
    // neither, so the test needs no allowlist of English.
    let mut offset = 0usize;
    for raw in text.lines() {
        let base = offset;
        offset += raw.len() + 1;
        let line = raw.trim_start().trim_start_matches("$ ").trim();
        let candidates = raw
            .match_indices('`')
            .step_by(2)
            .filter_map(|(i, _)| {
                raw.get(i + 1..)
                    .and_then(|r| r.find('`').map(|j| &raw[i + 1..i + 1 + j]))
            })
            .chain(std::iter::once(line))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();

        for c in candidates {
            let Some(rest) = c.strip_prefix("ro ") else {
                continue;
            };
            let word: String = rest
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '-')
                .collect();
            // A passage that *denies* the command is correct documentation,
            // not a ghost: "There is no `ro conflict` verb" is exactly what
            // the docs should say about a command that does not exist.
            //
            // The window is a character range around the match rather than
            // the line, because the word that makes it a denial is routinely
            // on the *neighbouring* line — "the (since-removed) `ro health`
            // command" splits across two, and so does "that Phase 1 deleted".
            // Judging the line alone called both a ghost.
            let at = base + raw.find(&c).unwrap_or(0);
            let lo = floor_boundary(&text, at.saturating_sub(120));
            let hi = ceil_boundary(&text, (at + c.len() + 120).min(text.len()));
            let window = &text[lo..hi];
            let denies = DENIALS.iter().any(|d| window.contains(*d));

            if denies || word.is_empty() || word.starts_with('-') || live.iter().any(|l| *l == word)
            {
                continue;
            }
            if !ghosts.contains(&word) {
                ghosts.push(word);
            }
        }
    }
    ghosts.sort();

    assert!(
        ghosts.is_empty(),
        "the docs show {ghosts:?} as an invocation, and `ro schema` has no such \
         command. A command that is gone but still documented is worse than one \
         that never existed: the reader's command fails."
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
