//! ro — GitHub-first repo orchestration CLI.
//!
//! Binary entry point. Logic lives in library crates; this file
//! wires clap commands to those functions.
//!
//! Exit codes:
//! - 0: success / healthy
//! - 1: one or more failures
//! - 64: usage error

mod doctor;
mod ship;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use ro_config::paths::ConfigPaths;
use ro_sync::sync::SyncStrategy;

/// Output format for commands that support it.
#[derive(Debug, Clone, Copy, ValueEnum, Default)]
enum OutputFormat {
    #[default]
    Text,
    Json,
    /// One JSON object per line, for a consumer that reads a stream.
    ///
    /// Not a second machine format: it is the same objects `json` emits,
    /// framed so a reader can process twenty repos without holding all
    /// twenty in memory. That framing is the only difference, and it is
    /// only worth a variant where there is more than one row.
    Ndjson,
}

#[derive(Debug, Parser)]
#[command(name = "ro", about = "GitHub-first repo orchestration CLI", version, long_about = None)]
struct Cli {
    /// Override the config directory (default: $XDG_CONFIG_HOME/ro)
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,

    /// Override the state directory (default: $XDG_STATE_HOME/ro)
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,

    /// Suppress non-essential output
    #[arg(long, short, global = true)]
    quiet: bool,

    /// Enable verbose output
    #[arg(long, global = true)]
    verbose: bool,

    /// Never prompt for confirmation
    #[arg(long, global = true)]
    non_interactive: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    // ── Repo management ──────────────────────────────────────────────
    /// Initialize config + SQLite state directory
    Init,

    /// Add a repo to tracking
    Add {
        /// A remote spec (owner/repo, github.com/owner/repo, https://…, git@…)
        /// to clone, or a path to an existing checkout to adopt in place.
        ///
        /// The two are told apart structurally: a recognised host prefix is a
        /// spec, anything else that is a git checkout is a path, and anything
        /// else is refused by name.
        spec: String,
        /// Display and lookup alias.
        #[arg(long)]
        name: Option<String>,
        /// Where a remote clone lands.
        #[arg(long)]
        clone_to: Option<PathBuf>,
        /// Branch to clone. A clone parameter with no lifetime — it is
        /// deliberately not cached on the row.
        #[arg(long)]
        branch: Option<String>,
        /// Per-repo credential *reference*, `env:VAR` or `keychain:ENTRY`.
        /// A pasted token is an error, not a value.
        #[arg(long)]
        credential: Option<String>,
        /// Per-repo engine for this row.
        #[arg(long)]
        engine: Option<String>,
        /// Which `[identity.*]` profile commits this repo.
        #[arg(long)]
        author: Option<String>,
        /// Tag this repo. Repeatable.
        #[arg(long = "tag")]
        tags: Vec<String>,
    },

    /// Remove a repo from tracking
    Remove {
        /// Repo key: owner/repo, alias, or id
        key: String,
    },

    /// List tracked repos
    List {
        /// Filter by owner
        #[arg(long)]
        owner: Option<String>,
        /// Output format
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },

    // ── Sync / Status ────────────────────────────────────────────────
    /// Sync all tracked repos
    Sync {
        /// Sync strategy: ff-only (default), rebase, merge
        #[arg(long, value_enum, default_value_t = SyncStrategy::FfOnly)]
        strategy: SyncStrategy,
        /// Output format
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
        /// Preview changes without making them
        #[arg(long)]
        dry_run: bool,
        /// Only clone missing repos, don't pull
        #[arg(long)]
        clone_only: bool,
        /// Only pull existing repos, don't clone
        #[arg(long)]
        pull_only: bool,
        /// Stash changes before pull, pop after
        #[arg(long)]
        autostash: bool,
        /// Number of repos to sync concurrently
        #[arg(long, short = 'j', default_value_t = 1)]
        parallel: u32,
        /// Network timeout in seconds
        #[arg(long)]
        timeout: Option<u32>,
        /// Resume an interrupted sync
        #[arg(long)]
        resume: bool,
    },

    /// Show status of tracked repos
    Status {
        /// Specific repo key
        repo: Option<String>,
        /// Output format
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },

    /// Prune removed/missing repos
    Prune {
        /// Also prune archived repos
        #[arg(long)]
        archived: bool,
        /// Also prune missing repos
        #[arg(long)]
        missing: bool,
        /// Scan the projects dir for working copies not in the inventory
        #[arg(long)]
        orphans: bool,
        /// Move orphaned working copies into <state-dir>/archived
        #[arg(long, conflicts_with = "delete")]
        archive: bool,
        /// Permanently delete orphaned working copies (destructive)
        #[arg(long)]
        delete: bool,
    },

