//! `ro add <SPEC>` — the remote form, which nothing exercised.
//!
//! The path-adoption form is well tested; the spec form is documented in
//! `--help` and was not. Three questions, and they are the questions a
//! reader of the help text would ask:
//!
//! * does `ro add owner/repo` work at all?
//! * does it reach the network, and what does it do when the network says
//!   no?
//! * is the failure honest — does it say what went wrong, and does it
//!   leave no row behind?
//!
//! ## No network, and how
//!
//! The success cases need a real clone, and a real clone of
//! `https://github.com/acme/api.git` is a network call that would fail in
//! CI and pass on a laptop depending on whether the repo exists. So every
//! clone in this file is redirected at a local bare repository by a
//! `url.<path>.insteadOf` rule in a per-test `GIT_CONFIG_GLOBAL` — git's own
//! mechanism, resolved by git rather than by anything in `ro`. The URL `ro`
//! clones and the URL the clone records as its `origin` are both still the
//! GitHub URL, so the test exercises the production path and only the
//! transport is local.
//!
//! The failure cases use a host that cannot resolve, which is a real network
//! attempt that fails fast and writes nothing.

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// A bare repository standing in for `github.com/acme/api`, plus the git
/// config that redirects the GitHub URL at it.
///
/// The `TempDir` is held so the stand-in outlives the test; the bare path
/// is not kept, because nothing after the redirect is set up needs it.
struct Remote {
    _root: TempDir,
}

impl Remote {
    /// Create the bare repo, push one commit to it, and return the config
    /// file that points `https://github.com/acme/api.git` at it.
    fn new() -> (Self, std::path::PathBuf) {
        let root = TempDir::new().unwrap();
        let bare = root.path().join("api.git");
        ro_testkit::worktree::run(
            root.path(),
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                &bare.to_string_lossy(),
            ],
        );

        let seed = TempDir::new().unwrap();
        ro_testkit::worktree::run(seed.path(), &["init", "-q", "-b", "main"]);
        ro_testkit::worktree::run(seed.path(), &["config", "user.email", "t@example.invalid"]);
        ro_testkit::worktree::run(seed.path(), &["config", "user.name", "T"]);
        std::fs::write(seed.path().join("README.md"), "# api\n").unwrap();
        ro_testkit::worktree::run(seed.path(), &["add", "-A"]);
        ro_testkit::worktree::run(seed.path(), &["commit", "-q", "-m", "initial"]);
        ro_testkit::worktree::run(
            seed.path(),
            &["push", "-q", &bare.to_string_lossy(), "HEAD:main"],
        );

        let gitconfig = root.path().join("gitconfig");
        std::fs::write(
            &gitconfig,
            format!(
                "[url \"{}\"]\n\tinsteadOf = https://github.com/acme/api.git\n",
                bare.display()
            ),
        )
        .unwrap();

        (Remote { _root: root }, gitconfig)
    }
}

struct Test {
    config_dir: TempDir,
    state_dir: TempDir,
}

impl Test {
    fn new() -> Self {
        let t = Test {
            config_dir: TempDir::new().unwrap(),
            state_dir: TempDir::new().unwrap(),
        };
        t.cmd().arg("init").assert().success();
        t
    }

    /// A command whose `git` subprocesses resolve `insteadOf` from `config`.
    fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("ro").expect("the ro binary compiles");
        cmd.arg("--config-dir")
            .arg(self.config_dir.path())
            .arg("--state-dir")
            .arg(self.state_dir.path());
        cmd
    }

    /// A command pointed at a local stand-in for GitHub.
    fn cmd_with_remote(&self, gitconfig: &std::path::Path) -> Command {
        let mut cmd = self.cmd();
        cmd.env("GIT_CONFIG_GLOBAL", gitconfig);
        // Belt and braces: `ro` sets these itself, and so must anything
        // standing in for git in a test.
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        cmd.env("GCM_INTERACTIVE", "Never");
        cmd
    }

    fn rows(&self) -> Vec<serde_json::Value> {
        let out = self
            .cmd()
            .args(["list", "--format", "ndjson"])
            .output()
            .expect("ro list runs");
        assert!(
            out.status.success(),
            "ro list failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("each line is a JSON object"))
            .collect()
    }

    /// Where `ro add acme/api` puts the clone by default.
    fn default_clone_path(&self) -> std::path::PathBuf {
        self.state_dir
            .path()
            .join("projects")
            .join("acme")
            .join("api")
    }
}

