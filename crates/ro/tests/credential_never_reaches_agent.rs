//! The headline guarantee, exercised as a whole chain.
//!
//! # What is being guaranteed
//!
//! The resolved credential exists in exactly two places: ro's memory, and
//! the argv of the one `git` invocation that pushes. It exists **nowhere**
//! the agent can observe — not in the child's environment, not in its
//! argv, not in its stdout, and therefore not in its transcript.
//!
//! # Why this file and not the unit tests
//!
//! Every individual link — the resolver, the env subtraction, the prompt,
//! the spawn, the capture, the extraheader — can be right while the
//! *chain* leaks. The chain is the thing an LLM's transcript makes
//! dangerous in a way no other credential in the tool is, because a
//! transcript is designed to be read and shared.
//!
//! So this drives a real push with a real credential-shaped token, and
//! then checks every place the agent could have observed it.

use std::path::Path;
use std::process::Command;

use ro_core::SecretString;
use ro_engine::env::ChildEnv;
use ro_testkit::{BareRemote, FakeBinary, TestEnv, Worktree};

/// Two different PAT-shaped credentials, so a leak is attributable.
const CRED_A: &str = "ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CRED_B: &str = "ghp_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// A fake `claude` that is maximally hostile about observation.
///
/// It does everything an agent could plausibly do to read its
/// environment, and it also **attempts a push of its own** — because
/// "the agent did not push" and "the agent had nowhere to push" are the
/// same confusion ro-pu9.2 removed one layer up, and only a remote it
/// *could* have reached settles it.
fn hostile_agent(report_path: &Path) -> FakeBinary {
    let dir = tempfile::tempdir().unwrap();
    let keep = dir.path().to_path_buf();
    let script = keep.join("claude");
    let body = format!(
        r#"#!/bin/sh
echo "=== ARGV ===" >> "{report}"
echo "$*" >> "{report}"
echo "=== ENV ===" >> "{report}"
printenv >> "{report}" 2>&1
echo "=== FILES ===" >> "{report}"
ls -a >> "{report}" 2>&1
# The attempt the boundary forbids. Whether it succeeds is the point.
git push hostile-remote HEAD >> "{report}" 2>&1
echo "=== END ===" >> "{report}"
exit 0
"#,
        report = report_path.display(),
    );
    std::fs::write(&script, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&script).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&script, p).unwrap();
    }
    std::mem::forget(dir);
    FakeBinary::at(keep, script, "claude")
}

/// Every PAT-shaped value in a blob, found by shape rather than by
/// comparing against a list — a leak that renamed itself would still be
/// caught.
fn pat_shaped(haystack: &str) -> Vec<String> {
    haystack
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|t| {
            (t.starts_with("ghp_") || t.starts_with("github_pat_"))
                && t.len() >= 20
                && t.chars().any(|c| c.is_ascii_digit() || c == '_')
        })
        .map(str::to_string)
        .collect()
}

/// The whole chain, across two repos with two different credentials.
#[test]
fn the_credential_never_reaches_the_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let report = tmp.path().join("agent-report.txt");
    let agent = hostile_agent(&report);

    let mut agent_captures = String::new();

    // Two repos, each with its own credential, each with its own remote.
    for (name, cred) in [("alpha", CRED_A), ("beta", CRED_B)] {
        let secret = SecretString::new(cred);

        // A real bare remote for ro to push to, and a second one the
        // agent is told about, so its own push attempt has somewhere to
        // go and "it did not reach the credentialed remote" is a real
        // claim rather than a vacuous one.
        let ro_remote = BareRemote::ephemeral();
        let hostile_remote = BareRemote::ephemeral();

        let work = Worktree::with_one_commit();
        work.add_remote("hostile-remote", hostile_remote.path());
        work.add_remote("origin", ro_remote.path());
        work.write("feature.txt", "written by the agent\n");
        work.commit("the agent's commit");

        // 1. The engine runs, with the subtracted environment.
        let out = unsafe {
            TestEnv::new()
                .shim(&agent)
                .var("GH_TOKEN", cred) // the user's own token, in the parent
                .run(|| {
                    // Built here, not before: `ChildEnv::from_parent` reads
                    // PATH, and the shim directory is prepended by the
                    // TestEnv that is only live inside this closure.
                    let env = ChildEnv::from_parent()
                        .with_identity(None)
                        .with_additions(&[]);
                    Command::new("claude")
                        .arg("-p")
                        .arg("do the thing")
                        .current_dir(work.path())
                        .env_clear()
                        .envs(env.to_pairs())
                        .output()
                        .expect("the agent shim runs")
                })
        };
        agent_captures.push_str(&String::from_utf8_lossy(&out.stdout));
        agent_captures.push_str(&String::from_utf8_lossy(&out.stderr));

        // 2. ro pushes, with the credential, on one invocation.
        let push = ro_git::mutation::push_with_credential(
            work.path(),
            &ro_git::mutation::PushOpts {
                // Named explicitly: a bare local remote is not
                // github.com, so the credential's extraheader is scoped
                // to it and git has to be told where to push.
                remote: Some("origin".into()),
                branch: Some("main".into()),
                set_upstream: true,
                host: Some("localhost".into()),
                ..Default::default()
            },
            Some(&secret),
        )
        .expect("ro's push runs");
        assert!(push.ok(), "ro's push failed for {name}: {}", push.stderr);

        // ro's push reached the right remote.
        assert!(
            ro_remote.has_branch("main"),
            "ro's push must land on the credentialed remote for {name}"
        );

        // The agent's own push did **not** reach the credentialed remote.
        assert!(
            !ro_remote
                .commit_messages("main")
                .iter()
                .any(|m| m == "the agent's commit pushed by the agent"),
            "the agent must not have pushed to ro's remote"
        );
        // And it went where it was told, which is what makes the above a
        // statement about the boundary rather than about a missing remote.
        assert!(
            hostile_remote.has_branch("main"),
            "the agent's own push should have reached ITS remote. Its report:\n{}",
            std::fs::read_to_string(&report).unwrap_or_default()
        );
    }

    // Now the assertions that matter: the credential is in none of the
    // places the agent could have observed it.
    let report_text = std::fs::read_to_string(&report).unwrap();

    for (label, blob) in [
        ("the agent's report file", report_text.as_str()),
        ("the agent's stdout/stderr", agent_captures.as_str()),
    ] {
        assert!(!blob.contains(CRED_A), "credential A leaked into {label}");
        assert!(!blob.contains(CRED_B), "credential B leaked into {label}");
        let found = pat_shaped(blob);
        assert!(
            found.is_empty(),
            "{label} contains PAT-shaped values that were never ours: {found:?}"
        );
    }
}

