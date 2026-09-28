//! What a name, a pattern or a filter means — and what happens when it
//! means nothing.
//!
//! # The property under test
//!
//! One resolver, so `ro status`, `ro sync` and `ro ship` cannot disagree
//! about what a name means. Three defects shared that rule and all broke it
//! the same way — by turning "matched nothing" into "matched everything", or
//! into a silent success:
//!
//!   * `ro sync --filter tag:nonexistent` selected the **entire fleet** and
//!     reported a clean run. `ro status` with the identical filter correctly
//!     returned nothing, so the two verbs disagreed about identical input.
//!   * `ro status <name>` for a name that matches nothing printed nothing on
//!     stdout, nothing on stderr, and exited 0 — while `ro sync <name>`
//!     exits 64, `ro tag <name>` exits 70, and `ro remove <name>` succeeds.
//!     Four verbs, four answers to "what does the name alpha mean".
//!   * `ro ship a b` — the documented plural form, and the example in three
//!     `--help` texts — selected nothing and exited 64, because the caller
//!     joined the names with a comma and the resolver splits on whitespace.

use assert_cmd::Command;
use predicates::prelude::*;
use ro_testkit::Worktree;
use tempfile::TempDir;

struct Test {
    config_dir: TempDir,
    state_dir: TempDir,
}

