//! Repo management: add, remove, list, init.
//!
//! `ro init`: initialize config + SQLite state directory.
//! `ro add`: parse spec, insert into state DB (offline-first; GitHub enrichment optional).
//! `ro remove`: delete repo from state DB.
//! `ro list`: enumerate tracked repos.

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use ro_config::paths::ConfigPaths;
use ro_core::CredentialRef;
use ro_core::repo_spec::RepoSpec;

/// A tracked repo as returned by list queries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrackedRepo {
    pub id: String,
    pub host: String,
    pub owner: String,
    pub name: String,
    pub branch: Option<String>,
    pub alias: Option<String>,
    pub clone_url: String,
    pub local_path: String,
    pub visibility: String,
    pub archived: bool,
    pub disabled: bool,
    /// The per-repo config layer (schema V5). Every one of these is
    /// `None`-means-inherit, so a row that predates V5 reads back as
    /// "configured by the global config" rather than as a blank.
    ///
    /// `credential_ref` is a *reference* — `env:VAR` or `keychain:name` —
    /// never the secret itself. This struct is `Serialize`, so a raw secret
    /// here would land in `--format json` output and in anything that logs it.
    pub credential_ref: Option<String>,
    /// Names a profile from the global `[identity]` table, not an address.
    pub author_ref: Option<String>,
    /// `claude` | `codex` | `git`; `None` means use `[agent].engine`.
    pub engine: Option<String>,
    /// `None` means use `[agent].command`.
    pub engine_args: Option<String>,
}

impl std::fmt::Display for TrackedRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)?;
        if let Some(ref a) = self.alias {
            write!(f, " as {a}")?;
        }
        Ok(())
    }
}

/// Every column `row_to_tracked` reads, in the order it reads them.
///
/// Kept in lockstep with `row_to_tracked` and with the `INSERT` in `add`. This
/// string is the whole reason a schema change needs three edits in one commit:
/// a stale enumeration is not a compile error, it is a query that succeeds and
/// leaves the new fields at their defaults — so a repo the user configured
/// reads back as unconfigured, and nothing anywhere says so.
const REPO_COLUMNS: &str = "id, host, owner, name, branch, alias, clone_url, local_path, \
                             visibility, archived, disabled, credential_ref, author_ref, \
                             engine, engine_args";

fn row_to_tracked(row: &rusqlite::Row<'_>) -> std::result::Result<TrackedRepo, rusqlite::Error> {
    Ok(TrackedRepo {
        id: row.get(0)?,
        host: row.get(1)?,
        owner: row.get(2)?,
        name: row.get(3)?,
        branch: row.get(4)?,
        alias: row.get(5)?,
        clone_url: row.get(6)?,
        local_path: row.get(7)?,
        visibility: row.get(8)?,
        archived: row.get::<_, i64>(9)? != 0,
        disabled: row.get::<_, i64>(10)? != 0,
        credential_ref: row.get(11)?,
        author_ref: row.get(12)?,
        engine: row.get(13)?,
        engine_args: row.get(14)?,
    })
}

/// Initialize ro: create config file and state database if absent.
/// Returns `true` if anything was created, `false` if already initialized.
pub fn init(paths: &ConfigPaths) -> Result<bool> {
    paths.ensure_all()?;
    let mut created = false;

    let cfg_path = paths.config_toml();
    if !cfg_path.exists() {
        ro_config::loader::write_default(&cfg_path).context("writing default config")?;
        created = true;
    }

    let db_path = paths.state_db();
    if !db_path.exists() {
        let _conn = ro_state::open_db(&db_path)?;
        created = true;
    }

    Ok(created)
}

/// Add a repo to tracking. Parses the spec, resolves the local path, and
/// inserts into the `repos` table. Fails on duplicate (host, owner, name).
/// Owner and name are normalized to lowercase (GitHub is case-insensitive).
pub fn add(conn: &Connection, spec_str: &str, projects_dir: &Path) -> Result<TrackedRepo> {
    let mut spec =
        RepoSpec::parse(spec_str).map_err(|e| anyhow::anyhow!("invalid repo spec: {e}"))?;
    spec.owner = spec.owner.to_ascii_lowercase();
    spec.name = spec.name.to_ascii_lowercase();

    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM repos WHERE host = ?1 AND owner = ?2 AND name = ?3",
            params![spec.host, spec.owner, spec.name],
            |r| r.get(0),
        )
        .ok();
    if let Some(existing_id) = &existing {
        bail!(
            "repo {}/{} already tracked (id={})",
            spec.owner,
            spec.name,
            existing_id
        );
    }

    let id = Uuid::new_v4().to_string();
    let local_path = resolve_local_path(projects_dir, &spec);
    let now = now_secs();

    conn.execute(
        "INSERT INTO repos (id, host, owner, name, branch, alias, clone_url, local_path, \
                            visibility, archived, disabled, added_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'unknown', 0, 0, ?9, ?10)",
        params![
            id,
            spec.host,
            spec.owner,
            spec.name,
            spec.branch,
            spec.alias,
            spec.clone_url,
            local_path,
            now,
            now,
        ],
    )
    .context("inserting repo into state DB")?;

    Ok(TrackedRepo {
        id,
        host: spec.host,
        owner: spec.owner,
        name: spec.name,
        branch: spec.branch,
        alias: spec.alias,
        clone_url: spec.clone_url,
        local_path,
        visibility: "unknown".to_string(),
        archived: false,
        disabled: false,
        // A newly added repo has no per-repo config. NULL here is the whole
        // point: it means "inherit the global [auth]/[identity]/[agent]", which
        // is what a fresh repo should do. The INSERT deliberately does not
        // name these four columns — an omitted column takes its default, and
        // hard-coding NULL in a VALUES list would be a second place to forget
        // when the next per-repo setting is added.
        credential_ref: None,
        author_ref: None,
        engine: None,
        engine_args: None,
    })
}

