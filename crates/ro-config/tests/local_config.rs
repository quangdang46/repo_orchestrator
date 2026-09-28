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

use ro_config::local::{ensure_gitignored, RepoLocalConfig, LOCAL_IGNORE};
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
    let c = RepoLocalConfig::load(tmp.path()).unwrap().expect("file exists");
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
    write(tmp.path(), "author = \"personal\"\ncredential = \"env:PERSONAL\"\n");
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
    let err = RepoLocalConfig::load(tmp.path())
        .expect_err("a typo must fail");
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
        std::fs::write(tmp.path().join(".gitignore"), format!("target/\n{existing}\n")).unwrap();
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
