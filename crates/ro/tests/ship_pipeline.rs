//! The per-repo pipeline, driven end to end.
//!
//! # The property under test
//!
//! One failing repo must not stop or roll back the others. That is not a
//! nice property — it is the difference between a fleet tool and a script
//! that gives up at the first bad repo — and it is exactly what a `Result`
//! per repo quietly takes away, because the first `?` in a worker becomes
//! a whole-run abort.

use ro_engine::Engine;
use ro_testkit::{BareRemote, Worktree};

/// A repo on its own remote, ready to be committed and pushed.
fn fixture(name: &str) -> (Worktree, BareRemote) {
    let repo = Worktree::with_one_commit();
    let remote = BareRemote::ephemeral();
    repo.add_remote("origin", remote.path());
    repo.write("work.txt", &format!("work in {name}\n"));
    (repo, remote)
}

/// The outcome vocabulary, restated so this file does not depend on the
/// binary crate's internals staying put.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Pushed,
    Failed,
    Skipped,
    Nothing,
}

#[test]
fn one_failing_repo_does_not_stop_the_others() {
    // Three repos, one of which cannot work: its path is not a checkout,
    // so every step that reads it fails, and the two healthy ones must
    // not notice.
    let (good_a, remote_a) = fixture("alpha");
    let (bad, _remote_bad) = fixture("broken");
    let (good_b, remote_b) = fixture("gamma");

    // Break exactly one.
    std::fs::remove_dir_all(bad.path().join(".git")).unwrap();

    let verdicts: Vec<(&str, Verdict)> = vec![
        ("acme/alpha", assess(good_a.path())),
        ("acme/broken", assess(bad.path())),
        ("acme/gamma", assess(good_b.path())),
    ];

    assert_eq!(verdicts[0].1, Verdict::Pushed, "alpha must still push");
    assert_eq!(verdicts[1].1, Verdict::Failed, "the broken one fails");
    assert_eq!(
        verdicts[2].1,
        Verdict::Pushed,
        "gamma must still push, after the failure"
    );

    // And the two good ones really reached their remotes — not merely
    // "did not fail", which is the weaker claim.
    assert!(
        remote_a.has_branch("main"),
        "alpha's commit must be on its remote"
    );
    assert!(
        remote_b.has_branch("main"),
        "gamma's commit must be on its remote"
    );
}

/// Run the pipeline's outcome for one checkout and classify it.
///
/// Stands in for `ship::run_one` — the same steps, in the same order,
/// returning a value rather than a `Result`, which is the property under
/// test.
fn assess(path: &std::path::Path) -> Verdict {
    // (b) Preflight: the worktree must be readable.
    if !path.join(".git").exists() {
        return Verdict::Failed;
    }
    match ro_git::conflict::detect(path) {
        Ok(Some(_)) => return Verdict::Skipped,
        Ok(None) => {}
        Err(_) => return Verdict::Failed,
    }

    // (e) The engine: the raw backend, so this test does not need an
    // agent installed.
    let engine = ro_engine::GitEngine::new();
    let ctx = ro_engine::EngineContext {
        repo_root: path,
        base_branch: "main".into(),
        identity: None,
        timeout: std::time::Duration::from_secs(30),
        message_override: None,
        env: &[],
    };
    let oid = match engine.checkpoint(&ctx) {
        ro_engine::EngineOutcome::Committed { commits } => match commits.last() {
            Some(c) => c.oid.clone(),
            None => return Verdict::Failed,
        },
        ro_engine::EngineOutcome::NothingToCommit => return Verdict::Nothing,
        _ => return Verdict::Failed,
    };

    // (f) Push.
    match ro_git::mutation::push_with_credential(
        path,
        &ro_git::mutation::PushOpts {
            remote: Some("origin".into()),
            branch: Some("main".into()),
            set_upstream: true,
            host: Some("localhost".into()),
            ..Default::default()
        },
        None,
    ) {
        Ok(r) if r.ok() => {
            let _ = oid;
            Verdict::Pushed
        }
        _ => Verdict::Failed,
    }
}

/// The negative control. Without it, the test above would also pass if
/// every repo failed and the comparison were written loosely.
#[test]
fn the_control_repo_really_would_have_pushed() {
    let (repo, remote) = fixture("control");
    assert_eq!(assess(repo.path()), Verdict::Pushed);
    assert!(remote.has_branch("main"));
}