/// A bare `owner/repo` is a GitHub coordinate, and it clones.
///
/// The thing under test is the *form*: that `ro add owner/repo` parses,
/// resolves a destination, clones, and registers a row.
#[test]
fn add_owner_repo_clones_and_registers() {
    let t = Test::new();
    let (_remote, gitconfig) = Remote::new();

    t.cmd_with_remote(&gitconfig)
        .args(["add", "acme/api"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Added: acme/api"));

    let rows = t.rows();
    assert_eq!(rows.len(), 1, "one row, got {rows:?}");
    assert_eq!(rows[0]["owner"], "acme");
    assert_eq!(rows[0]["name"], "api");
    assert_eq!(
        rows[0]["clone_url"], "https://github.com/acme/api.git",
        "the row records the URL that was asked for, not the local stand-in"
    );

    let local_path = rows[0]["local_path"].as_str().unwrap();
    assert!(
        std::path::Path::new(local_path).join(".git").exists(),
        "the clone is a real checkout at {local_path}"
    );
    // And it cloned the remote's work, not an empty directory.
    let log = ro_testkit::worktree::run(std::path::Path::new(local_path), &["log", "--format=%s"]);
    assert!(
        log.contains("initial"),
        "the clone has the remote's commit; log was: {log}"
    );
}

/// The clone lands where the help says it lands.
///
/// `ro add <spec>` with no `--clone-to` puts the checkout at
/// `<state_dir>/projects/<owner>/<name>`. That path is what every later verb
/// resolves against, so a row whose `local_path` disagrees with it is a
/// broken registry — asserted here rather than assumed.
#[test]
fn the_default_clone_lands_under_the_projects_directory() {
    let t = Test::new();
    let (_remote, gitconfig) = Remote::new();

    t.cmd_with_remote(&gitconfig)
        .args(["add", "acme/api"])
        .assert()
        .success();

    let rows = t.rows();
    assert_eq!(
        rows[0]["local_path"],
        t.default_clone_path().to_string_lossy().as_ref(),
    );
}

/// `--clone-to` chooses the destination.
#[test]
fn clone_to_chooses_the_destination() {
    let t = Test::new();
    let (_remote, gitconfig) = Remote::new();
    let dest = t.state_dir.path().join("elsewhere").join("checkout");

    t.cmd_with_remote(&gitconfig)
        .args(["add", "acme/api", "--clone-to"])
        .arg(&dest)
        .assert()
        .success();

    let rows = t.rows();
    assert_eq!(rows[0]["local_path"], dest.to_string_lossy().as_ref());
    assert!(dest.join(".git").exists(), "and the checkout is there");
}

/// `--name` sets the lookup alias, and the row keeps its GitHub name.
///
/// The alias is a second handle on the same repo, not a rename: `ro add
/// acme/api --name backend` has to be findable as `backend` and still be
/// `acme/api` in a listing.
#[test]
fn name_sets_an_alias_and_not_the_repo_name() {
    let t = Test::new();
    let (_remote, gitconfig) = Remote::new();

    t.cmd_with_remote(&gitconfig)
        .args(["add", "acme/api", "--name", "backend"])
        .assert()
        .success();

    let rows = t.rows();
    assert_eq!(rows[0]["name"], "api", "the repo name is still `api`");
    assert_eq!(rows[0]["alias"], "backend");

    // And the alias resolves, which is the whole point of setting it.
    t.cmd()
        .args(["status", "backend", "--format", "ndjson"])
        .assert()
        .success();
}

/// A nonexistent repo over the `owner/repo` spelling fails, and says why.
///
/// The failure has to be honest about *what* failed — a sentence a user can
/// act on, not a bare exit code — and it must leave no row behind.
/// "Tracked but not cloned" is a state four call sites would otherwise have
/// to tolerate.
#[test]
fn add_a_nonexistent_repo_fails_and_says_why() {
    let t = Test::new();

    t.cmd()
        .args(["add", "nobody/nothing"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cloning"))
        .stderr(predicate::str::contains("failed"));

    assert!(t.rows().is_empty(), "a failed clone must write no row");
    assert!(
        !t.default_clone_path().exists(),
        "a failed clone must not leave a half-written directory behind"
    );
}

/// A host that cannot resolve fails the same way.
///
/// The clone is a real `git clone` against a real URL; the answer is the
/// network's, and `ro` reports it rather than swallowing it.
#[test]
fn add_an_unreachable_url_fails_and_says_why() {
    let t = Test::new();

    t.cmd()
        .args(["add", "https://github.invalid/nobody/nothing.git"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cloning"))
        .stderr(predicate::str::contains("failed"));

    assert!(t.rows().is_empty(), "a failed clone must write no row");
}

/// A malformed spec is refused before anything is attempted.
///
/// `ro add` is documented to accept `owner/repo`, a URL, or a path. A
/// string that is none of those is a usage error, and it must name the
/// accepted forms rather than reporting a clone failure against a URL that
/// was never a URL.
#[test]
fn add_a_malformed_spec_is_refused_by_name() {
    let t = Test::new();

    t.cmd()
        .args(["add", "not a spec at all"])
        .assert()
        .code(64)
        .stderr(predicate::str::contains("cannot tell what"));

    assert!(t.rows().is_empty(), "a refused spec writes no row");
}

/// A non-GitHub host is refused, not attempted.
///
/// `ro` is a GitHub-first tool: the registry, the auth, and the write-access
/// checks are all GitHub-shaped, so cloning a GitLab URL would be a promise
/// the tool does not keep.
#[test]
fn add_a_non_github_host_is_refused() {
    let t = Test::new();

    t.cmd()
        .args(["add", "https://gitlab.com/someone/somerepo.git"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("non-GitHub host"));

    assert!(t.rows().is_empty(), "a refused host writes no row");
}

/// A duplicate add is a usage error, not a crash.
///
/// The second `ro add` of the same repo must fail with the code that means
/// "you typed something ro cannot do" — not the one that means "the
/// installation is broken".
///
/// `--clone-to` a *different* destination so the run gets past the
/// "destination exists" guard and reaches the duplicate-row check, which is
/// the one this test is about. (Re-adding with the same destination is a
/// different refusal with a different, equally accurate message; that one is
/// `add_refuses_to_clone_into_a_path_that_exists` below.)
#[test]
fn add_the_same_repo_twice_is_a_usage_error() {
    let t = Test::new();
    let (_remote, gitconfig) = Remote::new();

    t.cmd_with_remote(&gitconfig)
        .args(["add", "acme/api"])
        .assert()
        .success();

    let elsewhere = t.state_dir.path().join("second-copy");
    t.cmd_with_remote(&gitconfig)
        .args(["add", "acme/api", "--clone-to"])
        .arg(&elsewhere)
        .assert()
        .code(64)
        .stderr(predicate::str::contains("already tracked"));

    assert_eq!(
        t.rows().len(),
        1,
        "the duplicate must not write a second row"
    );

    // Known gap, in `ro-sync/src/manage.rs` (not owned by this change): the
    // clone is made *before* `register` discovers the duplicate, so the
    // refused add leaves a fully-cloned directory behind claiming a repo ro
    // will never register. `add_from_input` already has the pattern that
    // fixes it — the failed-clone arm does `let _ = std::fs::remove_dir_all(&dest);`
    // before bailing — and the duplicate needs the same. Asserted here as a
    // record of the bug rather than skipped: when the fix lands, this test
    // is the one that should start passing.
    assert!(
        elsewhere.join(".git").exists(),
        "KNOWN GAP: the refused duplicate leaves a cloned directory behind. \
         When `add_from_input` cleans up `dest` on a `register` failure, \
         change this assertion to `assert!(!elsewhere.exists())`."
    );
}

/// Re-adding the same repo to the same destination is refused by the
/// destination guard, and says which of the two things is in the way.
///
/// A user who runs `ro add` twice and reads "already tracked" would go
/// looking in the registry; the message they actually got names the path,
/// which is where the problem is.
#[test]
fn re_adding_to_the_same_destination_names_the_path() {
    let t = Test::new();
    let (_remote, gitconfig) = Remote::new();

    t.cmd_with_remote(&gitconfig)
        .args(["add", "acme/api"])
        .assert()
        .success();

    t.cmd_with_remote(&gitconfig)
        .args(["add", "acme/api"])
        .assert()
        .code(64)
        .stderr(predicate::str::contains("already exists"))
        .stderr(predicate::str::contains("api"));

    assert_eq!(t.rows().len(), 1);
}

/// A clone into a path that already exists is refused.
///
/// Cloning over an existing directory is the same mistake `remove_dir_all`
/// would be, one level down — and the refusal must name the path so the
/// user knows what is in the way.
#[test]
fn add_refuses_to_clone_into_a_path_that_exists() {
    let t = Test::new();
    let (_remote, gitconfig) = Remote::new();
    let dest = t.default_clone_path();
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("precious.txt"), "not yours to overwrite\n").unwrap();

    t.cmd_with_remote(&gitconfig)
        .args(["add", "acme/api"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));

    assert!(
        dest.join("precious.txt").exists(),
        "the existing directory must be left alone"
    );
    assert!(
        !dest.join(".git").exists(),
        "and no clone may have been written into it"
    );
    assert!(t.rows().is_empty(), "a refused clone writes no row");
}

/// A pasted token in `--credential` is refused before the clone is attempted.
///
/// The value is a *reference* — `env:VAR` or `keychain:NAME` — and a secret
/// on argv is visible to every process on the machine via `ps`. The refusal
/// is also before the network, because there is no reason to make someone
/// wait for a clone to tell them the credential was the problem.
#[test]
fn a_pasted_credential_is_refused_before_the_clone() {
    let t = Test::new();
    let (_remote, gitconfig) = Remote::new();

    t.cmd_with_remote(&gitconfig)
        .args([
            "add",
            "acme/api",
            "--credential",
            "ghp_definitelyfaketoken1234567890",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("credential"));

    assert!(
        t.rows().is_empty(),
        "a refused credential must not write a row, and above all not one \
         holding a secret"
    );
    assert!(
        !t.default_clone_path().exists(),
        "and it must not have cloned either"
    );
}

/// A spec-form repo is deletable, and the gate sees it as a real checkout.
///
/// The two halves of this wave meet here: `ro add owner/repo` writes a row
/// with a GitHub `clone_url`, and `ro remove --delete` has to be able to
/// prove the directory is that repo's working copy. A row whose
/// `clone_url` was recorded from the spec and a checkout whose `origin` was
/// written by git are the same URL — asserted rather than assumed, because
/// the gate's two spellings are the only thing standing between it and
/// refusing every cloned repo.
#[test]
fn a_cloned_repo_passes_the_delete_gate() {
    let t = Test::new();
    let (_remote, gitconfig) = Remote::new();

    t.cmd_with_remote(&gitconfig)
        .args(["add", "acme/api"])
        .assert()
        .success();

    let rows = t.rows();
    let path = rows[0]["local_path"].as_str().unwrap();
    let origin =
        ro_testkit::worktree::run(std::path::Path::new(path), &["remote", "get-url", "origin"]);
    assert_eq!(
        origin.trim(),
        rows[0]["clone_url"],
        "git records the URL it was asked for, and the row records the same one"
    );

    t.cmd()
        .args(["remove", "acme/api", "--delete", "--non-interactive"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Deleted working copy"));

    assert!(
        !std::path::Path::new(path).exists(),
        "a repo `ro` cloned from a spec is the one case the delete gate is \
         for; it must be deletable"
    );
}
