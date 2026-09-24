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

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use ro_config::paths::ConfigPaths;
use ro_sync::sync::SyncStrategy;

/// Output format for commands that support it.
#[derive(Debug, Clone, Copy, ValueEnum, Default)]
enum OutputFormat {
    #[default]
    Text,
    Json,
}

/// Auto-approve level for plan application.
#[derive(Debug, Clone, Copy, ValueEnum, Default)]
enum AutoApproveLevel {
    #[default]
    None,
    Low,
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
        /// Repo spec: owner/repo, github.com/owner/repo, https://..., etc.
        spec: String,
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

    /// Bulk import repos from a file, GitHub stars, org, or user
    Import {
        /// Path to repos.list file
        file: Option<PathBuf>,
        /// Import from your GitHub stars
        #[arg(long)]
        stars: bool,
        /// Import from an organization
        #[arg(long)]
        org: Option<String>,
        /// Import from a user's repos
        #[arg(long)]
        user: Option<String>,
        /// Maximum repos to import
        #[arg(long)]
        limit: Option<usize>,
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
    },

    // ── Health ───────────────────────────────────────────────────────
    /// Show health score for repos
    Health {
        /// Specific repo key
        repo: Option<String>,
        /// Output format
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
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

    // ── Review ───────────────────────────────────────────────────────
    /// Review plan/apply commands
    Review {
        #[command(subcommand)]
        sub: ReviewCommands,
    },

    // ── Sweep ────────────────────────────────────────────────────────
    /// Sweep commands (commit, agent)
    Sweep {
        #[command(subcommand)]
        sub: SweepCommands,
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

    // ── Self-update ──────────────────────────────────────────────────
    /// Update ro to the latest version
    SelfUpdate {
        /// Only check for updates, don't install
        #[arg(long)]
        check: bool,
    },

    // ── Robot docs ───────────────────────────────────────────────────
    /// Machine-readable CLI documentation (JSON)
    RobotDocs {
        /// Topic: commands, quickstart, examples, exit-codes, formats, schemas
        topic: Option<String>,
    },

    // ── Fork ─────────────────────────────────────────────────────────
    /// Fork management commands
    Fork {
        #[command(subcommand)]
        sub: ForkCommands,
    },
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
enum ReviewCommands {
    /// Create a review plan
    Plan {
        /// Repo key
        repo: String,
        /// Plan description
        #[arg(long)]
        summary: Option<String>,
        /// Risk level: low, medium, high
        #[arg(long)]
        risk: Option<String>,
    },
    /// Approve a pending plan so it can be applied
    Approve { plan_id: String },
    /// Reject a pending or approved plan
    Reject { plan_id: String },
    /// Apply a previously-approved review plan
    Apply { plan_id: String },
    /// Roll back a previously-applied plan
    Rollback { plan_id: String },
    /// List plans
    ListPlans,
}

#[derive(Debug, Subcommand)]
enum SweepCommands {
    /// Sweep commit with safety checks
    Commit {
        /// Repo path
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Commit message
        #[arg(long)]
        message: String,
    },
    /// Run sweep agent on a repo (or multiple repos)
    Agent {
        /// Repo path
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Repo id for tracking
        #[arg(long)]
        repo_id: Option<String>,
        /// Target repos by pattern (e.g. "owner/*")
        #[arg(long)]
        repos: Option<String>,
        /// Target repos by filter (e.g. "tag:needs-fmt", "health:<50")
        #[arg(long)]
        filter: Option<String>,
        /// Target all managed repos
        #[arg(long)]
        all: bool,
        /// Preview without changes
        #[arg(long)]
        dry_run: bool,
        /// Auto-approve level: none (default), low
        #[arg(long, value_enum, default_value_t = AutoApproveLevel::None)]
        auto_approve: AutoApproveLevel,
        /// NDJSON event stream output
        #[arg(long)]
        output: Option<String>,
    },
    /// Group dirty worktrees into logical conventional commits across repos
    CommitSweep {
        /// Target all managed repos
        #[arg(long)]
        all: bool,
        /// Target repos by pattern (e.g. "owner/*")
        #[arg(long)]
        repos: Option<String>,
        /// Target repos by filter (e.g. "tag:needs-fmt", "health:<50")
        #[arg(long)]
        filter: Option<String>,
        /// Target a single repo by path instead of the tracked inventory
        #[arg(long)]
        path: Option<PathBuf>,
        /// Actually create the commits (default: dry run)
        #[arg(long)]
        execute: bool,
        /// Push each branch after a successful commit
        #[arg(long)]
        push: bool,
        /// Remote to push to (default: origin)
        #[arg(long)]
        push_remote: Option<String>,
        /// Use --force-with-lease instead of a plain push
        #[arg(long)]
        force_with_lease: bool,
        /// Keep manually staged files as a separate `wip` commit
        #[arg(long)]
        respect_staging: bool,
        /// Also commit on protected branches (main, master, release/*, ...)
        #[arg(long)]
        allow_protected: bool,
    },
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

#[derive(Debug, Subcommand)]
enum ForkCommands {
    /// Show fork status for tracked repos
    Status {
        /// Specific repo key
        repo: Option<String>,
        /// Output format
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    /// Sync forks with upstream
    Sync {
        /// Specific repo key
        repo: Option<String>,
    },
    /// Clean up stale fork branches
    Clean {
        /// Specific repo key
        repo: Option<String>,
        /// Preview only
        #[arg(long)]
        dry_run: bool,
    },
}

/// Parse a `--risk` flag value into a [`ro_review::plan::PlanRisk`].
fn parse_risk(s: &str) -> Result<ro_review::plan::PlanRisk> {
    match s.to_ascii_lowercase().as_str() {
        "low" => Ok(ro_review::plan::PlanRisk::Low),
        "medium" | "med" => Ok(ro_review::plan::PlanRisk::Medium),
        "high" => Ok(ro_review::plan::PlanRisk::High),
        other => anyhow::bail!("unknown risk class {other:?} (expected low|medium|high)"),
    }
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

fn generate_robot_docs(topic: Option<&str>) -> serde_json::Value {
    match topic {
        Some("commands") => serde_json::json!({
            "commands": [
                {"name": "init", "description": "Initialize config + SQLite state directory"},
                {"name": "add", "description": "Add a repo to tracking", "args": ["<spec>"]},
                {"name": "remove", "description": "Remove a repo from tracking", "args": ["<key>"]},
                {"name": "list", "description": "List tracked repos", "flags": ["--owner", "--format"]},
                {"name": "import", "description": "Import repos from file, stars, org, or user", "flags": ["--stars", "--org", "--user", "--limit"]},
                {"name": "sync", "description": "Sync all tracked repos", "flags": ["--strategy", "--dry-run", "--clone-only", "--pull-only", "--autostash", "--parallel", "--timeout", "--resume"]},
                {"name": "status", "description": "Show status of tracked repos", "args": ["[repo]"]},
                {"name": "prune", "description": "Prune removed/missing repos", "flags": ["--archived", "--missing"]},
                {"name": "health", "description": "Show health score for repos", "args": ["[repo]"]},
                {"name": "run", "description": "Run management", "subcommands": ["list", "show", "timeline"]},
                {"name": "conflict", "description": "Conflict resolver", "subcommands": ["list", "explain", "abort", "mark-resolved"]},
                {"name": "review", "description": "Review plan/apply", "subcommands": ["plan", "approve", "reject", "apply", "rollback", "list-plans"]},
                {"name": "sweep", "description": "Sweep commands", "subcommands": ["commit", "agent"]},
                {"name": "doctor", "description": "Diagnose installation health", "flags": ["--fix"]},
                {"name": "config", "description": "Show or set config", "subcommands": ["print", "set"]},
                {"name": "self-update", "description": "Update ro to latest version", "flags": ["--check"]},
                {"name": "robot-docs", "description": "Machine-readable CLI documentation"},
                {"name": "fork", "description": "Fork management", "subcommands": ["status", "sync", "clean"]},
            ]
        }),
        Some("exit-codes") => serde_json::json!({
            "exit_codes": [
                {"code": 0, "meaning": "success"},
                {"code": 1, "meaning": "one or more failures"},
                {"code": 64, "meaning": "usage error"},
            ]
        }),
        Some("formats") => serde_json::json!({
            "formats": [
                {"name": "text", "description": "Human-readable terminal output (default)"},
                {"name": "json", "description": "One JSON document per line (NDJSON-friendly)"},
            ]
        }),
        Some("quickstart") => serde_json::json!({
            "steps": [
                "ro init",
                "ro add owner/repo",
                "ro sync",
                "ro status",
                "ro health",
            ]
        }),
        Some("examples") => serde_json::json!({
            "examples": [
                {"description": "Daily check-in", "commands": ["ro sync", "ro health", "ro status"]},
                {"description": "Review a change", "commands": ["ro review plan owner/repo --summary 'Bump deps' --risk medium", "ro review list-plans", "ro review apply <plan-id>"]},
                {"description": "Safe commit", "commands": ["ro sweep commit --path . --message 'chore: format'"]},
            ]
        }),
        Some("schemas") => serde_json::json!({
            "schemas": {
                "sync_result": {"repo_id": "string", "action": "string", "status": "string", "duration_ms": "u64", "error": "string|null"},
                "health_snapshot": {"id": "string", "repo_id": "string", "score": "i64", "class": "string"},
            }
        }),
        _ => serde_json::json!({
            "topics": ["commands", "quickstart", "examples", "exit-codes", "formats", "schemas"],
            "usage": "ro robot-docs <topic>"
        }),
    }
}

/// Resolve multi-repo targets from --repos/--filter/--all flags.
fn resolve_multi_repo_targets(
    conn: &ro_state::Connection,
    repos_pattern: Option<&str>,
    filter: Option<&str>,
    all: bool,
    paths: &ConfigPaths,
) -> Result<Vec<(String, PathBuf)>> {
    use ro_sync::manage;

    if !all && repos_pattern.is_none() && filter.is_none() {
        return Ok(Vec::new());
    }

    let tracked = manage::list(conn, None)?;
    let mut targets: Vec<(String, PathBuf)> = Vec::new();

    for repo in &tracked {
        let label = format!("{}/{}", repo.owner, repo.name);
        let matches = if all {
            true
        } else if let Some(pattern) = repos_pattern {
            let glob =
                globset::Glob::new(pattern).unwrap_or_else(|_| globset::Glob::new("*").unwrap());
            let matcher = glob.compile_matcher();
            matcher.is_match(&label)
        } else if let Some(filt) = filter {
            if let Some(rest) = filt.strip_prefix("health:<") {
                if let Ok(threshold) = rest.parse::<i64>() {
                    let snap = ro_state::queries::score_repo_health(conn, &repo.id).ok();
                    snap.is_some_and(|s| s.score < threshold)
                } else {
                    false
                }
            } else if let Some(rest) = filt.strip_prefix("tag:") {
                label.contains(rest)
            } else {
                filt.starts_with("has:")
            }
        } else {
            false
        };

        if matches {
            let local = if repo.local_path.is_empty() {
                paths
                    .state_dir
                    .join("projects")
                    .join(&repo.owner)
                    .join(&repo.name)
            } else {
                PathBuf::from(&repo.local_path)
            };
            targets.push((repo.id.clone(), local));
        }
    }

    Ok(targets)
}

fn run() -> Result<()> {
    use ro_sync::manage;
    use ro_sync::prune;
    use ro_sync::status;
    use ro_sync::sync;

    let cli = Cli::parse();
    let paths = resolve_paths(&cli)?;
    let db_path = paths.state_db();

    match cli.command {
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

        Commands::Add { spec } => {
            let conn = ro_state::open_db(&db_path).context("opening state database")?;
            let projects_dir = paths.state_dir.join("projects");
            let repo = manage::add(&conn, &spec, &projects_dir).context("adding repo")?;
            eprintln!("Added: {}/{} (id={})", repo.owner, repo.name, repo.id);
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
                        OutputFormat::Json => {
                            println!("{}", serde_json::to_string(&repo)?);
                        }
                    }
                }
            }
        }

        Commands::Import {
            file,
            stars,
            org,
            user,
            limit,
        } => {
            let conn = ro_state::open_db(&db_path).context("opening state database")?;
            let projects_dir = paths.state_dir.join("projects");
            if let Some(file) = file {
                let (added, skipped, errors) =
                    manage::import(&conn, &file, &projects_dir).context("importing repos")?;
                if !added.is_empty() {
                    eprintln!("Added {} repos", added.len());
                }
                if !skipped.is_empty() {
                    eprintln!("Skipped {} duplicates", skipped.len());
                }
                if !errors.is_empty() {
                    for (line, err) in &errors {
                        eprintln!("Error: {line}: {err}");
                    }
                }
            } else if stars || org.is_some() || user.is_some() {
                let specs = ro_github::import::fetch_import_specs(
                    stars,
                    org.as_deref(),
                    user.as_deref(),
                    limit,
                )?;
                let mut added_count = 0u32;
                let mut skip_count = 0u32;
                for spec in &specs {
                    match manage::add(&conn, spec, &projects_dir) {
                        Ok(repo) => {
                            if !cli.quiet {
                                eprintln!("Added: {}/{}", repo.owner, repo.name);
                            }
                            added_count += 1;
                        }
                        Err(_) => {
                            skip_count += 1;
                        }
                    }
                }
                eprintln!("Imported {added_count} repos, skipped {skip_count} duplicates");
            } else {
                anyhow::bail!("provide a file path, --stars, --org, or --user");
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
                    OutputFormat::Json => {
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
                        println!(
                            "{}/{}: {}{} ahead={} behind={}",
                            s.owner,
                            s.name,
                            s.branch.as_deref().unwrap_or("HEAD"),
                            dirty,
                            s.ahead,
                            s.behind
                        );
                    }
                    OutputFormat::Json => {
                        println!("{}", serde_json::to_string(s)?);
                    }
                }
            }
        }

        Commands::Prune { archived, missing } => {
            let conn = ro_state::open_db(&db_path).context("opening state database")?;
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

        // ── Health / Inbox ──
        Commands::Health { repo, format } => {
            let conn = ro_state::open_db(&db_path).context("opening state database")?;
            let snapshots = match repo {
                Some(key) => {
                    let found = manage::find_repo(&conn, &key)?;
                    vec![ro_state::queries::score_repo_health(&conn, &found.id)?]
                }
                None => ro_state::queries::score_all_health(&conn)?,
            };
            let repo_labels = repo_labels_by_id(&conn);
            for snap in &snapshots {
                match format {
                    OutputFormat::Text => {
                        let label = repo_labels
                            .get(&snap.repo_id)
                            .cloned()
                            .unwrap_or_else(|| snap.repo_id.clone());
                        println!("{}: score={} class={}", label, snap.score, snap.class);
                    }
                    OutputFormat::Json => {
                        println!("{}", serde_json::to_string(snap)?);
                    }
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

        // ── Review ──
        Commands::Review { sub } => {
            let conn = ro_state::open_db(&db_path).context("opening state database")?;
            match sub {
                ReviewCommands::Plan {
                    repo,
                    summary,
                    risk,
                } => {
                    let found = manage::find_repo(&conn, &repo)?;
                    let risk_class = match risk.as_deref() {
                        Some(r) => Some(parse_risk(r)?),
                        None => None,
                    };
                    let input = ro_review::plan::PlanInput {
                        repo_id: Some(found.id.clone()),
                        kind: "review".into(),
                        plan_json: serde_json::json!({"summary": summary.unwrap_or_default()})
                            .to_string(),
                        risk_class,
                        risk_reasons_json: None,
                        rollback_json: None,
                    };
                    let plan = ro_review::plan::create_plan(&conn, &input)?;
                    eprintln!("Created plan {} for {}", plan.id, repo);
                }
                ReviewCommands::Approve { plan_id } => {
                    ro_review::plan::approve_plan(&conn, &plan_id)?;
                    eprintln!("Approved plan {plan_id}.");
                }
                ReviewCommands::Reject { plan_id } => {
                    ro_review::plan::reject_plan(&conn, &plan_id)?;
                    eprintln!("Rejected plan {plan_id}.");
                }
                ReviewCommands::Apply { plan_id } => {
                    let result = ro_review::apply::apply_plan(&conn, &plan_id)?;
                    eprintln!(
                        "Applied plan {} (status={}, applied_at={})",
                        result.plan_id, result.status, result.applied_at
                    );
                }
                ReviewCommands::Rollback { plan_id } => {
                    ro_review::apply::rollback_plan(&conn, &plan_id)?;
                    eprintln!("Rolled back plan {plan_id}.");
                }
                ReviewCommands::ListPlans => {
                    let plans = ro_review::plan::list_plans(&conn, None)?;
                    for p in &plans {
                        let repo_label = p.repo_id.as_deref().unwrap_or("-");
                        println!("{} {} {} {}", p.id, repo_label, p.kind, p.status);
                    }
                }
            }
        }

        // ── Sweep ──
        Commands::Sweep { sub } => match sub {
            SweepCommands::Commit { path, message } => {
                let result = ro_sweep::commit::sweep_commit(&path, &message)?;
                match result {
                    ro_sweep::commit::CommitOutcome::Committed { oid, .. } => {
                        eprintln!("Committed: {oid}");
                    }
                    ro_sweep::commit::CommitOutcome::NothingToCommit => {
                        eprintln!("Nothing to commit.");
                    }
                    ro_sweep::commit::CommitOutcome::BlockedByGates { failures } => {
                        eprintln!("Blocked by gates: {:?}", failures);
                    }
                    ro_sweep::commit::CommitOutcome::BlockedBySecrets { files } => {
                        eprintln!("Blocked by secrets: {:?}", files);
                    }
                    ro_sweep::commit::CommitOutcome::BlockedByDenylist { files } => {
                        eprintln!("Blocked by denylist: {:?}", files);
                    }
                }
            }
            SweepCommands::Agent {
                path,
                repo_id,
                repos,
                filter,
                all,
                dry_run,
                auto_approve,
                output,
            } => {
                let use_ndjson = output.as_deref() == Some("json");

                if repos.is_some() || filter.is_some() || all {
                    let conn = ro_state::open_db(&db_path).context("opening state database")?;
                    let targets = resolve_multi_repo_targets(
                        &conn,
                        repos.as_deref(),
                        filter.as_deref(),
                        all,
                        &paths,
                    )?;
                    if use_ndjson {
                        let event = ro_output::ndjson::NdjsonEvent::batch_start(
                            targets.len() as u32,
                            "sweep-agent",
                            dry_run,
                        );
                        println!("{}", serde_json::to_string(&event)?);
                    }
                    let mut applied = 0u32;
                    let mut skipped = 0u32;
                    let mut failed = 0u32;
                    for (rid, repo_path) in &targets {
                        if use_ndjson {
                            let event = ro_output::ndjson::NdjsonEvent::repo_start(rid);
                            println!("{}", serde_json::to_string(&event)?);
                        }
                        if dry_run {
                            eprintln!("[dry-run] would sweep {rid}");
                            skipped += 1;
                            if use_ndjson {
                                let event =
                                    ro_output::ndjson::NdjsonEvent::repo_done(rid, "skipped");
                                println!("{}", serde_json::to_string(&event)?);
                            }
                            continue;
                        }
                        let summary = ro_sweep::agent::sweep_repo(repo_path, rid)?;
                        if summary.plan_created {
                            if use_ndjson {
                                let event =
                                    ro_output::ndjson::NdjsonEvent::gates_passed(rid, "plan");
                                println!("{}", serde_json::to_string(&event)?);
                            }
                            if matches!(auto_approve, AutoApproveLevel::Low) && summary.gates_passed
                            {
                                applied += 1;
                                if use_ndjson {
                                    let event =
                                        ro_output::ndjson::NdjsonEvent::repo_done(rid, "ok");
                                    println!("{}", serde_json::to_string(&event)?);
                                }
                            } else {
                                skipped += 1;
                                if use_ndjson {
                                    let event = ro_output::ndjson::NdjsonEvent::repo_done(
                                        rid,
                                        "needs-approval",
                                    );
                                    println!("{}", serde_json::to_string(&event)?);
                                }
                            }
                        } else {
                            failed += 1;
                            if use_ndjson {
                                let reason = summary.error.as_deref().unwrap_or("gates failed");
                                let event =
                                    ro_output::ndjson::NdjsonEvent::gates_failed(rid, reason);
                                println!("{}", serde_json::to_string(&event)?);
                                let event =
                                    ro_output::ndjson::NdjsonEvent::repo_done(rid, "failed");
                                println!("{}", serde_json::to_string(&event)?);
                            }
                        }
                    }
                    if use_ndjson {
                        let event =
                            ro_output::ndjson::NdjsonEvent::batch_done(applied, skipped, failed);
                        println!("{}", serde_json::to_string(&event)?);
                    } else {
                        eprintln!(
                            "Sweep complete: {applied} applied, {skipped} skipped, {failed} failed"
                        );
                    }
                } else {
                    let id = repo_id.as_deref().unwrap_or("unknown");
                    let summary = ro_sweep::agent::sweep_repo(&path, id)?;
                    println!("{}", serde_json::to_string_pretty(&summary)?);
                }
            }
            SweepCommands::CommitSweep {
                all,
                repos,
                filter,
                path,
                execute,
                push,
                push_remote,
                force_with_lease,
                respect_staging,
                allow_protected,
            } => {
                use ro_sweep::commit_sweep::{
                    RepoOutcome, SkipReason, SweepOptions, apply_repo, plan_repo,
                };

                let opts = SweepOptions {
                    execute,
                    respect_staging,
                    push,
                    push_remote,
                    force_with_lease,
                    allow_protected,
                };

                // A --path targets one working copy directly; otherwise the
                // tracked inventory is resolved, same as sweep agent.
                let targets: Vec<(String, PathBuf)> = match path {
                    Some(p) => vec![(p.display().to_string(), p)],
                    None => {
                        let conn =
                            ro_state::open_db(&db_path).context("opening state database")?;
                        resolve_multi_repo_targets(
                            &conn,
                            repos.as_deref(),
                            filter.as_deref(),
                            all,
                            &paths,
                        )?
                    }
                };

                if targets.is_empty() {
                    eprintln!("No repos selected. Use --all, --repos, --filter, or --path.");
                    return Ok(());
                }

                if !execute {
                    eprintln!("Dry run — pass --execute to create commits.\n");
                }

                let mut plans = Vec::new();
                for (rid, repo_path) in &targets {
                    if !repo_path.join(".git").exists() {
                        eprintln!("{rid}: not a git repo, skipping");
                        continue;
                    }
                    match plan_repo(repo_path, rid, &opts) {
                        Ok(plan) => plans.push((repo_path.clone(), plan)),
                        Err(e) => eprintln!("{rid}: {e}"),
                    }
                }

                for (_repo_path, plan) in &plans {
                    if let Some(reason) = &plan.skipped {
                        match reason {
                            SkipReason::Clean => continue,
                            other => {
                                let detail = match other {
                                    SkipReason::ProtectedBranch { branch } => {
                                        format!("protected branch '{branch}'")
                                    }
                                    _ => "nothing committable".to_string(),
                                };
                                eprintln!("{}: skipped — {detail}", plan.repo_id);
                                continue;
                            }
                        }
                    }
                    println!("── {} [{}]", plan.repo_id, plan.branch);
                    for w in &plan.warnings {
                        eprintln!("   ⚠ {w}");
                    }
                    for c in &plan.commits {
                        println!("   {}  ({} file(s))", c.message, c.files.len());
                        for f in &c.files {
                            println!("      {f}");
                        }
                    }
                }

                if !execute {
                    let total: usize = plans
                        .iter()
                        .filter(|(_, p)| p.skipped.is_none())
                        .map(|(_, p)| p.commits.len())
                        .sum();
                    eprintln!(
                        "\nPlanned {total} commit(s) across {} repo(s). Pass --execute to apply.",
                        plans.iter().filter(|(_, p)| p.skipped.is_none()).count()
                    );
                    return Ok(());
                }

                let mut outcomes: Vec<RepoOutcome> = Vec::new();
                for (repo_path, plan) in &plans {
                    if plan.skipped.is_some() {
                        continue;
                    }
                    let outcome = apply_repo(repo_path, plan, &opts);
                    if outcome.failed > 0 {
                        eprintln!(
                            "{}: {} committed, {} failed",
                            outcome.repo_id, outcome.committed, outcome.failed
                        );
                    } else {
                        match &outcome.push_error {
                            Some(err) => eprintln!("{}: {} commit(s), {err}", outcome.repo_id, outcome.committed),
                            None => eprintln!(
                                "{}: {} commit(s){}",
                                outcome.repo_id,
                                outcome.committed,
                                if outcome.pushed { ", pushed" } else { "" }
                            ),
                        }
                    }
                    outcomes.push(outcome);
                }

                let committed: u32 = outcomes.iter().map(|o| o.committed).sum();
                let failed: u32 = outcomes.iter().map(|o| o.failed).sum();
                let pushed = outcomes.iter().filter(|o| o.pushed).count();
                eprintln!(
                    "\nSweep complete: {committed} committed, {failed} failed, {pushed} pushed."
                );
                if failed > 0 {
                    std::process::exit(1);
                }
            }
        },

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
                OutputFormat::Json => {
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
                    let mut config = ro_config::load_config(&cfg_path)?;
                    match key.trim() {
                        "core.projects_dir" => config.core.projects_dir = value.trim().to_string(),
                        "core.layout" => config.core.layout = value.trim().to_string(),
                        "core.parallel" => config.core.parallel = value.trim().parse()?,
                        "core.timeout_secs" => config.core.timeout_secs = value.trim().parse()?,
                        "git.update_strategy" => {
                            config.git.update_strategy = value.trim().to_string()
                        }
                        "git.autostash" => config.git.autostash = value.trim().parse()?,
                        "safety.secret_scan" => {
                            config.safety.secret_scan = value.trim().to_string()
                        }
                        other => anyhow::bail!("unknown config key: {other}"),
                    }
                    ro_config::validate(&config)?;
                    let toml_str = toml::to_string_pretty(&config)?;
                    std::fs::write(&cfg_path, &toml_str)?;
                    eprintln!("Set {key} = {}", value.trim());
                }
            }
        }

        // ── Self-update ──
        Commands::SelfUpdate { check } => {
            let current = env!("CARGO_PKG_VERSION");
            if check {
                eprintln!("Current version: {current}");
                eprintln!("Check https://github.com/quangdang46/repo_orchestrator/releases for updates.");
            } else {
                eprintln!("Current version: {current}");
                eprintln!("To update, re-run the install script:");
                #[cfg(unix)]
                eprintln!(
                    "  curl -fsSL https://raw.githubusercontent.com/quangdang46/repo_orchestrator/main/install.sh | bash"
                );
                #[cfg(windows)]
                eprintln!(
                    "  irm https://raw.githubusercontent.com/quangdang46/repo_orchestrator/main/install.ps1 | iex"
                );
                #[cfg(not(any(unix, windows)))]
                eprintln!("  See https://github.com/quangdang46/repo_orchestrator#install");
            }
        }

        // ── Robot docs ──
        Commands::RobotDocs { topic } => {
            let docs = generate_robot_docs(topic.as_deref());
            println!("{}", serde_json::to_string_pretty(&docs)?);
        }

        // ── Fork ──
        Commands::Fork { sub } => {
            let conn = ro_state::open_db(&db_path).context("opening state database")?;
            match sub {
                ForkCommands::Status { repo, format } => {
                    let repos = match repo {
                        Some(key) => vec![manage::find_repo(&conn, &key)?],
                        None => manage::list(&conn, None)?,
                    };
                    for r in &repos {
                        let path = PathBuf::from(&r.local_path);
                        let is_fork = ro_git::read::has_remote(&path, "upstream");
                        match format {
                            OutputFormat::Text => {
                                let status = if is_fork {
                                    "fork (has upstream)"
                                } else {
                                    "origin only"
                                };
                                println!("{}/{}: {status}", r.owner, r.name);
                            }
                            OutputFormat::Json => {
                                let json = serde_json::json!({
                                    "repo": format!("{}/{}", r.owner, r.name),
                                    "is_fork": is_fork,
                                });
                                println!("{}", serde_json::to_string(&json)?);
                            }
                        }
                    }
                }
                ForkCommands::Sync { repo } => {
                    let repos = match repo {
                        Some(key) => vec![manage::find_repo(&conn, &key)?],
                        None => manage::list(&conn, None)?,
                    };
                    for r in &repos {
                        let path = PathBuf::from(&r.local_path);
                        if ro_git::read::has_remote(&path, "upstream") {
                            match ro_git::mutation::fetch_remote(&path, "upstream") {
                                Ok(()) => {
                                    eprintln!("{}/{}: fetched upstream", r.owner, r.name);
                                }
                                Err(e) => {
                                    eprintln!("{}/{}: fetch upstream failed: {e}", r.owner, r.name);
                                }
                            }
                        }
                    }
                }
                ForkCommands::Clean { repo, dry_run } => {
                    let repos = match repo {
                        Some(key) => vec![manage::find_repo(&conn, &key)?],
                        None => manage::list(&conn, None)?,
                    };
                    for r in &repos {
                        let path = PathBuf::from(&r.local_path);
                        let merged = ro_git::read::merged_branches(&path);
                        for branch in &merged {
                            if dry_run {
                                eprintln!(
                                    "[dry-run] {}/{}: would delete branch {branch}",
                                    r.owner, r.name
                                );
                            } else {
                                eprintln!("{}/{}: cleaned branch {branch}", r.owner, r.name);
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}