/// What `ro add` was handed, decided structurally rather than by guessing.
///
/// The ordering is the design. A recognised host prefix is checked **first**,
/// so a URL is never probed on the filesystem, and the local-path branch never
/// tries to read a directory as a GitHub coordinate. Anything that is neither
/// is refused, naming both accepted forms, because silently falling through to
/// one of them is how a user ends up with a clone in the wrong place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddSource {
    /// A remote spec: clone it, then register the clone.
    Remote(RepoSpec),
    /// An existing checkout on disk: adopt it where it already is.
    Local {
        /// Canonicalized, so the stored `local_path` compares equal to whatever
        /// the walker in `prune` produces later.
        path: PathBuf,
        owner: String,
        name: String,
    },
}

/// Prefixes that mean "this is a remote spec, do not touch the filesystem".
const REMOTE_PREFIXES: &[&str] = &[
    "github.com/",
    "https://",
    "http://",
    "git@",
    "gh:",
    "ssh://",
];

/// Classify what the user typed.
///
/// Exposed separately from `add_from_input` because the rule is worth testing
/// on its own: it is the whole disambiguation, and the rest is bookkeeping.
pub fn classify_add_input(input: &str) -> Result<AddSource> {
    let value = input.trim();

    // 1. A recognised host prefix is a remote spec. `RepoSpec::parse` then
    //    validates the rest, so this branch can report a malformed URL as
    //    such rather than as a missing directory.
    if REMOTE_PREFIXES
        .iter()
        .any(|p| value.to_ascii_lowercase().starts_with(p))
    {
        let spec = RepoSpec::parse(value).map_err(|e| anyhow::anyhow!("invalid repo spec: {e}"))?;
        return Ok(AddSource::Remote(spec));
    }

    // 2. Anything else that is a git checkout is a local path to adopt.
    let path = std::path::Path::new(value);
    if path.join(".git").exists() {
        let canonical = path
            .canonicalize()
            .with_context(|| format!("resolving local path: {}", path.display()))?;
        let name = canonical
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "cannot derive a repo name from {}: the checkout's directory has no name",
                    canonical.display()
                )
            })?
            .to_ascii_lowercase();
        // The owner is the parent directory, which is the shape every local
        // fleet layout uses (`~/projects/<owner>/<name>`). A checkout at the
        // filesystem root has no parent to read, so say which input was bad
        // rather than inventing an owner.
        let owner = canonical
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "cannot derive an owner from {}: expected a checkout at <owner>/<name>",
                    canonical.display()
                )
            })?
            .to_ascii_lowercase();
        return Ok(AddSource::Local {
            path: canonical,
            owner,
            name,
        });
    }

    // 3. Neither. Name both forms, because the user is one keystroke from
    //    either and guessing wrong is the expensive branch.
    bail!(
        "cannot tell what {value:?} is.\n\
         It is not a remote spec (no recognised host prefix: {}) and not a local \
         checkout (no .git directory there).\n\
         Pass a remote spec such as 'owner/repo', 'github.com/owner/repo', or \
         'git@github.com:owner/repo.git' to clone it,\n\
         or a path to an existing checkout such as '.' or '../my-repo' to adopt \
         it in place.",
        REMOTE_PREFIXES.join(", ")
    );
}

/// Options for [`add_from_input`].
#[derive(Debug, Clone, Default)]
pub struct AddOptions {
    /// Display and lookup alias.
    pub name: Option<String>,
    /// Where a remote clone lands. Defaults to `projects_dir/<owner>/<name>`.
    pub clone_to: Option<PathBuf>,
    /// `clone -b`. A clone parameter with no lifetime — this is deliberately
    /// **not** cached on the row, because V4 dropped `repos.default_branch`
    /// for exactly that reason and re-adding a column to hold a branch name
    /// git can be asked for reintroduces the drift.
    pub branch: Option<String>,
    pub credential_ref: Option<String>,
    pub engine: Option<String>,
    pub author_ref: Option<String>,
    pub tags: Vec<String>,
}

