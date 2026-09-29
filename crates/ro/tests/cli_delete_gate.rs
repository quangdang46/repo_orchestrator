//! The `--delete` gate refuses a directory that is not the tracked working copy.
//!
//! This is the one code path in the tool that destroys user data, and it is
//! the one path where every other gate is a lie: gates 1 and 4 print and ask,
//! gates 2 and 5 consult the registry, and a **corrupted row defeats all
//! four** — the row is the only evidence the directory is yours, so a row
//! naming the wrong path passes all four checks and then `remove_dir_all`.
//!
//! Every test here drives the real binary against a directory the test
//! created under `tempfile`. Nothing outside a temp dir is ever named as a
//! deletion target, and each test asserts the directory *survives* — a test
//! that only checked the exit code would pass even if the refusal came after
//! the deletion.

use assert_cmd::Command;
use predicates::prelude::*;
use ro_state::rusqlite;
use ro_testkit::Worktree;
use tempfile::TempDir;

/// An isolated installation plus a checkout registered in it.
struct Test {
    config_dir: TempDir,
    state_dir: TempDir,
    repo: Worktree,
}

impl Test {
    fn new() -> Self {
        let t = Test {
            config_dir: TempDir::new().unwrap(),
            state_dir: TempDir::new().unwrap(),
            repo: Worktree::empty(),
        };
        t.repo.write("README.md", "# fixture\n");
        t.repo.commit("initial");
        t
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("ro").expect("the ro binary compiles");
        cmd.arg("--config-dir")
            .arg(self.config_dir.path())
            .arg("--state-dir")
            .arg(self.state_dir.path());
        cmd
    }

    /// Register the checkout, and return its `owner/name`.
    fn add(&self) -> String {
        self.cmd()
            .arg("add")
            .arg(self.repo.path())
            .assert()
            .success();
        let out = self
            .cmd()
            .args(["list", "--format", "ndjson"])
            .output()
            .expect("ro list runs");
        let rows: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("each line is a JSON object"))
            .collect();
        assert_eq!(rows.len(), 1, "one row, got {rows:?}");
        format!(
            "{}/{}",
            rows[0]["owner"].as_str().unwrap(),
            rows[0]["name"].as_str().unwrap()
        )
    }

    /// Point the row's `local_path` at `path`, the way a corrupted row or a
    /// path-arithmetic bug would.
    ///
    /// Written straight to SQLite: `ro config set` validates what it will
    /// write, and the whole point is a row that was not written by a path
    /// that validates.
    fn repoint_row(&self, label: &str, path: &std::path::Path) {
        let conn =
            ro_state::open_db(&self.state_dir.path().join("state.db")).expect("the state db opens");
        let (owner, name) = label.split_once('/').expect("label is owner/name");
        let n = conn
            .execute(
                "UPDATE repos SET local_path = ?1 WHERE owner = ?2 AND name = ?3",
                rusqlite_params(&[
                    path.to_string_lossy().to_string(),
                    owner.to_string(),
                    name.to_string(),
                ]),
            )
            .expect("the row is repointed");
        assert_eq!(n, 1, "exactly one row was repointed");
    }

    /// Insert a *second* row naming the same path, under a different label.
    ///
    /// `ro add` refuses a duplicate, which is the right behaviour and makes
    /// the CLI useless for building this fixture. Two rows sharing a path is
    /// exactly what a restored backup, a copied state.db, or a hand-edited
    /// registry produces — so the fixture is written the way those things
    /// write it.
    fn insert_second_row(&self, label: &str) {
        let conn =
            ro_state::open_db(&self.state_dir.path().join("state.db")).expect("the state db opens");
        let (owner, name) = label.split_once('/').expect("label is owner/name");
        let path: String = conn
            .query_row(
                "SELECT local_path FROM repos WHERE owner = ?1 AND name = ?2",
                rusqlite_params(&[owner.to_string(), name.to_string()]),
                |row| row.get(0),
            )
            .expect("the first row is readable");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let n = conn
            .execute(
                "INSERT INTO repos (id, host, owner, name, branch, alias, clone_url, \
                 local_path, visibility, archived, disabled, added_at, updated_at) \
                 VALUES (?1, 'github.com', ?2, ?3, NULL, ?4, ?5, ?6, 'public', 0, 0, ?7, ?7)",
                rusqlite_params(&[
                    "second-row-id".to_string(),
                    "second".to_string(),
                    "other".to_string(),
                    "second".to_string(),
                    "https://github.com/second/other.git".to_string(),
                    path,
                    now.to_string(),
                ]),
            )
            .expect("the second row is inserted");
        assert_eq!(n, 1, "exactly one row was inserted");
    }
}

