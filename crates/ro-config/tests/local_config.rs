//! The per-repo config file: `.ro/config.local.toml`.
//!
//! The registry is this machine's registry. It is the right layer for the
//! common case, and it cannot cover a repo you did not register, a
//! colleague's clone, or a setting that has to travel with the code. That
//! is what this file is for, and the tests below pin the three properties
//! that make it safe to have at all:
//!
//!   1. **a missing file is not an error** — most repos will not have one
//!   2. **the file outranks the registry row** — otherwise it cannot correct it
//!   3. **a typo is an error, not a setting that does nothing** — the same
//!      rule `ro.local.toml` never got
//!
//! And the one that is a safety property rather than a convenience:
//!
//!   4. **the gitignore entry is a directory pattern, and idempotent**

use ro_config::local::{LOCAL_IGNORE, RepoLocalConfig, ensure_gitignored};
use std::path::Path;
use tempfile::TempDir;

fn write(repo: &Path, body: &str) {
    let dir = repo.join(".ro");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.local.toml"), body).unwrap();
}

#[test]
fn a_missing_file_is_not_an_error() {
    let tmp = TempDir::new().unwrap();
    assert!(
        RepoLocalConfig::load(tmp.path()).unwrap().is_none(),
        "most repos will not have this file; a missing one is the design"
    );
}

#[test]
fn a_full_file_parses() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        r#"
author = "work"
credential = "env:GH_WORK"
engine = "codex"
engine-args = "--sandbox danger"
"#,
    );
    let c = RepoLocalConfig::load(tmp.path())
        .unwrap()
        .expect("file exists");
    assert_eq!(c.author.as_deref(), Some("work"));
    assert_eq!(c.credential.as_deref(), Some("env:GH_WORK"));
    assert_eq!(c.engine.as_deref(), Some("codex"));
    assert_eq!(c.engine_args.as_deref(), Some("--sandbox danger"));
}

#[test]
fn a_partial_file_is_a_partial_overlay() {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "engine = \"codex\"\n");
    let c = RepoLocalConfig::load(tmp.path()).unwrap().unwrap();

    // The row's values survive; only the one named key changes.
    let (mut author, mut cred, mut engine, mut args) = (
        Some("work".to_string()),
        Some("env:ROW".to_string()),
        Some("claude".to_string()),
        None::<String>,
    );
    c.apply_to(&mut author, &mut cred, &mut engine, &mut args);

    assert_eq!(author.as_deref(), Some("work"), "untouched keys inherit");
    assert_eq!(cred.as_deref(), Some("env:ROW"), "untouched keys inherit");
    assert_eq!(engine.as_deref(), Some("codex"), "the named key overrides");
    assert_eq!(args, None, "untouched keys inherit");
}

#[test]
fn the_file_outranks_the_row() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "author = \"personal\"\ncredential = \"env:PERSONAL\"\n",
    );
    let c = RepoLocalConfig::load(tmp.path()).unwrap().unwrap();

    let (mut author, mut cred, _, _) = (
        Some("work".to_string()),
        Some("env:ROW".to_string()),
        None::<String>,
        None::<String>,
    );
    c.apply_to(&mut author, &mut cred, &mut None.clone(), &mut None.clone());
    assert_eq!(author.as_deref(), Some("personal"), "the file wins");
    assert_eq!(cred.as_deref(), Some("env:PERSONAL"), "the file wins");
}

#[test]
fn a_typo_is_an_error_rather_than_a_key_that_does_nothing() {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "authent = \"work\"\n");
    let err = RepoLocalConfig::load(tmp.path()).expect_err("a typo must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("authent") || msg.contains("unknown field"),
        "the error should name the offending key, got: {msg}"
    );
}

#[test]
fn an_empty_config_is_empty_and_worth_not_writing() {
    let c = RepoLocalConfig::default();
    assert!(
        c.is_empty(),
        "seeding an all-None file writes a file that says nothing, and a file \
         that mirrors the row is a second copy that drifts on the first \
         `ro config set`"
    );
}

#[test]
fn the_gitignore_entry_is_a_directory_pattern() {
    // Not the filename: the moment ro adds a second thing under `.ro/` —
    // a cache, a state marker — a filename entry stops being correct and
    // that file shows up as untracked.
    assert_eq!(LOCAL_IGNORE, ".ro/");
    assert!(
        LOCAL_IGNORE.ends_with('/'),
        "a filename pattern would be correct only until the directory grew"
    );
}