/// `ro add`, whatever form the input took.
///
/// Cloning is all-or-nothing per repo: on a remote that is unreachable the
/// clone fails and **no row is written**, so "tracked but not cloned" is a
/// state the system never enters rather than one four call sites have to
/// tolerate.
pub fn add_from_input(
    conn: &Connection,
    input: &str,
    projects_dir: &Path,
    opts: &AddOptions,
) -> Result<TrackedRepo> {
    // Validate the credential before anything else, including before the
    // clone. `ro add --credential ghp_...` should fail without a network
    // round trip, and certainly without a row that holds a secret.
    if let Some(ref_value) = &opts.credential_ref {
        let _: CredentialRef = ref_value.parse()?;
    }

    match classify_add_input(input)? {
        AddSource::Remote(mut spec) => {
            if opts.name.is_some() {
                spec.alias = opts.name.clone();
            }
            if opts.branch.is_some() {
                spec.branch = opts.branch.clone();
            }
            let dest = opts
                .clone_to
                .clone()
                .unwrap_or_else(|| projects_dir.join(&spec.owner).join(&spec.name));

            if dest.exists() {
                bail!(
                    "{} already exists — refusing to clone into a non-empty path. \
                     Pass --clone-to to choose another destination.",
                    dest.display()
                );
            }

            let clone_opts = ro_git::mutation::CloneOpts {
                branch: opts.branch.clone(),
                ..Default::default()
            };
            let outcome = ro_git::mutation::clone(&spec.clone_url, &dest, &clone_opts)
                .map_err(|e| anyhow::anyhow!("cloning {} failed: {e}", spec.clone_url))?;

            // `clone` reports a non-zero git exit in the outcome rather than as
            // an Err — it did run, and the process it ran produced a result.
            // Checking `ok()` here is what makes the clone all-or-nothing; miss
            // it and a failed clone silently registers a repo that is not on
            // disk.
            if !outcome.result.ok() {
                // Clean up after ourselves: a failed clone that leaves a
                // half-written directory behind is the same trap one level down.
                let _ = std::fs::remove_dir_all(&dest);
                bail!(
                    "cloning {} failed: {}",
                    spec.clone_url,
                    outcome.result.stderr.trim()
                );
            }

            register(conn, &spec, &dest.to_string_lossy(), opts)
        }
        AddSource::Local { path, owner, name } => {
            let remote = git_remote_url(&path);
            let spec = RepoSpec {
                host: "github.com".to_string(),
                owner,
                name,
                branch: None,
                alias: opts.name.clone(),
                clone_url: remote,
            };
            register(conn, &spec, &path.to_string_lossy(), opts)
        }
    }
}

/// The origin URL of a local checkout, or a synthesised one.
///
/// A checkout with no remote is an ordinary state — a repo you have not
/// pushed — so this must not fail. The synthesised value is a real, correct
/// GitHub URL when the layout is the conventional one, and clearly marked
/// otherwise, because `clone_url` is what a later `ro sync` would use.
fn git_remote_url(path: &Path) -> String {
    if let Some(url) = ro_git::read::remote_url(path, "origin") {
        return url;
    }
    let owner = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    format!("https://github.com/{owner}/{name}.git")
}

/// Insert the row. Shared by both branches so the column list, the duplicate
/// check and the defaults live in exactly one place.
fn register(
    conn: &Connection,
    spec: &RepoSpec,
    local_path: &str,
    opts: &AddOptions,
) -> Result<TrackedRepo> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM repos WHERE host = ?1 AND owner = ?2 AND name = ?3",
            params![spec.host, spec.owner, spec.name],
            |r| r.get(0),
        )
        .ok();
    if let Some(existing_id) = &existing {
        bail!(
            "repo {}/{} already tracked (id={})",
            spec.owner,
            spec.name,
            existing_id
        );
    }

    let id = Uuid::new_v4().to_string();
    let now = now_secs();

    // The four per-repo config columns are named only when the caller supplied
    // one, so the common case takes the column default and there is no NULL
    // literal here to forget the next time a setting is added.
    conn.execute(
        "INSERT INTO repos (id, host, owner, name, branch, alias, clone_url, local_path, \
                            visibility, archived, disabled, added_at, updated_at, \
                            credential_ref, author_ref, engine) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'unknown', 0, 0, ?9, ?10, ?11, ?12, ?13)",
        params![
            id,
            spec.host,
            spec.owner,
            spec.name,
            spec.branch,
            spec.alias,
            spec.clone_url,
            local_path,
            now,
            now,
            opts.credential_ref,
            opts.author_ref,
            opts.engine,
        ],
    )
    .context("inserting repo into state DB")?;

    for tag in &opts.tags {
        conn.execute(
            "INSERT OR IGNORE INTO repo_tags (repo_id, tag) VALUES (?1, ?2)",
            params![id, tag],
        )
        .with_context(|| format!("tagging repo with {tag}"))?;
    }

    Ok(TrackedRepo {
        id,
        host: spec.host.clone(),
        owner: spec.owner.clone(),
        name: spec.name.clone(),
        branch: spec.branch.clone(),
        alias: spec.alias.clone(),
        clone_url: spec.clone_url.clone(),
        local_path: local_path.to_string(),
        visibility: "unknown".to_string(),
        archived: false,
        disabled: false,
        credential_ref: opts.credential_ref.clone(),
        author_ref: opts.author_ref.clone(),
        engine: opts.engine.clone(),
        engine_args: None,
    })
}