    // ── Runs / Timeline ──────────────────────────────────────────────
    /// Run management commands
    Run {
        #[command(subcommand)]
        sub: RunCommands,
    },

    // ── Conflict ─────────────────────────────────────────────────────
    /// Conflict resolver commands
    Conflict {
        #[command(subcommand)]
        sub: ConflictCommands,
    },
    /// Commit what the engine finds, and stop
    Commit {
        /// Target repos by pattern (e.g. "owner/*")
        #[arg(long)]
        repos: Option<String>,
        /// Target repos by filter (e.g. "tag:needs-fmt")
        #[arg(long)]
        filter: Option<String>,
        /// Target all tracked repos
        #[arg(long)]
        all: bool,
        /// Which engine commits
        #[arg(long)]
        engine: Option<String>,
        /// A binary under another name, for a nightly or an odd install
        #[arg(long)]
        engine_bin: Option<String>,
        /// Preview without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// Commit and push
    Push {
        #[arg(long)]
        repos: Option<String>,
        #[arg(long)]
        filter: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        engine: Option<String>,
        #[arg(long)]
        engine_bin: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// The whole thing: fetch, rebase, commit, push
    Ship {
        #[arg(long)]
        repos: Option<String>,
        #[arg(long)]
        filter: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        engine: Option<String>,
        #[arg(long)]
        engine_bin: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },

    // ── Doctor ───────────────────────────────────────────────────────
    /// Diagnose installation health
    Doctor {
        /// Apply repairs
        #[arg(long)]
        fix: bool,
        /// Output format
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },

    // ── Config ───────────────────────────────────────────────────────
    /// Show or set configuration values
    Config {
        #[command(subcommand)]
        sub: Option<ConfigCommands>,
    },

    // ── Schema ───────────────────────────────────────────────────────
    /// Machine-readable CLI reference, generated from the live command tree
    Schema,
}

#[derive(Debug, Subcommand)]
enum RunCommands {
    /// List recent runs
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Show a specific run
    Show { run_id: String },
    /// Show timeline for a run
    Timeline { run_id: String },
}

#[derive(Debug, Subcommand)]
enum ConflictCommands {
    /// List conflicted repos
    List,
    /// Explain a conflict
    Explain { repo: String },
    /// Abort a conflict
    Abort { repo: String },
    /// Mark a conflict as resolved
    MarkResolved { repo: String },
}

#[derive(Debug, Subcommand)]
enum ConfigCommands {
    /// Print all configuration values
    Print,
    /// Set a configuration value (KEY=VALUE)
    Set {
        /// KEY=VALUE pair
        pair: String,
    },
}

/// Build a `repo.id -> "owner/name"` map for friendlier text rendering.
fn repo_labels_by_id(conn: &ro_state::Connection) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Ok(mut stmt) = conn.prepare("SELECT id, owner, name FROM repos") else {
        return map;
    };
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    });
    if let Ok(rows) = rows {
        for r in rows.flatten() {
            map.insert(r.0, format!("{}/{}", r.1, r.2));
        }
    }
    map
}

/// Ask for a yes/no answer on stdin. Returns true only on `y`/`yes`.
///
/// Callers must gate on `--non-interactive` and TTY state before reaching
/// here; this only handles the prompt itself.
fn confirm(prompt: &str) -> bool {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        eprintln!("stdin is not a terminal; cannot prompt. Re-run with --non-interactive.");
        return false;
    }
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn resolve_paths(cli: &Cli) -> Result<ConfigPaths> {
    match (&cli.config_dir, &cli.state_dir) {
        (Some(config_dir), Some(state_dir)) => {
            let cache_dir = state_dir.join("cache");
            Ok(ConfigPaths {
                config_dir: config_dir.clone(),
                state_dir: state_dir.clone(),
                cache_dir,
            })
        }
        _ => ConfigPaths::discover(),
    }
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:?}");
        std::process::exit(1);
    }
}

