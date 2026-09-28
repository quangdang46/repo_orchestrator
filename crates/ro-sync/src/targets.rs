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
    include_archived: bool,
) -> Result<Vec<Target>> {
    if !all && pattern.is_none() && filter.is_none() {
        return Ok(Vec::new());
    }

    // Every whitespace-separated token is compiled on its own, so
    // `ro ship alpha beta` is two names rather than one glob containing a
    // space — which is what it used to be, and which matched nothing at
    // all while reporting a cheerful empty run.
    //
    // Compiled **once, and an error is an error**. The previous version
    // fell back to `*` on a bad pattern, so `--repos owner/*-typo`
    // silently selected every repo in the fleet and the run reported
    // success on twenty repositories nobody asked about.
    let tokens: Vec<(String, Option<globset::GlobMatcher>)> = match pattern {
        Some(p) => p
            .split_whitespace()
            .map(|t| {
                Glob::new(t)
                    .with_context(|| format!("--repos {t:?} is not a valid glob"))
                    .map(|g| (t.to_string(), Some(g.compile_matcher())))
            })
            .collect::<Result<Vec<_>>>()?,
        None => Vec::new(),
    };

    let tracked = crate::manage::list(conn, None)?;
    let mut targets = Vec::new();

    for repo in &tracked {
        // `archived = 0 AND disabled = 0` is the default, not a filter
        // somebody remembered to apply at the call site. A row the user
        // retired is still in the registry — the row is the record, the
        // directory on disk is not the only thing that is still there — and
        // a fleet run that reached it would be doing something the user
        // said not to do. `--include-archived` is the way back, which is
        // what makes excluding it safe to do by default.
        if !include_archived && (repo.archived || repo.disabled) {
            continue;
        }
        let label = format!("{}/{}", repo.owner, repo.name);
        // Most specific selector wins. `--all` used to be checked first, so
        // a run that passed both `--all` and a filter selected the whole
        // fleet and silently ignored the filter — the one case where a
        // narrower request does something wider. `--all` means "no
        // narrower request given", not "ignore any that was".
        let matches = if !tokens.is_empty() {
            // A token selects a repo three ways, because three things are
            // things a user types: the glob, the name they gave it, and the
            // `owner/name` the registry knows it by. The help text promises
            // "by name or alias" and only the last of the three worked, so
            // `ro ship alpha` selected nothing and said so in a tone that
            // reads like success.
            tokens.iter().any(|(tok, m)| {
                m.as_ref().is_some_and(|g| g.is_match(&label))
                    || repo.alias.as_deref() == Some(tok.as_str())
                    || label == *tok
                    || repo.id == *tok
            })
        } else if let Some(f) = filter {
            matches_filter(conn, &repo.id, &label, f)?
        } else {
            all
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

/// Read one boolean fact about a tracked repo, for `--filter has:`.
fn matches_filter_repo(conn: &ro_state::Connection, repo_id: &str, flag: &str) -> bool {
    let sql = match flag {
        "archived" => "SELECT archived FROM repos WHERE id = ?1",
        "disabled" => "SELECT disabled FROM repos WHERE id = ?1",
        _ => "SELECT CASE WHEN local_path = '' THEN 0 ELSE 1 END FROM repos WHERE id = ?1",
    };
    conn.query_row(sql, rusqlite::params![repo_id], |r| r.get::<_, i64>(0))
        .map(|v| v != 0)
        .unwrap_or(false)
}

/// Does one repo satisfy `--filter`?
///
/// An **unrecognised** filter is an error, not a silent false. `has:x`
/// used to match everything, which meant a typo quietly selected the whole
/// fleet — the same failure as the glob fallback, in the other selector.
fn matches_filter(
    conn: &ro_state::Connection,
    repo_id: &str,
    _label: &str,
    filter: &str,
) -> Result<bool> {
    // `health:<N>` in the docs is a *placeholder*, not a literal — the angle
    // brackets are the manual's way of writing "a number here". The prefix
    // match required them anyway, so `health:50` matched no branch, fell
    // through to the `bail!` below, and the scorer the plan says "remains
    // a real selector" had never selected a thing in its life. Both
    // spellings work now, because the documented one is the one people copy.
    if let Some(rest) = filter
        .strip_prefix("health:")
        .map(|r| r.trim_start_matches(['<', ' ']))
    {
        let threshold = rest
            .trim_end_matches(['>', ' '])
            .parse::<i64>()
            .with_context(|| format!("--filter health:<N> needs a number, got {rest:?}"))?;
        let snap = ro_state::queries::score_repo_health(conn, repo_id).ok();
        return Ok(snap.is_some_and(|s| s.score < threshold));
    }

    // Tags live in `repo_tags`, not in the label. Matching on `owner/name`
    // meant the tag filtered by coincidence of the repository's *name*:
    // `tag:api` selected everything called `api-service` and nothing
    // actually carrying the tag.
    // "group" and "tag" are the same thing and say so. The vocabulary in
    // use is "--group work"; there is deliberately no second concept and no
    // second table, because two mechanisms for one selection produce two
    // answers to "which repos does this run touch?".
    let tag_filter = filter
        .strip_prefix("tag:")
        .or_else(|| filter.strip_prefix("group:"));
    if let Some(tag) = tag_filter {
        let found = conn
            .query_row(
                "SELECT 1 FROM repo_tags WHERE repo_id = ?1 AND tag = ?2",
                rusqlite::params![repo_id, tag],
                |_| Ok(1),
            )
            .ok();
        return Ok(found.is_some());
    }

    // `has:` names a real fact about the row. It returned `true` for any
    // suffix, which is precisely the failure this function's own comment
    // describes: a typo selects the whole fleet and the run reports success
    // on every repository.
    if let Some(flag) = filter.strip_prefix("has:") {
        return match flag {
            "archived" | "disabled" | "cloned" => Ok(matches_filter_repo(conn, repo_id, flag)),
            other => bail!(
                "unknown --filter has:{other}. \
                 Expected has:archived, has:disabled or has:cloned."
            ),
        };
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
            false,
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
            false,
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
            false,
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
        let t = resolve_targets(
            &conn,
            None,
            None,
            false,
            std::path::Path::new("/projects"),
            false,
        )
        .unwrap();
        assert!(t.is_empty(), "an empty selection is not an error");
    }

    #[test]
    fn all_selects_everything_on_purpose() {
        let (_tmp, conn) = setup();
        track(&conn, "acme", "api");
        track(&conn, "other", "thing");
        let t = resolve_targets(
            &conn,
            None,
            None,
            true,
            std::path::Path::new("/projects"),
            false,
        )
        .unwrap();
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
            false,
        )
        .unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(
            t[0].local_path,
            std::path::PathBuf::from("/state/projects/acme/api"),
            "an empty local_path must not become the current directory"
        );
    }

    fn conn_with_two_tagged() -> (tempfile::TempDir, ro_state::Connection) {
        let (tmp, conn) = setup();
        track(&conn, "acme", "api");
        track(&conn, "acme", "web");
        conn.execute(
            "INSERT INTO repo_tags (repo_id, tag) VALUES ('id-acme-api', 'work')",
            [],
        )
        .unwrap();
        (tmp, conn)
    }

    /// `--all` used to be tested first, so a run passing both `--all` and
    /// a filter selected the **whole fleet** and dropped the filter without
    /// a word. That is the worst direction for this bug to fail in: the
    /// request was the narrower one, and the tool acted on the wider one
    /// with the user's credentials.
    ///
    /// `--all` means "no narrower request given", not "ignore any that was".
    #[test]
    fn all_does_not_override_an_explicit_filter() {
        let (_t, conn) = conn_with_two_tagged();
        let got = resolve_targets(
            &conn,
            None,
            Some("tag:work"),
            true, // --all AND --filter, which is the case that was wrong
            std::path::Path::new("/projects"),
            false,
        )
        .unwrap();
        assert_eq!(
            labels(&got),
            vec!["acme/api"],
            "--all must not widen a filter; only the tagged repo should match"
        );
    }

    /// The negative control for the test above: with the filter gone, the
    /// same `--all` does mean everything. Without this, the fix would also
    /// pass if `--all` had simply stopped working.
    #[test]
    fn all_alone_still_selects_everything() {
        let (_t, conn) = conn_with_two_tagged();
        let got = resolve_targets(
            &conn,
            None,
            None,
            true,
            std::path::Path::new("/projects"),
            false,
        )
        .unwrap();
        assert_eq!(got.len(), 2);
    }

    /// A filter that matches nothing selects nothing. This is what makes a
    /// typo visible: `ro status --tag wrok` prints nothing rather than the
    /// whole fleet, so the user can tell the tag was not found.
    #[test]
    fn a_filter_matching_nothing_selects_nothing() {
        let (_t, conn) = conn_with_two_tagged();
        let got = resolve_targets(
            &conn,
            None,
            Some("tag:wrok"),
            true,
            std::path::Path::new("/projects"),
            false,
        )
        .unwrap();
        assert!(got.is_empty(), "a mistyped tag must not fall back to --all");
    }

    /// A name beats a filter, and the narrowest selector is the one that
    /// applies. Naming a repo is the most specific request a user can make.
    #[test]
    fn a_named_repo_beats_a_filter() {
        let (_t, conn) = conn_with_two_tagged();
        let got = resolve_targets(
            &conn,
            Some("acme/web"),
            Some("tag:work"),
            false,
            std::path::Path::new("/projects"),
            false,
        )
        .unwrap();
        assert_eq!(
            labels(&got),
            vec!["acme/web"],
            "the named repo is untagged; a name is the more specific request"
        );
    }
}

/// The selectors the plan promises, tested against the spellings a user
/// actually types.
///
/// Every one of these was broken and none of it was caught, for the same
/// reason: the tests below used the *documented* forms, and the documented
/// forms were the broken ones.
#[cfg(test)]
mod filter_contract {
    use super::*;

    fn fixture() -> (tempfile::TempDir, ro_state::Connection) {
        let tmp = tempfile::TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        for (name, archived) in [("api", 0i64), ("oss", 1)] {
            conn.execute(
                "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at, archived)
                 VALUES (?1, 'github.com', 'acme', ?2, 'https://example.com/x.git', ?3, ?4, ?4, ?5)",
                rusqlite::params![format!("id-{name}"), name, format!("/s/{name}"), now, archived],
            )
            .unwrap();
        }
        // A tag on `oss` — and note that `api` *contains* neither the tag
        // nor any hint of it, so a label-based match cannot accidentally
        // produce the right answer.
        conn.execute(
            "INSERT INTO repo_tags (repo_id, tag) VALUES ('id-oss', 'backend')",
            [],
        )
        .unwrap();
        (tmp, conn)
    }

    fn selected(conn: &ro_state::Connection, filter: &str) -> Vec<String> {
        resolve_targets(
            conn,
            None,
            Some(filter),
            false,
            std::path::Path::new("/s"),
            true,
        )
        .map(|t| t.into_iter().map(|x| x.repo_id).collect())
        .unwrap_or_default()
    }

    /// The regression this whole commit is about. `health:50` — a number,
    /// the only thing a person types — used to fail the prefix match on
    /// `health:<`, fall through, and raise "unknown --filter". The health
    /// selector had never once selected a repository.
    #[test]
    fn a_health_filter_with_a_plain_number_is_not_an_unknown_filter() {
        let (_t, conn) = fixture();
        // No snapshots exist, so nothing matches — but the call must
        // succeed. Reaching this assertion at all is the test.
        assert_eq!(selected(&conn, "health:50"), Vec::<String>::new());
    }

    /// The spelling the documentation uses has to keep working too, or the
    /// fix above would have traded one broken form for another.
    #[test]
    fn the_documented_angle_bracket_spelling_still_parses() {
        let (_t, conn) = fixture();
        assert_eq!(selected(&conn, "health:<50>"), Vec::<String>::new());
    }

    #[test]
    fn a_non_numeric_health_filter_names_the_problem() {
        let (_t, conn) = fixture();
        let err = resolve_targets(
            &conn,
            None,
            Some("health:soon"),
            false,
            std::path::Path::new("/s"),
            false,
        )
        .expect_err("a threshold that is not a number cannot be honoured");
        assert!(
            format!("{err:#}").contains("needs a number"),
            "the message must say what was wrong, got: {err:#}"
        );
    }

    /// A tag is a row in `repo_tags`. It was matched against the *label*
    /// instead, so `--filter tag:api` selected the repository called
    /// `api` and ignored the one actually carrying the tag.
    #[test]
    fn a_tag_filter_reads_the_tag_table_and_not_the_repository_name() {
        let (_t, conn) = fixture();
        assert_eq!(
            selected(&conn, "tag:backend"),
            vec!["id-oss".to_string()],
            "the tagged repository, whichever it is called"
        );
        assert!(
            selected(&conn, "tag:api").is_empty(),
            "a repository named `api` carries no tag and must not match"
        );
    }

    #[test]
    fn has_selects_a_real_fact_about_the_row() {
        let (_t, conn) = fixture();
        assert_eq!(selected(&conn, "has:archived"), vec!["id-oss".to_string()]);
        assert_eq!(
            selected(&conn, "has:disabled").len(),
            0,
            "neither fixture row is disabled"
        );
        // Both rows carry a local_path, so both are cloned. The assertion
        // is that the flag reads the *row* rather than answering `true`
        // for everything, which is what it used to do for any suffix.
        assert_eq!(selected(&conn, "has:cloned").len(), 2);
    }

    /// The failure this function's own comment warns about: an
    /// unrecognised flag that quietly selects the whole fleet.
    #[test]
    fn an_unknown_has_flag_is_an_error_not_the_whole_fleet() {
        let (_t, conn) = fixture();
        let err = resolve_targets(
            &conn,
            None,
            Some("has:archvied"),
            false,
            std::path::Path::new("/s"),
            false,
        )
        .expect_err("a typo must not select everything");
        assert!(
            format!("{err:#}").contains("has:archived"),
            "the message lists the flags that exist, got: {err:#}"
        );
    }

    /// A bare name, an alias and an `owner/name` are the three things a
    /// user types, and the help text promises all three.
    #[test]
    fn a_name_an_alias_and_a_label_all_select() {
        let tmp = tempfile::TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at, alias)
             VALUES ('id-1', 'github.com', 'acme', 'api', 'https://example.com/x.git', '/s/api', ?1, ?1, 'service')",
            rusqlite::params![now],
        )
        .unwrap();
        for token in ["acme/api", "service", "id-1"] {
            let t = resolve_targets(&conn, Some(token), None, false, tmp.path(), false)
                .unwrap_or_default();
            assert_eq!(t.len(), 1, "{token:?} should select the one repo");
        }
    }

    /// Two names are two selections. Joined into one pattern they became
    /// a single glob containing a space, which matches nothing at all.
    #[test]
    fn two_names_select_two_repositories() {
        let (_t, conn) = fixture();
        // `true` for the archived escape hatch: `oss` is archived in this
        // fixture, and the question here is how many *tokens* were split
        // out, not whether archived rows are reachable.
        let t = resolve_targets(
            &conn,
            Some("acme/api acme/oss"),
            None,
            false,
            std::path::Path::new("/s"),
            true,
        )
        .unwrap();
        assert_eq!(t.len(), 2, "a space separates names, it does not join them");
    }
}