/// Remove a tracked repo by `owner/name` or alias. Returns the removed repo.
pub fn remove(conn: &Connection, key: &str) -> Result<TrackedRepo> {
    let repo = find_repo(conn, key).context("repo not found")?;
    delete_repo_cascade(conn, &repo.id).context("deleting repo from state DB")?;
    Ok(repo)
}

/// Delete a repo and all rows in dependent tables that reference it.
///
/// SQLite enforces `PRAGMA foreign_keys=ON` (see `ro_state::open_db`), so a
/// plain `DELETE FROM repos` fails once `sync_results`, `repo_health_snapshots`,
/// `context_cache`, `plans`, or `jobs` reference the repo. The schema does not
/// declare `ON DELETE CASCADE`, so we emulate it here in a single transaction
/// to keep `remove` / `prune` atomic.
pub fn delete_repo_cascade(conn: &Connection, repo_id: &str) -> rusqlite::Result<()> {
    // Tables with a NOT NULL FK to repos(id) — these would block the parent
    // delete outright and must be cleared first.
    const CHILD_TABLES: &[&str] = &["sync_results", "repo_health_snapshots", "context_cache"];
    // Tables with a nullable FK to repos(id). We null them out so historical
    // run/job records survive a prune (audit-friendly) but no longer
    // hold a reference to a row that's about to disappear.
    //
    // "plans" was here and is now gone with the V4 migration that drops its
    // table. Leaving the name in would make `ro remove` issue an UPDATE
    // against a table that no longer exists — green in every static check,
    // broken at runtime.
    const NULLABLE_FK_TABLES: &[&str] = &["jobs"];

    let tx = conn.unchecked_transaction()?;
    for table in CHILD_TABLES {
        tx.execute(
            &format!("DELETE FROM {table} WHERE repo_id = ?1"),
            params![repo_id],
        )?;
    }
    for table in NULLABLE_FK_TABLES {
        tx.execute(
            &format!("UPDATE {table} SET repo_id = NULL WHERE repo_id = ?1"),
            params![repo_id],
        )?;
    }
    tx.execute("DELETE FROM repos WHERE id = ?1", params![repo_id])?;
    tx.commit()
}

/// List all tracked repos, optionally filtered by owner prefix.
pub fn list(conn: &Connection, owner_filter: Option<&str>) -> Result<Vec<TrackedRepo>> {
    let mut repos = Vec::new();

    match owner_filter {
        Some(owner) => {
            let pattern = format!("{owner}%");
            let mut stmt = conn.prepare(&format!(
                "SELECT {REPO_COLUMNS} FROM repos WHERE owner LIKE ?1 ORDER BY owner, name"
            ))?;
            let mut rows = stmt.query(params![pattern])?;
            while let Some(row) = rows.next()? {
                repos.push(row_to_tracked(row)?);
            }
        }
        None => {
            let mut stmt = conn.prepare(&format!(
                "SELECT {REPO_COLUMNS} FROM repos ORDER BY owner, name"
            ))?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                repos.push(row_to_tracked(row)?);
            }
        }
    }

    Ok(repos)
}

/// The per-repo config keys that may be set, and whether they are validated as
/// a credential reference.
///
/// A `credential_ref` column is the only place in the whole system a secret
/// could plausibly be written, because it is the only column whose *meaning*
/// is "the thing ro reads instead of asking for a token". So it is validated on
/// the way in, here, rather than in whichever command happens to write it.
pub const REPO_CONFIG_KEYS: &[(&str, bool)] = &[
    // (column, is_credential_reference)
    ("credential_ref", true),
    ("author_ref", false),
    ("engine", false),
    ("engine_args", false),
];

/// Set one per-repo config value, validating it first.
///
/// `repo` is any key [`find_repo`] accepts (`owner/name`, an alias, or an id)
/// and `config_key` must be one of [`REPO_CONFIG_KEYS`]. A `credential_ref` is
/// parsed as a `CredentialRef` **before** the UPDATE, so a pasted GitHub token
/// is rejected and the row is left exactly as it was. The alternative is a
/// live credential in a database that gets backed up, synced and pasted into
/// issues, discovered long after the keystroke that created it.
///
/// The validation lives here rather than in `ro config set` so that every
/// writer gets it, including a future one.
pub fn set_repo_config(conn: &Connection, repo: &str, config_key: &str, value: &str) -> Result<()> {
    let target = find_repo(conn, repo).with_context(|| format!("repo not found: {repo}"))?;

    let Some((column, is_credential)) = REPO_CONFIG_KEYS
        .iter()
        .find(|(k, _)| *k == config_key)
        .copied()
    else {
        let known: Vec<&str> = REPO_CONFIG_KEYS.iter().map(|(k, _)| *k).collect();
        bail!(
            "unknown per-repo config key {config_key:?}; expected one of: {}",
            known.join(", ")
        );
    };

    // Parse for the side effect of failing, then write the canonical rendering,
    // so a value with surrounding whitespace is stored in the one form the
    // resolver will compare against later.
    let stored = if is_credential {
        let parsed: CredentialRef = value.parse()?;
        parsed.to_string()
    } else {
        value.to_string()
    };

    let changed = conn.execute(
        &format!("UPDATE repos SET {column} = ?1 WHERE id = ?2"),
        params![stored, target.id],
    )?;
    if changed == 0 {
        bail!("repo {repo} matched but no row was updated");
    }
    Ok(())
}