/// Bind a `Vec<String>` for `rusqlite::params!`.
///
/// The slice of trait objects has to outlive the statement, so it is
/// leaked deliberately: this is a fixture that runs once per test, and a
/// static allocation for it is the honest shape.
fn rusqlite_params(items: &[String]) -> &[&dyn rusqlite::ToSql] {
    let refs: Vec<&dyn rusqlite::ToSql> = items.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    Box::leak(refs.into_boxed_slice())
}

/// A plain directory holding a user's files, with no git repository in it.
fn innocent_directory(parent: &std::path::Path) -> std::path::PathBuf {
    let dir = parent.join("my-important-notes");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("thesis.txt"), "eleven years of work\n").unwrap();
    dir
}

/// A stale `local_path` naming a directory that is not a checkout at all.
///
/// The row is the only evidence, and the evidence is wrong.
#[test]
fn delete_refuses_a_path_that_is_not_a_git_checkout() {
    let t = Test::new();
    let label = t.add();
    let innocent = innocent_directory(t.config_dir.path());
    t.repoint_row(&label, &innocent);

    t.cmd()
        .args(["remove", &label, "--delete", "--non-interactive"])
        .assert()
        .code(64)
        .stderr(predicate::str::contains("refused"))
        .stderr(predicate::str::contains("not a git repository"));

    assert!(
        innocent.join("thesis.txt").exists(),
        "the directory must survive: this is the whole point of the gate"
    );
}

/// The same refusal interactively.
///
/// `assert_cmd` gives the child a null stdin, so the confirmation prompt
/// would refuse on its own — which is exactly why the assertion is on the
/// word "refused" and on the specific reason. A gate that only fires when
/// nobody is watching is not a gate.
#[test]
fn delete_refuses_the_same_path_interactively() {
    let t = Test::new();
    let label = t.add();
    let innocent = innocent_directory(t.config_dir.path());
    t.repoint_row(&label, &innocent);

    t.cmd()
        .args(["remove", &label, "--delete"])
        .assert()
        .code(64)
        .stderr(predicate::str::contains("refused"))
        .stderr(predicate::str::contains("not a git repository"));

    assert!(
        innocent.join("thesis.txt").exists(),
        "the directory must survive"
    );
}

/// A row pointing at a path that does not exist is refused, not waved
/// through as "already absent".
///
/// "The row says this path" and "this path is a checkout" are different
/// facts. Collapsing them means a row whose `local_path` was never right
/// reads as a benign no-op instead of as a row that cannot be trusted to
/// delete anything.
#[test]
fn delete_refuses_a_row_whose_path_does_not_exist() {
    let t = Test::new();
    let label = t.add();
    let missing = t.config_dir.path().join("never-existed");
    t.repoint_row(&label, &missing);

    t.cmd()
        .args(["remove", &label, "--delete", "--non-interactive"])
        .assert()
        .code(64)
        .stderr(predicate::str::contains("refused"))
        // The reason, not the word "does not exist": the path is printed in
        // full above, and the refusal has to say which fact failed.
        .stderr(predicate::str::contains("does not exist, so it is not the working copy"));

    assert!(
        !missing.exists(),
        "there was nothing there, and there must be nothing there now"
    );
}

/// A git checkout that belongs to a *different* repo is refused.
///
/// This is the case a registry-only gate cannot see: the directory is a real
/// repo, it is not anybody else's registered row, and it is not nested in
/// one. It is simply the wrong repository, and its origin says so.
#[test]
fn delete_refuses_a_checkout_whose_origin_is_a_different_repo() {
    let t = Test::new();
    let label = t.add();

    // A second, unrelated repository. It has its own origin, and it is not
    // the one on the row.
    let other = Worktree::empty();
    other.write("other.md", "# not the repo you are removing\n");
    other.commit("initial");
    let origin = TempDir::new().unwrap();
    let bare = origin.path().join("somebody-elses-repo.git");
    ro_testkit::worktree::run(
        origin.path(),
        &["init", "-q", "--bare", &bare.to_string_lossy()],
    );
    other.add_remote("origin", &bare);

    t.repoint_row(&label, other.path());

    t.cmd()
        .args(["remove", &label, "--delete", "--non-interactive"])
        .assert()
        .code(64)
        .stderr(predicate::str::contains("refused"))
        .stderr(predicate::str::contains("not the clone URL"));

    assert!(
        other.path().join("other.md").exists(),
        "the other repository's work must survive"
    );
    assert!(
        other.path().join(".git").exists(),
        "the other repository must survive whole"
    );
}

