//! `ro ship` — and the two verbs that are prefixes of it.
//!
//! One module serves `ro commit`, `ro push` and `ro ship`, parameterised
//! by how far the pipeline runs. The steps are the same for all three; a
//! shorter verb stops earlier rather than being a different code path,
//! because three paths that mostly agree drift.
//!
//! ```
//! a. lock      RepoLock — released when the guard drops
//! b. preflight conflict → Skip
//!             clean    → Skip (for commit)
//!             denylist + secret scan → Block, BEFORE the engine runs
//! c. identity  from the row's author_ref
//! d. base      git fetch
//! e. engine    engine.checkpoint(ctx)   <- `ro commit` returns here
//! f. push      git push, credential per the row's credential_ref
//! g. unlock
//! ```
//!
//! # Per-repo independence is a VALUE return, not a `Result`
//!
//! One repo's failure must never stop the others. A `Result` per repo
//! invites a `?` in a worker, and one `?` that escapes converts a
//! per-repo failure into a fleet abort — twenty repos, one with a bad
//! credential, and nineteen never touched.
//!
//! # The coordinator owns all DB access
//!
//! `rusqlite::Connection` is `!Sync`, so `std::thread::scope` cannot share
//! one. Workers touch **only git and the filesystem**; the coordinator
//! reads and writes the database. WAL mode and `busy_timeout` are already
//! set in `ro-state` for exactly this.

pub mod emit;
pub mod orchestrator;
pub mod summary;

pub use emit::{plan_for, run};
pub use orchestrator::{HowFar, RunOptions};
pub use summary::Summary;

use ro_config::ConfigPaths;

/// The shared body of `commit`, `push` and `ship`.
///
/// Resolves the config, the targets and the engine on this thread, hands
/// the plans to the workers, prints the summary, and **exits with the
/// summary's code**. The verb only chose `HowFar`.
#[allow(clippy::too_many_arguments)]
pub fn run_verb(
    paths: &ConfigPaths,
    how_far: HowFar,
    // Repos named positionally, by name or alias; and a glob from
    // `--pattern`. They are separate parameters rather than one
    // comma-joined string so the precedence between them is visible in
    // the signature.
    named: &[String],
    pattern: Option<&str>,
    filter: Option<&str>,
    all: bool,
    engine: Option<&str>,
    engine_bin: Option<&str>,
    onto: Option<&str>,
    dry_run: bool,
) -> ! {
    // The engine the user named, else the config, else an error naming
    // the three. Never a silent fall-through to `git`: a user who asked
    // for an agent and got the raw backend would get one commit per repo
    // and have no way to tell why.
    let config = ro_config::load_config(&paths.config_toml()).unwrap_or_default();
    let engine_name = match engine.or(config.agent.engine.as_deref()) {
        Some(e) => e.to_string(),
        None => {
            eprintln!(
                "no engine selected. Pass --engine claude|codex|git, or set \\
                 `agent.engine` in the config.\n\\
                 A typo'd name is an error rather than a default, because \\
                 falling back silently would commit with the raw backend \\
                 while you believed an agent was reading the diff."
            );
            std::process::exit(crate::exit::EX_USAGE as i32);
        }
    };

    let conn = match ro_state::open_db(&paths.state_db()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    };

    let projects_dir = paths.state_dir.join("projects");

    // A name someone typed outranks a flag left in a shell profile, so the
    // positional list wins when both are given. `--all` is explicit and
    // beats both: it is the only way to say "everything".
    let selection: Option<String> = if all {
        None
    } else if !named.is_empty() {
        Some(named.join(","))
    } else {
        pattern.map(str::to_string)
    };

    let targets = match ro_sync::targets::resolve_targets(
        &conn,
        selection.as_deref(),
        filter,
        all,
        &projects_dir,
    ) {
        Ok(t) => t,
        Err(e) => {
            // An invalid glob or an unknown filter is a **usage** error:
            // exit 2, distinct from a run that failed.
            eprintln!("error: {e:#}");
            std::process::exit(crate::exit::EX_USAGE as i32);
        }
    };

    if targets.is_empty() {
        eprintln!("no repos matched. Use --all, or a --repos pattern.");
        std::process::exit(0);
    }

    let slots = ro_engine::EngineSlots::default();
    let global_identity = None::<ro_core::CommitIdentity>;
    let mut plans = Vec::with_capacity(targets.len());
    for t in &targets {
        match ro_sync::manage::find_repo(&conn, &t.repo_id) {
            Ok(repo) => match plan_for(
                &repo,
                &slots,
                &engine_name,
                engine_bin,
                global_identity.as_ref(),
            ) {
                Ok(mut p) => {
                    p.onto = onto.map(str::to_string);
                    plans.push(p)
                }
                Err(e) => {
                    eprintln!("error planning {}: {e:#}", t.label);
                    std::process::exit(crate::exit::EX_FATAL as i32);
                }
            },
            Err(e) => {
                eprintln!("error reading {}: {e:#}", t.label);
                std::process::exit(crate::exit::EX_FATAL as i32);
            }
        }
    }

    let opts = RunOptions {
        how_far,
        state_dir: paths.state_dir.clone(),
        ..Default::default()
    };
    let summary = if dry_run {
        Summary::new(
            plans
                .iter()
                .map(|p| crate::ship::summary::SummaryRow {
                    label: p.label.clone(),
                    branch: p.base_branch.clone(),
                    engine: engine_name.clone(),
                    account: None,
                    outcome: crate::ship::orchestrator::RepoOutcome::NothingToCommit,
                })
                .collect(),
        )
    } else {
        run(&plans, &opts)
    };

    print!("{}", summary.render());
    // The summary counts; the table decides. A summary that assigned a
    // code itself would collapse "all failed" and "some failed", which
    // is the distinction the table exists for.
    let (succeeded, failed) = summary.counts();
    let code = crate::exit::RunExit::from_counts(succeeded, failed).code();
    if dry_run && code == 0 {
        eprintln!("(dry run — nothing was written)");
    }
    std::process::exit(code as i32);
}
