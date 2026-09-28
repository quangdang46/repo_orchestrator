//! Running the plans, and the worker/coordinator split.
//!
//! # The coordinator owns every DB read
//!
//! `rusqlite::Connection` is `!Sync` — it contains a `RefCell` — so
//! `std::thread::scope` cannot share one across workers. Attempting it is
//! a compile error (E0277), not a runtime surprise, which is the right
//! way to learn it.
//!
//! So the shape is required, not chosen: **workers touch only git and the
//! filesystem.** Everything the workers need was read on the coordinator
//! before they started, and nothing they produce is written back to the
//! database. WAL mode and `busy_timeout` are already set in `ro-state`
//! for exactly this shape.
//!
//! # The scan happens once, before any worker
//!
//! An earlier shape planned every repo in one loop, printed, then applied
//! in a second. Once an engine can mutate a worktree, a plan computed in
//! loop 1 is **stale by loop 2** — the branch it names may not exist any
//! more. One scan up front removes the staleness without persisting a
//! plan artifact, and it is what makes `base_branch` trustworthy.

use std::sync::Mutex;

use anyhow::Result;

use crate::ship::orchestrator::{RepoOutcome, RepoPlan, RunOptions, run_one};
use crate::ship::summary::{Summary, SummaryRow};

/// Run every plan, in parallel, and collect the outcomes.
///
/// The results come back **in the order the plans were given**, not the
/// order they finished, so the summary reads the same way twice.
pub fn run(plans: &[RepoPlan], opts: &RunOptions) -> Summary {
    if plans.is_empty() {
        return Summary::default();
    }

    // A `try_lock` rather than a `lock`, so one panic in a worker does
    // not take down the run for every other repo.
    let outcomes: Mutex<Vec<Option<RepoOutcome>>> =
        Mutex::new((0..plans.len()).map(|_| None).collect());

    let parallel = opts.parallel.max(1).min(plans.len());
    let chunk = plans.len().div_ceil(parallel);

    let mut chunk_start = 0usize;
    std::thread::scope(|scope| {
        for group in plans.chunks(chunk) {
            let outcomes = &outcomes;
            let base = chunk_start;
            chunk_start += chunk;
            scope.spawn(move || {
                // Each worker takes what it is assigned and runs it. No
                // DB handle crosses this boundary, which is the whole
                // reason the coordinator exists.
                for (offset, plan) in group.iter().enumerate() {
                    let outcome = run_one(plan, opts);
                    let mut slots = outcomes.lock().unwrap_or_else(|e| e.into_inner());
                    // The index, not the repo id: a repo id is a UUID and
                    // parsing one as a slot number would be a silent
                    // mis-assignment, which is the exact shape of bug this
                    // module exists to avoid.
                    slots[base + offset] = Some(outcome);
                }
            });
        }
    });

    let slots = outcomes.into_inner().unwrap_or_else(|e| e.into_inner());
    let rows = plans
        .iter()
        .zip(slots)
        .map(|(plan, outcome)| SummaryRow {
            label: plan.label.clone(),
            branch: plan.base_branch.clone(),
            engine: plan.engine.label().to_string(),
            account: None,
            outcome: outcome.unwrap_or(RepoOutcome::Failed {
                error: "the worker produced no result".into(),
            }),
        })
        .collect();

    Summary::new(rows)
}

