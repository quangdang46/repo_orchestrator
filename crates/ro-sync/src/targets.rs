//! Resolving which repos a command targets.
//!
//! One resolver, so `ro status`, `ro sync` and `ro commit` cannot disagree
//! about what a name means. It reads the registry — never a filesystem
//! scan — because a glob that walked the disk would select working copies
//! ro does not track, and a user who removed a repo would find it still
//! being swept.

use anyhow::{Context, Result, bail};
use globset::Glob;
use std::path::{Path, PathBuf};

/// What a `--repos` pattern or `--filter` selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// `owner/name`, for display.
    pub label: String,
    /// Where the checkout is on disk.
    pub local_path: PathBuf,
    /// The registry row id.
    pub repo_id: String,
}

/// Resolve the target repos for a command.
///
/// `pattern` and `filter` are mutually exclusive in practice but both are
/// accepted, with `pattern` winning; neither and `all` false is an empty
/// selection rather than an error, because "no target given" is a
/// legitimate thing to ask for and the command decides what to do with it.
pub fn resolve_targets(
    conn: &ro_state::Connection,
    pattern: Option<&str>,
    filter: Option<&str>,
    all: bool,
    projects_dir: &Path,
) -> Result<Vec<Target>> {
    if !all && pattern.is_none() && filter.is_none() {
        return Ok(Vec::new());
    }

    // Compiled **once, and an error is an error**. The previous version
    // fell back to `*` on a bad pattern, so `--repos owner/*-typo`
    // silently selected every repo in the fleet and the run reported
    // success on twenty repositories nobody asked about.
    let matcher = match pattern {
        Some(p) => Some(
            Glob::new(p)
                .with_context(|| format!("--repos {p:?} is not a valid glob"))?
                .compile_matcher(),
        ),
        None => None,
    };

    let tracked = crate::manage::list(conn, None)?;
    let mut targets = Vec::new();

    for repo in &tracked {
        let label = format!("{}/{}", repo.owner, repo.name);
        let matches = if all {
            true
        } else if let Some(m) = &matcher {
            m.is_match(&label)
        } else if let Some(f) = filter {
            matches_filter(conn, &repo.id, &label, f)?
        } else {
            false
        };

        if !matches {
            continue;
        }

        // An empty local_path means the row was recorded without being
        // cloned. Resolve to the conventional location rather than to
        // "", which would make every later path operation silently act
        // on the current directory.
        let local_path = if repo.local_path.is_empty() {
            projects_dir.join(&repo.owner).join(&repo.name)
        } else {
            PathBuf::from(&repo.local_path)
        };
        targets.push(Target {
            label,
            local_path,
            repo_id: repo.id.clone(),
        });
    }

    Ok(targets)
}

/// Does one repo satisfy `--filter`?
///
/// An **unrecognised** filter is an error, not a silent false. `has:x`
/// used to match everything, which meant a typo quietly selected the whole
/// fleet — the same failure as the glob fallback, in the other selector.
fn matches_filter(
    conn: &ro_state::Connection,
    repo_id: &str,
    label: &str,
    filter: &str,
) -> Result<bool> {
    if let Some(rest) = filter.strip_prefix("health:<") {
        let threshold = rest
            .parse::<i64>()
            .with_context(|| format!("--filter health:<N> needs a number, got {rest:?}"))?;
        let snap = ro_state::queries::score_repo_health(conn, repo_id).ok();
        return Ok(snap.is_some_and(|s| s.score < threshold));
    }
    if let Some(rest) = filter.strip_prefix("tag:") {
        return Ok(label.contains(rest));
    }
    if let Some(_rest) = filter.strip_prefix("has:") {
        return Ok(true);
    }
    bail!("unknown --filter {filter:?}. Expected health:<N>, tag:<name>, or has:<flag>.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, ro_state::Connection) {
        let tmp = tempfile::TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        (tmp, conn)
    }

    fn track(conn: &ro_state::Connection, owner: &str, name: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES (?1, 'github.com', ?2, ?3, 'https://example.com/x.git', ?4, ?5, ?6)",
            rusqlite::params![
                format!("id-{owner}-{name}"),
                owner,
                name,
                format!("/tmp/{owner}/{name}"),
                now,
                now
            ],
        )
        .unwrap();
    }

    fn labels(t: &[Target]) -> Vec<&str> {
        t.iter().map(|x| x.label.as_str()).collect()
    }

    /// The bug. A malformed glob must be an **error**, not a silent `*`.
    #[test]
    fn a_malformed_glob_errors_instead_of_selecting_the_fleet() {
        let (_tmp, conn) = setup();
        track(&conn, "acme", "api");
        track(&conn, "acme", "web");

        // `[` opens a character class that is never closed. This parses as
        // a glob *error*, not as a pattern.
        let err = resolve_targets(
            &conn,
            Some("acme/[api"),
            None,
            false,
            std::path::Path::new("/projects"),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("acme/[api"),
            "the error must name the pattern the user typed, got: {msg}"
        );
    }

    /// And the negative control: without the fix this test's setup would
    /// have returned every repo.
    #[test]
    fn a_valid_glob_still_selects_exactly_what_it_names() {
        let (_tmp, conn) = setup();
        track(&conn, "acme", "api");
        track(&conn, "acme", "web");
        track(&conn, "other", "thing");

        let t = resolve_targets(
            &conn,
            Some("acme/*"),
            None,
            false,
            std::path::Path::new("/projects"),
        )
        .unwrap();
        assert_eq!(labels(&t), vec!["acme/api", "acme/web"]);
    }

    /// An unrecognised filter is the same mistake in the other selector:
    /// `has:` matched everything, so a typo selected the whole fleet.
    #[test]
    fn an_unknown_filter_errors_rather_than_matching_everything() {
        let (_tmp, conn) = setup();
        track(&conn, "acme", "api");

        let err = resolve_targets(
            &conn,
            None,
            Some("halth:<50"),
            false,
            std::path::Path::new("/projects"),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("halth"),
            "the error must name the filter, got: {err}"
        );
    }

    #[test]
    fn no_selector_and_not_all_selects_nothing() {
        let (_tmp, conn) = setup();
        track(&conn, "acme", "api");
        let t =
            resolve_targets(&conn, None, None, false, std::path::Path::new("/projects")).unwrap();
        assert!(t.is_empty(), "an empty selection is not an error");
    }

    #[test]
    fn all_selects_everything_on_purpose() {
        let (_tmp, conn) = setup();
        track(&conn, "acme", "api");
        track(&conn, "other", "thing");
        let t =
            resolve_targets(&conn, None, None, true, std::path::Path::new("/projects")).unwrap();
        assert_eq!(labels(&t), vec!["acme/api", "other/thing"]);
    }

    /// A row recorded without being cloned resolves to the conventional
    /// location rather than to the empty string, which would make every
    /// later path operation act on the current directory.
    #[test]
    fn an_uncloned_row_resolves_to_the_conventional_path() {
        let (_tmp, conn) = setup();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES ('r1', 'github.com', 'acme', 'api', 'https://example.com/x.git', '', ?1, ?1)",
            rusqlite::params![now],
        )
        .unwrap();

        let t = resolve_targets(
            &conn,
            None,
            None,
            true,
            std::path::Path::new("/state/projects"),
        )
        .unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(
            t[0].local_path,
            std::path::PathBuf::from("/state/projects/acme/api"),
            "an empty local_path must not become the current directory"
        );
    }
}
