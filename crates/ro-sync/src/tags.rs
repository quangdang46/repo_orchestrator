//! Tagging a tracked repo, after the fact.
//!
//! `ro add --tag` writes a tag at enrolment time, and `--filter tag:<t>`
//! selects on it. What was missing was everything between: a repo tagged
//! once could never be re-tagged, and a tag could never be taken off, so
//! the set of tags was append-only for the life of the registry. Adding
//! needs an inverse.
//!
//! # "Make it so" is idempotent, deliberately
//!
//! `tag` twice is a no-op at exit 0. `untag` something that is not there
//! is also a no-op at exit 0. An error here would make shell loops and
//! scripts awkward for no gain — the user asked for a state, not for a
//! ceremony, and the row is already in the state they asked for.
//!
//! # A group is a tag
//!
//! The vocabulary in use is `--group work`, and there is a temptation to
//! make that a second concept with its own table. There should not be: a
//! group *is* a tag, and two mechanisms for one selection produce two
//! answers to "which repos does this run touch?". `ro tag` writes the same
//! rows `--tag` writes, and `--group` is a synonym for `--tag` in the
//! filter.

use anyhow::{Context, Result};
use ro_state::Connection;
use rusqlite::params;

/// Resolve a repo key to its id, with the same key rules as every other
/// verb: bare name, alias, or `owner/name`.
fn repo_id(conn: &Connection, key: &str) -> Result<String> {
    crate::manage::find_repo(conn, key)
        .map(|r| r.id)
        .with_context(|| format!("no tracked repo matches {key:?}"))
}

/// Add tags. Returns the number newly added, so the caller can say
/// "already there" rather than implying something changed.
pub fn add(conn: &Connection, key: &str, tags: &[String]) -> Result<usize> {
    let id = repo_id(conn, key)?;
    let mut added = 0;
    for tag in tags {
        let tag = tag.trim();
        if tag.is_empty() {
            continue;
        }
        let n = conn.execute(
            "INSERT OR IGNORE INTO repo_tags (repo_id, tag) VALUES (?1, ?2)",
            params![id, tag],
        )?;
        added += n;
    }
    Ok(added)
}

/// Remove tags. Returns the number actually removed.
pub fn remove(conn: &Connection, key: &str, tags: &[String]) -> Result<usize> {
    let id = repo_id(conn, key)?;
    let mut removed = 0;
    for tag in tags {
        removed += conn.execute(
            "DELETE FROM repo_tags WHERE repo_id = ?1 AND tag = ?2",
            params![id, tag.trim()],
        )?;
    }
    Ok(removed)
}

/// Every tag on one repo, sorted.
pub fn of(conn: &Connection, key: &str) -> Result<Vec<String>> {
    let id = repo_id(conn, key)?;
    let mut stmt = conn
        .prepare("SELECT tag FROM repo_tags WHERE repo_id = ?1 ORDER BY tag")?;
    let rows = stmt.query_map(params![id], |r| r.get::<_, String>(0))?;
    Ok(rows.filter_map(Result::ok).collect())
}

/// Every tag in use, with how many repos carry it — the shape a
/// `--tag` picker needs.
pub fn all_with_counts(conn: &Connection) -> Result<Vec<(String, i64)>> {
    let mut stmt =
        conn.prepare("SELECT tag, COUNT(*) FROM repo_tags GROUP BY tag ORDER BY tag")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    Ok(rows.filter_map(Result::ok).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The functions take `&[String]`; a test should not have to shout it.
    fn t(s: &str) -> String {
        s.to_string()
    }

    fn setup() -> ro_state::Connection {
        let conn = ro_state::open_memory().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        for name in ["api", "web"] {
            conn.execute(
                "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
                 VALUES (?1, 'github.com', 'acme', ?2, 'https://example.com/x.git', ?3, ?4, ?4)",
                params![format!("id-acme-{name}"), name, format!("/tmp/{name}"), now],
            )
            .unwrap();
        }
        conn
    }

    /// The three verbs are one statement each, and they are inverses. A tag
    /// with a writer and no remover is how a table grows forever and the
    /// only remedy is editing SQLite by hand — which is how people stop
    /// using a tool.
    #[test]
    fn add_then_remove_returns_the_tag_to_where_it_started() {
        let c = setup();
        assert!(of(&c, "api").unwrap().is_empty());

        let added = add(&c, "api", &["work".to_string(), "oss".to_string()]).unwrap();
        assert_eq!(added, 2);
        assert_eq!(of(&c, "api").unwrap(), vec!["oss", "work"]);
        assert!(
            of(&c, "web").unwrap().is_empty(),
            "tagging one repo must not tag another"
        );

        let removed = remove(&c, "api", &["work".to_string()]).unwrap();
        assert_eq!(removed, 1);
        assert_eq!(of(&c, "api").unwrap(), vec!["oss"]);
    }

    /// "Make it so" is idempotent. Tagging twice is not an error, and
    /// removing a tag that was never there is not an error either — the row
    /// is already in the state the user asked for, and an error there makes
    /// shell loops awkward for no gain.
    #[test]
    fn tagging_twice_and_removing_a_stranger_are_both_no_ops() {
        let c = setup();
        add(&c, "api", &[t("work")]).unwrap();
        assert_eq!(add(&c, "api", &[t("work")]).unwrap(), 0, "second add changed nothing");
        assert_eq!(remove(&c, "api", &["nope".to_string()]).unwrap(), 0);
        assert_eq!(of(&c, "api").unwrap(), vec!["work"]);
    }

    /// A repo key is a key, not a path: the same spellings `ro ship` accepts
    /// work here too, so `ro tag acme/api work` and `ro tag api work` reach
    /// the same row.
    #[test]
    fn a_repo_resolves_by_owner_slash_name_and_by_bare_name() {
        let c = setup();
        add(&c, "acme/api", &[t("by-owner-name")]).unwrap();
        add(&c, "api", &[t("by-bare-name")]).unwrap();
        assert_eq!(
            of(&c, "acme/api").unwrap(),
            vec!["by-bare-name", "by-owner-name"],
            "both spellings must reach one row, not create two"
        );
    }

    /// The registry-wide listing is what a `--tag` picker reads, so the count
    /// has to be the number of *distinct repos* — `PRIMARY KEY (repo_id, tag)`
    /// is what stops the same tag being counted twice for one repo.
    #[test]
    fn counts_are_distinct_repos_not_rows() {
        let c = setup();
        add(&c, "api", &[t("work")]).unwrap();
        // A duplicate that slipped past the INSERT OR IGNORE would inflate
        // this count if the key were not what the table says it is.
        conn_exec(&c, "INSERT OR IGNORE INTO repo_tags (repo_id, tag) VALUES ('id-acme-api', 'work')");
        add(&c, "web", &[t("work")]).unwrap();

        let counts = all_with_counts(&c).unwrap();
        assert_eq!(counts, vec![("work".to_string(), 2)]);
    }

    /// An unknown repo is an error, not a silent no-op. `ro tag typo work`
    /// must say so rather than report success and change nothing.
    #[test]
    fn an_unknown_repo_is_an_error() {
        let c = setup();
        let err = add(&c, "nope", &[t("work")]).unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("nope"),
            "the error must name the repo the user typed, got: {err}"
        );
    }

    fn conn_exec(c: &Connection, sql: &str) {
        c.execute(sql, []).unwrap();
    }
}
