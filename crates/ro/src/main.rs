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
use clap::{CommandFactory, Parser, Subcommand};
use ro_config::paths::ConfigPaths;
use ro_sync::sync::SyncStrategy;

/// One `--format` for every command that has one.
///
/// It lives in `ship::summary` because that is where the fleet verbs render,
/// and a second copy here is how the two drifted apart in the first place:
/// the read commands had `text|json|ndjson` and the fleet verbs had
/// `--format` not at all.
use crate::ship::summary::OutputFormat;

#[derive(Debug, Parser)]
#[command(name = "ro", about = "GitHub-first repo orchestration CLI", version, long_about = None)]
struct Cli {
    /// Where ro keeps its config: config.toml and the tracked-repo registry.
    ///
    /// Accepted before or after the subcommand, because it is `global`. The
    /// default is `$XDG_CONFIG_HOME/ro`, falling back to `~/.config/ro`.
    /// Overriding only this directory leaves the state directory alone, so the
    /// two can point at different installs.
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,

    /// Where ro keeps durable state: the state.db registry and per-run logs.
    ///
    /// Accepted before or after the subcommand, because it is `global`. The
    /// default is `$XDG_STATE_HOME/ro`, falling back to `~/.local/state/ro`.
    /// Overriding only this directory also moves the disposable cache to
    /// `<state-dir>/cache`, so the cache never sits beside a different
    /// installation's durable state.
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,

    /// Skip the confirmation prompt and act without asking.
    ///
    /// Takes no value: `--non-interactive`, never `--non-interactive=true`.
    /// Today it gates exactly one prompt — the "Type 'y' to delete it" consent
    /// before `ro remove --delete` destroys a working copy. Without it, that
    /// prompt is refused when stdin is not a terminal, so a CI run must pass
    /// this flag rather than pipe an answer in.
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
        /// Destructive, and gated five ways. Deleting a working copy is the
        /// only way to reclaim disk, and it is also the exact operation a
        /// path-arithmetic bug — or a corrupted row — would aim at some
        /// *other* directory, so before anything is removed the target has
        /// to be provably the working copy of a still-registered repo: it
        /// must be a git checkout whose `origin` is the URL on the row. A
        /// path that is not a checkout, or whose origin belongs to someone
        /// else, is refused by name. Around that: the path is printed in
        /// full, a directory that is another registered repo's working copy
        /// is refused outright, the registry is consulted again immediately
        /// before the removal, and an interactive run asks.
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
        /// Only repos carrying this tag. `--group` is the same flag.
        #[arg(long, visible_alias = "group")]
        tag: Option<String>,
        /// Print each repo's local path as well as its name.
        #[arg(long)]
        paths: bool,
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
        /// Delete remote-tracking refs whose branch is gone upstream.
        ///
        /// `git remote prune`, not `git fetch --prune`. The narrower of the
        /// two on purpose: this removes local bookkeeping for branches that
        /// no longer exist and never touches the remote, so it is safe to
        /// run across a fleet without asking first.
        #[arg(long)]
        prune: bool,
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
        /// Only repos with uncommitted changes
        #[arg(long)]
        dirty: bool,
        /// Only repos with commits the remote has not seen
        #[arg(long)]
        ahead: bool,
        /// Only repos the remote has moved past
        #[arg(long)]
        behind: bool,
        /// Fetch each repo's remote first, so ahead/behind is current.
        /// Costs a network round trip per repo; without it the counts are
        /// measured against whatever `origin/<branch>` was last.
        #[arg(long)]
        fetch: bool,
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
        /// Output format
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
        /// One commit per repo, with this subject.
        ///
        /// The engine still reads the diff and stages; it just has one
        /// message instead of a choice of them. There is no separate
        /// "do not split" flag, because supplying the subject *is* that
        /// request.
        #[arg(long, value_name = "MSG")]
        message: Option<String>,
        /// Fold the work into HEAD rather than adding a commit.
        ///
        /// Refused when HEAD is already on the remote: amending a commit
        /// someone else has seen rewrites history under a message that no
        /// longer describes it.
        #[arg(long)]
        amend: bool,
        /// The instruction handed to the agent, replacing the built-in one.
        ///
        /// For this run, on every repo it touches. The boundary is
        /// unchanged: the agent still commits, and ro still pushes.
        #[arg(long, value_name = "TEXT")]
        prompt: Option<String>,
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
        /// Every tracked repo — the default, stated out loud. Archived and
        /// switched-off rows are still left out unless `--include-archived`
        /// says otherwise.
        #[arg(long)]
        all: bool,
        /// Engine that commits this run: `claude`, `codex`, or `git`.
        /// Overrides `[agent].engine` from the config for this run only; it
        /// is never written to a row.
        #[arg(long)]
        engine: Option<String>,
        /// A binary that answers to a different name — a nightly build, or
        /// an engine installed outside `PATH`. Overrides
        /// `[agent].command`, and like `--engine` is not written to a row.
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
        /// is passed. The default is the safe one.
        #[arg(long)]
        include_archived: bool,
        /// Output format
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
        /// One commit per repo, with this subject, before pushing.
        #[arg(long, value_name = "MSG")]
        message: Option<String>,
        /// The instruction handed to the agent, replacing the built-in one.
        #[arg(long, value_name = "TEXT")]
        prompt: Option<String>,
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
        /// Every tracked repo — the default, stated out loud. Archived and
        /// switched-off rows are still left out unless `--include-archived`
        /// says otherwise.
        #[arg(long)]
        all: bool,
        /// Engine that commits this run: `claude`, `codex`, or `git`.
        /// Overrides `[agent].engine` from the config for this run only; it
        /// is never written to a row.
        #[arg(long)]
        engine: Option<String>,
        /// A binary that answers to a different name — a nightly build, or
        /// an engine installed outside `PATH`. Overrides
        /// `[agent].command`, and like `--engine` is not written to a row.
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
        /// Output format
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
        /// One commit per repo, with this subject.
        #[arg(long, value_name = "MSG")]
        message: Option<String>,
        /// The instruction handed to the agent, replacing the built-in one.
        #[arg(long, value_name = "TEXT")]
        prompt: Option<String>,
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

/// Are two paths the same directory?
///
/// Compared after canonicalisation, because the two sides arrive by different
/// routes: one is a path the row named, the other is an answer git gave. A
/// byte comparison would refuse a checkout reached through a symlinked parent
/// or a relative path, which is a real checkout the user really did register.
///
/// A path that cannot be canonicalised is never equal to one that could —
/// refusing is the only safe answer when the two cannot be shown to be the
/// same directory.
fn same_path(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// The first nested git repository at or under `root`, other than `root` itself.
///
/// This is a question about the **filesystem**, and it has to be: the thing
/// at risk from `ro remove --delete` on a directory containing a repository
/// is that repository, and by definition it is a repository ro has no row
/// for. Asking the registry instead — which is what the registered-row check
/// above does — answers the question only when the answer was already known.
///
/// A directory containing a `.git` entry is the test, and it is done by
/// walking rather than by listing depth 1, because a nested repository can
/// sit at any depth. The walk is bounded: it descends at most
/// [`NESTED_SCAN_DEPTH`] levels and skips the directories that are build
/// output, where a vendored copy is not a repository the user would miss.
fn find_nested_repo(
    root: &std::path::Path,
    self_dir: &std::path::Path,
) -> Option<std::path::PathBuf> {
    /// Deep enough for a `deps/`, shallow enough to be cheap.
    const NESTED_SCAN_DEPTH: usize = 4;
    /// Never descended into: build output and dependency trees, where a
    /// `.git` entry is vendored rather than a working copy.
    const SKIP: &[&str] = &["target", "node_modules", ".venv", "vendor", "dist", "build"];

    fn walk(
        dir: &std::path::Path,
        depth: usize,
        self_dir: &std::path::Path,
    ) -> Option<std::path::PathBuf> {
        if depth == 0 {
            return None;
        }
        let entries = std::fs::read_dir(dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if dir == self_dir && name == ".git" {
                // The repository we were asked about is not a nested one.
                continue;
            }
            if path.is_dir() && path.join(".git").exists() {
                return Some(path);
            }
            if SKIP.contains(&name.as_ref()) {
                continue;
            }
            if path.is_dir() {
                if let Some(found) = walk(&path, depth - 1, self_dir) {
                    return Some(found);
                }
            }
        }
        None
    }

    walk(root, NESTED_SCAN_DEPTH, self_dir)
}

/// Where does the working tree that contains `dir` actually start?
///
/// `git rev-parse --show-toplevel` is the authority, and it is asked with the
/// working directory set to `dir` so it answers for that path specifically.
/// A directory *inside* a checkout resolves upward to the checkout root; a
/// checkout resolves to itself; a directory belonging to no checkout at all
/// fails, which is the answer this gate wants.
///
/// `ro_git::read::discover` is not a substitute: it returns the path of the
/// `.git` directory, which is the repository, not the working tree the user
/// registered and would have deleted.
fn working_tree_root(dir: &std::path::Path) -> std::result::Result<std::path::PathBuf, String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| format!("running git rev-parse: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(if stderr.trim().is_empty() {
            "git rev-parse failed".to_string()
        } else {
            stderr.trim().to_string()
        });
    }
    let top = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if top.is_empty() {
        return Err("git rev-parse returned no working tree".to_string());
    }
    Ok(std::path::PathBuf::from(top))
}