/// Find a repo by `owner/name`, alias, or raw id.
/// Owner/name lookups are case-insensitive (GitHub convention).
pub fn find_repo(conn: &Connection, key: &str) -> Result<TrackedRepo> {
    // Try owner/name
    let parts: Vec<&str> = key.splitn(2, '/').collect();
    if parts.len() == 2 {
        if let Ok(r) = conn.query_row(
            &format!("SELECT {REPO_COLUMNS} FROM repos WHERE LOWER(owner)=LOWER(?1) AND LOWER(name)=LOWER(?2)"),
            params![parts[0], parts[1]],
            row_to_tracked,
        ) {
            return Ok(r);
        }
    }
    // Try alias
    if let Ok(r) = conn.query_row(
        &format!("SELECT {REPO_COLUMNS} FROM repos WHERE alias=?1"),
        params![key],
        row_to_tracked,
    ) {
        return Ok(r);
    }
    // Try id
    if let Ok(r) = conn.query_row(
        &format!("SELECT {REPO_COLUMNS} FROM repos WHERE id=?1"),
        params![key],
        row_to_tracked,
    ) {
        return Ok(r);
    }
    bail!("repo '{key}' not found")
}

/// Build the on-disk path a repo will occupy.
///
/// Uses `PathBuf::join` rather than `format!("{}/{}/{}")` so the stored
/// string carries the platform's own separator. The old `format!` wrote
/// forward slashes on Windows while `collect_git_dirs` produced
/// backslashes, and the two were compared as raw strings — so on Windows
/// *every* tracked repo was reported as an orphan.
///
/// Note this only fixes rows written from now on. `find_orphans`
/// canonicalizes before comparing, which is what rescues the forward-slash
/// rows already sitting in existing `state.db` files. Both halves are
/// required; either alone leaves users with a wrong orphan list.
fn resolve_local_path(projects_dir: &Path, spec: &RepoSpec) -> String {
    let joined = projects_dir.join(&spec.owner).join(&spec.name);
    ro_config::paths::expand_tilde(&joined.to_string_lossy())
        .to_string_lossy()
        .into_owned()
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn setup() -> (TempDir, Connection) {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        (tmp, conn)
    }

    fn projects_dir(tmp: &TempDir) -> PathBuf {
        tmp.path().join("projects")
    }

    #[test]
    fn init_creates_config_and_db() {
        let tmp = TempDir::new().unwrap();
        let paths = ConfigPaths {
            config_dir: tmp.path().join("cfg"),
            state_dir: tmp.path().join("state"),
            cache_dir: tmp.path().join("cache"),
        };
        assert!(init(&paths).unwrap());
        assert!(paths.config_toml().exists());
        assert!(paths.state_db().exists());
    }

    #[test]
    fn init_idempotent() {
        let tmp = TempDir::new().unwrap();
        let paths = ConfigPaths {
            config_dir: tmp.path().join("cfg"),
            state_dir: tmp.path().join("state"),
            cache_dir: tmp.path().join("cache"),
        };
        assert!(init(&paths).unwrap());
        assert!(!init(&paths).unwrap());
    }

    #[test]
    fn add_basic() {
        let (tmp, conn) = setup();
        let repo = add(&conn, "quangdang46/repo_orchestrator", &projects_dir(&tmp)).unwrap();
        assert_eq!(repo.owner, "quangdang46");
        assert_eq!(repo.name, "repo_orchestrator");
        assert_eq!(repo.host, "github.com");
        assert!(repo.clone_url.contains("repo_orchestrator"));
    }

    #[test]
    fn add_with_branch_and_alias() {
        let (tmp, conn) = setup();
        let repo = add(
            &conn,
            "quangdang46/repo_orchestrator#develop as ro",
            &projects_dir(&tmp),
        )
        .unwrap();
        assert_eq!(repo.branch.as_deref(), Some("develop"));
        assert_eq!(repo.alias.as_deref(), Some("ro"));
    }

    #[test]
    fn add_rejects_duplicate() {
        let (tmp, conn) = setup();
        add(&conn, "quangdang46/repo_orchestrator", &projects_dir(&tmp)).unwrap();
        let err = add(&conn, "quangdang46/repo_orchestrator", &projects_dir(&tmp)).unwrap_err();
        assert!(err.to_string().contains("already tracked"));
    }

    #[test]
    fn add_rejects_invalid_spec() {
        let (tmp, conn) = setup();
        let err = add(&conn, "notaslash", &projects_dir(&tmp)).unwrap_err();
        assert!(err.to_string().contains("invalid repo spec"));
    }

    #[test]
    fn remove_by_owner_name() {
        let (tmp, conn) = setup();
        add(&conn, "quangdang46/repo_orchestrator", &projects_dir(&tmp)).unwrap();
        let removed = remove(&conn, "quangdang46/repo_orchestrator").unwrap();
        assert_eq!(removed.name, "repo_orchestrator");
        assert!(list(&conn, None).unwrap().is_empty());
    }

    #[test]
    fn remove_by_alias() {
        let (tmp, conn) = setup();
        add(
            &conn,
            "quangdang46/repo_orchestrator as ro",
            &projects_dir(&tmp),
        )
        .unwrap();
        let removed = remove(&conn, "ro").unwrap();
        assert_eq!(removed.name, "repo_orchestrator");
    }

    #[test]
    fn remove_missing_fails() {
        let (_, conn) = setup();
        let err = remove(&conn, "nonexistent/repo").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    /// Regression: a repo with sync_results / health_snapshots referencing
    /// it must still be removable. Pre-fix `DELETE FROM repos` tripped
    /// SQLite's FK enforcement.
    #[test]
    fn remove_clears_dependent_rows() {
        let (tmp, conn) = setup();
        let repo = add(&conn, "alice/proj1", &projects_dir(&tmp)).unwrap();

        conn.execute(
            "INSERT INTO runs (id, command, started_at, args_json) VALUES (?1, ?2, ?3, ?4)",
            params!["run-1", "sync", 0i64, "[]"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sync_results (run_id, repo_id, action, status, duration_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params!["run-1", repo.id, "clone", "success", 0i64],
        )
        .unwrap();
        ro_state::queries::score_repo_health(&conn, &repo.id).unwrap();

        remove(&conn, &repo.id).unwrap();
        assert!(list(&conn, None).unwrap().is_empty());

        // Child rows are gone; the run record itself survives as audit history.
        let remaining_sync: i64 = conn
            .query_row("SELECT COUNT(*) FROM sync_results", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining_sync, 0);
        let remaining_health: i64 = conn
            .query_row("SELECT COUNT(*) FROM repo_health_snapshots", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(remaining_health, 0);
    }

    #[test]
    fn list_returns_added_repos() {
        let (tmp, conn) = setup();
        add(&conn, "alice/proj1", &projects_dir(&tmp)).unwrap();
        add(&conn, "bob/proj2", &projects_dir(&tmp)).unwrap();
        let repos = list(&conn, None).unwrap();
        assert_eq!(repos.len(), 2);
    }

    #[test]
    fn list_with_owner_filter() {
        let (tmp, conn) = setup();
        add(&conn, "alice/proj1", &projects_dir(&tmp)).unwrap();
        add(&conn, "bob/proj2", &projects_dir(&tmp)).unwrap();
        let repos = list(&conn, Some("alice")).unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].owner, "alice");
    }

    /// Path 2 of 3: the registry row.
    ///
    /// The global config rejects a pasted secret at parse time and the row
    /// rejects it at write time, and both because the value has to survive
    /// being a `CredentialRef` first. Assert the rejection is *loud* — the
    /// other failure mode is a value that quietly lands in `state.db`, which
    /// is a file that gets backed up and pasted into issues.
    #[test]
    fn setting_a_credential_ref_rejects_a_pasted_secret() {
        let (tmp, conn) = setup();
        add(&conn, "acme/api", &projects_dir(&tmp)).unwrap();

        let err = set_repo_config(
            &conn,
            "acme/api",
            "credential_ref",
            "ghp_16C7e42F292c6912E7710c838347Ae178B4a",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("env:VAR_NAME") && msg.contains("keychain:ENTRY_NAME"),
            "message must teach the two accepted forms, got: {msg}"
        );

        // The row is untouched: a rejected write must not half-apply.
        let row = &list(&conn, None).unwrap()[0];
        assert_eq!(
            row.credential_ref, None,
            "a rejected credential must not be stored"
        );
    }

    #[test]
    fn setting_a_credential_ref_accepts_a_reference() {
        let (tmp, conn) = setup();
        add(&conn, "acme/api", &projects_dir(&tmp)).unwrap();
        set_repo_config(&conn, "acme/api", "credential_ref", "env:WORK_GH_TOKEN").unwrap();
        set_repo_config(&conn, "acme/api", "author_ref", "work").unwrap();
        set_repo_config(&conn, "acme/api", "engine", "codex").unwrap();

        let row = &list(&conn, None).unwrap()[0];
        assert_eq!(row.credential_ref.as_deref(), Some("env:WORK_GH_TOKEN"));
        assert_eq!(row.author_ref.as_deref(), Some("work"));
        assert_eq!(row.engine.as_deref(), Some("codex"));
    }

    /// `engine` and `author_ref` are not credential references, so they are not
    /// parsed as one — otherwise `author_ref = "Tran Quang Dang"` would fail,
    /// and the four keys would need four different validation rules at the
    /// call site instead of one flag in the table.
    #[test]
    fn non_credential_keys_are_not_parsed_as_references() {
        let (tmp, conn) = setup();
        add(&conn, "acme/api", &projects_dir(&tmp)).unwrap();
        set_repo_config(&conn, "acme/api", "author_ref", "Tran Quang Dang").unwrap();
        set_repo_config(&conn, "acme/api", "engine_args", "--model opus").unwrap();
        let row = &list(&conn, None).unwrap()[0];
        assert_eq!(row.author_ref.as_deref(), Some("Tran Quang Dang"));
        assert_eq!(row.engine_args.as_deref(), Some("--model opus"));
    }

    #[test]
    fn setting_an_unknown_config_key_is_a_named_error() {
        let (tmp, conn) = setup();
        add(&conn, "acme/api", &projects_dir(&tmp)).unwrap();
        let err = set_repo_config(&conn, "acme/api", "token", "whatever").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown per-repo config key"), "got: {msg}");
        // `token` is precisely the key a user reaches for, so the error has to
        // point at the real ones.
        assert!(msg.contains("credential_ref"), "got: {msg}");
    }

    #[test]
    fn setting_config_on_an_unknown_repo_is_an_error_not_a_no_op() {
        let (tmp, conn) = setup();
        let err = set_repo_config(&conn, "acme/nope", "engine", "codex").unwrap_err();
        assert!(err.to_string().contains("not found"), "got: {err}");
    }

    /// The classification is the whole disambiguation, so it is tested on its
    /// own rather than only through `ro add`.
    #[test]
    fn classify_prefers_a_host_prefix_over_the_filesystem() {
        // A spec-shaped string that happens to name a real directory must
        // still be a spec. Probing the filesystem first is how a URL ends up
        // cloned into a path the user did not choose.
        match classify_add_input("github.com/acme/api").unwrap() {
            AddSource::Remote(s) => {
                assert_eq!(s.owner, "acme");
                assert_eq!(s.name, "api");
            }
            other => panic!("expected a remote spec, got {other:?}"),
        }
        for input in [
            "https://github.com/acme/api",
            "git@github.com:acme/api.git",
            "gh:acme/api",
        ] {
            assert!(
                matches!(classify_add_input(input).unwrap(), AddSource::Remote(_)),
                "{input} should be a remote spec"
            );
        }
    }

    #[test]
    fn classify_recognises_a_local_checkout() {
        let tmp = TempDir::new().unwrap();
        let checkout = tmp.path().join("acme").join("api");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::create_dir_all(checkout.join(".git")).unwrap();

        match classify_add_input(checkout.to_str().unwrap()).unwrap() {
            AddSource::Local { path, owner, name } => {
                // Canonicalised, so the stored path compares equal to whatever
                // the orphan walker produces later.
                assert_eq!(path, checkout.canonicalize().unwrap());
                assert_eq!(owner, "acme");
                assert_eq!(name, "api");
            }
            other => panic!("expected a local path, got {other:?}"),
        }
    }

    /// Neither form, and the message has to name both — the user is one
    /// keystroke from either, and guessing wrong is the expensive branch.
    #[test]
    fn classify_refuses_ambiguity_by_naming_both_forms() {
        let tmp = TempDir::new().unwrap();
        let err = classify_add_input(tmp.path().to_str().unwrap()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("owner/repo"),
            "must name the remote form: {msg}"
        );
        assert!(msg.contains("checkout"), "must name the local form: {msg}");
    }

    /// A failed clone writes no row. Without this, "tracked but not cloned" is
    /// a state the system has to tolerate in four places.
    #[test]
    fn a_failed_clone_writes_no_row() {
        let (tmp, conn) = setup();
        let projects = projects_dir(&tmp);
        // A path that cannot be a real remote and cannot be resolved.
        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM repos", [], |r| r.get(0))
            .unwrap();

        let result = add_from_input(
            &conn,
            "git@github.com:acme/definitely-not-a-real-repo-xyz.git",
            &projects,
            &AddOptions::default(),
        );
        assert!(result.is_err(), "cloning a nonexistent remote must fail");

        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM repos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, after, "a failed clone must not write a row");
    }

    /// A credential is validated before the row exists, so
    /// `ro add --credential ghp_...` fails without ever writing a secret.
    #[test]
    fn add_rejects_a_pasted_credential_before_writing_a_row() {
        let (tmp, conn) = setup();
        let err = add_from_input(
            &conn,
            "acme/api",
            &projects_dir(&tmp),
            &AddOptions {
                // Not a real clone, so this cannot get past validation into a
                // network call.
                credential_ref: Some("ghp_16C7e42F292c6912E7710c838347Ae178B4a".into()),
                clone_to: Some(projects_dir(&tmp).join("acme").join("api")),
                ..Default::default()
            },
        );
        // The credential check runs before the clone, so this is the
        // credential error rather than a network failure.
        let msg = format!("{:#}", err.unwrap_err());
        assert!(msg.contains("env:VAR_NAME"), "got: {msg}");
    }

    #[test]
    fn adopting_a_local_checkout_stores_its_real_path() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let checkout = tmp.path().join("acme").join("web");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::create_dir_all(checkout.join(".git")).unwrap();

        let repo = add_from_input(
            &conn,
            checkout.to_str().unwrap(),
            &tmp.path().join("projects"),
            &AddOptions {
                engine: Some("codex".into()),
                author_ref: Some("work".into()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(repo.owner, "acme");
        assert_eq!(repo.name, "web");
        // Not a synthesised projects_dir path: the checkout is adopted where
        // it already is.
        assert_eq!(
            Path::new(&repo.local_path).canonicalize().unwrap(),
            checkout.canonicalize().unwrap()
        );
        assert_eq!(repo.engine.as_deref(), Some("codex"));
        assert_eq!(repo.author_ref.as_deref(), Some("work"));
    }

    /// The trap V5 had two halves to.
    ///
    /// `list` selects the `REPO_COLUMNS` string, and a schema migration adds
    /// columns to the table. If the two drift, the SELECT still succeeds — it
    /// just asks for fewer columns than exist — so a repo the user configured
    /// reads back as unconfigured. Nothing errors, anywhere. Writing the
    /// columns directly and reading them back is what closes that gap: only a
    /// `REPO_COLUMNS` that actually names them can pass.
    #[test]
    fn per_repo_config_survives_the_list_round_trip() {
        let (tmp, conn) = setup();
        let added = add(&conn, "acme/api", &projects_dir(&tmp)).unwrap();

        conn.execute(
            "UPDATE repos SET credential_ref = ?1, author_ref = ?2, engine = ?3, engine_args = ?4 \
             WHERE id = ?5",
            params!["env:GH_TOKEN", "work", "codex", "--yolo", added.id],
        )
        .unwrap();

        let listed = list(&conn, None).unwrap();
        let row = listed.iter().find(|r| r.id == added.id).unwrap();
        assert_eq!(row.credential_ref.as_deref(), Some("env:GH_TOKEN"));
        assert_eq!(row.author_ref.as_deref(), Some("work"));
        assert_eq!(row.engine.as_deref(), Some("codex"));
        assert_eq!(row.engine_args.as_deref(), Some("--yolo"));
    }

    /// The other half of the same trap: writing one repo's config must not
    /// leak onto its neighbours, which is what would happen if the columns
    /// were resolved by position rather than by name.
    #[test]
    fn per_repo_config_is_per_row() {
        let (tmp, conn) = setup();
        let a = add(&conn, "acme/api", &projects_dir(&tmp)).unwrap();
        let b = add(&conn, "acme/web", &projects_dir(&tmp)).unwrap();

        conn.execute(
            "UPDATE repos SET engine = 'claude' WHERE id = ?1",
            params![a.id],
        )
        .unwrap();

        let listed = list(&conn, None).unwrap();
        let row_a = listed.iter().find(|r| r.id == a.id).unwrap();
        let row_b = listed.iter().find(|r| r.id == b.id).unwrap();
        assert_eq!(row_a.engine.as_deref(), Some("claude"));
        assert_eq!(
            row_b.engine, None,
            "an unconfigured row must still read as inheriting the global default"
        );
    }

    /// A repo added before V5 existed, or added without any config, reads back
    /// as inheriting rather than as broken. NULL is the encoding for
    /// "use the global [auth]/[identity]/[agent]", not an absence.
    #[test]
    fn a_freshly_added_repo_inherits_everything() {
        let (tmp, conn) = setup();
        add(&conn, "acme/api", &projects_dir(&tmp)).unwrap();
        let row = &list(&conn, None).unwrap()[0];
        assert_eq!(row.credential_ref, None);
        assert_eq!(row.author_ref, None);
        assert_eq!(row.engine, None);
        assert_eq!(row.engine_args, None);
    }

    #[test]
    fn tracked_repo_display() {
        let repo = TrackedRepo {
            id: "x".into(),
            host: "github.com".into(),
            owner: "quangdang46".into(),
            name: "repo_orchestrator".into(),
            branch: None,
            alias: Some("ro".into()),
            clone_url: String::new(),
            local_path: String::new(),
            visibility: "unknown".into(),
            archived: false,
            disabled: false,
            credential_ref: None,
            author_ref: None,
            engine: None,
            engine_args: None,
        };
        assert_eq!(format!("{repo}"), "quangdang46/repo_orchestrator as ro");
    }

    #[test]
    fn find_repo_by_id() {
        let (tmp, conn) = setup();
        let added = add(&conn, "alice/proj1", &projects_dir(&tmp)).unwrap();
        let found = find_repo(&conn, &added.id).unwrap();
        assert_eq!(found.name, "proj1");
    }
}
