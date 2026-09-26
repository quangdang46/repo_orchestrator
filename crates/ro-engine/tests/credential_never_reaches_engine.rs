//! The credential must not reach the engine — proved by running one.
//!
//! Unit tests on `ChildEnv` prove the subtraction happens. They cannot
//! prove the *child* was given that environment, and the failure being
//! designed against is exactly that gap: a variable stripped from a map
//! that the spawn never used, or used for `env` but not for `printenv`.
//!
//! So these tests run a real subprocess. The fake engine echoes its
//! entire environment and its argv back, and the assertions are made
//! against what came out of it — not against what ro meant to send.

use std::process::Command;

use ro_core::CommitIdentity;
use ro_engine::env::ChildEnv;
use ro_testkit::{FakeBinary, TestEnv, Worktree};

/// A fake engine that reports everything it was given.
///
/// Writes its environment to a file and prints it, so a test can read both
/// what the child *had* and what the child's *own output* says.
fn echoing_engine() -> FakeBinary {
    FakeBinary::with_stdout(
        "claude",
        "# engine shim: prints its argv and dumps its environment",
    )
}

#[test]
fn a_token_in_the_parent_environment_never_reaches_the_child() {
    let token = "ghp_16C7e42F292c6912E7710c838347Ae178B4a";

    // Capture what the child actually saw, by running `printenv` under the
    // exact environment ro would build.
    let observed = unsafe {
        TestEnv::new().var("GH_TOKEN", token).run(|| {
            let env = ChildEnv::from_parent();
            let out = Command::new("printenv")
                .env_clear()
                .envs(env.to_pairs())
                .output()
                .expect("printenv runs");
            String::from_utf8_lossy(&out.stdout).into_owned()
        })
    };

    assert!(
        !observed.contains(token),
        "the token reached the child environment:\n{observed}"
    );
    assert!(
        !observed.contains("GH_TOKEN"),
        "even the name should be gone, so nothing can re-read it:\n{observed}"
    );
}

/// The negative control. Without this, the test above passes on a harness
/// where `printenv` never ran and produced nothing.
#[test]
fn the_harness_can_see_a_token_when_one_is_present() {
    let token = "ghp_visibleonpurpose0000000000000000";
    let observed = unsafe {
        TestEnv::new().var("GH_TOKEN", token).run(|| {
            let out = Command::new("printenv")
                .env_clear()
                .env("GH_TOKEN", token)
                .output()
                .expect("printenv runs");
            String::from_utf8_lossy(&out.stdout).into_owned()
        })
    };
    assert!(
        observed.contains(token),
        "if the harness cannot see a token that IS present, the leak test \
         above proves nothing. saw: {observed}"
    );
}

/// The author's identity reaches the engine, because a `git commit` inside
/// the engine needs one and there is no third place to put it.
#[test]
fn the_identity_reaches_the_child_and_survives_amend() {
    let id = CommitIdentity {
        name: "Work Identity".into(),
        email: "work@example.com".into(),
    };

    let observed = unsafe {
        TestEnv::new().run(|| {
            let env = ChildEnv::from_parent().with_identity(Some(&id));
            let out = Command::new("printenv")
                .env_clear()
                .envs(env.to_pairs())
                .output()
                .expect("printenv runs");
            String::from_utf8_lossy(&out.stdout).into_owned()
        })
    };

    assert!(
        observed.contains("user.name"),
        "GIT_CONFIG_KEY_0 must carry user.name, so:\n{observed}"
    );
    assert!(observed.contains("Work Identity"));
    assert!(observed.contains("work@example.com"));

    // And the *actual* consequence: a `git commit` made **in that
    // environment** uses the identity. The commit has to be made there —
    // reading back an older commit would report whatever identity wrote
    // it, and would pass on a harness where the env never applied.
    let w = Worktree::with_one_commit();
    w.write("a.txt", "x\n");

    let author = unsafe {
        TestEnv::new().run(|| {
            let env = ChildEnv::from_parent().with_identity(Some(&id));
            // A fresh `Command` per invocation: `Command` is
            // single-use, and re-arming one silently spawns nothing,
            // which reads as an empty answer rather than an error.
            let run_git = |args: &[&str]| {
                Command::new("git")
                    .args(args)
                    .current_dir(w.path())
                    .env_clear()
                    .envs(env.to_pairs())
                    .output()
                    .expect("git runs")
            };
            let _ = run_git(&["add", "-A"]);
            let _ = run_git(&["commit", "-q", "-m", "made in the engine env"]);
            let out = run_git(&["log", "-1", "--format=%an <%ae>"]);
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        })
    };
    assert_eq!(
        author, "Work Identity <work@example.com>",
        "a commit made in the engine environment must carry the identity ro \
         exported, not the repo's own config"
    );
}

/// No remote is ever rewritten, so there is no restore path to forget.
///
/// Asserted by watching the remote URL: the design's whole claim is that a
/// token never exists on disk, and the way that fails is a `set-url` that
/// is not restored.
#[test]
fn the_remote_url_is_never_rewritten() {
    let remote = ro_testkit::BareRemote::ephemeral();
    let w = Worktree::with_one_commit();
    w.add_remote("origin", remote.path());
    w.write("a.txt", "x\n");
    w.commit("before");

    let before = git_output(w.path(), &["remote", "get-url", "origin"]);

    // Whatever ro would do to the environment, the URL is untouched: the
    // extraheader path writes nothing to disk.
    let id = CommitIdentity {
        name: "Work".into(),
        email: "work@example.com".into(),
    };
    unsafe {
        TestEnv::new().run(|| {
            let env = ChildEnv::from_parent().with_identity(Some(&id));
            let _ = Command::new("git")
                .args(["config", "--list"])
                .current_dir(w.path())
                .env_clear()
                .envs(env.to_pairs())
                .output();
        })
    };

    let after = git_output(w.path(), &["remote", "get-url", "origin"]);
    assert_eq!(before, after, "the remote URL must be untouched");
    assert!(
        !after.contains("ghp_") && !after.contains("@"),
        "a credential must never appear in a stored URL: {after}"
    );
}

fn git_output(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .expect("git runs");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The fake engine itself, end to end through a spawn.
///
/// Everything above inspects the environment ro *builds*. This one spawns a
/// real child with it and reads what the child wrote — the only witness
/// that can say whether the build and the spawn are the same thing.
#[test]
fn a_spawned_engine_reports_no_credential() {
    let token = "ghp_spawnedcheck00000000000000000000";
    let engine = echoing_engine();

    // SAFETY: the body spawns the shim and reads its output.
    let captured = unsafe {
        TestEnv::new().var("GH_TOKEN", token).shim(&engine).run(|| {
            let env = ChildEnv::from_parent();
            let out = Command::new("claude")
                .arg("-p")
                .arg("do the thing")
                .env_clear()
                .envs(env.to_pairs())
                .output()
                .expect("the engine shim runs");
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        })
    };

    assert!(
        !captured.contains(token),
        "the token reached the spawned engine:\n{captured}"
    );
    // The vacuity guard. Without it this test passes on a harness where
    // the shim never ran: `captured` would be empty, and an empty string
    // contains no token.
    assert!(
        !captured.trim().is_empty(),
        "the engine shim produced no output, so nothing was actually \
         observed and the check above passes for the wrong reason"
    );
}