/// Does a checkout's origin refer to the same repository as the URL on the
/// registry row?
///
/// Both spellings of the same remote are accepted, and nothing else: a
/// `git@host:owner/repo` and an `https://host/owner/repo` URL for one repo
/// are the same repository, while two different hosts, owners, or names are
/// not. The comparison is on the URL's shape, not on its byte string,
/// because the row was written from one spelling and git is holding
/// another — `ro add <path>` records the checkout's own `origin` verbatim,
/// and that is very often the SSH form.
///
/// Credentials are stripped before comparing. A `https://user:pass@host/…`
/// URL carries a secret in its path, and a checkout recorded that way is
/// still the same repository — but comparing the two forms with the
/// credential in place would refuse every such checkout, and printing the
/// credential to explain why would put a secret on stderr.
fn origin_matches(origin: &str, clone_url: &str) -> bool {
    /// `git@host:owner/repo`, `ssh://git@host/owner/repo` and
    /// `https://host/owner/repo` all reduce to `host/owner/repo`.
    ///
    /// The scp-like form is the reason this function is not a `trim` pair:
    /// it has no scheme and separates host from path with a colon, so a
    /// `://`-only normalization leaves `github.com:acme/api` to be compared
    /// against `github.com/acme/api` and never matches. Git accepts both
    /// spellings for the same remote, so a gate that rejected one of them
    /// would refuse a `git clone` of it — a checkout made with the SSH key
    /// rather than a token.
    fn normalize(url: &str) -> String {
        let trimmed = url.trim();
        // `.git` is a repository-name suffix, not part of the name, and
        // either side may or may not carry it.
        let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
        // No scheme means the scp-like spelling, where a colon separates the
        // host from the path. With a scheme, a colon is a port and must be
        // left alone.
        let (after_scheme, scp_form) = match trimmed.find("://") {
            Some(i) => (&trimmed[i + 3..], false),
            None => (trimmed, true),
        };
        let no_credentials = match after_scheme.rfind('@') {
            Some(i) => &after_scheme[i + 1..],
            None => after_scheme,
        };
        let slash_form = match (scp_form, no_credentials.find(':')) {
            (true, Some(i)) => format!("{}/{}", &no_credentials[..i], &no_credentials[i + 1..]),
            _ => no_credentials.to_string(),
        };
        slash_form.trim_matches('/').to_ascii_lowercase()
    }
    normalize(origin) == normalize(clone_url)
}