impl Test {
    fn new() -> Self {
        Self {
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
        cmd
    }

    fn initialised() -> Self {
        let t = Self::new();
        t.cmd().arg("init").assert().success();
        t
    }

    /// Register `path` and return **its own** `owner/name` label.
    ///
    /// Matched on `local_path`, never "the first row": two temp dirs in one
    /// test can produce the same owner, and picking the first row made two
    /// labels name the same repo.
    fn register(&self, path: &std::path::Path) -> String {
        self.cmd().arg("add").arg(path).assert().success();
        let out = self
            .cmd()
            .args(["list", "--format", "json"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let wanted = path.to_str().unwrap();
        text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("each line is a JSON object"))
            .find(|v| v["local_path"].as_str() == Some(wanted))
            .map(|v| format!("{}/{}", v["owner"].as_str().unwrap(), v["name"].as_str().unwrap()))
            .unwrap_or_else(|| panic!("no row has local_path {wanted:?}; ro list said:\n{text}"))
    }
}

/// Three repos, so "the whole fleet" is distinguishable from "one".
fn three_repos(t: &Test) -> Vec<(String, Worktree)> {
    let mut out = Vec::new();
    for _name in ["alpha", "beta", "gamma"] {
        let repo = Worktree::with_one_commit();
        let label = t.register(repo.path());
        out.push((label, repo));
    }
    out
}

/// A `--filter` that matches nothing must not select the whole fleet.
///
/// `ro sync` fed the resolver's output straight into
/// `.filter(|r| selected.is_empty() || selected.contains(&r.id))`, where an
/// empty list is indistinguishable from "no selector given" — and both mean
/// "everything". So `--filter tag:nonexistent` synced five repos and reported
/// a clean run. The selector was correct; the empty-result handling was not.
#[test]
fn a_filter_matching_nothing_does_not_sync_the_whole_fleet() {
    let t = Test::initialised();
    three_repos(&t);

    let out = t
        .cmd()
        .args(["sync", "--filter", "tag:nonexistent"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.trim().is_empty(),
        "a filter that matches nothing must sync nothing, not the fleet. Got:\n{stdout}"
    );
    assert_ne!(
        out.status.code(),
        Some(0),
        "a selector that matched nothing is a usage error, not a clean run"
    );
}

/// The same for `--tag`, which is shorthand for `--filter tag:<T>`.
#[test]
fn a_tag_matching_nothing_does_not_sync_the_whole_fleet() {
    let t = Test::initialised();
    three_repos(&t);

    let out = t.cmd().args(["sync", "--tag", "nonexistent"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.trim().is_empty(),
        "a tag that matches nothing must sync nothing, not the fleet. Got:\n{stdout}"
    );
}

/// `ro status` and `ro sync` must agree about what a filter means.
#[test]
fn status_and_sync_agree_about_an_empty_selection() {
    let t = Test::initialised();
    three_repos(&t);

    let sync_out = t
        .cmd()
        .args(["sync", "--filter", "tag:nonexistent"])
        .output()
        .unwrap();
    let status_out = t
        .cmd()
        .args(["status", "--filter", "tag:nonexistent"])
        .output()
        .unwrap();

    assert_eq!(
        String::from_utf8_lossy(&sync_out.stdout).trim(),
        String::from_utf8_lossy(&status_out.stdout).trim(),
        "the two verbs must not disagree about identical input"
    );
}

/// A bare `ro sync` on an empty registry stays exit 0.
///
/// The negative control the empty-selection rule needs: asking for
/// everything when there is nothing is not an error, so the "matched
/// nothing" refusal must be scoped to a *selector* being present.
#[test]
fn a_bare_sync_on_an_empty_registry_is_not_an_error() {
    let t = Test::initialised();
    t.cmd()
        .args(["sync"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

/// A name that matches nothing is a **usage** error on `ro status`, the same
/// as on `ro sync`. It used to print nothing on either stream and exit 0.
#[test]
fn a_status_name_matching_nothing_is_a_usage_error() {
    let t = Test::initialised();
    three_repos(&t);

    t.cmd()
        .args(["status", "nonexistentname"])
        .assert()
        .code(64)
        .stderr(predicate::str::contains("nonexistentname"));
}

/// A good name among bad ones still works, but the bad one is named.
#[test]
fn a_status_name_that_matches_is_not_an_error() {
    let t = Test::initialised();
    let repos = three_repos(&t);

    t.cmd()
        .args(["status", &repos[0].0])
        .assert()
        .success()
        .stdout(predicate::str::contains(&repos[0].0));
}

/// The documented plural positional form must select both repos.
///
/// `--help` on `ship`, `push` and `commit` gives `ro sync cass
/// voice-ai-agent` — two names — as the canonical invocation. The caller
/// joined them with a comma and the resolver splits on whitespace, so the
/// comma-joined string compiled as a single glob that matched nothing.
#[test]
fn a_plural_positional_selects_every_named_repo() {
    let t = Test::initialised();
    let repos = three_repos(&t);

    let out = t
        .cmd()
        .args(["ship", "--dry-run", "--engine", "git", &repos[0].0, &repos[1].0])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains(&repos[0].0),
        "the first named repo must be selected. Got:\n{stdout}"
    );
    assert!(
        stdout.contains(&repos[1].0),
        "the second named repo must be selected. Got:\n{stdout}"
    );
    assert!(
        !stdout.contains(&repos[2].0),
        "a repo nobody named must not be selected. Got:\n{stdout}"
    );
}

/// A narrower request that matched nothing must NOT be widened to the fleet
/// by `--all`.
///
/// `--all --pattern 'work/nomatch*'` acted on every repo in the fleet and
/// then reported "4 failed" as though four repos were the request. A typo
/// plus `--all` committing the whole fleet is the exact failure the
/// resolver's own comment says it prevents: "`--all` means 'no narrower
/// request given', not 'ignore any that was'". With an empty pattern the
/// narrower request **is** given and **is** being ignored.
#[test]
fn all_plus_a_pattern_matching_nothing_does_not_widen_to_the_fleet() {
    let t = Test::initialised();
    three_repos(&t);

    let out = t
        .cmd()
        .args(["ship", "--dry-run", "--engine", "git", "--all", "--pattern", "work/nomatch*"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert_ne!(
        out.status.code(),
        Some(0),
        "a narrower request that matched nothing is a usage error, not a \
         successful run over the whole fleet. Got:\n{stdout}"
    );
    assert!(
        !stdout.contains("work/"),
        "no repo may be acted on when the only request matched nothing. Got:\n{stdout}"
    );
}

/// The same for a `--tag` that matches nothing. The old message even
/// suggested `--all` as the remedy for the command that already had it.
#[test]
fn all_plus_a_tag_matching_nothing_is_a_usage_error() {
    let t = Test::initialised();
    three_repos(&t);

    t.cmd()
        .args(["commit", "--dry-run", "--engine", "git", "--all", "--tag", "nonexistent"])
        .assert()
        .code(64);
}

/// A `--tag` that matches nothing is a usage error even **without** `--all`.
#[test]
fn a_tag_matching_nothing_is_a_usage_error() {
    let t = Test::initialised();
    three_repos(&t);

    t.cmd()
        .args(["commit", "--dry-run", "--engine", "git", "--tag", "nonexistent"])
        .assert()
        .code(64);
}

/// The negative control: a bare `ro commit` on an empty registry is still a
/// clean exit 0. "Nothing to do" and "nothing asked for" must stay distinct.
#[test]
fn a_bare_commit_on_an_empty_registry_is_not_an_error() {
    let t = Test::initialised();
    t.cmd()
        .args(["commit", "--dry-run", "--engine", "git"])
        .assert()
        .success();
}

/// A repo that was actually selected still runs, so the "matched nothing"
/// refusal cannot be satisfied by refusing everything.
///
/// Asserts on **selection**, not on the exit code: the fixtures sit on
/// `main`, which is protected, so the run itself legitimately exits 2. What
/// matters here is that the named repo got as far as producing a row.
#[test]
fn a_matching_pattern_still_runs() {
    let t = Test::initialised();
    let repos = three_repos(&t);

    let out = t
        .cmd()
        .args(["commit", "--dry-run", "--engine", "git", "--pattern", &repos[0].0])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains(&repos[0].0),
        "a matching pattern must select the repo. Got:\n{stdout}"
    );
    assert!(
        !stdout.contains(&repos[1].0),
        "a matching pattern must not select anything else. Got:\n{stdout}"
    );
    assert_ne!(
        out.status.code(),
        Some(64),
        "a pattern that matched must not be refused as a usage error. Got:\n{stdout}"
    );
}

/// `--all` is a default, not an override.
///
/// It used to be computed as `if all { None } else { ... }`, so
/// `--all --pattern <name>` silently discarded the pattern and ran the
/// whole fleet — the exact "a narrower request was discarded in favour of
/// everything" failure, arriving through a different door than the one the
/// comment above it claimed to have fixed.
///
/// This is a fleet tool: a typo'd `--pattern` combined with `--all`
/// committing every repository in the registry is the failure that costs
/// someone a revert at 5pm.
#[test]
fn all_does_not_discard_a_narrower_request() {
    let t = Test::initialised();
    let (alpha_label, alpha) = {
        let repo = Worktree::with_one_commit();
        let label = t.register(repo.path());
        (label, repo)
    };
    let (_beta_label, beta) = {
        let repo = Worktree::with_one_commit();
        let label = t.register(repo.path());
        (label, repo)
    };

    // Feature branches: `main` is protected, and a refusal would prove
    // nothing about selection.
    for r in [&alpha, &beta] {
        ro_testkit::worktree::run(r.path(), &["checkout", "-q", "-b", "feat/x"]);
    }

    // Both dirty. Only the named one may be committed.
    alpha.write("a.txt", "x\n");
    beta.write("b.txt", "x\n");

    // The pattern is the narrower request; `--all` must not win.
    t.cmd()
        .args([
            "commit",
            "--all",
            "--pattern",
            &alpha_label,
            "--engine",
            "git",
        ])
        .assert()
        .success();

    assert!(
        alpha.porcelain().is_empty(),
        "the named repo should have been committed, got: {}",
        alpha.porcelain()
    );
    assert!(
        beta.porcelain().contains("b.txt"),
        "`--all` discarded the narrower --pattern and committed the whole \
         fleet. The untouched repo is still dirty: {}",
        beta.porcelain()
    );
}

/// `ro ship alpha` — the form the help text promises — must select alpha.
///
/// Every verb's `--help` says "Repos to act on, by name or alias", and
/// three of the five worked. The bare `name` did not: the matcher compared
/// the token against the full `owner/name` label, the alias, and the id.
/// Since `*` does not cross a `/` in globset, the token was not a glob
/// that matched either — so the documented invocation selected nothing and
/// reported it in the tone of a successful no-op.
#[test]
fn a_bare_name_selects_the_repo_the_help_text_promises() {
    let t = Test::initialised();
    let (alpha_label, alpha) = {
        let repo = Worktree::with_one_commit();
        let label = t.register(repo.path());
        (label, repo)
    };
    let (_beta_label, beta) = {
        let repo = Worktree::with_one_commit();
        let label = t.register(repo.path());
        (label, repo)
    };
    for r in [&alpha, &beta] {
        ro_testkit::worktree::run(r.path(), &["checkout", "-q", "-b", "feat/x"]);
    }

    alpha.write("a.txt", "x\n");
    beta.write("b.txt", "x\n");

    // The bare name, not `owner/name`.
    let bare = alpha_label.rsplit('/').next().expect("a label has a name").to_string();
    t.cmd()
        .args(["commit", &bare, "--engine", "git"])
        .assert()
        .success();

    assert!(
        alpha.porcelain().is_empty(),
        "the repo named by the bare token should have been committed, got: {}",
        alpha.porcelain()
    );
    assert!(
        beta.porcelain().contains("b.txt"),
        "a bare name must select that repo and NOT the whole fleet. \
         The other repo is still dirty: {}",
        beta.porcelain()
    );
}