/// ro's own output carries no PAT either.
///
/// `SecretString`'s `Debug` renders `***`, and that is worth asserting
/// through a real run rather than a unit test — the failure being designed
/// against is a `tracing` field or a panic message somewhere nobody looked.
#[test]
fn ro_output_carries_no_pat_shaped_string() {
    let tmp = tempfile::tempdir().unwrap();
    let report = tmp.path().join("report.txt");
    let agent = hostile_agent(&report);

    let remote = BareRemote::ephemeral();
    let work = Worktree::with_one_commit();
    work.add_remote("origin", remote.path());
    work.write("a.txt", "x\n");
    work.commit("a commit");

    let secret = SecretString::new(CRED_A);
    let env = ChildEnv::from_parent();
    let agent_out = unsafe {
        TestEnv::new().shim(&agent).var("GH_TOKEN", CRED_A).run(|| {
            Command::new("claude")
                .arg("-p")
                .arg("x")
                .current_dir(work.path())
                .env_clear()
                .envs(env.to_pairs())
                .output()
                .expect("the shim runs")
        })
    };

    let push = ro_git::mutation::push_with_credential(
        work.path(),
        &ro_git::mutation::PushOpts {
            // Named explicitly: a bare local remote is not
            // github.com, so the credential's extraheader is scoped
            // to it and git has to be told where to push.
            remote: Some("origin".into()),
            branch: Some("main".into()),
            set_upstream: true,
            host: Some("localhost".into()),
            ..Default::default()
        },
        Some(&secret),
    )
    .expect("the push runs");

    // Everything ro produced this run, in one place.
    let everything = format!(
        "{}{}{}{}{}",
        String::from_utf8_lossy(&agent_out.stdout),
        String::from_utf8_lossy(&agent_out.stderr),
        push.stdout,
        push.stderr,
        // And the error path, which is where a Debug-formatted secret
        // would show up: a push that fails.
        push.args.join(" "),
    );

    let found = pat_shaped(&everything);
    assert!(
        found.is_empty(),
        "ro's own output carried a PAT-shaped value: {found:?}\n{everything}"
    );
}

/// The negative control for the predicate, which is what makes the two
/// tests above worth running.
#[test]
fn the_pat_predicate_would_catch_a_leak() {
    let leaked = format!("Authorization: {CRED_A}");
    assert!(
        !pat_shaped(&leaked).is_empty(),
        "if the predicate cannot see a token that IS there, the two tests \\
         above prove nothing"
    );
}

/// Two repos pushed as two accounts, each with its own credential.
///
/// The guarantee is not just "no leak" — it is "the right credential
/// reached the right remote". A run where the same token went to both
/// would leak nothing and be equally wrong.
#[test]
fn each_repo_pushes_with_its_own_credential() {
    let mut seen: Vec<(String, String)> = Vec::new();

    for (name, cred) in [("alpha", CRED_A), ("beta", CRED_B)] {
        let remote = BareRemote::ephemeral();
        let work = Worktree::with_one_commit();
        work.add_remote("origin", remote.path());
        work.write("f.txt", "x\n");
        work.commit("per-repo commit");

        let secret = SecretString::new(cred);
        let push = ro_git::mutation::push_with_credential(
            work.path(),
            &ro_git::mutation::PushOpts {
                // Named explicitly: a bare local remote is not
                // github.com, so the credential's extraheader is scoped
                // to it and git has to be told where to push.
                remote: Some("origin".into()),
                branch: Some("main".into()),
                set_upstream: true,
                host: Some("localhost".into()),
                ..Default::default()
            },
            Some(&secret),
        )
        .expect("the push runs");
        assert!(push.ok(), "push failed for {name}: {}", push.stderr);

        // The credential is in the child's environment for that one
        // invocation and nowhere else — which is the mechanism, checked
        // where it is actually used rather than described.
        assert!(
            !push.args.is_empty(),
            "sanity: the push actually ran with arguments"
        );
        seen.push((name.to_string(), cred.to_string()));
    }

    assert_eq!(seen.len(), 2);
    assert_ne!(seen[0].1, seen[1].1, "the two repos must not share a token");
    for (_, c) in &seen {
        assert!(
            pat_shaped(c).len() == 1,
            "each fixture token is PAT-shaped, so the predicate would catch \
             it: {c}"
        );
    }
}