#[test]
fn the_gitignore_entry_is_written_once() {
    let tmp = TempDir::new().unwrap();
    assert!(ensure_gitignored(tmp.path()).unwrap(), "first call writes");
    assert!(!ensure_gitignored(tmp.path()).unwrap(), "second is a no-op");
    assert!(!ensure_gitignored(tmp.path()).unwrap(), "third is a no-op");

    let text = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
    assert_eq!(
        text.matches(LOCAL_IGNORE).count(),
        1,
        "the entry must not duplicate:\n{text}"
    );
}

#[test]
fn an_existing_entry_in_any_spelling_is_left_alone() {
    for existing in [".ro/", "/.ro/", "  .ro/  ", "/.ro"] {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".gitignore"),
            format!("target/\n{existing}\n"),
        )
        .unwrap();
        assert!(
            !ensure_gitignored(tmp.path()).unwrap(),
            "{existing:?} already ignores the directory; adding again duplicates it"
        );
    }
}

#[test]
fn an_existing_ignore_file_is_never_rewritten_out_of_order() {
    let tmp = TempDir::new().unwrap();
    let before = "target/\nnode_modules/\n";
    std::fs::write(tmp.path().join(".gitignore"), before).unwrap();
    ensure_gitignored(tmp.path()).unwrap();
    let after = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
    assert!(
        after.starts_with(before),
        "existing lines must survive untouched and in order:\n{after}"
    );
}

#[test]
fn a_file_without_a_trailing_newline_is_not_corrupted() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".gitignore"), "target/").unwrap();
    ensure_gitignored(tmp.path()).unwrap();
    let after = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
    assert!(
        after.starts_with("target/\n"),
        "the last existing line needs its newline, or the new entry concatenates onto it:\n{after}"
    );
    assert!(after.contains(LOCAL_IGNORE));
}

// ── Precedence: every key must actually take effect ─────────────────────
//
// The documented chain is flag > local file > registry row > config file.
// Last wave fixed `credential_ref` — it was merged into a local variable and
// then thrown away, so the row's value was what left the machine. These tests
// pin the other three, because the same shape is invisible from the outside:
// the file parses, the run succeeds, and the setting does nothing.

/// `author` outranks the row's `author_ref`, and the name resolves against
/// `[identity.*]` in the global config.
#[test]
fn the_local_files_author_outranks_the_row() {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "author = \"personal\"\n");
    let c = RepoLocalConfig::load(tmp.path()).unwrap().unwrap();

    let (mut author, mut cred, mut engine, mut args) = (
        Some("work".to_string()),
        Some("env:ROW".to_string()),
        Some("claude".to_string()),
        None::<String>,
    );
    c.apply_to(&mut author, &mut cred, &mut engine, &mut args);

    assert_eq!(author.as_deref(), Some("personal"), "the file wins");
    assert_eq!(cred.as_deref(), Some("env:ROW"), "untouched keys inherit");
    assert_eq!(engine.as_deref(), Some("claude"), "untouched keys inherit");
    assert_eq!(args, None, "untouched keys inherit");
}

/// `engine` outranks the row's `engine`.
///
/// This is the key that is inert in the shipped code. `plan_for` merges the
/// file's value into a local and then resolves the engine from the value
/// *passed in* — the CLI flag or `[agent] engine` — so the file's `engine` is
/// read, merged, and discarded. The merge is correct; the consumer is not.
/// The fix is in `crates/ro/src/ship/emit.rs`, which this wave does not own.
#[test]
fn the_local_files_engine_outranks_the_row() {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "engine = \"codex\"\n");
    let c = RepoLocalConfig::load(tmp.path()).unwrap().unwrap();

    let (mut author, mut cred, mut engine, mut args) = (
        Some("work".to_string()),
        Some("env:ROW".to_string()),
        Some("claude".to_string()),
        None::<String>,
    );
    c.apply_to(&mut author, &mut cred, &mut engine, &mut args);

    assert_eq!(engine.as_deref(), Some("codex"), "the file wins");
    assert_eq!(author.as_deref(), Some("work"), "untouched keys inherit");
    assert_eq!(cred.as_deref(), Some("env:ROW"), "untouched keys inherit");
}