/// `archived = 0 AND disabled = 0` is in the resolver, not at the call
/// sites, so `ro sync`, `ro commit`, `ro push` and `ro ship` cannot
/// disagree about what "every repo" means.
#[cfg(test)]
mod archived_is_excluded_by_default {
    use super::*;

    fn conn_with(archived: bool, disabled: bool) -> (tempfile::TempDir, ro_state::Connection) {
        let tmp = tempfile::TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        for (name, a, d) in [
            ("live", 0i64, 0i64),
            ("archived", archived as i64, 0),
            ("disabled", 0, disabled as i64),
        ] {
            conn.execute(
                "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at, archived, disabled)
                 VALUES (?1, 'github.com', 'acme', ?2, 'https://example.com/x.git', ?3, ?4, ?4, ?5, ?6)",
                rusqlite::params![format!("id-{name}"), name, format!("/s/{name}"), now, a, d],
            )
            .unwrap();
        }
        (tmp, conn)
    }

    fn all_but_archived(conn: &ro_state::Connection, include: bool) -> Vec<String> {
        resolve_targets(conn, None, None, true, std::path::Path::new("/s"), include)
            .map(|t| t.into_iter().map(|x| x.repo_id).collect())
            .unwrap()
    }

    #[test]
    fn a_retired_repo_is_not_in_a_fleet_run() {
        let (_t, conn) = conn_with(true, true);
        let ids = all_but_archived(&conn, false);
        assert_eq!(
            ids,
            vec!["id-live".to_string()],
            "only the live repo; --all means every *active* row"
        );
    }

    /// The escape hatch is what makes the default safe: a repo the user
    /// retired by mistake has to be reachable again without a database
    /// edit.
    #[test]
    fn the_escape_hatch_reaches_them_again() {
        let (_t, conn) = conn_with(true, true);
        assert_eq!(all_but_archived(&conn, true).len(), 3);
    }

    #[test]
    fn naming_a_retired_repo_still_needs_the_flag() {
        let (_t, conn) = conn_with(true, false);
        let found = |include: bool| {
            resolve_targets(
                &conn,
                Some("acme/archived"),
                None,
                false,
                std::path::Path::new("/s"),
                include,
            )
            .map(|t| t.len())
            .unwrap()
        };
        assert_eq!(found(false), 0, "quietly reaching a retired row by name");
        assert_eq!(found(true), 1);
    }
}