/// Build a plan for a tracked row, resolving the engine and the identity.
///
/// Called on the coordinator, once per repo, **before** any worker runs.
pub fn plan_for(
    repo: &ro_sync::manage::TrackedRepo,
    slots: &ro_engine::EngineSlots,
    engine_name: &str,
    engine_bin: Option<&str>,
    profiles: &ro_config::schema::IdentityConfig,
    global_identity: Option<&ro_core::CommitIdentity>,
) -> Result<RepoPlan> {
    let local_path = std::path::PathBuf::from(&repo.local_path);
    let base_branch = ro_git::read::current_branch(&local_path)
        .ok()
        .flatten()
        .or_else(|| repo.branch.clone())
        .unwrap_or_else(|| "main".into());

    // A row with no `author_ref` inherits the global identity, which is
    // how adding one upgrades every existing repo without touching a
    // row.
    //
    // A row WITH `author_ref` names a **profile** in `[identity.*]`, and an
    // unknown name is an error. This used to synthesise
    // `name = "work", email = "work@localhost"` — a real commit attributed
    // to a host that does not exist, reported as success, with nothing
    // anywhere saying the author was invented. That is worse than refusing:
    // the commit lands and the author is wrong.
    // The per-repo file outranks the row. It is loaded here rather than in
    // the registry so there is exactly one place the two layers meet.
    //
    // A missing file is not an error: most repos will not have one, and
    // that is the design rather than a gap.
    let local = ro_config::local::RepoLocalConfig::load(&local_path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e:#}", local_path.join(".ro/config.local.toml").display()))?;
    let mut author_ref = repo.author_ref.clone();
    let mut credential_ref = repo.credential_ref.clone();
    let mut engine_row = repo.engine.clone();
    let mut engine_args = repo.engine_args.clone();
    if let Some(l) = &local {
        l.apply_to(
            &mut author_ref,
            &mut credential_ref,
            &mut engine_row,
            &mut engine_args,
        );
    }

    let identity = match author_ref.as_deref() {
        Some(name) => Some(profiles.resolve(name).map_err(|why| {
            anyhow::anyhow!("repo {}/{}: author_ref = {name:?}\n  {why}", repo.owner, repo.name)
        })?),
        None => global_identity.cloned(),
    };

    let engine =
        ro_engine::resolve(engine_name, slots, engine_bin).map_err(|e| anyhow::anyhow!(e))?;

    Ok(RepoPlan {
        repo_id: repo.id.clone(),
        label: format!("{}/{}", repo.owner, repo.name),
        local_path,
        clone_url: repo.clone_url.clone(),
        base_branch,
        // The **merged** values, not the row's. Writing `repo.credential_ref`
        // here is what made the per-repo file a no-op for the credential: the
        // merge above wrote into a local variable, the identity was resolved
        // from it (so `author` worked), and then the plan was built from the
        // unmerged row. The operator believed the local file pinned the
        // credential; the row's other token was what actually left the
        // machine, and nothing said otherwise. The comment above this
        // function claims "the per-repo file outranks the row" — this is
        // where that claim is either true or a lie.
        author_ref: author_ref.clone(),
        credential_ref,
        onto: None,
        identity,
        engine,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ro_config::schema::IdentityConfig;

    /// A tracked row with a `credential_ref` that cannot be resolved, and a
    /// `.ro/config.local.toml` naming a different one.
    ///
    /// The local file is documented to outrank the row — the shipped
    /// config.toml's own precedence list says so, and `ro-config` documents
    /// the key. `plan_for` merged the file's value into a local variable and
    /// then wrote the **unmerged row value** into the `RepoPlan`, so the
    /// operator believed the local file pinned the credential while the row's
    /// other token was what actually left the machine. That fails in the
    /// dangerous direction: the wrong token is used and nothing says so.
    #[test]
    fn the_local_files_credential_outranks_the_row() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo_dir = tmp.path().join("repo");
        std::fs::create_dir_all(repo_dir.join(".ro")).unwrap();
        std::fs::write(
            repo_dir.join(".ro/config.local.toml"),
            "credential = \"env:RO_FROM_LOCAL_FILE\"\n",
        )
        .unwrap();

        let repo = ro_sync::manage::TrackedRepo {
            id: "id-1".into(),
            host: "github.com".into(),
            owner: "acme".into(),
            name: "api".into(),
            branch: None,
            alias: None,
            clone_url: "https://example.com/x.git".into(),
            local_path: repo_dir.to_string_lossy().into_owned(),
            visibility: "private".into(),
            archived: false,
            disabled: false,
            credential_ref: Some("env:RO_FROM_THE_ROW".into()),
            author_ref: None,
            engine: None,
            engine_args: None,
        };

        let plan = plan_for(
            &repo,
            &ro_engine::EngineSlots::default(),
            "git",
            None,
            &IdentityConfig::default(),
            None,
        )
        .expect("the plan builds");

        assert_eq!(
            plan.credential_ref.as_deref(),
            Some("env:RO_FROM_LOCAL_FILE"),
            "the local file's credential must outrank the row's"
        );
    }

    /// The same rule for `author_ref`, which already worked — pinned so the
    /// credential fix cannot quietly break it.
    #[test]
    fn the_local_files_author_outranks_the_row() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo_dir = tmp.path().join("repo");
        std::fs::create_dir_all(repo_dir.join(".ro")).unwrap();
        std::fs::write(
            repo_dir.join(".ro/config.local.toml"),
            "author = \"work\"\n",
        )
        .unwrap();

        let repo = ro_sync::manage::TrackedRepo {
            id: "id-1".into(),
            host: "github.com".into(),
            owner: "acme".into(),
            name: "api".into(),
            branch: None,
            alias: None,
            clone_url: "https://example.com/x.git".into(),
            local_path: repo_dir.to_string_lossy().into_owned(),
            visibility: "private".into(),
            archived: false,
            disabled: false,
            credential_ref: None,
            author_ref: Some("personal".into()),
            engine: None,
            engine_args: None,
        };

        let mut profiles = IdentityConfig::default();
        profiles.profiles.insert(
            "work".into(),
            ro_config::schema::CommitIdentityConfig {
                name: Some("Work Person".into()),
                email: Some("work@example.invalid".into()),
            },
        );

        let plan = plan_for(
            &repo,
            &ro_engine::EngineSlots::default(),
            "git",
            None,
            &profiles,
            None,
        )
        .expect("the plan builds");

        assert_eq!(
            plan.identity.as_ref().map(|i| i.email.as_str()),
            Some("work@example.invalid"),
            "the local file's author must outrank the row's"
        );
    }
}
