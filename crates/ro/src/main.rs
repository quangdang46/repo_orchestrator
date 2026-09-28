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
mod exit;
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
        /// Also delete the working copy on disk
        ///
        /// Destructive, and gated four ways. Deleting a working copy is the
        /// only way to reclaim disk, and it is also the exact operation a
        /// path-arithmetic bug would aim at *other* repositories — so the
        /// path is printed in full, checked against the registry again
        /// immediately before the removal, and a directory whose origin
        /// belongs to a different registered repo is refused outright.
        #[arg(long)]
        delete: bool,
    },

    /// List tracked repos
    ///
    /// `ro health` was this command. A hidden alias for one release,
    /// so an existing script keeps working.
    #[command(alias = "health")]
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
        /// Repos to sync, by name or alias; omit for every tracked repo
        ///
        /// The same shape as `ro commit` / `ro push` / `ro ship`, and for
        /// the same reason: a name someone typed should outrank a flag left
        /// in a shell profile. Without it `ro sync cass` is a clap error,
        /// which is a usage failure wearing the costume of a name that
        /// happens not to be a flag.
        #[arg(value_name = "REPO")]
        repos: Vec<String>,
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
        /// Network timeout in seconds
        #[arg(long)]
        timeout: Option<u32>,
        /// Restrict to repos matching a selector, e.g. `health:50`
        /// Target repos by filter (e.g. "tag:needs-fmt")
        #[arg(long)]
        filter: Option<String>,
        /// Target repos carrying this tag. `--group` is the same flag.
        ///
        /// Shorthand for `--filter tag:<T>`, and the spelling people
        /// actually reach for. Both names are one mechanism — a tag is a
        /// row in `repo_tags`, and there is no second concept it aliases.
        #[arg(long, visible_alias = "group")]
        tag: Option<String>,
        /// Every tracked repo — the default, stated so the fleet verbs can
        /// be spelled the same way at every verb
        #[arg(long)]
        all: bool,
        /// Retired repos, and repos the user switched off, are skipped
        /// unless this is passed. The default is the safe one: a row that
        /// was archived is a row the user said to leave alone.
        #[arg(long)]
        include_archived: bool,
    },

    /// Show status of tracked repos
    Status {
        /// Specific repo keys: `ro status cass voice-ai-agent`
        #[arg(value_name = "REPO")]
        repos: Vec<String>,
        /// Restrict to repos carrying this tag. `--group` is the same flag.
        #[arg(long, visible_alias = "group")]
        tag: Option<String>,
        /// Restrict to repos matching a selector, e.g. `health:50`
        #[arg(long)]
        filter: Option<String>,
        /// Output format
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },

    // ── Tags (the vocabulary people say is "groups") ─────────────────
    /// Tag a tracked repo
    Tag {
        /// Repo key: name, alias, or owner/name
        repo: String,
        /// One or more tags
        #[arg(required = true)]
        tags: Vec<String>,
    },

    /// Remove tags from a tracked repo
    Untag {
        /// Repo key: name, alias, or owner/name
        repo: String,
        /// One or more tags
        #[arg(required = true)]
        tags: Vec<String>,
    },

    /// List tags, for one repo or across the registry
    Tags {
        /// Repo key; omit to list every tag with a count
        repo: Option<String>,
    },

    // ── Conflict ─────────────────────────────────────────────────────
    /// Commit what the engine finds, and stop
    Commit {
        /// Repos to act on, by name or alias: `ro sync cass voice-ai-agent`
        ///
        /// Positional because that is how the command is typed, and because a
        /// name someone typed should outrank a flag left in a shell profile.
        /// `--pattern` still works and is the glob form.
        #[arg(value_name = "REPO")]
        repos: Vec<String>,

        /// Target repos by glob (e.g. "owner/*")
        #[arg(long)]
        pattern: Option<String>,
        /// Target repos by filter (e.g. "tag:needs-fmt")
        #[arg(long)]
        filter: Option<String>,
        /// Target repos carrying this tag. `--group` is the same flag.
        ///
        /// Shorthand for `--filter tag:<T>`, and the spelling people
        /// actually reach for. Both names are one mechanism — a tag is a
        /// row in `repo_tags`, and there is no second concept it aliases.
        #[arg(long, visible_alias = "group")]
        tag: Option<String>,
        /// Target all tracked repos
        #[arg(long)]
        all: bool,
        /// Which engine commits
        #[arg(long)]
        engine: Option<String>,
        /// A binary under another name, for a nightly or an odd install
        #[arg(long)]
        engine_bin: Option<String>,
        /// The branch the work should land on, when it is not the current one
        #[arg(long)]
        onto: Option<String>,
        /// Let the engine resolve a merge conflict, then verify and
        /// continue the rebase ro owns.
        ///
        /// Off by default: it is the one step where a model edits files
        /// mid-rebase. The common cause of a rejected push is a stale
        /// branch, which is three git commands and no model.
        #[arg(long)]
        resolve: bool,
        /// Preview without writing
        #[arg(long)]
        dry_run: bool,
        /// Retired repos, and repos switched off, are skipped unless this
        /// is passed.
        ///
        /// The default is the safe one: a row the user archived is a row
        /// they said to leave alone, and a fleet run that reached it would
        /// be doing something they did not ask for. This is the way back.
        #[arg(long)]
        include_archived: bool,
    },
    /// Commit and push
    Push {
        /// Repos to act on, by name or alias: `ro sync cass voice-ai-agent`
        ///
        /// Positional because that is how the command is typed, and because a
        /// name someone typed should outrank a flag left in a shell profile.
        /// `--pattern` still works and is the glob form.
        #[arg(value_name = "REPO")]
        repos: Vec<String>,
        /// Target repos by glob (e.g. "owner/*")
        #[arg(long)]
        pattern: Option<String>,
        /// Target repos by filter (e.g. "tag:needs-fmt")
        #[arg(long)]
        filter: Option<String>,
        /// Target repos carrying this tag. `--group` is the same flag.
        ///
        /// Shorthand for `--filter tag:<T>`, and the spelling people
        /// actually reach for. Both names are one mechanism — a tag is a
        /// row in `repo_tags`, and there is no second concept it aliases.
        #[arg(long, visible_alias = "group")]
        tag: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        engine: Option<String>,
        #[arg(long)]
        engine_bin: Option<String>,
        /// The branch the work should land on, when it is not the current one
        #[arg(long)]
        onto: Option<String>,
        /// Let the engine resolve a merge conflict, then verify and
        /// continue the rebase ro owns.
        ///
        /// Off by default: it is the one step where a model edits files
        /// mid-rebase. The common cause of a rejected push is a stale
        /// branch, which is three git commands and no model.
        #[arg(long)]
        resolve: bool,
        #[arg(long)]
        dry_run: bool,
        /// Retired repos, and repos switched off, are skipped unless this
        /// is passed. The default is the safe one.
        #[arg(long)]
        include_archived: bool,
    },
    /// The whole thing: fetch, rebase, commit, push
    Ship {
        /// Repos to act on, by name or alias: `ro sync cass voice-ai-agent`
        ///
        /// Positional because that is how the command is typed, and because a
        /// name someone typed should outrank a flag left in a shell profile.
        /// `--pattern` still works and is the glob form.
        #[arg(value_name = "REPO")]
        repos: Vec<String>,
        /// Target repos by glob (e.g. "owner/*")
        #[arg(long)]
        pattern: Option<String>,
        /// Target repos by filter (e.g. "tag:needs-fmt")
        #[arg(long)]
        filter: Option<String>,
        /// Target repos carrying this tag. `--group` is the same flag.
        ///
        /// Shorthand for `--filter tag:<T>`, and the spelling people
        /// actually reach for. Both names are one mechanism — a tag is a
        /// row in `repo_tags`, and there is no second concept it aliases.
        #[arg(long, visible_alias = "group")]
        tag: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        engine: Option<String>,
        #[arg(long)]
        engine_bin: Option<String>,
        /// The branch the work should land on, when it is not the current one
        #[arg(long)]
        onto: Option<String>,
        /// Let the engine resolve a merge conflict, then verify and
        /// continue the rebase ro owns.
        ///
        /// Off by default: it is the one step where a model edits files
        /// mid-rebase. The common cause of a rejected push is a stale
        /// branch, which is three git commands and no model.
        #[arg(long)]
        resolve: bool,
        #[arg(long)]
        dry_run: bool,
        /// The old `ro sweep commit-sweep`, for one release.
        ///
        /// A flag rather than a hidden subcommand: clap's optional
        /// subcommands have to be enums, and an enum whose only
        /// purpose is to survive one release earns less than the flag
        /// it would replace. The translation is mechanical:
        /// `--execute` becomes the default, since the new verb opts
        /// out with `--dry-run` rather than in.
        #[arg(long, hide = true)]
        commit_sweep: bool,
        /// Retired repos, and repos the user switched off, are skipped
        /// unless this is passed. The default is the safe one: a row that
        /// was archived is a row the user said to leave alone.
        #[arg(long)]
        include_archived: bool,
        /// The old opt-in switch. Now the default.
        #[arg(long, hide = true)]
        execute: bool,
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
    ///
    /// `ro robot-docs` was this command. Hidden alias, one release.
    #[command(alias = "robot-docs")]
    Schema,
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

/// Fold `--tag <T>` into the single filter string the resolver takes.
///
/// `--tag` is shorthand for `--filter tag:<T>`, not a second selector, so
/// the two together are two different answers to "which repos". That is an
/// error rather than a silent precedence rule: picking one would mean a run
/// the user did not ask for, over a fleet that pushes with their
/// credentials.
fn selector_filter(
    verb: &str,
    filter: Option<&str>,
    tag: Option<&str>,
) -> Result<Option<String>> {
    match (filter, tag) {
        (Some(_), Some(_)) => Err(exit::FatalError::usage(format!(
            "`{verb}` takes one selector. `--tag` is shorthand for `--filter tag:<T>` — \
             pass the tag form, or fold it into a single `--filter` expression."
        ))
        .into()),
        (Some(f), None) => Ok(Some(f.to_string())),
        (None, Some(t)) => Ok(Some(format!("tag:{t}"))),
        (None, None) => Ok(None),
    }
}

/// Add the per-repo config's ignore rule, reporting rather than failing.
///
/// Only ever called when `.ro/config.local.toml` exists. Adding the line to a
/// repo that has no `.ro/` dirties the user's working tree, and `ro add`
/// making a repo dirty on the way in is a bug with a long tail: the first
/// thing a user does after enrolling a repo is run `ro status`, and it tells
/// them their new repo has uncommitted changes.
fn ignore_local_config(repo_root: &std::path::Path) {
    if let Err(e) = ro_config::local::ensure_gitignored(repo_root) {
        eprintln!(
            "  warning: could not add `{}` to .gitignore: {e:#}",
            ro_config::local::LOCAL_IGNORE
        );
    }
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
        // A fatal is its own code. Exiting 1 here reported a config file
        // that will not parse as "some repos succeeded", which is worse
        // than a wrong number: it invites a retry that cannot work.
        let (message, code) = match err.downcast_ref::<exit::FatalError>() {
            Some(f) => (f.message.clone(), f.code()),
            // Anything that escapes without a FatalError is still fatal —
            // it just did not say so at the point it was raised.
            None => (format!("{err:#}"), exit::EX_FATAL),
        };
        eprintln!("error: {message}");
        std::process::exit(code as i32);
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
    use ro_sync::status;
    use ro_sync::sync;

    let cli = Cli::parse();
    let paths = resolve_paths(&cli)?;
    let db_path = paths.state_db();
    let non_interactive = cli.non_interactive;

    match cli.command {
        // ── commit / push / ship ───────────────────────────────────────
        //
        // One module, three verbs. The difference is `HowFar`, and a
        // shorter verb stops earlier rather than taking a different path:
        // three paths that mostly agree drift, and the drift surfaces as
        // "commit blocked but push did it anyway".
        Commands::Commit {
            repos: named,
            pattern: glob,
            filter,
            tag,
            all,
            engine,
            engine_bin,
            onto,
            resolve,
            dry_run,
            include_archived,
        } => ship::run_verb(
            &paths,
            ship::HowFar::Commit,
            &named,
            glob.as_deref(),
            selector_filter("commit", filter.as_deref(), tag.as_deref())?.as_deref(),
            all,
            engine.as_deref(),
            engine_bin.as_deref(),
            onto.as_deref(),
            resolve,
            dry_run,
            include_archived,
        ),
        Commands::Push {
            repos: named,
            pattern: glob,
            filter,
            tag,
            all,
            engine,
            engine_bin,
            onto,
            resolve,
            dry_run,
            include_archived,
        } => ship::run_verb(
            &paths,
            ship::HowFar::Push,
            &named,
            glob.as_deref(),
            selector_filter("push", filter.as_deref(), tag.as_deref())?.as_deref(),
            all,
            engine.as_deref(),
            engine_bin.as_deref(),
            onto.as_deref(),
            resolve,
            dry_run,
            include_archived,
        ),
        Commands::Ship {
            repos: named,
            pattern: glob,
            filter,
            tag,
            all,
            engine,
            engine_bin,
            onto,
            commit_sweep,
            execute,
            resolve,
            dry_run,
            include_archived,
        } => {
            // The old `ro sweep commit-sweep` spelling, for one release.
            // A script that breaks on a rename is a script the user has to
            // read the release notes to fix.
            if commit_sweep {
                eprintln!(
                    "warning: ro sweep commit-sweep is now ro ship, and is removed \
                     in the next release. This spelling still works for one release."
                );
                ship::run_verb(
                    &paths,
                    ship::HowFar::Ship,
                    /* named */ &[],
                    /* pattern */ None,
                    /* filter */ None,
                    /* all */ true,
                    /* engine */ None,
                    /* engine_bin */ None,
                    /* onto */ None,
                    /* resolve */ false,
                    /* dry_run */ !execute,
                    /* include_archived */ false,
                );
            }
            ship::run_verb(
                &paths,
                ship::HowFar::Ship,
                &named,
                glob.as_deref(),
                selector_filter("ship", filter.as_deref(), tag.as_deref())?.as_deref(),
                all,
                engine.as_deref(),
                engine_bin.as_deref(),
                onto.as_deref(),
                resolve,
                dry_run,
                include_archived,
            );
        }

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
            let conn = ro_state::open_db(&db_path)
                .map_err(|e| exit::FatalError::new(format!("opening state database: {e}")))?;
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

            // `ro add` records the row and nothing else. An earlier
            // revision also wrote `.ro/config.local.toml` from the flags,
            // which was wrong twice over: it made the file a second copy of
            // columns the registry already holds — the drift this tool is
            // built to avoid — and it left a `.gitignore` behind, so the
            // repo read as dirty the moment it was enrolled. The first
            // thing anyone does after adding a repo is run `ro status`.
            //
            // The file is still read, and still outranks the row. It is for
            // the cases the registry cannot cover: a repo that is not in
            // the registry at all, and settings that must travel with the
            // code to another machine. Both want a file someone wrote on
            // purpose, not one ro manufactured from a row that already
            // says the same thing.
            let local_path = std::path::PathBuf::from(&repo.local_path);
            if ro_config::local::RepoLocalConfig::path_in(&local_path).exists() {
                // A file is already here. Say so, and make sure it is
                // ignored — this is the one case where editing the repo's
                // `.gitignore` is right, because the file is the user's.
                eprintln!("  {} already present", ro_config::local::LOCAL_REL);
                ignore_local_config(&local_path);
            }
        }

        Commands::Remove { key, delete } => {
            let conn = ro_state::open_db(&db_path)
                .map_err(|e| exit::FatalError::new(format!("opening state database: {e}")))?;

            // Resolve **before** removing the row, because the deletion is
            // the part that needs the registry, and a row deleted first
            // would be the one place the check could not consult.
            //
            // A name that matches nothing is a **usage** error, the same
            // code the removal below reports it as. A typo must not read as
            // a broken installation, and it must not read as one on the
            // `--delete` path and not on the other.
            let target = ro_sync::manage::find_repo(&conn, &key).map_err(|e| {
                eprintln!("error: {e:#}");
                std::process::exit(exit::EX_USAGE as i32);
            })?;

            if delete {
                let path = std::path::PathBuf::from(&target.local_path);

                // (1) The path, in full, before anything is asked or done.
                // A prompt that shows a shortened form is a prompt the user
                // cannot check, and this is the only line they get.
                eprintln!("Will delete the working copy at:");
                eprintln!("  {}", path.display());

                // (2) Refuse if this directory is a *different* registered
                // repo. Two repos pointing at one path is the copy-paste
                // accident, and deleting through it removes someone else's
                // work to reclaim this one's disk.
                let wanted = path.to_string_lossy().to_string();
                let clash = manage::list(&conn, None)
                    .unwrap_or_default()
                    .into_iter()
                    .find(|r| r.id != target.id && r.local_path == wanted);
                if let Some(other) = clash {
                    eprintln!(
                        "refused: {} is also the working copy of {}/{}; \
                         remove that one first, or move this one out of the way.",
                        path.display(),
                        other.owner,
                        other.name
                    );
                    std::process::exit(exit::EX_USAGE as i32);
                }

                // (3) Consent: an interactive confirmation, or the caller
                // having said up front that it is not going to answer.
                if !non_interactive && !confirm("Type 'y' to delete it: ") {
                    eprintln!("Not deleted.");
                    std::process::exit(exit::EX_USAGE as i32);
                }

                // (4) The registry, one last time, immediately before the
                // removal. Between (2) and here the only thing that ran was
                // a human reading a line.
                if ro_sync::manage::find_repo(&conn, &key).is_err() {
                    eprintln!("refused: {key} is no longer tracked; nothing was deleted.");
                    std::process::exit(exit::EX_USAGE as i32);
                }

                if path.exists() {
                    std::fs::remove_dir_all(&path).map_err(|e| {
                        exit::FatalError::new(format!("deleting {}: {e}", path.display()))
                    })?;
                    eprintln!("Deleted working copy: {}", path.display());
                } else {
                    eprintln!("Working copy already absent: {}", path.display());
                }
            }

            // A name that matches nothing is a **usage** error, not a
            // fatal one. `context` turned "no such repo" into exit 70,
            // which is the same code as a config file that will not parse
            // — so a typo read as a broken installation.
            let repo = manage::remove(&conn, &key).map_err(|e| {
                eprintln!("error: {e:#}");
                std::process::exit(exit::EX_USAGE as i32);
            })?;
            eprintln!("Removed: {}/{}", repo.owner, repo.name);
        }

        Commands::List { owner, format } => {
            let conn = ro_state::open_db(&db_path)
                .map_err(|e| exit::FatalError::new(format!("opening state database: {e}")))?;
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
            repos,
            strategy,
            format,
            dry_run,
            clone_only,
            pull_only,
            autostash,
            timeout,
            filter,
            tag,
            all,
            include_archived,
        } => {
            if clone_only && pull_only {
                anyhow::bail!("--clone-only and --pull-only cannot be used together");
            }
            let conn = ro_state::open_db(&db_path)
                .map_err(|e| exit::FatalError::new(format!("opening state database: {e}")))?;

            // Names, or everything. Resolved through the same helper the
            // fleet verbs use, so a name that means the same thing to `ro
            // ship` means the same thing here.
            //
            // A name that matches nothing is a **usage** error, not a
            // silent no-op: the user asked for a repository and was told,
            // in a report full of other repositories, that nothing
            // happened. A bare `ro sync` on an empty registry stays exit 0 —
            // there is nothing wrong with asking for everything when there
            // is nothing.
            let selected: Vec<String> = if repos.is_empty() {
                // No name and no pattern: the whole registry. `--all` is the
                // same thing said out loud, for scripts, and a filter narrows
                // it. The fleet verbs already default this way; `sync` did
                // not, so the same argument list meant two different sets of
                // repos depending on which verb it was.
                if all || filter.is_some() || tag.is_some() || include_archived {
                    let targets = ro_sync::targets::resolve_targets(
                        &conn,
                        None,
                        selector_filter("sync", filter.as_deref(), tag.as_deref())?.as_deref(),
                        all,
                        &paths.state_dir.join("projects"),
                        include_archived,
                    )
                    .map_err(|e| {
                        eprintln!("error: {e:#}");
                        std::process::exit(exit::EX_USAGE as i32);
                    })?;
                    targets.iter().map(|t| t.repo_id.clone()).collect()
                } else {
                    Vec::new()
                }
            } else {
                let targets = ro_sync::targets::resolve_targets(
                    &conn,
                    Some(&repos.join(" ")),
                    selector_filter("sync", filter.as_deref(), tag.as_deref())?.as_deref(),
                    all,
                    &paths.state_dir.join("projects"),
                    include_archived,
                )
                .map_err(|e| {
                    eprintln!("error: {e:#}");
                    std::process::exit(exit::EX_USAGE as i32);
                })?;
                if targets.is_empty() {
                    eprintln!(
                        "no repo matched {}",
                        repos
                            .iter()
                            .map(|r| format!("`{r}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    std::process::exit(exit::EX_USAGE as i32);
                }
                targets.iter().map(|t| t.repo_id.clone()).collect()
            };
            let opts = sync::SyncOptions {
                strategy,
                autostash,
                timeout_secs: timeout.unwrap_or(30),
                dry_run,
                clone_only,
                pull_only,
            };
            let repo_labels = repo_labels_by_id(&conn);
            let results = sync::sync_all(&conn, &opts, &selected).context("syncing repos")?;
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

        Commands::Status {
            repos: named,
            tag,
            filter,
            format,
        } => {
            let conn = ro_state::open_db(&db_path)
                .map_err(|e| exit::FatalError::new(format!("opening state database: {e}")))?;
            // Selection goes through the same resolver the fleet verbs use,
            // so "what does that name mean" has one answer in this tool
            // rather than one per verb.
            let filter = selector_filter("status", filter.as_deref(), tag.as_deref())?;
            let statuses: Vec<status::RepoStatus> = if !named.is_empty() || filter.is_some() {
                let names = named.join(" ");
                let targets = ro_sync::targets::resolve_targets(
                    &conn,
                    if names.is_empty() { None } else { Some(names.as_str()) },
                    filter.as_deref(),
                    true,
                    &paths.state_dir.join("projects"),
                    false,
                )
                .map_err(|e| {
                    eprintln!("error: {e:#}");
                    std::process::exit(exit::EX_USAGE as i32);
                })?;
                let mut out = Vec::with_capacity(targets.len());
                for t in &targets {
                    out.push(status::status_repo(&conn, &t.repo_id)?);
                }
                out
            } else {
                status::status_all(&conn)?
            };
            for s in &statuses {
                match format {
                    OutputFormat::Text => {
                        let dirty = if s.is_dirty { " (dirty)" } else { "" };
                        // The two flags that decide whether the *next*
                        // command can work, printed where the eye already
                        // is. A protected branch is not a warning — half
                        // the fleet is on `main` by choice — so it is a
                        // parenthetical, not a banner. A conflict is a
                        // different thing: it invalidates the rest of the
                        // row, so it is named first.
                        let conflict = if s.in_conflict { " [CONFLICT] " } else { "" };
                        let protected = if s.is_protected { " (protected)" } else { "" };
                        // An unmeasurable repo prints `ahead=unknown`, never
                        // `ahead=0`. Printing zero here is the bug this bead
                        // exists to remove: a green board over rows nobody
                        // measured is worse than a visibly broken one,
                        // because the user stops looking.
                        match (s.ahead, s.behind) {
                            (Some(a), Some(b)) => println!(
                                "{}/{}: {}{}{}{} ahead={} behind={}",
                                s.owner,
                                s.name,
                                s.branch.as_deref().unwrap_or("HEAD"),
                                conflict,
                                dirty,
                                protected,
                                a,
                                b
                            ),
                            _ => println!(
                                "{}/{}: {}{}{}{} ahead=unknown behind=unknown — {}",
                                s.owner,
                                s.name,
                                s.branch.as_deref().unwrap_or("HEAD"),
                                conflict,
                                dirty,
                                protected,
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

        // ── Tags ──
        //
        // "Make it so" is idempotent at the exit code: tagging twice, or
        // removing a tag that was never there, both succeed quietly. The
        // row is already in the state the user asked for, and an error
        // there makes shell loops awkward for no gain. The *count* is
        // still printed, so a run that changed nothing says so.
        Commands::Tag { repo, tags } => {
            let conn = ro_state::open_db(&db_path)
                .map_err(|e| exit::FatalError::new(format!("opening state database: {e}")))?;
            let added = ro_sync::tags::add(&conn, &repo, &tags)?;
            let now = ro_sync::tags::of(&conn, &repo)?;
            if added == 0 {
                println!("{repo}: already tagged ({})", now.join(", "));
            } else {
                println!("{repo}: tagged {added} ({})", now.join(", "));
            }
        }

        Commands::Untag { repo, tags } => {
            let conn = ro_state::open_db(&db_path)
                .map_err(|e| exit::FatalError::new(format!("opening state database: {e}")))?;
            let removed = ro_sync::tags::remove(&conn, &repo, &tags)?;
            let now = ro_sync::tags::of(&conn, &repo)?;
            if removed == 0 {
                println!("{repo}: no such tag ({})", now.join(", "));
            } else {
                println!("{repo}: removed {removed} ({})", now.join(", "));
            }
        }

        Commands::Tags { repo } => {
            let conn = ro_state::open_db(&db_path)
                .map_err(|e| exit::FatalError::new(format!("opening state database: {e}")))?;
            match repo {
                Some(key) => {
                    for t in ro_sync::tags::of(&conn, &key)? {
                        println!("{t}");
                    }
                }
                None => {
                    for (t, n) in ro_sync::tags::all_with_counts(&conn)? {
                        println!("{t}\t{n}");
                    }
                }
            }
        }

        // ── Conflict ──

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
            // The doctor is not a fleet command: its own code, and its
            // Severity::Optional probes can never move it.
            std::process::exit(report.exit_code() as i32);
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
                        let conn = ro_state::open_db(&db_path).map_err(|e| {
                            exit::FatalError::new(format!("opening state database: {e}"))
                        })?;
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