/// A checkout with no remote is refused, because the row cannot be
/// confirmed against it.
///
/// `ro add <path>` adopts a local-only checkout and synthesises a
/// `clone_url` for it. There is then no origin to compare against, and a
/// check that cannot be made is a check that does not exist — so this
/// refuses, and the message names the way out.
#[test]
fn delete_refuses_a_checkout_with_no_origin() {
    let t = Test::new();
    let label = t.add();

    // `Worktree::empty` has no remote, so the row's `clone_url` is the
    // synthesised one. This is the ordinary "adopted a local checkout"
    // state, and it must not be deletable through the registry.
    t.cmd()
        .args(["remove", &label, "--delete", "--non-interactive"])
        .assert()
        .code(64)
        .stderr(predicate::str::contains("refused"))
        .stderr(predicate::str::contains("no origin remote"));

    assert!(
        t.repo.path().join(".git").exists(),
        "the checkout must survive"
    );
}

/// A symlink at the row's `local_path` pointing at somebody else's checkout.
///
/// `git remote get-url` runs with the CWD set to the target, so it follows
/// the symlink and reads the *destination's* config. The destination is a
/// real checkout of a real repo, and if its origin happens to match the row
/// the gate would delete through the symlink and take the other repo's work
/// with it. The gate must refuse on the symlink itself, before git is ever
/// asked to look through it.
#[test]
fn delete_refuses_a_symlink_aimed_at_another_checkout() {
    let t = Test::new();
    let label = t.add();

    // A second repository, registered or not, that the row must not reach.
    let other = Worktree::empty();
    other.write("other.md", "# not the repo you are removing\n");
    other.commit("initial");
    let origin = TempDir::new().unwrap();
    let bare = origin.path().join("somebody-elses-repo.git");
    ro_testkit::worktree::run(
        origin.path(),
        &["init", "-q", "--bare", &bare.to_string_lossy()],
    );
    other.add_remote("origin", &bare);

    // The row names a symlink; the symlink names the other checkout.
    let link = t.config_dir.path().join("aimed-elsewhere");
    std::os::unix::fs::symlink(other.path(), &link).unwrap();
    t.repoint_row(&label, &link);

    t.cmd()
        .args(["remove", &label, "--delete", "--non-interactive"])
        .assert()
        .code(64)
        .stderr(predicate::str::contains("refused"));

    assert!(
        other.path().join("other.md").exists(),
        "the symlink's target must survive: the gate must not delete through a link"
    );
    assert!(
        other.path().join(".git").exists(),
        "the symlink's target must survive whole"
    );
}

/// A row whose `local_path` is a *parent* of the real checkout.
///
/// The path exists, it is a git repository, and `git remote get-url` run
/// there answers with the checkout's origin — so a gate that only asked
/// "is this a checkout whose origin matches" would pass, and
/// `remove_dir_all` would take the parent and everything nested in it. The
/// row must name the checkout itself, not a directory that merely contains
/// it.
#[test]
fn delete_refuses_a_parent_of_the_real_checkout() {
    let t = Test::new();
    let label = t.add();

    // The real checkout, nested one level below the path the row will name.
    let nested = t.repo.path().join("nested-checkout");
    std::fs::create_dir_all(&nested).unwrap();
    let bare_root = TempDir::new().unwrap();
    let bare = bare_root.path().join("origin.git");
    ro_testkit::worktree::run(
        bare_root.path(),
        &["init", "-q", "--bare", &bare.to_string_lossy()],
    );
    ro_testkit::worktree::run(
        &nested,
        &[
            "clone",
            "-q",
            &bare.to_string_lossy(),
            &nested.to_string_lossy(),
        ],
    );

    // Repoint the row at the *parent*, which is a git repository and whose
    // origin matches the row's clone_url.
    t.repoint_row(&label, t.repo.path());

    t.cmd()
        .args(["remove", &label, "--delete", "--non-interactive"])
        .assert()
        .code(64)
        .stderr(predicate::str::contains("refused"));

    assert!(
        nested.join(".git").exists(),
        "the nested checkout must survive: a parent directory is not the working copy"
    );
}

