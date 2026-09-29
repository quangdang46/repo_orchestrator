//! README.md must not promise things the binary does not do.
//!
//! Every claim here was measured against the binary and found false, and
//! each was fixed in the docs. A doc that lies about a CLI is worse than no
//! doc: a user follows it, gets an error, and concludes the tool is broken.
//!
//! The claims are the ones that are easy to state and easy to get wrong —
//! a verb's shape, an exit code, a dependency — rather than the prose around
//! them.

use std::path::Path;

/// The README, read from the workspace root.
///
/// Two `parent()` hops, not one: `CARGO_MANIFEST_DIR` is `crates/ro`, and the
/// documents live at the root beside `Cargo.toml`.
fn readme() -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("the workspace root");
    std::fs::read_to_string(root.join("README.md")).expect("README.md is readable")
}

/// The README's exit-code list must match what the binary actually returns.
///
/// It said "64 = bad usage". Clap usage errors are 2; 64 is a **ro** usage
/// error, and the two are produced by different layers for different reasons.
/// A script that cannot tell "you typed a flag wrong" from "you named a repo
/// that does not exist" cannot branch on the code it was given.
#[test]
fn the_readme_exit_codes_match_the_binary() {
    let readme = readme();

    // The two codes that are genuinely ambiguous, stated as such.
    assert!(
        readme.contains("64 is a **ro** usage error"),
        "README must say which layer produces 64"
    );
    assert!(
        readme.contains("clap's own usage error"),
        "README must say clap's usage errors are 2, not 64"
    );
    // And the old, wrong claim must not come back.
    assert!(
        !readme.contains("64 = bad usage"),
        "that sentence was measured false: clap usage errors exit 2"
    );
}

/// `ro sync` is not parallel, and the README must not say it is.
///
/// `core.parallel` bounds the fleet verbs — `ro ship`, `ro commit`, `ro push`
/// — which run repos concurrently. `ro sync` is a sequential loop over the
/// registry, and saying otherwise invites a user to expect a speedup that is
/// not there.
#[test]
fn the_readme_does_not_claim_parallel_sync() {
    let readme = readme();

    let sync_row = readme
        .lines()
        .find(|l| l.contains("First-class sync"))
        .expect("the README has a sync row in its feature table");
    assert!(
        !sync_row.contains("parallel"),
        "the sync row must not claim parallelism: {sync_row}"
    );
    // And the claim must be made where it is true, or not at all.
    assert!(
        readme.contains("Parallel fleet runs"),
        "the README must say which verbs are parallel"
    );
}

/// `ro plan`, `ro apply` and `ro rollback` do not exist.
///
/// The README's closing line promised them; all three exit 2 as
/// unrecognized subcommands, and FEATURES.md says so explicitly. A closing
/// line that names commands the tool does not have is the worst place for a
/// lie, because it is the last thing read.
#[test]
fn the_readme_does_not_promise_verbs_that_do_not_exist() {
    let readme = readme();

    for verb in ["ro plan", "ro apply", "ro rollback"] {
        assert!(
            !readme.contains(verb),
            "README promises {verb}, which exits 2 as an unrecognized subcommand"
        );
    }
}

/// `ro-testkit` is a dev-dependency and is not linked into the binary.
///
/// The README listed it among the crates the CLI depends on. It is compiled
/// into the test suite and never into the shipped binary — which is the right
/// place for it, and the reason the claim was wrong.
#[test]
fn the_readme_does_not_claim_the_testkit_is_linked() {
    let readme = readme();

    // The claim is two lines long and the qualifier is on the second, so
    // this reads the paragraph rather than the line: a test that greps one
    // line for a word that happens to be on the next passes a broken doc.
    let claim = readme
        .split("Workspace:")
        .nth(1)
        .and_then(|rest| rest.split("\n\n").next())
        .expect("the README names the workspace crates");
    assert!(
        claim.contains("ro-testkit"),
        "the paragraph should still name ro-testkit: {claim}"
    );
    assert!(
        claim.contains("dev-dependency"),
        "the README must say ro-testkit is a dev-dependency: {claim}"
    );
}