/// `engine-args` outranks the row's `engine_args`.
///
/// Same shape as `engine`, and the same defect: `plan_for` merges it into a
/// local that nothing reads. The row's `engine_args` is what reaches the
/// engine, so a per-repo `--sandbox danger` is silently dropped.
#[test]
fn the_local_files_engine_args_outranks_the_row() {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "engine-args = \"--sandbox danger\"\n");
    let c = RepoLocalConfig::load(tmp.path()).unwrap().unwrap();

    let (mut author, mut cred, mut engine, mut args) = (
        Some("work".to_string()),
        Some("env:ROW".to_string()),
        Some("claude".to_string()),
        Some("--model opus".to_string()),
    );
    c.apply_to(&mut author, &mut cred, &mut engine, &mut args);

    assert_eq!(args.as_deref(), Some("--sandbox danger"), "the file wins");
    assert_eq!(engine.as_deref(), Some("claude"), "untouched keys inherit");
}

/// All four at once, so the merge is checked as a whole rather than one key
/// at a time.
#[test]
fn all_four_keys_outrank_the_row_together() {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "author = \"personal\"\ncredential = \"env:PERSONAL\"\nengine = \"codex\"\nengine-args = \"--sandbox danger\"\n",
    );
    let c = RepoLocalConfig::load(tmp.path()).unwrap().unwrap();

    let (mut author, mut cred, mut engine, mut args) = (
        Some("work".to_string()),
        Some("env:ROW".to_string()),
        Some("claude".to_string()),
        Some("--model opus".to_string()),
    );
    c.apply_to(&mut author, &mut cred, &mut engine, &mut args);

    assert_eq!(author.as_deref(), Some("personal"));
    assert_eq!(cred.as_deref(), Some("env:PERSONAL"));
    assert_eq!(engine.as_deref(), Some("codex"));
    assert_eq!(args.as_deref(), Some("--sandbox danger"));
}

/// A local file naming an unknown profile is a clear error, not a silent
/// fall-back to the row's author.
///
/// The error is raised where the name is resolved — `IdentityConfig::resolve`
/// in `ro-config/src/schema.rs` — which is the one place that knows what the
/// valid names are. A fall-back here would commit every repo under the row's
/// identity while the user believed the file had pinned their own.
#[test]
fn an_unknown_profile_in_the_local_file_is_an_error_not_a_fallback() {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "author = \"nosuchprofile\"\n");
    let c = RepoLocalConfig::load(tmp.path()).unwrap().unwrap();

    // The file loads — it is well-formed. The error is at resolution, which
    // is where a name is checked against the profiles that exist.
    let mut author = c.author.clone();
    let (mut cred, mut engine, mut args) = (
        Some("env:ROW".to_string()),
        Some("claude".to_string()),
        None::<String>,
    );
    c.apply_to(&mut author, &mut cred, &mut engine, &mut args);
    assert_eq!(author.as_deref(), Some("nosuchprofile"), "the file wins");

    // And resolving it against an `[identity]` table that does not have it
    // is an error naming the known profiles.
    let profiles = ro_config::schema::IdentityConfig::default();
    let err = profiles
        .resolve("nosuchprofile")
        .expect_err("an unknown profile must not resolve");
    let msg = err.to_string();
    assert!(
        msg.contains("nosuchprofile"),
        "the error must name the profile that failed, got: {msg}"
    );
}

/// A local file naming an unknown engine is not silently accepted, and the
/// three built-ins are the whole set.
///
/// `ro-config` does not depend on `ro-engine`, so the check itself lives
/// there — `ro_engine::dispatch::resolve` turns an unknown name into an error
/// naming the three, rather than a fall-through to the raw backend. What is
/// pinned *here* is the half this crate owns: the file carries the name
/// through unchanged and without validation of its own, so the two layers
/// cannot disagree about which names exist.
#[test]
fn the_local_files_engine_is_carried_through_unchanged_for_the_engine_layer_to_check() {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "engine = \"cladue\"\n");
    let c = RepoLocalConfig::load(tmp.path()).unwrap().unwrap();

    // Not validated here, and not defaulted either: the value arrives at the
    // engine layer verbatim, which is where the "unknown engine" error with
    // the three names lives.
    assert_eq!(
        c.engine.as_deref(),
        Some("cladue"),
        "this crate must not rewrite the name; ro-engine owns that error"
    );
    assert_eq!(
        RepoLocalConfig::keys(),
        &["author", "credential", "engine", "engine-args"],
        "and the accepted keys are exactly the four the file documents"
    );
}
