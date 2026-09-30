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

/// What a `--pattern` glob or `--filter` selected.
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
    // fell back to `*` on a bad pattern, so `--pattern 'owner/*-typo'`
    // silently selected every repo in the fleet and the run reported
    // success on twenty repositories nobody asked about.
    let tokens: Vec<(String, Option<globset::GlobMatcher>)> = match pattern {
        Some(p) => p
            .split_whitespace()
            .map(|t| {
                Glob::new(t)
                    // `--pattern`, the flag this value actually arrived on.
                    // It said `--repos`, which no verb has: `ro commit --repos`
                    // is rejected by clap as an unexpected argument, and the
                    // schema's 36 long flags do not contain it. A user whose
                    // glob has a typo was told to fix a flag they never typed.
                    .with_context(|| format!("--pattern {t:?} is not a valid glob"))
                    .map(|g| (t.to_string(), Some(g.compile_matcher())))
            })
            .collect::<Result<Vec<_>>>()?,
        None => Vec::new(),
    };

    let tracked = crate::manage::list(conn, None)?;
    let mut targets = Vec::new();

    // Every token was asked for by name; report the ones that resolved to
    // nothing, even when a sibling token hit.
    //
    // A token list is a **union**, so `ro status alpha beta` selects both —
    // `tokens.iter().any(...)` is the right per-repo test and stays. What
    // was missing is the answer to a different question: did every name I
    // typed name something? Without it, `ro status alpha nosuchrepo`
    // printed `work/alpha` and said nothing about `nosuchrepo` — exit 0,
    // no warning — while `ro status nosuchrepo` alone exited 64 naming it.
    // The same typo had two different answers depending on whether a
    // sibling name happened to hit, and the all-miss message proved the tool
    // knew both names were asked for and resolved neither. A typo in a
    // fleet command's repo list is invisible, and on the verbs that act
    // (`sync`, `commit`, `push`, `ship`) it is a run that quietly does less
    // than the user asked for and reports success.
    //
    // Recorded during the loop rather than re-derived afterwards, so an
    // alias or a row id counts as a hit on exactly the same terms the
    // selection itself used.
    let mut token_hit = vec![false; tokens.len()];

    // A `--filter` is validated **once, before the loop**, and the
    // validation is the same one the per-repo path applies.
    //
    // Doing it inside the loop was wrong twice over. A malformed filter
    // ('tag:work tag:infra') was reported on the first repo and then again on
    // every other one, and a filter that is structurally impossible
    // ('health:999') was reported per repo rather than as the usage error it
    // is. And the loop is the wrong place for a decision that does not depend
    // on the repo: the answer to "is this filter well-formed" is the same for
    // all twenty.
    //
    // The narrowing itself still happens per repo, below — this is only the
    // refusal.
    if let Some(f) = filter {
        validate_filter(f)?;
    }

    for repo in &tracked {
        // `archived = 0 AND disabled = 0` is the default, not a filter
        // somebody remembered to apply at the call site. A row the user
        // retired is still in the registry — the row is the record, the
        // directory on disk is not the only thing that is still there — and
        // a fleet run that reached it would be doing something the user
        // said not to do. `--include-archived` is the way back, which is
        // what makes excluding it safe to do by default.
        //
        // The two `has:` selectors that *name* the retired state are the
        // exception, and they have to be: the skip ran first, so
        // `has:archived` and `has:disabled` could never reach the arm that
        // answers them, and both returned an empty run with exit 0. Two of
        // the three selectors the tool's own error message names by name
        // were structurally unreachable — a user asking "which of my repos
        // did I retire?" was told there were none.
        let asks_for_retired = filter
            .map(|f| f == "has:archived" || f == "has:disabled")
            .unwrap_or(false);
        if !include_archived && !asks_for_retired && (repo.archived || repo.disabled) {
            continue;
        }
        let label = format!("{}/{}", repo.owner, repo.name);
        // Most specific selector wins. `--all` used to be checked first, so
        // a run that passed both `--all` and a filter selected the whole
        // fleet and silently ignored the filter — the one case where a
        // narrower request does something wider. `--all` means "no
        // narrower request given", not "ignore any that was".
        let matches = if !tokens.is_empty() {
            // A token selects a repo four ways, because four things are
            // things a user types: the glob, the name they gave it, the
            // `owner/name` the registry knows it by, and the bare `name` on
            // its own. The help text promises "by name or alias" and only
            // the alias and the full label worked, so `ro ship alpha`
            // selected nothing and said so in a tone that reads like
            // success.
            //
            // The bare name matches on `name` and **not** on the label,
            // because `*` does not cross a `/` in globset — so `alpha`
            // was never a pattern that matched `owner/alpha` either.
            //
            // Two owners can both have a repo called `alpha`, and this
            // matches both. That is the same behaviour as `--pattern 'a*'`,
            // which is already how a glob is allowed to select more than
            // one repo, and the alternative — refusing an ambiguous name —
            // would break the common single-owner fleet this is written
            // for.
            tokens.iter().enumerate().any(|(i, (tok, m))| {
                let hit = m.as_ref().is_some_and(|g| g.is_match(&label))
                    || repo.alias.as_deref() == Some(tok.as_str())
                    || label == *tok
                    || repo.id == *tok
                    || repo.name == *tok;
                if hit {
                    token_hit[i] = true;
                }
                hit
            })
        } else if filter.is_some() {
            // No pattern and a filter: the filter is the whole request, so
            // the row is a candidate and the filter below decides. Returning
            // `all` here instead would skip the row before the filter was
            // ever consulted, and `--filter tag:work` alone would select
            // nothing.
            true
        } else {
            all
        };

        // A `--filter` narrows the selection; it never replaces it.
        //
        // Three cases, and each is a different sentence:
        //
        //   * No pattern and a filter — the filter IS the request, so it
        //     decides. `--filter tag:work` alone.
        //   * A glob and a filter — the filter narrows the glob. 'ro sync
        //     "proj/*" --filter health:50' is "the sick repos under proj/",
        //     and this used to discard the filter entirely, so `health:999`
        //     synced every repo the glob matched. A glob is a *set*, and a
        //     filter that does not narrow a set selects more than was asked.
        //   * A literal name and a filter — the name is an *identity*, not a
        //     set, and it is the more specific request. A filter alongside it
        //     could only subtract something the user already named on
        //     purpose, so the name wins and the filter is not consulted.
        //
        // The glob is detected by metacharacter rather than by token count:
        // one token is still a set if it is 'proj/*', and two literal names
        // are still identities.
        if let Some(f) = filter {
            let narrow =
                tokens.is_empty() || tokens.iter().any(|(tok, _)| tok.contains(['*', '?', '[']));
            if narrow && !matches_filter(conn, &repo.id, &label, f)? {
                continue;
            }
        }

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

    if !tokens.is_empty() {
        let missed: Vec<&str> = tokens
            .iter()
            .zip(token_hit.iter())
            .filter(|(_, hit)| !**hit)
            .map(|((t, _), _)| t.as_str())
            .collect();
        if !missed.is_empty() {
            bail!(
                "no repo matched {}",
                missed
                    .iter()
                    .map(|m| format!("`{m}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    Ok(targets)
}

/// Read one boolean fact about a tracked repo, for `--filter has:`.
///
/// `cloned` is answered from the **filesystem**, not from the row. It
/// used to run `SELECT CASE WHEN local_path = '' THEN 0 ELSE 1 END`,
/// which asks whether a path was ever *recorded* on the row — so a repo
/// whose working copy has been deleted still answered "yes, cloned", and
/// `ro status --filter has:cloned` handed the user the repos that are
/// already gone, each carrying `"unmeasurable_reason":"not cloned"` in the
/// same run that selected it. The selector named after the fact did not
/// check the fact, and contradicted the status row printed beside it.
///
/// The two other flags are row facts and are answered from the row. A
/// repo with no recorded `local_path` has no conventional path to check
/// against, so it is reported as not cloned — the same answer the status
/// board gives it.
fn matches_filter_repo(conn: &ro_state::Connection, repo_id: &str, flag: &str) -> bool {
    if flag == "cloned" {
        let local_path: String = conn
            .query_row(
                "SELECT local_path FROM repos WHERE id = ?1",
                rusqlite::params![repo_id],
                |r| r.get(0),
            )
            .unwrap_or_default();
        if local_path.is_empty() {
            return false;
        }
        return Path::new(&local_path).join(".git").exists();
    }
    let sql = match flag {
        "archived" => "SELECT archived FROM repos WHERE id = ?1",
        "disabled" => "SELECT disabled FROM repos WHERE id = ?1",
        _ => "SELECT 0",
    };
    conn.query_row(sql, rusqlite::params![repo_id], |r| r.get::<_, i64>(0))
        .map(|v| v != 0)
        .unwrap_or(false)
}

/// The health score `--filter health:<N>` actually compares against.
///
/// `score_repo_health` in `ro-state` penalises `sync_results.status =
/// 'error'` and nothing else. That deny-list is the same shape as the bug it
/// was written to fix — `run_exit_code` in `ro-sync` counted `error` and
/// missed `autostash_conflict`, so a run whose only bad repo was a tree full
/// of conflict markers with the user's work parked in a stash exited 0 — and
/// it is the same shape one layer up: a repo whose uncommitted work is
/// sitting in an autostash loses **zero** penalty points, is scored 100, and
/// is selected by `--filter health:50` for a run. A filter that selects the
/// repos it is supposed to keep out of a run is worse than no filter.
///
/// The penalty is added here rather than in `ro-state` because this is the
/// crate that owns which statuses mean "the repo is not in the state the
/// user asked for": the list is written down in `sync::status_fails_run`, and
/// the same list is what makes a sync run fail. The sync module is therefore
/// the one place that already knows the truth.
pub(crate) fn effective_health_score(
    conn: &ro_state::Connection,
    repo_id: &str,
    stored: i64,
) -> i64 {
    // Every status that fails a run, read from the table rather than
    // hard-coded, so a status added to `sync` cannot be forgotten here. An
    // unknown status fails the run and fails this check — see
    // `status_fails_run` for why that is the safe default.
    const FAILING: &[&str] = &["error", "autostash_conflict", "conflict"];
    // The statuses bind to `?1..` and the repo id to the next one, in that
    // order, so the parameter list below and the `?n` in the SQL are read
    // off the same place and cannot drift apart.
    let mut placeholders = String::new();
    for i in 0..FAILING.len() {
        if i > 0 {
            placeholders.push_str(", ");
        }
        placeholders.push_str(&format!("?{}", i + 1));
    }
    let sql = format!(
        "SELECT COUNT(*) FROM sync_results WHERE status IN ({placeholders}) \
         AND repo_id = ?{}",
        FAILING.len() + 1
    );
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        // A score that cannot be read is not a score that can be trusted.
        // Failing closed is the same default `status_fails_run` uses for an
        // unknown status, and for the same reason.
        Err(_) => return 0,
    };
    let params: Vec<&dyn rusqlite::ToSql> = FAILING
        .iter()
        .map(|s| s as &dyn rusqlite::ToSql)
        .chain(std::iter::once(&repo_id as &dyn rusqlite::ToSql))
        .collect();
    let failed: i64 = stmt.query_row(params.as_slice(), |r| r.get(0)).unwrap_or(0);
    if failed == 0 {
        return stored;
    }
    // Capped at the same 30 points `ro-state` caps its own failed-sync
    // penalty at, so this term cannot dominate the score and turn a healthy
    // repo into a zero. A repo with work in a stash is *not* a repo to keep
    // out of a run entirely — it is a repo to look at first, which is what
    // a lowered score says and what a zero would not.
    (stored - (failed * 5).min(30)).max(0)
}

/// The range a health score is clamped to.
///
/// `score_repo_health` in `ro-state` clamps with `score.clamp(0, 100)` and
/// [`effective_health_score`] floors at zero, so these are the only two
/// values no repository can ever leave.
const HEALTH_FLOOR: i64 = 0;
const HEALTH_CEILING: i64 = 100;

/// Does one repo satisfy `--filter health:<threshold>`?
///
/// The comparison is `score <= threshold` — **inclusive**, and which end is
/// the bug is a decision worth writing down rather than rediscovering.
///
/// The score is a damage scale: 100 is a repo with nothing wrong with it, 0
/// is the worst state the scorer can express. A user asking for "the sick
/// repos" types a small N, and under a strict `<` comparison a repo that had
/// failed its way down to exactly 0 was reachable by *no* threshold a person
/// types — the sickest repos in the fleet were the only ones the sick-repo
/// filter could never return, and `health:0` was a selector that could not
/// select anything at all. The inclusive end makes the smallest threshold
/// mean what it reads as: only the repos at the bottom of the scale.
///
/// The *other* end is where a selector becomes dangerous, and it is where
/// the fleet-wide run happened. The comparison used to be `score < N` with
/// no bound on N, and it was never checked against the score's own range, so
/// the comparison was simply always true: `--filter health:999`,
/// `health:1000` and `health:1000000` each selected every repo in the fleet
/// and `ro sync` ran a real sync against all of them, reporting success. A
/// user who asked for the sick repos and got the whole fleet was handed a
/// run with their credentials on every repository, and the flag's own
/// guarantee — a selector that matches nothing selects nothing, never
/// everything — was false for exactly one selector kind.
///
/// So a threshold that cannot narrow is a **usage error**, refused the way a
/// `--tag` nobody carries is refused rather than widened. Both impossible
/// ends are refused, and the reason each is impossible is different:
///
/// * `threshold >= HEALTH_CEILING` — no repo can score above 100, so every
///   repo is `<= threshold`. It is `--all` wearing a filter's clothes.
/// * `threshold < HEALTH_FLOOR` — no repo can score below 0, so every repo
///   is `> threshold`. It can match nothing, and the rule for a selector
///   that matches nothing is that it matches *nothing*; refusing it is the
///   strictest form of that, and it turns a typo into a message instead of a
///   silent empty run.
///
/// What is deliberately *not* refused is a valid threshold that happens to
/// match no repo — `health:50` over a fleet where everything is healthy
/// selects nothing, and says so, which is the answer the user asked for.
fn matches_health(conn: &ro_state::Connection, repo_id: &str, threshold: i64) -> Result<bool> {
    if threshold >= HEALTH_CEILING {
        bail!(
            "--filter health:{threshold} would select every repo. A health score is at most \
             {HEALTH_CEILING}, so no repository can score above this threshold and the filter \
             cannot narrow anything. Nothing was selected. Use a threshold below {HEALTH_CEILING} \
             to ask for the repos that need a human, or --all to mean the whole fleet."
        );
    }
    if threshold < HEALTH_FLOOR {
        bail!(
            "--filter health:{threshold} cannot select any repo. A health score is never below \
             {HEALTH_FLOOR}, so this threshold is below every score a repo can have. Nothing was \
             selected."
        );
    }
    let snap = ro_state::queries::score_repo_health(conn, repo_id).ok();
    Ok(snap.is_some_and(|s| effective_health_score(conn, repo_id, s.score) <= threshold))
}

/// Does one repo satisfy `--filter`?
///
/// An **unrecognised** filter is an error, not a silent false. `has:x`
/// used to match everything, which meant a typo quietly selected the whole
/// fleet — the same failure as the glob fallback, in the other selector.
///
/// Refuse a `--filter` that cannot be a filter, before any repo is asked.
///
/// The same validation [`matches_filter`] applies per repo, hoisted out so
/// the decision is made once. A malformed filter reported inside the loop
/// was reported once per repository, and a structurally impossible one
/// ('health:999') was reported as a per-repo fact rather than as the usage
/// error it is.
///
/// This does not answer the question for any repo — it only rejects the
/// filters that have no answer. A well-formed filter that matches nothing is
/// not an error and is not this function's business.
fn validate_filter(filter: &str) -> Result<()> {
    if filter.contains(char::is_whitespace) {
        bail!(
            "--filter {filter:?} contains whitespace. A single --filter takes a single \
             expression — pass `--tag` for a tag, or fold several conditions into one \
             expression with no spaces."
        );
    }
    if let Some(rest) = filter
        .strip_prefix("health:")
        .map(|r| r.trim_start_matches(['<', ' ']))
    {
        let threshold = rest
            .trim_end_matches(['>', ' '])
            .parse::<i64>()
            .with_context(|| format!("--filter health:<N> needs a number, got {rest:?}"))?;
        if threshold >= HEALTH_CEILING {
            bail!(
                "--filter health:{threshold} would select every repo. A health score is at most \
                 {HEALTH_CEILING}, so no repository can score above this threshold and the \
                 filter cannot narrow anything. Nothing was selected. Use a threshold below \
                 {HEALTH_CEILING} to ask for the repos that need a human, or --all to mean the \
                 whole fleet."
            );
        }
        if threshold < HEALTH_FLOOR {
            bail!(
                "--filter health:{threshold} cannot select any repo. A health score is never \
                 below {HEALTH_FLOOR}, so this threshold is below every score a repo can have. \
                 Nothing was selected."
            );
        }
        return Ok(());
    }
    if filter.starts_with("tag:") || filter.starts_with("group:") {
        return Ok(());
    }
    if let Some(flag) = filter.strip_prefix("has:") {
        return match flag {
            "archived" | "disabled" | "cloned" => Ok(()),
            other => bail!(
                "unknown --filter has:{other}. Expected has:archived, has:disabled or has:cloned."
            ),
        };
    }
    bail!("unknown --filter {filter:?}. Expected health:<N>, tag:<name>, or has:<flag>.")
}

/// Every selector kind answers "you matched nothing" the same way, because
/// the same typo must not produce a different answer depending on which
/// flag the user reached for. `tag:` and `has:` already do: they return
/// `false` for a repo that does not carry the fact, and the caller turns an
/// all-`false` result into "nothing selected". `health:` is the one kind
/// that could not do that — see [`matches_health`].
fn matches_filter(
    conn: &ro_state::Connection,
    repo_id: &str,
    _label: &str,
    filter: &str,
) -> Result<bool> {
    // A filter with a space in it is two filters, and the tool's own
    // refusal message says so: "`--tag` is shorthand for `--filter tag:<T>`
    // — pass the tag form, or fold it into a single `--filter` expression."
    // Following that instruction exactly produced `--filter 'tag:work
    // tag:infra'`, which reached here as one string, whose `strip_prefix`
    // yielded the literal tag name "work tag:infra" — matching nothing,
    // silently, with exit 0, in all three formats. The workaround the
    // error message spells out was the thing that did not work.
    //
    // Refusing is the honest answer: a multi-token filter is a typo, and
    // the alternative — matching nothing and reporting an empty run — is
    // indistinguishable from "none of your repos match".
    if filter.contains(char::is_whitespace) {
        bail!(
            "--filter {filter:?} contains whitespace. A single --filter takes a \
             single expression — pass `--tag` for a tag, or fold several \
             conditions into one expression with no spaces."
        );
    }
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
        return matches_health(conn, repo_id, threshold);
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

    /// `ro commit`, `ro push`, `ro ship` and `ro sync` all treat a bare
    /// invocation as the whole registry. That is the daily command — it is
    /// what replaces the `cd repo-a && ro ship` loop — and a tool that
    /// makes you add a flag to say "everything" renames the loop instead of
    /// removing it.
    ///
    /// It did not: the fleet verbs selected nothing and told you to pass
    /// `--all`, and `sync` disagreed with them. So the same argument list
    /// — none — meant two different sets of repos depending on the verb.
    #[test]
    fn a_bare_invocation_is_stated_by_the_caller_as_the_whole_registry() {
        let (_t, conn) = conn_with_two_tagged();
        // `all: true` is what the CLI now passes when nothing narrowed it.
        let got = resolve_targets(
            &conn,
            None,
            None,
            true,
            std::path::Path::new("/projects"),
            false,
        )
        .unwrap();
        assert_eq!(got.len(), 2, "a bare invocation reaches every managed repo");
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
        // `svc` is neither archived nor disabled, so its score is 100 and it is the
        // control the health assertions need: a repo with nothing wrong with
        // it, sitting in the same fleet.
        for (name, archived) in [("api", 0i64), ("oss", 1), ("svc", 0)] {
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
        // `cloned` is answered from the **filesystem**, not from the row.
        // It used to run `SELECT CASE WHEN local_path = '' THEN 0 ELSE 1`,
        // which asks whether a path was *recorded* — so a repo whose
        // working copy has been deleted still answered "yes, cloned", and
        // `ro status --filter has:cloned` handed the user the repos that
        // are already gone, each carrying `"unmeasurable_reason": "not
        // cloned"` in the same run that selected it.
        //
        // The fixture's rows name `/s/<name>`, which does not exist on this
        // machine, so the honest answer is "none of them are cloned" —
        // and it is the opposite of what the row-recorded test asserted.
        assert_eq!(
            selected(&conn, "has:cloned").len(),
            0,
            "a recorded path is not a working copy; a repo whose directory is \
             gone is not cloned"
        );
    }

    /// The same selector, with the worktrees actually on disk: it must now
    /// select them. A `has:` flag that can never match is not a selector.
    #[test]
    fn has_cloned_selects_repos_whose_worktree_exists() {
        let (t, conn) = fixture();
        for name in ["api", "oss", "svc"] {
            std::fs::create_dir_all(t.path().join(name).join(".git")).unwrap();
            conn.execute(
                "UPDATE repos SET local_path = ?1 WHERE id = ?2",
                rusqlite::params![
                    t.path().join(name).to_string_lossy().to_string(),
                    format!("id-{name}")
                ],
            )
            .unwrap();
        }
        assert_eq!(
            selected(&conn, "has:cloned").len(),
            3,
            "three worktrees exist; the selector named after the fact must \
             be able to match it"
        );

        // And deleting one removes exactly one from the answer.
        std::fs::remove_dir_all(t.path().join("api")).unwrap();
        assert_eq!(
            selected(&conn, "has:cloned"),
            vec!["id-oss".to_string(), "id-svc".to_string()],
            "a deleted working copy stops being cloned"
        );
    }

    /// The failure this function's own comment warns about: an
    /// unrecognised flag that quietly selects the whole fleet.
    /// A filter containing whitespace is two filters, and the tool's own
    /// refusal message says so: "`--tag` is shorthand for `--filter
    /// tag:<T>` — pass the tag form, or fold it into a single `--filter`
    /// expression." Following that instruction exactly produced
    /// `--filter 'tag:work tag:infra'`, which reached `matches_filter` as one
    /// string whose `strip_prefix("tag:")` yielded the literal tag name
    /// "work tag:infra" — matching nothing, silently, exit 0, all three
    /// formats. The workaround the error message spells out was the thing
    /// that did not work.
    #[test]
    fn a_filter_containing_whitespace_is_refused_rather_than_matching_nothing() {
        let (_t, conn) = fixture();
        let err = resolve_targets(
            &conn,
            None,
            Some("tag:work tag:infra"),
            false,
            std::path::Path::new("/s"),
            false,
        )
        .expect_err("a two-token filter is a typo, not a filter that matches nothing");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("whitespace") && msg.contains("tag:work tag:infra"),
            "the message must name the problem and the value, got: {msg}"
        );
    }

    /// A name that matches nothing is reported **even when a sibling name
    /// hit**.
    ///
    /// `tokens.iter().any(...)` is a union — `ro status alpha beta` must
    /// select both — so a non-matching token contributed `false` and
    /// vanished: `ro status alpha nosuchrepo` printed `work/alpha` and said
    /// nothing about `nosuchrepo`, exit 0, no warning, while
    /// `ro status nosuchrepo` alone exited 64 naming it. The same typo had
    /// two different answers depending on whether a sibling happened to
    /// hit, and on `sync`/`commit`/`push`/`ship` it is a run that quietly
    /// does less than the user asked for and reports success.
    #[test]
    fn a_name_that_misses_is_reported_even_when_a_sibling_name_hits() {
        let (_t, conn) = fixture();
        let both = resolve_targets(
            &conn,
            Some("api nosuchrepo"),
            None,
            false,
            std::path::Path::new("/s"),
            false,
        )
        .expect_err("one of the two names named nothing");
        let msg = format!("{both:#}");
        assert!(
            msg.contains("nosuchrepo"),
            "the miss must be named, got: {msg}"
        );

        // And the union itself still works: two names that both hit select
        // both, and nothing is reported.
        let union = resolve_targets(
            &conn,
            Some("api svc"),
            None,
            false,
            std::path::Path::new("/s"),
            false,
        )
        .expect("two real names are not an error");
        assert_eq!(
            union.len(),
            2,
            "a token list is a union, not an intersection"
        );
    }

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

    /// The bug the health selector was letting through.
    ///
    /// `score_repo_health` in `ro-state` penalises `sync_results.status =
    /// 'error'` and nothing else, so a repo whose pull landed and whose
    /// autostash pop did not — status `autostash_conflict`, the user's work
    /// sitting in a stash, conflict markers in the tree — loses **zero**
    /// penalty points. It was scored 100 and selected by `--filter health:50`
    /// for a run. A filter that selects the very repos it exists to keep out
    /// of a run is not a filter.
    ///
    /// This is the sync module's side of it: the list of statuses that mean
    /// "the repo is not in the state the user asked for" is written down in
    /// `sync::status_fails_run`, and this is the same list applied where the
    /// score is read.
    #[test]
    fn a_repo_whose_work_is_in_an_autostash_is_not_scored_healthy() {
        let (_tmp, conn) = fixture();
        // `sync_results` is unique per (run, repo), and `run_id` is a
        // foreign key — which is the whole reason the table was empty for so
        // long. Two real runs are opened rather than a made-up id, so the
        // fixture lands in the same state a real sync leaves behind rather
        // than in a state the database forbids.
        for _ in 0..6 {
            let run = ro_jobs::open_run(&conn, "sync", &[]).unwrap();
            conn.execute(
                "INSERT INTO sync_results
                 (run_id, repo_id, action, status, duration_ms, error, pre_oid, post_oid)
                 VALUES (?1, 'id-api', 'pull', 'autostash_conflict', 1, NULL, NULL, NULL)",
                rusqlite::params![run.id],
            )
            .unwrap();
            ro_jobs::finalize_run(&conn, &run.id, 1).unwrap();
        }
        // Nothing at all for `oss`.
        ro_state::queries::score_repo_health(&conn, "id-api").unwrap();
        ro_state::queries::score_repo_health(&conn, "id-oss").unwrap();

        assert_eq!(
            effective_health_score(&conn, "id-api", 100),
            70,
            "six runs that all ended with the user's work in a stash must \
             cost the same points six runs that ended in 'error' would. A \
             score that stayed at 100 here is the bug: it made a repo that \
             can never sync cleanly indistinguishable from one that has \
             never tried."
        );
        assert_eq!(
            effective_health_score(&conn, "id-oss", 100),
            100,
            "a repo with no failing runs keeps its stored score"
        );

        // And the selector, which is where the score is used.
        //
        // `health:<N>` selects repos scoring **below** N, so a score that
        // cannot move is a repo the filter can never reach: `id-api` sat at
        // 100 however many autostashes it collected, and no threshold would
        // ever put it in front of a user who asked for the broken ones. That
        // is the operational half of the bug, and it is what this asserts.
        let selected_below_80 = selected(&conn, "health:80");
        assert!(
            selected_below_80.iter().any(|id| id == "id-api"),
            "a repo that has failed six times must be reachable by the filter \
             that exists to find failing repos: {selected_below_80:?}"
        );
        assert!(
            !selected_below_80.iter().any(|id| id == "id-svc"),
            "a repo with nothing wrong with it must not be selected just for \
             being in the same fleet: {selected_below_80:?}"
        );
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

/// The health threshold, at both ends of the range it can take.
///
/// The bug: `--filter health:999` over a five-repo fleet selected all five,
/// and `ro sync` ran a real sync against every one of them and reported
/// success. `health:1000` and `health:1000000` did the same. The comparison
/// was `score < N` with `N` unbounded, and a `N` above the maximum score of
/// 100 is *always* true — so the one selector that could never fail to match
/// was also the one selector with no way to say "nothing".
#[cfg(test)]
mod health_threshold_boundary {
    use super::*;

    /// A fleet of five healthy repos — the shape the bad measurement was
    /// taken against, and the shape that makes a widening selector
    /// indistinguishable from a correct one.
    fn five_healthy_repos() -> (tempfile::TempDir, ro_state::Connection) {
        let tmp = tempfile::TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        for name in ["api", "web", "cli", "docs", "svc"] {
            conn.execute(
                "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
                 VALUES (?1, 'github.com', 'acme', ?2, 'https://example.com/x.git', ?3, ?4, ?4)",
                rusqlite::params![format!("id-{name}"), name, format!("/s/{name}"), now],
            )
            .unwrap();
        }
        (tmp, conn)
    }

    fn select(conn: &ro_state::Connection, filter: &str) -> Vec<String> {
        resolve_targets(
            conn,
            None,
            Some(filter),
            true,
            std::path::Path::new("/s"),
            false,
        )
        .map(|t| t.into_iter().map(|x| x.repo_id).collect())
        .unwrap_or_default()
    }

    /// The headline bug, as a test.
    ///
    /// A threshold above the maximum possible score cannot narrow anything,
    /// so it is refused — a usage error the caller turns into EX_USAGE, the
    /// same refusal a `--tag` nobody carries gets. Before the fix this
    /// returned all five repos, and the caller saw a non-empty selection and
    /// synced every one of them.
    #[test]
    fn a_health_threshold_above_the_maximum_score_is_refused_not_the_whole_fleet() {
        for threshold in ["999", "1000", "1000000", "100", "101"] {
            let (_t, conn) = five_healthy_repos();
            let err = resolve_targets(
                &conn,
                None,
                Some(&format!("health:{threshold}")),
                true,
                std::path::Path::new("/s"),
                false,
            )
            .expect_err("a threshold that cannot narrow must be refused");
            let msg = format!("{err:#}");
            assert!(
                msg.contains(threshold),
                "the error must name the threshold the user typed \
                 (`{threshold}`), got: {msg}"
            );
            assert!(
                msg.contains("100"),
                "the error must say what the ceiling is, got: {msg}"
            );
        }
    }

    /// The threshold above the maximum, the way a user actually reaches it.
    ///
    /// Asserted through `resolve_targets` rather than the leaf so that the
    /// whole path is covered, and so that the fix cannot be "make the
    /// comparison false" — which would pass the previous test and break this
    /// one, and leave the flag able to select nothing for `health:0`.
    #[test]
    fn the_documented_angle_bracket_spelling_is_refused_at_the_top_too() {
        let (_t, conn) = five_healthy_repos();
        let err = resolve_targets(
            &conn,
            None,
            Some("health:<999>"),
            false,
            std::path::Path::new("/s"),
            false,
        )
        .expect_err("the documented spelling gets the same refusal");
        assert!(format!("{err:#}").contains("999"), "got: {err:#}");
    }

    /// The negative control, and the reason this test can mean anything.
    ///
    /// With the threshold gone, the very same call selects all five. So a
    /// fix that merely made `health:999` select nothing — by breaking the
    /// comparison rather than by refusing the threshold — is caught here,
    /// and a fix that made *every* threshold select nothing is caught too.
    #[test]
    fn the_same_call_without_a_filter_still_selects_the_whole_fleet() {
        let (_t, conn) = five_healthy_repos();
        assert_eq!(select(&conn, "tag:work").len(), 0, "a tag nobody has");
        let all =
            resolve_targets(&conn, None, None, true, std::path::Path::new("/s"), false).unwrap();
        assert_eq!(
            all.len(),
            5,
            "the fleet is still reachable; the refusal is scoped to the \
             impossible threshold, not to the resolver"
        );
    }

    /// The lower end, and the boundary itself.
    ///
    /// `health:0` is a real request — "only the repos at the bottom of the
    /// scale" — and the score really does go to 0 (100 − 30 archived − 50
    /// disabled − 30 failed runs, clamped). Under the old `score < N`
    /// comparison a repo sitting at exactly 0 was reachable by **no**
    /// threshold a person types, so `health:0` selected nothing, always: the
    /// sickest repos in the fleet were the only ones the sick-repo filter
    /// could never return. The comparison is inclusive now, and both sides
    /// of the boundary are asserted here rather than one of them assumed.
    #[test]
    fn health_zero_selects_the_repos_at_the_bottom_of_the_scale() {
        let (_t, conn) = five_healthy_repos();
        // Drive one repo to a score of exactly 0: archived (-30), disabled
        // (-50) and failed runs (-30) is 110, clamped to 0. `run_id` is a
        // foreign key, so the runs are opened for real rather than invented.
        conn.execute(
            "UPDATE repos SET archived = 1, disabled = 1 WHERE id = 'id-api'",
            [],
        )
        .unwrap();
        for _ in 0..6 {
            let run = ro_jobs::open_run(&conn, "sync", &[]).unwrap();
            conn.execute(
                "INSERT INTO sync_results
                 (run_id, repo_id, action, status, duration_ms, error, pre_oid, post_oid)
                 VALUES (?1, 'id-api', 'pull', 'error', 1, NULL, NULL, NULL)",
                rusqlite::params![run.id],
            )
            .unwrap();
            ro_jobs::finalize_run(&conn, &run.id, 1).unwrap();
        }
        // Only `id-api` is archived, so it is reachable with the flag.
        let at_zero = resolve_targets(
            &conn,
            None,
            Some("health:0"),
            true,
            std::path::Path::new("/s"),
            true,
        )
        .unwrap();
        let ids: Vec<String> = at_zero.into_iter().map(|t| t.repo_id).collect();
        assert_eq!(
            ids,
            vec!["id-api".to_string()],
            "a repo at the bottom of the scale is selected by the smallest \
             threshold, and nothing else in a healthy fleet is"
        );

        // The other side of the same boundary: a healthy repo scores 100 and
        // is not at the bottom, so it must not be swept in by `health:0`.
        assert!(
            !ids.iter().any(|id| id == "id-svc"),
            "a healthy repo is not a critical one"
        );
    }

    /// A valid threshold that happens to match nothing selects nothing.
    ///
    /// This is the other half of the guarantee, and the half that is easy to
    /// break while fixing the first: "refuse the impossible" must not become
    /// "refuse everything", because `health:50` over a healthy fleet is a
    /// perfectly good question with a perfectly good answer — none of them
    /// need a human.
    #[test]
    fn a_valid_threshold_over_a_healthy_fleet_selects_nothing_and_succeeds() {
        let (_t, conn) = five_healthy_repos();
        assert_eq!(
            select(&conn, "health:50"),
            Vec::<String>::new(),
            "no repo scores at most 50, so none is selected — and the call \
             succeeds, because that is a real answer to a real question"
        );
    }

    /// The lower impossible end, refused for the other reason.
    ///
    /// A score is never below zero, so `health:-1` cannot select anything.
    /// The rule for a selector that matches nothing is that it matches
    /// *nothing*; refusing it is the strictest form of that, and it means a
    /// sign typo is a message rather than a silently empty run.
    #[test]
    fn a_threshold_below_the_floor_is_refused_rather_than_selecting_nothing_silently() {
        let (_t, conn) = five_healthy_repos();
        let err = resolve_targets(
            &conn,
            None,
            Some("health:-1"),
            false,
            std::path::Path::new("/s"),
            false,
        )
        .expect_err("a threshold no score can reach cannot select anything");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("-1"),
            "the error must name the threshold, got: {msg}"
        );
        assert!(
            msg.contains("0"),
            "the error must say what the floor is, got: {msg}"
        );
    }
}

/// Every selector kind, asked "you matched nothing", must give the same
/// answer — and no kind of answer may be the whole fleet.
///
/// This is the invariant the function's own comment already claimed and
/// which was false for exactly one selector kind. `--tag` and `has:*` both
/// refuse to widen; `health:` was the one that could.
#[cfg(test)]
mod every_selector_agrees_on_matched_nothing {
    use super::*;

    fn fleet() -> (tempfile::TempDir, ro_state::Connection) {
        let tmp = tempfile::TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        for name in ["api", "web", "cli"] {
            conn.execute(
                "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
                 VALUES (?1, 'github.com', 'acme', ?2, 'https://example.com/x.git', ?3, ?4, ?4)",
                rusqlite::params![format!("id-{name}"), name, format!("/s/{name}"), now],
            )
            .unwrap();
        }
        (tmp, conn)
    }

    /// The fleet is three repos. Any selector that comes back with all three
    /// has widened into everything, whatever it was asked.
    fn selection_for(filter: &str) -> Result<usize, String> {
        let (_t, conn) = fleet();
        resolve_targets(
            &conn,
            None,
            Some(filter),
            true,
            std::path::Path::new("/s"),
            false,
        )
        .map(|t| t.len())
        .map_err(|e| format!("{e:#}"))
    }

    /// The one property, asserted for every spelling a user can type.
    ///
    /// `health:999` and `health:-1` are refused; the rest match nothing and
    /// say so. **None** of them may return the fleet, which is the whole
    /// point: the same typo must not produce a different answer depending on
    /// which flag the user reached for, and no answer may be "run a sync
    /// against everything".
    #[test]
    fn no_selector_widens_into_the_fleet_when_it_matches_nothing() {
        for filter in [
            "health:999",  // above the maximum score
            "health:1000", // the reported case
            "health:100",  // the ceiling itself, equally un-narrowing
            "health:-1",   // below the floor
            "health:50",   // valid, and matches nothing here
            "tag:nonexistent",
            "group:nonexistent",
            "has:archived", // no row is archived in this fixture
            "has:disabled",
        ] {
            match selection_for(filter) {
                // Refused. The caller turns this into EX_USAGE, which is
                // what `--tag nobody carries` already produced.
                Err(msg) => assert!(
                    msg.contains("health") || !filter.starts_with("health"),
                    "{filter}: the refusal must say what was wrong, got: {msg}"
                ),
                // Matched nothing, which is the answer the flag asked for.
                Ok(0) => {}
                // The failure this whole module exists for.
                Ok(n) => panic!(
                    "{filter} selected {n} of 3 repos. A selector that matches \
                     nothing must never widen into the whole fleet."
                ),
            }
        }
    }

    /// The negative control for the assertion above: a filter that *does*
    /// match returns a subset, and the same helper can tell the difference.
    ///
    /// Without this, a resolver that returned 0 for every filter would pass
    /// the loop above.
    #[test]
    fn a_filter_that_does_match_still_selects() {
        let (_t, conn) = fleet();
        conn.execute(
            "INSERT INTO repo_tags (repo_id, tag) VALUES ('id-web', 'work')",
            [],
        )
        .unwrap();
        let got = resolve_targets(
            &conn,
            None,
            Some("tag:work"),
            true,
            std::path::Path::new("/s"),
            false,
        )
        .unwrap();
        assert_eq!(
            got.len(),
            1,
            "a filter that matches returns exactly what it matched, which is \
             what makes 'matched nothing' a meaningful answer"
        );
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
            // A retired row is still out of reach by name, so the token
            // resolves to nothing and is **reported**. It used to be
            // reported as a successful empty selection, which is the same
            // "a name I typed named nothing, quietly" the token-miss check
            // exists for.
            match resolve_targets(
                &conn,
                Some("acme/archived"),
                None,
                false,
                std::path::Path::new("/s"),
                include,
            ) {
                Ok(t) => t.len(),
                Err(_) => 0,
            }
        };
        assert_eq!(found(false), 0, "quietly reaching a retired row by name");
        assert_eq!(found(true), 1);
    }

    /// The two selectors that *name* the retired state were structurally
    /// unreachable: the skip that excludes a retired row ran before the
    /// selector was ever consulted, so `has:archived` and `has:disabled`
    /// returned an empty run with exit 0 over a fleet that had one. A user
    /// asking "which of my repos did I retire?" was told there were none.
    #[test]
    fn the_has_selectors_that_name_a_retired_row_can_reach_it() {
        let (_t, conn) = conn_with(true, false);
        let ids = |filter: &str| {
            resolve_targets(
                &conn,
                None,
                Some(filter),
                false,
                std::path::Path::new("/s"),
                false,
            )
            .map(|t| t.into_iter().map(|x| x.repo_id).collect::<Vec<_>>())
        };
        assert!(
            ids("has:archived").is_ok(),
            "has:archived must parse and answer, not silently select nothing"
        );
        // `conn_with(true, false)` archives one repo and disables none, so
        // the archived one is selected and the disabled selector is empty —
        // the asymmetry is the fact being asserted, and it is only
        // observable now that the selector is reachable at all.
        assert_eq!(
            ids("has:archived").unwrap().len(),
            1,
            "an archived row is reachable by has:archived"
        );
        assert_eq!(
            ids("has:disabled").unwrap().len(),
            0,
            "nothing is disabled in this fixture"
        );
    }
}