/// Build the machine-readable CLI reference by walking the live clap tree.
///
/// The previous `ro robot-docs` was a hand-written `json!` literal listing
/// commands and flags. It had already drifted in four places — it omitted
/// `commit-sweep`, omitted `prune --orphans/--archive/--delete`, listed only
/// two output formats, and recommended a command that was being deleted. A
/// mirror of the clap tree that is written by hand is wrong the moment it is
/// written, and nothing in the build or the test suite can see it.
///
/// Serializing `Cli::command()` makes that class of drift impossible rather
/// than merely fixing today's instance: a command that is added to the enum
/// appears here automatically, and one that is removed disappears. The
/// shape is a stable JSON contract, not the clap API — args are flattened
/// into `long`/`short`/`help` so a consumer does not have to parse
/// `clap::Arg` to find out what a flag is.
fn schema_json() -> serde_json::Value {
    fn arg_json(a: &clap::Arg) -> serde_json::Value {
        let mut o = serde_json::Map::new();
        o.insert("name".into(), a.get_id().as_str().into());
        // clap reports the long/short *names*; emit them invocable. A consumer
        // that has to remember that "strategy" means "--strategy" is doing the
        // parsing this flattening exists to remove.
        if let Some(l) = a.get_long() {
            o.insert("long".into(), format!("--{l}").into());
        }
        if let Some(s) = a.get_short() {
            o.insert("short".into(), format!("-{s}").into());
        }
        o.insert("required".into(), a.is_required_set().into());
        if let Some(h) = a.get_help() {
            o.insert("help".into(), h.to_string().into());
        }
        if !a.get_possible_values().is_empty() {
            o.insert(
                "values".into(),
                a.get_possible_values()
                    .iter()
                    .map(|v| serde_json::Value::from(v.get_name()))
                    .collect::<Vec<_>>()
                    .into(),
            );
        }
        serde_json::Value::Object(o)
    }

    fn command_json(c: &clap::Command) -> serde_json::Value {
        let mut o = serde_json::Map::new();
        o.insert("name".into(), c.get_name().into());
        if let Some(a) = c.get_about() {
            o.insert("about".into(), a.to_string().into());
        }
        let args: Vec<_> = c
            .get_arguments()
            .filter(|a| !a.is_hide_set())
            .map(arg_json)
            .collect();
        if !args.is_empty() {
            o.insert("args".into(), args.into());
        }
        let subs: Vec<_> = c.get_subcommands().map(command_json).collect();
        if !subs.is_empty() {
            o.insert("subcommands".into(), subs.into());
        }
        serde_json::Value::Object(o)
    }

    let root = Cli::command();
    let mut o = serde_json::Map::new();
    o.insert("name".into(), root.get_name().into());
    o.insert(
        "version".into(),
        root.get_version().unwrap_or_default().into(),
    );
    if let Some(about) = root.get_about() {
        o.insert("about".into(), about.to_string().into());
    }
    let subs: Vec<_> = root.get_subcommands().map(command_json).collect();
    o.insert("commands".into(), subs.into());
    serde_json::Value::Object(o)
}

