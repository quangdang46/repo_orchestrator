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
pub mod resolve;
pub mod summary;

pub use emit::{plan_for, run};
pub use orchestrator::{HowFar, RunOptions};
// `Summary` is built by `run`, and nothing else here names it. The
// re-export existed for the dry-run branch that fabricated rows by hand.

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
    resolve: bool,
    dry_run: bool,
    include_archived: bool,
) -> ! {
    // The engine the user named, else the config, else an error naming
    // the three. Never a silent fall-through to `git`: a user who asked
    // for an agent and got the raw backend would get one commit per repo
    // and have no way to tell why.
    let config = ro_config::load_config(&paths.config_toml()).unwrap_or_default();
    // The engine the user named, else the config, else `claude`.
    //
    // The default is `claude` and deliberately **not** `git`. A silent
    // fall-through to the raw backend would commit one message per repo
    // while the user believed an agent was reading the diff. A missing
    // `claude` is reported per repo, by name, at dispatch — which is a
    // different thing from downgrading.
    //
    // There was no default at all before this, which is worse than
    // either: the tool refused to run until you passed `--engine` or
    // edited a config file, so the first thing every new user had to
    // discover was the flag.
    let engine_name = engine
        .or(config.agent.engine.as_deref())
        .unwrap_or("claude")
        .to_string();

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

    // No argument at all means the whole registry. That is the daily
    // invocation — it is the command that replaces `cd repo-a && ro ship`
    // in a loop, and a tool that makes you add a flag to say "everything"
    // does not replace the loop, it renames it.
    //
    // `--all` says the same thing out loud, for scripts. A name, a pattern
    // and a filter are all narrower, so any of them turns this off.
    let all = all || (named.is_empty() && pattern.is_none() && filter.is_none());

    let targets = match ro_sync::targets::resolve_targets(
        &conn,
        selection.as_deref(),
        filter,
        all,
        &projects_dir,
        include_archived,
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
        // Names the user typed and nothing matched is a **usage** error, not
        // an empty run. `ro ship alpha` used to print this and exit 0 —
        // a typo reported as a success, in a script's terms indistinguishable
        // from having done the work. Only a request that named nothing (a
        // bare `ro ship`, or a filter that selected none) is exit 0.
        let named_something = !named.is_empty() || pattern.is_some();
        eprintln!("no repos matched. Use --all, or a --repos pattern.");
        std::process::exit(if named_something {
            crate::exit::EX_USAGE as i32
        } else {
            0
        });
    }

    let slots = ro_engine::EngineSlots::default();
    // The global identity is the `[identity] default` profile, resolved here
    // rather than per row. It was hardcoded to `None` while the config
    // carried an `[agent]` table nobody read, so `ro config set` of an
    // author was a no-op and every commit fell through to git's own
    // config — the per-repo author feature was advertised and inert.
    let global_identity: Option<ro_core::CommitIdentity> = match config.identity.fallback() {
        Ok(v) => v,
        Err(why) => {
            // A `default` naming a profile that does not exist, or one that
            // is half-written, would otherwise be papered over — and then
            // every repo in the fleet commits under some other identity.
            eprintln!("error: [identity] default: {why}");
            std::process::exit(crate::exit::EX_USAGE as i32);
        }
    };
    let mut plans = Vec::with_capacity(targets.len());
    for t in &targets {
        match ro_sync::manage::find_repo(&conn, &t.repo_id) {
            Ok(repo) => match plan_for(
                &repo,
                &slots,
                &engine_name,
                engine_bin,
                &config.identity,
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
        resolve_conflicts: resolve,
        // The dry run goes down the **same** path as the real one. It used
        // to be short-circuited here, with a fabricated
        // `NothingToCommit` row per plan — so `ro ship --dry-run` reported
        // "nothing to commit" for a fleet with a dirty repo on a protected
        // branch, and the real run then refused. The plan is meant to be
        // what the run would say; building it here instead of there made
        // it a guess, and a guess is not a preview.
        dry_run,
        ..Default::default()
    };
    let summary = run(&plans, &opts);

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