/// Fold `--tag <T>` into the single filter string the resolver takes.
///
/// `--tag` is shorthand for `--filter tag:<T>`, not a second selector, so
/// the two together are two different answers to "which repos". That is an
/// error rather than a silent precedence rule: picking one would mean a run
/// the user did not ask for, over a fleet that pushes with their
/// credentials.
fn selector_filter(verb: &str, filter: Option<&str>, tag: Option<&str>) -> Result<Option<String>> {
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

/// Resolve the config, state, and cache directories from the flags and the
/// environment.
///
/// Each flag overrides exactly the directory it names, and discovery fills
/// only the ones no flag claimed. This used to match only `(Some, Some)` and
/// fall back to `ConfigPaths::discover()` otherwise, so `--config-dir` alone
/// and `--state-dir` alone were each parsed and then discarded — the flag the
/// user typed had no effect on anything. Measured against the real binary in a
/// clean XDG environment, `ro init --state-dir /tmp/vB/state` printed
/// "Initialized: /tmp/vA/cfg/ro", created nothing under `/tmp/vB`, and exited
/// 0. A CI run isolating its state dir wrote to the real state database with
/// no diagnostic, which is the most dangerous shape the bug can take: it looks
/// like it worked.
///
/// The cache directory is derived from the state directory whenever the state
/// directory is overridden, and otherwise left to discovery. The both-flags
/// arm has always done this, and a single-flag override that pointed
/// `cache_dir` back at the discovered state would put disposable caches beside
/// a *different* installation's durable state — the two would disagree about
/// which install they are talking about.
fn resolve_paths(cli: &Cli) -> Result<ConfigPaths> {
    // Both directories given: nothing to discover. Kept as its own arm
    // because it is the one case that must not depend on the environment at
    // all — `discover()` fails outright when there is no resolvable home and
    // no XDG variable, and a caller who named both directories explicitly has
    // already said where everything goes.
    if let (Some(config_dir), Some(state_dir)) = (&cli.config_dir, &cli.state_dir) {
        return Ok(ConfigPaths {
            config_dir: config_dir.clone(),
            state_dir: state_dir.clone(),
            cache_dir: state_dir.join("cache"),
        });
    }
    // At least one directory is unclaimed, so discovery fills in whatever the
    // flags did not name — and only that.
    let discovered = ConfigPaths::discover()?;
    let config_dir = cli
        .config_dir
        .clone()
        .unwrap_or_else(|| discovered.config_dir.clone());
    let state_dir = cli
        .state_dir
        .clone()
        .unwrap_or_else(|| discovered.state_dir.clone());
    let cache_dir = if cli.state_dir.is_some() {
        state_dir.join("cache")
    } else {
        discovered.cache_dir.clone()
    };
    Ok(ConfigPaths {
        config_dir,
        state_dir,
        cache_dir,
    })
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
        // `values` is only meaningful for an arg that takes a value. A
        // `SetTrue` flag has possible values in clap's model — `--non-interactive`
        // reports `["true", "false"]` — but the flag consumes no operand, so
        // publishing the list told a consumer to build `--non-interactive=true`,
        // which clap rejects as "unexpected value ... no more were expected".
        // A machine-readable reference that advertises an invocation the binary
        // refuses is the same defect class as a flag with no help text, so the
        // list is gated on the arg actually taking a value.
        if !a.get_possible_values().is_empty() && a.get_action().takes_values() {
            o.insert(
                "values".into(),
                a.get_possible_values()
                    .iter()
                    .map(|v| serde_json::Value::from(v.get_name()))
                    .collect::<Vec<_>>()
                    .into(),
            );
        }
        // `takes_value`, so a consumer can tell an arg that consumes an
        // operand from a `SetTrue` flag without guessing from the presence of
        // a `values` list. Without it the two are distinguishable only when
        // the arg has possible values at all, and `--message <MSG>` consumes
        // an operand with no list — so a consumer building an invocation had
        // no published fact to consult. The schema is the tool's machine
        // contract; what it leaves out is a fact the binary knows.
        o.insert("takes_value".into(), a.get_action().takes_values().into());
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
    // The root's own args, in the same flat shape the subcommands get.
    //
    // These were absent entirely, and they are the three flags a person reads
    // first: `--config-dir`, `--state-dir`, `--non-interactive`. They are
    // `global = true`, so they are accepted on every subcommand — but the
    // mirror only walked subcommands, so a consumer of `ro schema` could not
    // see them, and a guard that asserts "every arg has help text" had nothing
    // to check. The flags were documented in `--help` the whole time; they
    // were simply invisible to anything reading the machine-readable tree.
    let root_args: Vec<_> = root
        .get_arguments()
        .filter(|a| !a.is_hide_set())
        .map(arg_json)
        .collect();
    if !root_args.is_empty() {
        o.insert("args".into(), root_args.into());
    }
    let subs: Vec<_> = root.get_subcommands().map(command_json).collect();
    o.insert("commands".into(), subs.into());
    serde_json::Value::Object(o)
}

/// An unknown repo name on `tag` / `untag` / `tags` is a **usage** error.
///
/// `ro_sync::tags::repo_id` returns a plain `anyhow::Context` error, so the
/// top-level handler downcast to `FatalError`, found none, and reported
/// `EX_FATAL` — a mistyped repo name reported as a broken install, on three
/// verbs, where every sibling verb (`ro status nosuchrepo`) reports `EX_USAGE`.
/// That is the exact confusion `exit.rs` documents the 64/70 split as
/// existing to prevent.
fn usage_for_tag_verb(e: anyhow::Error) -> anyhow::Error {
    if e.downcast_ref::<exit::FatalError>().is_some() {
        return e;
    }
    exit::FatalError::usage(format!("{e:#}")).into()
}

/// Resolve multi-repo targets from --pattern/--filter/--all flags.
fn run() -> Result<()> {
    use ro_sync::manage;
    use ro_sync::status;
    use ro_sync::sync;

    let cli = Cli::parse();
    let paths = resolve_paths(&cli)?;
    let db_path = paths.state_db();
    let config = ro_config::load_config(&paths.config_toml()).unwrap_or_default();
    // `core.projects_dir` and `core.layout` are read here, once, and every
    // verb that needs a projects directory asks for it. Both used to be
    // ignored: four call sites did `paths.state_dir.join("projects")` and
    // both path builders did an unconditional `.join(&spec.owner)`, so
    // setting either key changed nothing — `ro add` reported the state-dir
    // path as the destination even when the configured directory was
    // pre-created and empty, and `layout = "flat"` and `layout = "nested"`
    // produced byte-identical destinations.
    //
    // **A key that has not been set keeps the behaviour every existing
    // install already depends on.** The shipped defaults are
    // `projects_dir = "~/projects"` and `layout = "flat"`, but what the code
    // actually does is `<state_dir>/projects/<owner>/<name>`. Honouring the
    // literal default would silently relocate every repo an existing user
    // has on disk, on upgrade, with no migration and no message. So: a
    // value that differs from the shipped default is the user's, and is
    // honoured; the shipped default means "unchanged". `ro config set
    // core.projects_dir=...` still works, which is the whole of the
    // finding.
    let projects_dir = if config.core.projects_dir == SHIPPED_PROJECTS_DIR {
        paths.state_dir.join("projects")
    } else {
        ro_config::paths::expand_tilde(&config.core.projects_dir)
    };
    // `--yes` on push/ship is this same instruction under the spelling a CI
    // script reaches for. One variable, two spellings, so there is a single
    // place that decides whether ro may act without asking.
    let non_interactive = cli.non_interactive;

    // The shipped defaults, spelled out so the "keep the existing
    // behaviour unless the user changed it" rule above has one place to
    // name. See the comment on `projects_dir`.
    const SHIPPED_PROJECTS_DIR: &str = "~/projects";
    let layout: &str = if config.core.layout == "flat" {
        "nested"
    } else {
        config.core.layout.as_str()
    };

    // Say which directories this run is actually using, whenever a flag was
    // given. The bug this replaces did not merely compute the wrong paths — it
    // computed the *discovered* ones and printed nothing, so an isolated CI run
    // looked exactly like a successful one while writing to the real state
    // database. A flag that redirects ro's attention somewhere the user may not
    // have meant is worth one line on the way out, and it costs nothing when no
    // flag was given.
    if cli.config_dir.is_some() || cli.state_dir.is_some() {
        eprintln!("ro: config dir {}", paths.config_dir.display());
        eprintln!("ro: state dir  {}", paths.state_dir.display());
    }

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
            format,
            message,
            amend,
            prompt,
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
            format,
            message,
            amend,
            prompt,
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
            format,
            message,
            prompt,
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
            format,
            message,
            /* amend */ false,
            prompt,
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
            format,
            message,
            prompt,
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
                    /* format */ OutputFormat::Text,
                    /* message */ None,
                    /* amend */ false,
                    /* prompt */ None,
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
                format,
                message,
                /* amend */ false,
                prompt,
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
            let opts = manage::AddOptions {
                name,
                clone_to,
                branch,
                credential_ref: credential,
                engine,
                author_ref: author,
                tags,
            };
            let repo = manage::add_from_input(&conn, &spec, &projects_dir, &opts, layout).map_err(
                |e| {
                    // A bad invocation is a usage error, not a broken
                    // installation. `add_from_input` refuses three things
                    // that are all the caller's fault and none of which
                    // retrying unchanged can fix: a spec that is neither a
                    // remote nor a checkout, a destination that already
                    // exists, and a repo that is already tracked. All three
                    // used to surface as `EX_FATAL` — the same code as a
                    // config file that will not parse — so a typo read as a
                    // broken install and invited a retry that cannot work.
                    // `exit.rs` already says a duplicate add is a usage
                    // problem; this is where that promise is kept.
                    // Raised rather than printed: `main`'s top-level
                    // handler prints it once, so the message appears
                    // exactly one time on the way out.
                    exit::FatalError::usage(format!("adding repo: {e:#}"))
                },
            )?;
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

                // (1a) The row's path must be the checkout, not a link to it.
                //
                // Everything below asks git about `path` with the working
                // directory set there, and git resolves every component of
                // that path before it reads anything. A symlink at `path`
                // therefore means the origin, the repository, and the
                // `remove_dir_all` target are all the *destination's* — so
                // the origin check below happily confirms a directory the
                // user never registered, and the deletion either follows the
                // link into a checkout the row has nothing to do with or
                // takes the real contents of a second clone of the same repo
                // along with the link. Neither is "the thing I registered".
                //
                // `symlink_metadata` rather than `metadata`: it does not
                // follow the link, so a symlink is still recognisable as one
                // even when its target is gone.
                //
                // An error here is deliberately *not* a refusal of its own: the
                // common cause is a path that is not there at all, and the
                // check further down reports that with a message naming the
                // row and the repo, which is more use than "cannot be
                // inspected". Refusing twice would only cost the better
                // message.
                if let Ok(md) = std::fs::symlink_metadata(&path) {
                    if md.file_type().is_symlink() {
                        eprintln!(
                            "refused: {} is a symlink, so it is not the working copy \
                             the row registered — it points somewhere else, and \
                             deleting through it would act on that somewhere \
                             else. Nothing was deleted. Point the row at the \
                             real checkout, or remove the link yourself.",
                            path.display()
                        );
                        std::process::exit(exit::EX_USAGE as i32);
                    }
                }

                // (2) Refuse if this directory is, or *contains*, a
                // different registered repo. Two repos pointing at one path
                // is the copy-paste accident, and deleting through it removes
                // someone else's work to reclaim this one's disk.
                //
                // Containment is the case the exact-equality check missed,
                // and it is the worse one: a repo registered *inside* the
                // one being deleted has a different `local_path`, so the
                // check did not fire, `remove_dir_all` took the nested
                // checkout with it, and the nested row was left dangling at
                // a path that no longer exists — so the next `ro ship` or
                // `ro doctor` reports "not a git repo" for a repo the user
                // never asked to touch. The help text promises a directory
                // belonging to another registered repo is "refused outright";
                // a directory *containing* one belongs to it at least as much.
                // Containment is compared on **canonicalised** paths. It was
                // a raw string prefix over the row's verbatim `local_path`,
                // so a trailing slash turned the probe into `<path>//` and a
                // nested row was never seen — the exact case this gate
                // exists to catch, defeated by a spelling. `ro add`
                // canonicalises, so this needs a hand-edited or
                // arithmetically-derived row, which is the threat model the
                // gate's own comment claims to cover.
                let wanted = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                let wanted = wanted.to_string_lossy().to_string();
                let clash = manage::list(&conn, None)
                    .unwrap_or_default()
                    .into_iter()
                    .find(|r| {
                        if r.id == target.id {
                            return false;
                        }
                        let other = std::fs::canonicalize(&r.local_path)
                            .unwrap_or_else(|_| PathBuf::from(&r.local_path));
                        let other = other.to_string_lossy().to_string();
                        other == wanted
                            || other.starts_with(&format!("{wanted}/"))
                            || other.starts_with(&format!("{wanted}\\"))
                    });
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

                // (3) The target must actually BE the working copy of the
                // row being removed — not merely the path the row happens
                // to name. A row is a claim about the filesystem, and a
                // claim is not evidence: a corrupted row, or one written by
                // path arithmetic that resolved wrong, names a directory
                // that has nothing to do with this repo, and the only thing
                // standing between it and `remove_dir_all` was the row
                // itself. So the check is made against the filesystem,
                // and it is made against the *origin*, not against the
                // row's `clone_url` column: a checkout whose origin is a
                // fork, a mirror, or a local path is a real checkout of a
                // real repo, and refusing it would make `--delete` unusable
                // for exactly the repos people keep locally. What has to
                // match is that the directory is a git repository at all,
                // and that it has an origin to compare — a checkout with no
                // remote is not this repo's working copy, whatever the row
                // says.
                //
                // This is the gate that makes `--non-interactive` safe. It
                // is the only one of the five that does not consult the
                // registry, so it is the only one a corrupted row cannot
                // defeat.
                //
                // A path that does not exist is refused here rather than
                // falling through to the "already absent" branch below.
                // That branch is for a checkout that was moved or deleted
                // out from under the row; a path that was never there is
                // the row naming something that is not this repo, and the
                // two are different facts with different fixes.
                if !path.exists() {
                    eprintln!(
                        "refused: {} does not exist, so it is not the working copy \
                         of {}/{}; nothing was deleted. The row names this path, \
                         and the filesystem does not agree — fix the row, or \
                         restore the checkout, before deleting through it.",
                        path.display(),
                        target.owner,
                        target.name
                    );
                    std::process::exit(exit::EX_USAGE as i32);
                }
                let origin = match ro_git::read::remote_url(&path, "origin") {
                    Ok(Some(url)) => url,
                    Ok(None) => {
                        eprintln!(
                            "refused: {} is a git checkout with no origin remote, \
                             so it cannot be the working copy of {}/{}; \
                             nothing was deleted.",
                            path.display(),
                            target.owner,
                            target.name
                        );
                        std::process::exit(exit::EX_USAGE as i32);
                    }
                    Err(e) => {
                        eprintln!(
                            "refused: {} is not a git repository ({e:#}); \
                             nothing was deleted.",
                            path.display()
                        );
                        std::process::exit(exit::EX_USAGE as i32);
                    }
                };
                if !origin_matches(&origin, &target.clone_url) {
                    eprintln!(
                        "refused: {} has origin {origin}, which is not the clone URL \
                         on the {}/{} row ({}); nothing was deleted.",
                        path.display(),
                        target.owner,
                        target.name,
                        target.clone_url
                    );
                    std::process::exit(exit::EX_USAGE as i32);
                }

                // (3b) The path must be the checkout, not a directory that
                // merely contains it.
                //
                // A parent of the real checkout passes every check above: it
                // exists, it is a git repository, and `git remote get-url`
                // run there answers with the nested checkout's origin — which
                // is the row's clone URL. So the gate would wave it through
                // and `remove_dir_all` would take the parent and everything
                // nested in it, on the strength of a path that names a
                // directory the checkout happens to sit inside. The row must
                // name the checkout itself.
                //
                // `git rev-parse --show-toplevel` is the authority on where the
                // checkout actually starts, and it is asked of the path the
                // row named — so a parent, a grandparent, or a sibling that
                // happens to be inside the same repository all refuse. A
                // worktree's own `.git` file makes this the one check that
                // distinguishes "the checkout" from "somewhere inside it".
                let toplevel = match working_tree_root(&path) {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!(
                            "refused: {} is not inside a git working tree ({e}), so it \
                             is not the working copy of {}/{}; nothing was deleted.",
                            path.display(),
                            target.owner,
                            target.name
                        );
                        std::process::exit(exit::EX_USAGE as i32);
                    }
                };
                if !same_path(&toplevel, &path) {
                    eprintln!(
                        "refused: {} is not the working copy of {}/{} — the \
                         checkout it contains starts at {}. Deleting here would \
                         take that directory and everything nested in it. \
                         Nothing was deleted.",
                        path.display(),
                        target.owner,
                        target.name,
                        toplevel.display()
                    );
                    std::process::exit(exit::EX_USAGE as i32);
                }
                // `--show-toplevel` is not enough on its own. A parent
                // directory that is **its own** git repository answers
                // `--show-toplevel` with itself, so the check above is
                // satisfied by the very directory it exists to exclude —
                // and if that parent also carries an `origin` equal to the
                // row's `clone_url`, gate 3 waves it through too. The row
                // then deletes a directory containing a live registered
                // checkout, which is precisely what gate 2 is for, so gate
                // 2 is asked directly and unconditionally here.
                //
                // The existing test for this (`delete_refuses_a_parent_of_
                // the_real_checkout`) did not catch it: it built the parent
                // with `git init` and no remote, so the run was refused by
                // gate 3's "no origin remote" branch and never reached 3b —
                // asserting nothing about it, despite its comment claiming
                // the parent's origin matched the row.
                let nested = manage::list(&conn, None)
                    .unwrap_or_default()
                    .into_iter()
                    .find(|r| {
                        if r.id == target.id {
                            return false;
                        }
                        let other = std::fs::canonicalize(&r.local_path)
                            .unwrap_or_else(|_| PathBuf::from(&r.local_path));
                        same_path(&other, &path) || other.starts_with(&path)
                    });
                if let Some(other) = nested {
                    eprintln!(
                        "refused: {} is a parent of the registered working copy of \
                         {}/{} ({}). Deleting here would take that checkout with \
                         it. Nothing was deleted — remove the nested one first, or \
                         point this row at its own checkout.",
                        path.display(),
                        other.owner,
                        other.name,
                        other.local_path
                    );
                    std::process::exit(exit::EX_USAGE as i32);
                }

                // (3c) The same question asked of the **filesystem** rather
                // than of the registry.
                //
                // (3b) consults the rows, so it only fires when the nested
                // repository happens to be registered too. An unregistered
                // nested clone — which is what a stray `git clone` inside a
                // checkout is, and what someone tidying up is most likely to
                // have — sailed straight through and took a user's uncommitted
                // work with it. Reproduced on a real directory: `precious.txt`
                // destroyed, exit 0, "Deleted working copy" in the log.
                //
                // The registry cannot answer this question because the thing
                // at risk is precisely the thing nobody registered. A nested
                // repository is a fact about the directory being deleted, so
                // the directory is where it has to be read from — and the
                // working tree is walked, not the depth-1 listing, because a
                // clone can sit at any depth under a checkout.
                if let Some(found) = find_nested_repo(&path, &path) {
                    eprintln!(
                        "refused: {} contains another git repository at {}. Deleting it \
                         would destroy that repository too, and ro has no row saying it \
                         is safe to lose. Nothing was deleted — remove it first, or point \
                         this row at its own checkout.",
                        path.display(),
                        found.display()
                    );
                    std::process::exit(exit::EX_USAGE as i32);
                }

                // (4) Consent: an interactive confirmation, or the caller
                // having said up front that it is not going to answer.
                if !non_interactive && !confirm("Type 'y' to delete it: ") {
                    eprintln!("Not deleted.");
                    std::process::exit(exit::EX_USAGE as i32);
                }

                // (5) The registry, one last time, immediately before the
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
                    // Unreachable in practice: (3) refused above if the path
                    // was not there. Kept so a removal that races a
                    // concurrent delete is reported as what happened rather
                    // than as a `remove_dir_all` failure on a missing path.
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

        Commands::List {
            owner,
            tag,
            paths,
            format,
        } => {
            let conn = ro_state::open_db(&db_path)
                .map_err(|e| exit::FatalError::new(format!("opening state database: {e}")))?;
            let mut repos = manage::list(&conn, owner.as_deref()).context("listing repos")?;
            if let Some(t) = tag.as_deref() {
                // A real `repo_tags` lookup. Matching the tag against the
                // printed label is how `--filter tag:` used to select
                // repositories by coincidence of their *name*.
                repos.retain(|r| {
                    ro_sync::tags::of(&conn, &format!("{}/{}", r.owner, r.name))
                        .map(|v| v.iter().any(|x| x == t))
                        .unwrap_or(false)
                });
            }
            // The empty-registry message is for an empty registry. It used
            // to be printed whenever the list came back empty, so `ro list
            // --tag nonexistent` over a fleet of twelve answered "No tracked
            // repos. Use 'ro add <spec>' to add one." — a summary that
            // disagreed with the registry, written to stderr where it reads
            // as an error, in all three formats. A selector that matches
            // nothing is a question with an empty answer, not a claim that
            // nothing is tracked.
            let selector_given = owner.is_some() || tag.is_some();
            if repos.is_empty() && !selector_given {
                eprintln!("No tracked repos. Use 'ro add <spec>' to add one.");
            } else {
                for repo in &repos {
                    match format {
                        OutputFormat::Text => {
                            // The path is the thing you paste into `cd`, and
                            // `ro list` is where you go to find out where a
                            // repo is. Behind a flag it is one more thing to
                            // remember for the answer to the question the
                            // command is asked.
                            if paths {
                                println!("{}  {}", repo, repo.local_path);
                            } else {
                                println!("{}", repo);
                            }
                        }
                        // `json` is a document and `ndjson` is a stream.
                        // They shared one `println!`, so the flag named
                        // `json` produced ndjson and a consumer doing
                        // `json.load(stdout)` on it raised "Extra data:
                        // line 2 column 1".
                        OutputFormat::Json => {
                            let rows: Vec<String> = repos
                                .iter()
                                .map(serde_json::to_string)
                                .collect::<Result<Vec<_>, _>>()?;
                            println!("[{}]", rows.join(","));
                        }
                        // One object per line, so a consumer can read the
                        // first repo without waiting for the last.
                        OutputFormat::Ndjson => {
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
            prune,
        } => {
            if clone_only && pull_only {
                // `FatalError::usage`, not `bail!`. Two mutually exclusive flags
                // on the command line is the canonical usage error, and a
                // bare `bail!` is caught by the top-level handler, which has
                // nothing to downcast to and reports `EX_FATAL` — so a
                // mistyped command line read as a broken install.
                return Err(exit::FatalError::usage(
                    "--clone-only and --pull-only cannot be used together",
                )
                .into());
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
                        &projects_dir,
                        include_archived,
                    )
                    .map_err(|e| {
                        eprintln!("error: {e:#}");
                        std::process::exit(exit::EX_USAGE as i32);
                    })?;
                    // A selector that matched nothing is a **usage** error,
                    // not a clean run over the whole fleet. `sync_all`
                    // filters with `selected.is_empty() || …`, where an
                    // empty list is indistinguishable from "no selector
                    // given" — and both mean "everything". So
                    // `--filter tag:nonexistent` synced every repo in the
                    // fleet and reported success, while `ro status` with the
                    // identical filter correctly returned nothing. The
                    // resolver's own guarantee is that the two verbs cannot
                    // disagree about identical input.
                    //
                    // A bare `ro sync` on an empty registry stays exit 0:
                    // there is nothing wrong with asking for everything when
                    // there is nothing, and that is the case this refusal is
                    // scoped away from.
                    if targets.is_empty() && (filter.is_some() || tag.is_some()) {
                        eprintln!(
                            "no repo matched the given --filter/--tag. \
                             Nothing was synced."
                        );
                        std::process::exit(exit::EX_USAGE as i32);
                    }
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
                    &projects_dir,
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
            // `core.timeout_secs` is the shipped default, and the flag
            // outranks it. The shipped default happens to also be 30, so
            // hardcoding `unwrap_or(30)` was invisible until a user raised or
            // lowered the key — at which point the per-git deadline they
            // configured was silently not applied and a hung remote ran to
            // git's own limits.
            let opts = sync::SyncOptions {
                strategy,
                autostash,
                timeout_secs: timeout.unwrap_or(config.core.timeout_secs),
                dry_run,
                clone_only,
                pull_only,
                prune,
            };
            let repo_labels = repo_labels_by_id(&conn);
            // `core.parallel` threaded through the seam that exists for exactly
            // this. `sync_all` hardcodes `default_parallel()` — the compiled-in
            // 8 — so the key was accepted, echoed by `ro config`, and ignored,
            // and the `runs` row recorded `parallel=8` for a run the user had
            // asked to make one repo at a time.
            let results = sync::sync_all_bounded(
                &conn,
                &opts,
                &selected,
                config.core.parallel.max(1) as usize,
            )
            .context("syncing repos")?;
            for r in &results {
                match format {
                    // `json` is a document and is printed once, after the
                    // loop — see below. Nothing in this match handles it.
                    OutputFormat::Json => {}
                    OutputFormat::Text => {
                        let label = repo_labels
                            .get(&r.repo_id)
                            .cloned()
                            .unwrap_or_else(|| r.repo_id.clone());
                        // The reason belongs on the default line whenever
                        // there is one. It used to be printed only by the
                        // JSON arm, so a plain `ro sync` on a tree whose
                        // autostash pop conflicted said
                        //
                        //     work/mixed action=pull status=autostash_conflict
                        //
                        // and nothing else — dropping the very text the
                        // autostash fix was written to surface, which names
                        // `git stash list`, where the stash is, and how to
                        // bring the work back.
                        //
                        // `reason` is the other half. `skipped_dirty` and
                        // `skipped_unpushed` are skips, not errors, so their
                        // explanation rides on `reason` rather than on
                        // `error` — and the line below used to read `error`
                        // only, which meant `skipped_dirty` reached the user
                        // as the bare word and `(use --autostash)` reached
                        // the database and nobody else. A reader of the
                        // default line is exactly the reader the word was
                        // addressed to.
                        let why = r
                            .error
                            .as_deref()
                            .or(r.reason.as_deref())
                            .filter(|_| r.status != "success");
                        match why {
                            Some(why) => {
                                println!("{label} action={} status={} — {why}", r.action, r.status)
                            }
                            None => println!("{label} action={} status={}", r.action, r.status),
                        }
                        // The predict-then-verify warning. It is computed,
                        // stored on the row, and emitted in JSON — and it
                        // was rendered in no format at all, because
                        // `plan_mismatch` had 25 references in `sync.rs` and
                        // zero outside it. So a user running plain `ro sync`,
                        // in the human-facing format, never learned that the
                        // dry-run preview they based their decision on had
                        // been wrong. The field's own doc says "a
                        // disagreement nobody can read is a disagreement
                        // nobody acts on"; this is the line that makes it
                        // readable.
                        if let Some(why) = r.plan_mismatch.as_deref() {
                            println!("{label} plan-mismatch: {why}");
                        }
                    }
                    OutputFormat::Ndjson => {
                        println!("{}", serde_json::to_string(r)?);
                    }
                }
            }
            // `json` is a **document**, so it is printed once, after the
            // loop. It used to sit inside `for r in &results` and re-serialise
            // and re-print the ENTIRE fleet once per repo, so N repos produced
            // N concatenated copies of the whole array — 13 repos gave 13
            // arrays and 64,701 bytes, and `json.load()` raised "Extra data:
            // line 2 column 1". The identical bug was already found and fixed
            // for `ro list` above, with a comment naming that exact error;
            // the fix was not carried across to sync.
            if let OutputFormat::Json = format {
                let rows: Vec<String> = results
                    .iter()
                    .map(serde_json::to_string)
                    .collect::<Result<Vec<_>, _>>()?;
                println!("[{}]", rows.join(","));
            }

            // The run-level verdict has to reach the shell.
            //
            // `sync_all` computes `run_exit_code(&results)` and writes it to
            // the `runs` table via `finalize_run`, and then this arm used to
            // print the rows and fall off the end of `run()` — so the verdict
            // was computed, persisted, and dropped before any script could
            // see it. `doctor` and `ship` both call `process::exit`; `sync`
            // was the only fleet verb that did not, which meant a run whose
            // only bad repo was an autostash conflict — a tree full of
            // conflict markers with the user's work parked in a stash —
            // exited 0. FEATURES.md promises "the exit code carries the
            // run-level verdict"; this is where that promise is kept.
            std::process::exit(sync::run_exit_code(&results));
        }

        Commands::Status {
            repos: named,
            tag,
            filter,
            dirty: only_dirty,
            ahead: only_ahead,
            behind: only_behind,
            fetch,
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
                    if names.is_empty() {
                        None
                    } else {
                        Some(names.as_str())
                    },
                    filter.as_deref(),
                    true,
                    &projects_dir,
                    false,
                )
                .map_err(|e| {
                    eprintln!("error: {e:#}");
                    std::process::exit(exit::EX_USAGE as i32);
                })?;
                // A name that matches nothing is a **usage** error, the same
                // code `ro sync` reports it as. It used to print nothing on
                // stdout, nothing on stderr, and exit 0 — while `ro sync
                // <name>` exited 64, `ro tag <name>` exited 70, and `ro
                // remove <name>` succeeded. Four verbs, four answers to
                // "what does the name alpha mean", and the silent one is
                // indistinguishable from "that repo is clean".
                //
                // Scoped to a typed **name**. A `--filter` matching nothing
                // stays an empty report, because a filter is a question
                // about the fleet rather than a claim about a specific repo,
                // and "none of them are dirty" is a real answer to it.
                if targets.is_empty() && !named.is_empty() {
                    eprintln!(
                        "no repo matched {}",
                        named
                            .iter()
                            .map(|r| format!("`{r}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    std::process::exit(exit::EX_USAGE as i32);
                }
                let mut out = Vec::with_capacity(targets.len());
                for t in &targets {
                    out.push(status::status_repo_with(&conn, &t.repo_id, fetch)?);
                }
                out
            } else {
                status::status_all_with(&conn, fetch)?
            };

            // `--dirty` / `--ahead` / `--behind` narrow *after* selection,
            // not instead of it: `ro status --tag work --dirty` is the
            // question the flags exist for, and a filter that replaced the
            // tag would make the two mutually exclusive for no reason.
            //
            // `None` on ahead/behind means unmeasurable — a repo with no
            // upstream, or no worktree. Such a row is *excluded* from
            // `--ahead`/`--behind` rather than counted as zero: "not ahead"
            // and "not known" are different facts, and conflating them is
            // how a fleet board goes green over rows nobody measured.
            let statuses: Vec<status::RepoStatus> = statuses
                .into_iter()
                .filter(|s| !only_dirty || s.is_dirty)
                .filter(|s| !only_ahead || s.ahead.unwrap_or(0) > 0)
                .filter(|s| !only_behind || s.behind.unwrap_or(0) > 0)
                .collect();

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
                        // A repo with no `origin` measures 0/0 perfectly
                        // well — there is nothing to be behind — so the
                        // counts alone render it identically to a repo that
                        // is genuinely in sync with a remote it has. JSON
                        // carried `has_upstream: false` and the text board
                        // threw it away, so the default format a person
                        // reads could not tell "in sync" from "no remote to
                        // be in sync with". The marker is what `has_upstream`
                        // was added for.
                        let no_upstream = if s.has_upstream { "" } else { " (no origin)" };
                        // An unmeasurable repo prints `ahead=unknown`, never
                        // `ahead=0`. Printing zero here is the bug this bead
                        // exists to remove: a green board over rows nobody
                        // measured is worse than a visibly broken one,
                        // because the user stops looking.
                        match (s.ahead, s.behind) {
                            (Some(a), Some(b)) => println!(
                                "{}/{}: {}{}{}{}{} ahead={} behind={}",
                                s.owner,
                                s.name,
                                s.branch.as_deref().unwrap_or("HEAD"),
                                conflict,
                                dirty,
                                protected,
                                no_upstream,
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
                    // A **document**, not a stream. `--format json` and
                    // `--format ndjson` were the same `println!`, so a
                    // consumer doing `json.load(stdout)` on the flag named
                    // `json` — the obvious thing to do — raised
                    // "Extra data: line 2 column 1". The help text gives
                    // ndjson its own description ("One JSON object per
                    // line, for a consumer that reads a stream"), which was
                    // the behaviour `json` also had: a flag that changed
                    // nothing, under two names.
                    OutputFormat::Json => {
                        let rows: Vec<String> = statuses
                            .iter()
                            .map(serde_json::to_string)
                            .collect::<Result<Vec<_>, _>>()?;
                        println!("[{}]", rows.join(","));
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
            let added = ro_sync::tags::add(&conn, &repo, &tags).map_err(usage_for_tag_verb)?;
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
            let removed = ro_sync::tags::remove(&conn, &repo, &tags).map_err(usage_for_tag_verb)?;
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
                    for t in ro_sync::tags::of(&conn, &key).map_err(usage_for_tag_verb)? {
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
                state_db_path: None,
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
                        .ok_or_else(|| exit::FatalError::usage("expected KEY=VALUE format"))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── resolve_paths: each flag is independently overridable ────────────
    //
    // `resolve_paths` used to match only `(Some, Some)` and fall back to
    // `ConfigPaths::discover()` otherwise, so `--config-dir` alone and
    // `--state-dir` alone were both silently discarded. Measured against the
    // real binary: `ro init --state-dir /tmp/vB/state` printed
    // "Initialized: /tmp/vA/cfg/ro", created nothing under /tmp/vB, and
    // exited 0 — a CI run isolating its state wrote to the real state
    // database with no diagnostic. That is the most dangerous shape the bug
    // takes, so the tests below pin each of the four combinations.

    /// `--state-dir` alone must be honoured, not dropped.
    ///
    /// This is the case that was measured broken: the flag was parsed, the
    /// value was thrown away, and the run proceeded against the discovered
    /// state directory.
    #[test]
    fn state_dir_alone_is_honoured_not_discovered() {
        let cli = Cli {
            config_dir: None,
            state_dir: Some(PathBuf::from("/tmp/ro-test/state-only")),
            non_interactive: false,
            command: Commands::Init,
        };
        let paths = resolve_paths(&cli).expect("paths resolve");
        assert_eq!(
            paths.state_dir,
            PathBuf::from("/tmp/ro-test/state-only"),
            "--state-dir alone was discarded in favour of the discovered path"
        );
        // The config side is still discovered: only the flag that was given
        // is overridden.
        assert_ne!(
            paths.config_dir,
            PathBuf::from("/tmp/ro-test/state-only"),
            "an override of one directory must not invent the other"
        );
    }

    /// `--config-dir` alone must be honoured, not dropped.
    #[test]
    fn config_dir_alone_is_honoured_not_discovered() {
        let cli = Cli {
            config_dir: Some(PathBuf::from("/tmp/ro-test/cfg-only")),
            state_dir: None,
            non_interactive: false,
            command: Commands::Init,
        };
        let paths = resolve_paths(&cli).expect("paths resolve");
        assert_eq!(
            paths.config_dir,
            PathBuf::from("/tmp/ro-test/cfg-only"),
            "--config-dir alone was discarded in favour of the discovered path"
        );
        assert_ne!(
            paths.state_dir,
            PathBuf::from("/tmp/ro-test/cfg-only"),
            "an override of one directory must not invent the other"
        );
    }

    /// Both flags together: the case that already worked, pinned so the fix
    /// for the two single-flag cases cannot take it away.
    #[test]
    fn both_flags_together_are_honoured() {
        let cli = Cli {
            config_dir: Some(PathBuf::from("/tmp/ro-test/cfg")),
            state_dir: Some(PathBuf::from("/tmp/ro-test/state")),
            non_interactive: false,
            command: Commands::Init,
        };
        let paths = resolve_paths(&cli).expect("paths resolve");
        assert_eq!(paths.config_dir, PathBuf::from("/tmp/ro-test/cfg"));
        assert_eq!(paths.state_dir, PathBuf::from("/tmp/ro-test/state"));
    }

    /// Neither flag: discovery, unchanged.
    #[test]
    fn no_flags_still_discovers() {
        let cli = Cli {
            config_dir: None,
            state_dir: None,
            non_interactive: false,
            command: Commands::Init,
        };
        let paths = resolve_paths(&cli).expect("paths resolve");
        assert!(
            paths.config_dir.ends_with("ro"),
            "the discovered config dir is the XDG one, got {}",
            paths.config_dir.display()
        );
        assert!(
            paths.state_dir.ends_with("ro"),
            "the discovered state dir is the XDG one, got {}",
            paths.state_dir.display()
        );
    }

    /// The cache directory follows the state directory whenever the state
    /// directory is overridden.
    ///
    /// The both-flags arm already derived `cache_dir` from `state_dir`, so a
    /// single-flag override that left it pointing at the *discovered* state
    /// directory would put disposable caches next to a different
    /// installation's durable state — the two would disagree about which
    /// install they are talking about. With no override at all, discovery
    /// still wins, because that is where `XDG_CACHE_HOME` points and nobody
    /// asked for anything else.
    #[test]
    fn the_cache_dir_follows_the_state_dir_whenever_it_is_overridden() {
        for state_dir in [
            Some(PathBuf::from("/tmp/ro-test/s")),
            Some(PathBuf::from("/tmp/ro-test/other")),
        ] {
            let cli = Cli {
                config_dir: None,
                state_dir,
                non_interactive: false,
                command: Commands::Init,
            };
            let paths = resolve_paths(&cli).expect("paths resolve");
            assert_eq!(
                paths.cache_dir,
                paths.state_dir.join("cache"),
                "cache_dir must be state_dir/cache, got {} for state {}",
                paths.cache_dir.display(),
                paths.state_dir.display()
            );
        }
    }

    /// With no override, discovery still supplies all three directories.
    #[test]
    fn no_override_leaves_all_three_to_discovery() {
        let cli = Cli {
            config_dir: None,
            state_dir: None,
            non_interactive: false,
            command: Commands::Init,
        };
        let paths = resolve_paths(&cli).expect("paths resolve");
        for (label, dir) in [
            ("config", &paths.config_dir),
            ("state", &paths.state_dir),
            ("cache", &paths.cache_dir),
        ] {
            assert!(
                dir.ends_with("ro"),
                "the discovered {label} dir is the XDG one, got {}",
                dir.display()
            );
        }
    }

    /// The two spellings of one remote are the same repository.
    ///
    /// A row is written from whatever the user typed; git is holding
    /// whatever the remote answered with. Refusing a checkout because the
    /// two strings differ would make `--delete` unusable for the common
    /// case, and the common case is the one that runs unattended.
    #[test]
    fn an_ssh_origin_matches_the_https_url_on_the_row() {
        assert!(origin_matches(
            "git@github.com:acme/api.git",
            "https://github.com/acme/api.git"
        ));
        assert!(origin_matches(
            "https://github.com/acme/api.git",
            "git@github.com:acme/api.git"
        ));
    }

    /// Case and a trailing slash are spelling, not identity. GitHub is
    /// case-insensitive in both host and path, and a URL with and without
    /// the trailing slash is the same repository.
    #[test]
    fn case_and_a_trailing_slash_do_not_change_which_repo_it_is() {
        assert!(origin_matches(
            "https://github.com/Acme/API",
            "https://github.com/acme/api/"
        ));
        assert!(origin_matches(
            "git@github.com:ACME/api",
            "https://github.com/acme/api"
        ));
    }

    /// A different owner, name, or host is a different repository.
    ///
    /// This is the check the whole gate rests on: the two URLs must name
    /// the same repository, and "close enough" is not a property a
    /// destructive command can be built on.
    #[test]
    fn a_different_repo_is_a_different_repo() {
        assert!(!origin_matches(
            "https://github.com/acme/other.git",
            "https://github.com/acme/api.git"
        ));
        assert!(!origin_matches(
            "https://github.com/other/api.git",
            "https://github.com/acme/api.git"
        ));
        assert!(!origin_matches(
            "https://gitlab.com/acme/api.git",
            "https://github.com/acme/api.git"
        ));
    }

    /// A fork is not the upstream, and a mirror is not the origin.
    ///
    /// The gate compares the checkout's origin against the row's clone
    /// URL, so a checkout of a fork — a perfectly ordinary thing to have
    /// on disk — must be refused rather than deleted.
    #[test]
    fn a_fork_is_not_the_upstream() {
        assert!(!origin_matches(
            "https://github.com/contributor/api.git",
            "https://github.com/acme/api.git"
        ));
    }

    /// A credential in the URL is stripped before comparing, and never
    /// printed.
    ///
    /// `https://user:pass@github.com/…` is the same repository as
    /// `https://github.com/…`. Comparing the two forms with the credential
    /// in place would refuse every checkout recorded that way; printing it
    /// to explain the refusal would put a secret on stderr.
    #[test]
    fn a_credential_in_the_url_is_stripped_not_compared() {
        assert!(origin_matches(
            "https://x-access-token:ghp_faketoken123@github.com/acme/api.git",
            "https://github.com/acme/api.git"
        ));
        assert!(origin_matches(
            "https://github.com/acme/api.git",
            "https://x-access-token:ghp_faketoken123@github.com/acme/api.git"
        ));
    }

    /// A local path as an origin is not a GitHub URL, and the two must
    /// never compare equal — a checkout whose origin is a directory on
    /// disk is not the repo the row points at.
    #[test]
    fn a_local_path_origin_is_not_a_github_url() {
        assert!(!origin_matches(
            "/home/me/projects/acme/api",
            "https://github.com/acme/api.git"
        ));
        assert!(!origin_matches("../api", "https://github.com/acme/api.git"));
    }

    /// The empty string is not a URL, and must not match anything.
    #[test]
    fn an_empty_origin_matches_nothing() {
        assert!(!origin_matches("", "https://github.com/acme/api.git"));
        assert!(!origin_matches("https://github.com/acme/api.git", ""));
    }
}