/// Two rows sharing one path: neither removal may delete the directory.
///
/// The copy-paste accident is two rows naming one checkout, and the question
/// the gate has to answer is "am I about to delete the thing I registered, or
/// something else?" — with two rows, the honest answer is "I cannot tell which
/// one you mean, and deleting it would take both." So the first removal is
/// refused too, and the checkout survives. A gate that let the first one
/// through would delete a directory that was still somebody's registered
/// working copy, which is the exact outcome the check exists to prevent.
#[test]
fn delete_refuses_when_two_rows_share_one_path() {
    let t = Test::new();
    let first = t.add();

    // A second row naming the same path, under a different label.
    t.insert_second_row(&first);

    // The first removal is refused: the path is still the second row's
    // working copy, and the two rows cannot be told apart from the path alone.
    t.cmd()
        .args(["remove", &first, "--delete", "--non-interactive"])
        .assert()
        .code(64)
        .stderr(predicate::str::contains("refused"))
        .stderr(predicate::str::contains("also the working copy"));

    assert!(
        t.repo.path().join(".git").exists(),
        "the checkout must survive: it is still the second row's working copy"
    );

    // And so is the second removal, for the same reason.
    t.cmd()
        .args(["remove", "second/other", "--delete", "--non-interactive"])
        .assert()
        .code(64)
        .stderr(predicate::str::contains("refused"));

    assert!(
        t.repo.path().join(".git").exists(),
        "the checkout must survive the second attempt too"
    );
}

