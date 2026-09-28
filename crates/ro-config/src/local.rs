//! The per-repo config file: `<repo>/.ro/config.local.toml`.
//!
//! # Why this layer exists
//!
//! Per-repo settings live in the registry, which is right for the common
//! case: it is queryable, it travels with the workspace, and `ro config set
//! repos.<name>.<key>` can edit it without touching a working tree. But the
//! registry is *this machine's* registry, and there are cases it cannot
//! cover:
//!
//!   - a repo you did not register through ro, and do not intend to
//!   - a colleague's clone, where your identity is the wrong one and your
//!     credential is not present
//!   - a setting that must travel with the code, because the machine is not
//!     yours
//!
//! The file is for exactly those. It sits in the repo, is gitignored, and
//! **outranks the registry row** so a repo that has both can be corrected
//! without editing the database.
//!
//! # Precedence
//!
//! ```text
//!   CLI flag                  one run, one repo — wins outright
//!     ↓
//!   <repo>/.ro/config.local.toml   per repo, gitignored
//!     ↓
//!   the repos row                per repo, in the registry
//!     ↓
//!   ~/.config/ro/config.toml      per machine
//!     ↓
//!   built-in defaults
//! ```
//!
//! # It cannot grant management
//!
//! A repo carrying a config file is **not** managed. Only the registry is
//! what makes a repo a target: the file configures a repo the user has
//! already enrolled. A file arriving with a `git clone` must not silently
//! enrol anything and cause `ro sync` to push it with the user's
//! credential — that would make "what does ro push" a function of the
//! current directory.
//!
//! # Every key is optional
//!
//! The file is a **partial overlay**, not a replacement. A file setting one
//! key changes one thing; everything else still comes from the row and the
//! global config. `deny_unknown_fields` means a typo is an error naming the
//! file, rather than a setting that parses cleanly and does nothing.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The path, relative to a repo root.
pub const LOCAL_REL: &str = ".ro/config.local.toml";
/// The gitignore entry that covers it. A **directory** pattern, not the
/// filename: the moment ro adds a second thing under `.ro/` — a cache, a
/// state marker — a filename entry stops being correct and that file shows
/// up as untracked.
pub const LOCAL_IGNORE: &str = ".ro/";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoLocalConfig {
    /// Override the registry row's `author_ref`.
    pub author: Option<String>,
    /// Override the registry row's `credential_ref` — a **reference**
    /// (`env:VAR`, `keychain:ENTRY`), stored as the string the user wrote so
    /// the row and the file have one representation.
    pub credential: Option<String>,
    /// Override the registry row's `engine`.
    pub engine: Option<String>,
    /// Override the registry row's `engine_args`.
    #[serde(rename = "engine-args")]
    pub engine_args: Option<String>,
}

impl RepoLocalConfig {
    /// Read the file for a repo. A **missing file is not an error** — most
    /// repos will not have one, and that is the design rather than a gap.
    pub fn load(repo_root: &Path) -> Result<Option<Self>> {
        let path = Self::path_in(repo_root);
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: Self = toml::from_str(&raw).with_context(|| {
            format!(
                "parsing {}\n  every key is optional; a typo is an error rather \
                 than a setting that does nothing",
                path.display()
            )
        })?;
        Ok(Some(cfg))
    }

    pub fn path_in(repo_root: &Path) -> PathBuf {
        repo_root.join(".ro").join("config.local.toml")
    }

    /// Every key unset. Used to decide whether seeding the file is worth
    /// it: a file with nothing in it is noise, and a file that mirrors
    /// the row is a second copy that drifts on the first `ro config set`.
    pub fn is_empty(&self) -> bool {
        self.author.is_none()
            && self.credential.is_none()
            && self.engine.is_none()
            && self.engine_args.is_none()
    }

    /// Overlay onto a row's settings. **The file wins** — that is the whole
    /// reason it exists.
    pub fn apply_to(
        &self,
        author_ref: &mut Option<String>,
        credential_ref: &mut Option<String>,
        engine: &mut Option<String>,
        engine_args: &mut Option<String>,
    ) {
        if let Some(v) = &self.author {
            *author_ref = Some(v.clone());
        }
        if let Some(v) = &self.credential {
            *credential_ref = Some(v.clone());
        }
        if let Some(v) = &self.engine {
            *engine = Some(v.clone());
        }
        if let Some(v) = &self.engine_args {
            *engine_args = Some(v.clone());
        }
    }

    /// Every key this file accepts, for an error message.
    pub fn keys() -> &'static [&'static str] {
        &["author", "credential", "engine", "engine-args"]
    }
}

/// Append the gitignore entry, idempotently.
///
/// Never duplicates, never reorders, never rewrites what is there. A
/// pre-existing `.ro/`, `/.ro/` or a line that already ignores the
/// directory counts as present.
pub fn ensure_gitignored(repo_root: &Path) -> Result<bool> {
    let path = repo_root.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();

    let already = existing.lines().map(str::trim).any(|line| {
        let l = line.trim_start_matches('/').trim_end_matches('/');
        l == LOCAL_IGNORE.trim_end_matches('/')
    });
    if already {
        return Ok(false);
    }

    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&format!("\n# ro per-repo settings\n{LOCAL_IGNORE}\n"));

    std::fs::write(&path, updated)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}
