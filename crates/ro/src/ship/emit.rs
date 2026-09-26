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
    let identity = match repo.author_ref.as_deref() {
        Some(name) => Some(ro_core::CommitIdentity {
            name: name.to_string(),
            email: format!("{name}@localhost"),
        }),
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
        author_ref: repo.author_ref.clone(),
        credential_ref: repo.credential_ref.clone(),
        onto: None,
        identity,
        engine,
    })
}