/// The gate is not vacuous: the real working copy still goes.
///
/// A check that refuses everything is not a safety property, it is a
/// deletion command that stopped working. The row's `clone_url` is the
/// checkout's own `origin` here, so the gate passes and the directory goes.
#[test]
fn delete_still_removes_the_working_copy_it_can_prove() {
    let t = Test::new();
    let label = t.add();

    let bare_root = TempDir::new().unwrap();
    let bare = bare_root.path().join("origin.git");
    ro_testkit::worktree::run(
        bare_root.path(),
        &["init", "-q", "--bare", &bare.to_string_lossy()],
    );
    t.repo.add_remote("origin", &bare);
    // Re-add so the row's `clone_url` is the origin we just set.
    t.cmd().arg("remove").arg(&label).assert().success();
    t.cmd().arg("add").arg(t.repo.path()).assert().success();

    let rows: Vec<serde_json::Value> = {
        let out = t
            .cmd()
            .args(["list", "--format", "ndjson"])
            .output()
            .expect("ro list runs");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("a JSON object"))
            .collect()
    };
    let label = format!(
        "{}/{}",
        rows[0]["owner"].as_str().unwrap(),
        rows[0]["name"].as_str().unwrap()
    );
    let path = std::path::PathBuf::from(rows[0]["local_path"].as_str().unwrap());

    t.cmd()
        .args(["remove", &label, "--delete", "--non-interactive"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Deleted working copy"));

    assert!(
        !path.exists(),
        "the working copy is the one thing this flag is for; it must be gone"
    );
}

/// A checkout containing an **unregistered** nested repository must not be
/// deleted.
///
/// The registered-row check above only fires when the nested repository
/// happens to be a row too. A stray `git clone` inside a checkout is not a
/// row — it is exactly the thing nobody registered — and it sailed straight
/// through, destroying a user's uncommitted work with exit 0 and "Deleted
/// working copy" in the log. Reproduced on a real directory before the fix.
///
/// The registry cannot answer this question, because the thing at risk is
/// the thing nobody registered. The directory is where it has to be read.
#[test]
fn delete_refuses_a_checkout_containing_an_unregistered_nested_repo() {
    // A real clone with a real `origin`, because the gate that fires before
    // the nested check is the origin one: a checkout with no remote is
    // refused for a different reason, and the test would then assert
    // nothing about the check it exists for. This is the same trap the
    // parent test's doc comment records.
    let bare_root = TempDir::new().unwrap();
    let bare = bare_root.path().join("origin.git");
    ro_testkit::worktree::run(
        bare_root.path(),
        &["init", "-q", "--bare", &bare.to_string_lossy()],
    );
    let seed = bare_root.path().join("seed");
    ro_testkit::worktree::run(
        bare_root.path(),
        &["clone", "-q", &bare.to_string_lossy(), "seed"],
    );
    ro_testkit::worktree::run(&seed, &["config", "user.email", "t@e.com"]);
    ro_testkit::worktree::run(&seed, &["config", "user.name", "T"]);
    std::fs::write(seed.join("README.md"), "# fixture\n").unwrap();
    ro_testkit::worktree::run(&seed, &["add", "-A"]);
    ro_testkit::worktree::run(&seed, &["commit", "-q", "-m", "initial"]);
    ro_testkit::worktree::run(&seed, &["push", "-q", "-u", "origin", "HEAD:main"]);

    let repo_dir = TempDir::new().unwrap();
    ro_testkit::worktree::run(
        repo_dir.path(),
        &["clone", "-q", &bare.to_string_lossy(), "checkout"],
    );
    let checkout = repo_dir.path().join("checkout");
    ro_testkit::worktree::run(&checkout, &["config", "user.email", "t@e.com"]);
    ro_testkit::worktree::run(&checkout, &["config", "user.name", "T"]);

    let config_dir = TempDir::new().unwrap();
    let state_dir = TempDir::new().unwrap();
    let mut fresh = Command::cargo_bin("ro").expect("the ro binary compiles");
    fresh
        .arg("--config-dir")
        .arg(config_dir.path())
        .arg("--state-dir")
        .arg(state_dir.path())
        .arg("init")
        .assert()
        .success();
    let mut add = Command::cargo_bin("ro").expect("the ro binary compiles");
    add.arg("--config-dir")
        .arg(config_dir.path())
        .arg("--state-dir")
        .arg(state_dir.path())
        .arg("add")
        .arg(&checkout)
        .assert()
        .success();
    let key = {
        let out = Command::cargo_bin("ro")
            .expect("the ro binary compiles")
            .arg("--config-dir")
            .arg(config_dir.path())
            .arg("--state-dir")
            .arg(state_dir.path())
            .args(["list", "--format", "ndjson"])
            .output()
            .expect("ro list runs");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str::<serde_json::Value>(&l).expect("a JSON row"))
            .find(|v| v["local_path"].as_str() == Some(checkout.to_str().unwrap()))
            .map(|v| format!("{}/{}", v["owner"].as_str().unwrap(), v["name"].as_str().unwrap()))
            .expect("the checkout is registered")
    };

    // A nested clone at a depth the walk has to find. It is **not** a
    // registered row, which is the whole point: that is what the registry
    // check cannot see.
    //
    // The directory is deliberately not one of the walk's SKIP entries
    // (`target`, `node_modules`, `vendor`, …): a `.git` in build output is
    // vendored rather than a working copy, so the walk is right to skip it,
    // and a fixture placed there would prove nothing.
    let other = bare_root.path().join("other.git");
    ro_testkit::worktree::run(
        bare_root.path(),
        &["init", "-q", "--bare", &other.to_string_lossy()],
    );
    let nested = checkout.join("tools").join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    ro_testkit::worktree::run(
        &checkout,
        &["clone", "-q", &other.to_string_lossy(), "tools/nested"],
    );
    assert!(
        nested.join(".git").exists(),
        "the nested repository must exist for this test to mean anything"
    );
    // And something in the outer checkout that must survive.
    std::fs::write(checkout.join("precious.txt"), "user work\n").unwrap();

    let out = Command::cargo_bin("ro")
        .expect("the ro binary compiles")
        .arg("--config-dir")
        .arg(config_dir.path())
        .arg("--state-dir")
        .arg(state_dir.path())
        .args(["remove", &key, "--delete", "--non-interactive"])
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "a checkout containing a nested repository must be refused, got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("contains another git repository"),
        "the refusal must say what it found, got: {stderr}"
    );
    // The directory and its contents survive. This is the assertion that
    // matters: a gate that deletes the fixture and then reports success
    // passes for the wrong reason.
    assert!(
        checkout.join("precious.txt").exists(),
        "the user's work must survive the refusal"
    );
    assert!(
        nested.join(".git").exists(),
        "and so must the nested repository"
    );
}
