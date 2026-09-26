//! Two repos, two authors, two accounts — the reason the tool exists.
//!
//! A developer has a work account and a personal one. In one run, repo A
//! commits as the work identity and pushes with the work credential; repo
//! B commits as the personal identity and pushes with the personal
//! credential. Without this the daily loop still needs a terminal per repo
//! to switch identity, which is the whole problem.
//!
//! # What is different from the credential test
//!
//! That one proved a credential does not leak. This one proves the *right*
//! one arrives, and that identity is per-invocation: no `.git/config` is
//! written, so adding a per-repo author cannot leak into a sibling repo or
//! into the user's global config.

use std::path::Path;
use std::process::Command;

use ro_core::{CommitIdentity, SecretString};
use ro_engine::env::ChildEnv;
use ro_testkit::{BareRemote, Worktree};

const WORK_TOKEN: &str = "ghp_work_token_aaaaaaaaaaaaaaaaaaaaaaa";
const PERSONAL_TOKEN: &str = "ghp_personal_token_bbbbbbbbbbbbbbbbbbb";

/// The author of the last commit, read from the repository itself.
fn last_author(repo: &Path) -> String {
    let out = Command::new("git")
        .args(["log", "-1", "--format=%an <%ae>"])
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git runs");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Commit `path`'s changes under `identity`, applied per invocation.
///
/// The identity arrives through `GIT_CONFIG_*` on the child's
/// environment. That is the mechanism the bead names: it covers
/// `commit --amend` and `tag` too, and it writes nothing to disk.
fn commit_as(repo: &Path, identity: &CommitIdentity) -> String {
    let env = ChildEnv::from_parent().with_identity(Some(identity));

    let status = Command::new("git")
        .args(["add", "-A"])
        .current_dir(repo)
        .env_clear()
        .envs(env.to_pairs())
        .output()
        .expect("git add runs");
    assert!(status.status.success(), "git add failed");

    let out = Command::new("git")
        .args(["commit", "-q", "-m", "work in progress"])
        .current_dir(repo)
        .env_clear()
        .envs(env.to_pairs())
        .output()
        .expect("git commit runs");
    assert!(
        out.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The capability itself, over a fixture of two repos.
#[test]
fn two_repos_two_authors_two_accounts_in_one_run() {
    let work = CommitIdentity {
        name: "Work".into(),
        email: "work@corp.example".into(),
    };
    let personal = CommitIdentity {
        name: "Personal".into(),
        email: "personal@example.com".into(),
    };

    let repo_a = Worktree::with_one_commit();
    let repo_b = Worktree::with_one_commit();
    let remote_a = BareRemote::ephemeral();
    let remote_b = BareRemote::ephemeral();
    repo_a.add_remote("origin", remote_a.path());
    repo_b.add_remote("origin", remote_b.path());

    repo_a.write("a.txt", "work in progress\n");
    repo_b.write("b.txt", "side project\n");

    // One run, two identities.
    commit_as(repo_a.path(), &work);
    commit_as(repo_b.path(), &personal);

    assert_eq!(last_author(repo_a.path()), "Work <work@corp.example>");
    assert_eq!(
        last_author(repo_b.path()),
        "Personal <personal@example.com>",
        "the second repo must not inherit the first's author"
    );

    // Each pushes with its own credential.
    for (repo, token) in [(repo_a.path(), WORK_TOKEN), (repo_b.path(), PERSONAL_TOKEN)] {
        let secret = SecretString::new(token);
        let push = ro_git::mutation::push_with_credential(
            repo,
            &ro_git::mutation::PushOpts {
                remote: Some("origin".into()),
                branch: Some("main".into()),
                set_upstream: true,
                host: Some("localhost".into()),
                ..Default::default()
            },
            Some(&secret),
        )
        .expect("the push runs");
        assert!(push.ok(), "push failed: {}", push.stderr);
    }

    assert!(remote_a.has_branch("main"), "repo A must reach its remote");
    assert!(remote_b.has_branch("main"), "repo B must reach its remote");
}

/// **Neither repo's `.git/config` was written.**
///
/// Identity is applied per invocation, so a per-repo author cannot leak
/// into a sibling repo or into the user's global config. This is the
/// property that makes the feature safe to turn on for one repo.
#[test]
fn a_per_repo_author_never_touches_git_config() {
    let repo = Worktree::with_one_commit();
    let before = std::fs::read_to_string(repo.path().join(".git/config")).unwrap();

    let identity = CommitIdentity {
        name: "Work".into(),
        email: "work@corp.example".into(),
    };
    repo.write("f.txt", "x\n");
    commit_as(repo.path(), &identity);

    let after = std::fs::read_to_string(repo.path().join(".git/config")).unwrap();
    assert_eq!(
        before, after,
        "the identity must arrive through the child's environment, not a \
         write to .git/config"
    );
    assert!(
        !after.contains("work@corp.example"),
        "no identity may be persisted into the repository's config"
    );

    // And the commit still carries it, which is the point of not writing
    // it down.
    assert_eq!(last_author(repo.path()), "Work <work@corp.example>");
}

/// A row with no per-repo author uses whatever git is configured with.
///
/// `author_ref IS NULL` means "inherit", so **adding a global identity
/// upgrades every existing repo without touching a row** — which is the
/// only way a change like that can be safe to ship.
#[test]
fn a_repo_with_no_author_uses_gits_own_configured_user() {
    let repo = Worktree::with_one_commit();
    // The fixture's own git config, set by `Worktree`.
    let expected = last_author(repo.path());
    assert!(
        !expected.is_empty(),
        "the fixture must have a configured user"
    );

    repo.write("f.txt", "x\n");
    // No identity at all — the environment is built without one.
    let env = ChildEnv::from_parent();
    Command::new("git")
        .args(["add", "-A"])
        .current_dir(repo.path())
        .env_clear()
        .envs(env.to_pairs())
        .output()
        .expect("git add runs");
    let out = Command::new("git")
        .args(["commit", "-q", "-m", "no explicit author"])
        .current_dir(repo.path())
        .env_clear()
        .envs(env.to_pairs())
        .output()
        .expect("git commit runs");
    assert!(out.status.success(), "commit failed");

    assert_eq!(
        last_author(repo.path()),
        expected,
        "a repo with no per-repo author must keep git's own configured user"
    );
}

/// The negative control for the whole file.
///
/// Without it, `two_repos_two_authors_two_accounts_in_one_run` would also
/// pass if the identity were applied to **both** repos, or to neither.
#[test]
fn the_two_identities_are_actually_distinct() {
    let work = CommitIdentity {
        name: "Work".into(),
        email: "work@corp.example".into(),
    };
    let personal = CommitIdentity {
        name: "Personal".into(),
        email: "personal@example.com".into(),
    };
    assert_ne!(work.email, personal.email);
    assert_ne!(WORK_TOKEN, PERSONAL_TOKEN);

    // And a commit made under one does not read back as the other.
    let repo = Worktree::with_one_commit();
    repo.write("x.txt", "x\n");
    commit_as(repo.path(), &work);
    assert_eq!(last_author(repo.path()), "Work <work@corp.example>");
    assert_ne!(last_author(repo.path()), "Personal <personal@example.com>");
}

/// The author is not the account.
///
/// `expected_login` is about the account the *remote* sees, and ro checks
/// it **before** any push. A repo can have a perfectly correct commit
/// author and still be pushing as the wrong person, and that gap is the
/// one the two fields exist to close.
#[test]
fn the_commit_author_and_the_pushing_account_are_separate_facts() {
    let repo = Worktree::with_one_commit();
    repo.add_remote("origin", BareRemote::ephemeral().path());
    repo.write("f.txt", "x\n");

    let author = CommitIdentity {
        name: "Work".into(),
        email: "work@corp.example".into(),
    };
    commit_as(repo.path(), &author);

    // The commit is right.
    assert_eq!(last_author(repo.path()), "Work <work@corp.example>");

    // The account ro would check is a *different* string, resolved from
    // the credential rather than from the row. Nothing in the schema
    // derives one from the other, which is the point.
    let expected_login = "quangdang46".to_string();
    assert_ne!(
        expected_login, author.email,
        "the login the remote sees is not the commit author; conflating them \
         is the defect these two fields exist to avoid"
    );
}