/// Resolve multi-repo targets from --repos/--filter/--all flags.
fn run() -> Result<()> {
    use ro_sync::manage;
    use ro_sync::prune;
    use ro_sync::status;
    use ro_sync::sync;

    let cli = Cli::parse();
    let paths = resolve_paths(&cli)?;
    let db_path = paths.state_db();
    let non_interactive = cli.non_interactive;
    let _quiet = cli.quiet;

    match cli.command {
        // ── commit / push / ship ───────────────────────────────────────
        //
        // One module, three verbs. The difference is `HowFar`, and a
        // shorter verb stops earlier rather than taking a different path:
        // three paths that mostly agree drift, and the drift surfaces as
        // "commit blocked but push did it anyway".
        Commands::Commit {
            repos,
            filter,
            all,
            engine,
            engine_bin,
            dry_run,
        } => ship::run_verb(
            &paths,
            ship::HowFar::Commit,
            repos.as_deref(),
            filter.as_deref(),
            all,
            engine.as_deref(),
            engine_bin.as_deref(),
            dry_run,
        ),
        Commands::Push {
            repos,
            filter,
            all,
            engine,
            engine_bin,
            dry_run,
        } => ship::run_verb(
            &paths,
            ship::HowFar::Push,
            repos.as_deref(),
            filter.as_deref(),
            all,
            engine.as_deref(),
            engine_bin.as_deref(),
            dry_run,
        ),
        Commands::Ship {
            repos,
            filter,
            all,
            engine,
            engine_bin,
            dry_run,
        } => ship::run_verb(
            &paths,
            ship::HowFar::Ship,
            repos.as_deref(),
            filter.as_deref(),
            all,
            engine.as_deref(),
            engine_bin.as_deref(),
            dry_run,
        ),

        // ── Repo management ──
        Commands::Init => {
            let created = manage::init(&paths).context("initializing ro")?;
            if created {
                eprintln!("Initialized: {}", paths.config_dir.display());
            } else {
                eprintln!("Already initialized: {}", paths.config_dir.display());
                eprintln!("  Config:  {}", paths.config_toml().display());
                eprintln!("  State:   {}", db_path.display());
            }
        }

        Commands::Add {
            spec,
            name,
            clone_to,
            branch,
            credential,
            engine,
            author,
            tags,
        } => {
            let conn = ro_state::open_db(&db_path).context("opening state database")?;
            let projects_dir = paths.state_dir.join("projects");
            let opts = manage::AddOptions {
                name,
                clone_to,
                branch,
                credential_ref: credential,
                engine,
                author_ref: author,
                tags,
            };
            let repo = manage::add_from_input(&conn, &spec, &projects_dir, &opts)
                .context("adding repo")?;
            eprintln!("Added: {}/{} (id={})", repo.owner, repo.name, repo.id);
            eprintln!("  path: {}", repo.local_path);
        }

        Commands::Remove { key } => {
            let conn = ro_state::open_db(&db_path).context("opening state database")?;
            let repo = manage::remove(&conn, &key).context("removing repo")?;
            eprintln!("Removed: {}/{}", repo.owner, repo.name);
        }

        Commands::List { owner, format } => {
            let conn = ro_state::open_db(&db_path).context("opening state database")?;
            let repos = manage::list(&conn, owner.as_deref()).context("listing repos")?;
            if repos.is_empty() {
                eprintln!("No tracked repos. Use 'ro add <spec>' to add one.");
            } else {
                for repo in repos {
                    match format {
                        OutputFormat::Text => println!("{}", repo),
                        // One object per line, so a consumer can read the
                        // first repo without waiting for the last.
                        OutputFormat::Json | OutputFormat::Ndjson => {
                            println!("{}", serde_json::to_string(&repo)?);
                        }
                    }
                }
            }
        }

        // ── Sync / Status ──
        Commands::Sync {
            strategy,
            format,
            dry_run,
            clone_only,
            pull_only,
            autostash,
            parallel: _,
            timeout,
            resume: _,
        } => {
            if clone_only && pull_only {
                anyhow::bail!("--clone-only and --pull-only cannot be used together");
            }
            let conn = ro_state::open_db(&db_path).context("opening state database")?;
            let opts = sync::SyncOptions {
                strategy,
                autostash,
                timeout_secs: timeout.unwrap_or(30),
                dry_run,
                clone_only,
                pull_only,
            };
            let repo_labels = repo_labels_by_id(&conn);
            let results = sync::sync_all(&conn, &opts).context("syncing repos")?;
            for r in &results {
                match format {
                    OutputFormat::Text => {
                        let label = repo_labels
                            .get(&r.repo_id)
                            .cloned()
                            .unwrap_or_else(|| r.repo_id.clone());
                        println!("{} action={} status={}", label, r.action, r.status);
                    }
                    OutputFormat::Json | OutputFormat::Ndjson => {
                        println!("{}", serde_json::to_string(r)?);
                    }
                }
            }
        }

        Commands::Status { repo, format } => {
            let conn = ro_state::open_db(&db_path).context("opening state database")?;
            let statuses: Vec<status::RepoStatus> = match repo {
                Some(key) => {
                    let found = manage::find_repo(&conn, &key)?;
                    let r = status::status_repo(&conn, &found.id)?;
                    vec![r]
                }
                None => status::status_all(&conn)?,
            };
            for s in &statuses {
                match format {
                    OutputFormat::Text => {
                        let dirty = if s.is_dirty { " (dirty)" } else { "" };
                        // An unmeasurable repo prints `ahead=unknown`, never
                        // `ahead=0`. Printing zero here is the bug this bead
                        // exists to remove: a green board over rows nobody
                        // measured is worse than a visibly broken one,
                        // because the user stops looking.
                        match (s.ahead, s.behind) {
                            (Some(a), Some(b)) => println!(
                                "{}/{}: {}{} ahead={} behind={}",
                                s.owner,
                                s.name,
                                s.branch.as_deref().unwrap_or("HEAD"),
                                dirty,
                                a,
                                b
                            ),
                            _ => println!(
                                "{}/{}: {}{} ahead=unknown behind=unknown — {}",
                                s.owner,
                                s.name,
                                s.branch.as_deref().unwrap_or("HEAD"),
                                dirty,
                                s.unmeasurable_reason.as_deref().unwrap_or("unknown reason")
                            ),
                        }
                    }
                    OutputFormat::Json => {
                        println!("{}", serde_json::to_string(s)?);
                    }
                    // One object per line, no wrapping array. The point
                    // is that a consumer can start reading before the
                    // last repo has been walked, which a JSON array of
                    // twenty rows cannot offer.
                    OutputFormat::Ndjson => {
                        println!("{}", serde_json::to_string(s)?);
                    }
                }
            }
        }

        Commands::Prune {
            archived,
            missing,
            orphans,
            archive,
            delete,
        } => {
            let conn = ro_state::open_db(&db_path).context("opening state database")?;

            if orphans || archive || delete {
                use ro_sync::prune::{OrphanAction, find_orphans, handle_orphans};

                if delete
                    && !non_interactive
                    && !std::io::IsTerminal::is_terminal(&std::io::stdin())
                {
                    eprintln!(
                        "prune --delete needs an interactive terminal, or pass --non-interactive."
                    );
                    std::process::exit(3);
                }

                let found = find_orphans(&conn, &paths.state_dir.join("projects"))?;
                if found.is_empty() {
                    eprintln!("No orphan working copies found.");
                } else {
                    eprintln!("Found {} orphan working cop(ies):", found.len());
                    for o in &found {
                        eprintln!("  {}", o.path);
                    }
                }

                let action = if delete {
                    if !confirm("Permanently delete these directories? [y/N] ") {
                        eprintln!("Aborted.");
                        return Ok(());
                    }
                    OrphanAction::Delete
                } else if archive {
                    OrphanAction::Archive
                } else {
                    OrphanAction::Report
                };

                let done = handle_orphans(&found, action, &paths.state_dir)?;
                if !done.is_empty() {
                    eprintln!("Acted on {} orphan(s).", done.len());
                }
            }

            let mut pruned: Vec<prune::PruneResult> = Vec::new();
            if archived {
                let results = prune::prune_archived(&conn)?;
                pruned.extend(results);
            }
            if missing {
                let results = prune::prune_missing(&conn)?;
                pruned.extend(results);
            }
            if !pruned.is_empty() {
                eprintln!("Pruned {} repos:", pruned.len());
                for p in &pruned {
                    eprintln!("  {}/{}", p.owner, p.name);
                }
            } else {
                let mut hints = Vec::new();
                if !archived {
                    hints.push("--archived");
                }
                if !missing {
                    hints.push("--missing");
                }
                if hints.is_empty() {
                    eprintln!("Nothing to prune.");
                } else {
                    eprintln!("Nothing to prune. Try: ro prune {}", hints.join(" "));
                }
            }
        }

        // ── Runs / Timeline ──
        Commands::Run { sub } => {
            let conn = ro_state::open_db(&db_path).context("opening state database")?;
            match sub {
                RunCommands::List { limit } => {
                    let runs = ro_jobs::recent_runs(&conn, limit)?;
                    for run in &runs {
                        let status = if run.is_finished() {
                            format!("exit={}", run.exit_code.unwrap_or(0))
                        } else {
                            "running".into()
                        };
                        println!("{} {} {} ({})", run.id, run.command, status, run.started_at);
                    }
                }
                RunCommands::Show { run_id } => match ro_jobs::get_run(&conn, &run_id)? {
                    Some(run) => println!("{}", serde_json::to_string_pretty(&run)?),
                    None => eprintln!("Run {run_id} not found."),
                },
                RunCommands::Timeline { run_id } => {
                    let events = ro_jobs::events_for_run(&conn, &run_id)?;
                    if events.is_empty() {
                        eprintln!("No events for run {run_id}.");
                    }
                    for ev in &events {
                        println!("[{}] {}: {}", ev.level, ev.ts, ev.message);
                    }
                }
            }
        }

        // ── Conflict ──
        Commands::Conflict { sub } => {
            let conn = ro_state::open_db(&db_path).context("opening state database")?;
            match sub {
                ConflictCommands::List => {
                    let repos = manage::list(&conn, None)?;
                    let paths: Vec<PathBuf> =
                        repos.iter().map(|r| PathBuf::from(&r.local_path)).collect();
                    let conflicts = ro_git::conflict::list_conflicts(&paths);
                    if conflicts.is_empty() {
                        eprintln!("No conflicts.");
                    }
                    for (path, state) in &conflicts {
                        let explanation = ro_git::conflict::explain(state);
                        println!("{}: {}", path.display(), explanation);
                    }
                }
                ConflictCommands::Explain { repo } => {
                    let found = manage::find_repo(&conn, &repo)?;
                    let path = PathBuf::from(&found.local_path);
                    if let Some(state) = ro_git::conflict::detect(&path)? {
                        println!("{}", ro_git::conflict::explain(&state));
                    } else {
                        eprintln!("No conflict in {repo}.");
                    }
                }
                ConflictCommands::Abort { repo } => {
                    let found = manage::find_repo(&conn, &repo)?;
                    let path = PathBuf::from(&found.local_path);
                    ro_git::conflict::abort(&path)?;
                    eprintln!("Aborted conflict in {repo}.");
                }
                ConflictCommands::MarkResolved { repo } => {
                    let found = manage::find_repo(&conn, &repo)?;
                    let path = PathBuf::from(&found.local_path);
                    // First check if user already staged resolution (no conflict markers, no unmerged)
                    let already_resolved = match ro_git::conflict::verify_resolved(&path) {
                        Ok(()) => true,
                        // Ignore "still in progress" — that's expected if user `git add`'d but hasn't finished merge
                        Err(ro_git::conflict::MarkResolvedError::OperationStillInProgress(_)) => {
                            false
                        }
                        Err(e) => anyhow::bail!("cannot mark {repo} resolved: {e}"),
                    };
                    if already_resolved {
                        eprintln!("{repo} already resolved.");
                    } else {
                        // User resolved files & staged them; finish the merge
                        ro_git::conflict::finish(&path)
                            .context("finishing merge after resolution")?;
                        eprintln!("Marked {repo} as resolved.");
                    }
                }
            }
        }

        // ── Doctor ──
        Commands::Doctor { fix, format } => {
            let opts = doctor::DoctorOptions {
                config_token: None,
                fix,
                binary_lookup_path: None,
                paths: Some(paths.clone()),
            };
            let report = doctor::run(opts);
            match format {
                OutputFormat::Text => println!("{}", doctor::render_text(&report)),
                OutputFormat::Json | OutputFormat::Ndjson => {
                    // A doctor report is one object, so the two machine
                    // formats serialise identically here. The variant is
                    // accepted so the spelling is not command-specific.
                    let json =
                        serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".into());
                    println!("{json}");
                }
            }
            std::process::exit(report.exit_code());
        }

        // ── Config ──
        Commands::Config { sub } => {
            let cfg_path = paths.config_toml();
            match sub {
                Some(ConfigCommands::Print) | None => {
                    let config = ro_config::load_config(&cfg_path)?;
                    let toml_str = toml::to_string_pretty(&config)?;
                    println!("{toml_str}");
                }
                Some(ConfigCommands::Set { pair }) => {
                    let (key, value) = pair
                        .split_once('=')
                        .ok_or_else(|| anyhow::anyhow!("expected KEY=VALUE format"))?;
                    let key = key.trim();
                    let value = value.trim();

                    // The registry tier. `repos.<name>.<key>` writes the row in
                    // state.db, not the file — which is the form most per-repo
                    // changes should take, because it is queryable, diffable in
                    // a backup, and does not litter a working tree.
                    if let Some(rest) = key.strip_prefix("repos.") {
                        let (repo, config_key) = rest.split_once('.').ok_or_else(|| {
                            anyhow::anyhow!("expected repos.<name>.<key>, got: {key}")
                        })?;
                        if repo.is_empty() || config_key.is_empty() {
                            anyhow::bail!("expected repos.<name>.<key>, got: {key}");
                        }
                        let conn = ro_state::open_db(&db_path).context("opening state database")?;
                        manage::set_repo_config(&conn, repo, config_key, value)
                            .with_context(|| format!("setting {key}"))?;
                        eprintln!("Set {key} = {value} in the registry");
                    } else {
                        ro_config::set_key_in_file(&cfg_path, key, value)
                            .with_context(|| format!("setting {key}"))?;
                        eprintln!("Set {key} = {value}");
                    }
                }
            }
        }

        // ── Schema ──
        Commands::Schema => {
            let schema = schema_json();
            println!("{}", serde_json::to_string_pretty(&schema)?);
        }
    }

    Ok(())
}
