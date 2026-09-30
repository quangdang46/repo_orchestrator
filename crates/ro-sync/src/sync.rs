//! Sync engine implementation.
//!
//! Read repos from SQLite → create run record → sync each repo (clone if
//! missing → fetch → pull per strategy) → record the outcome → finalise the
//! run.
//!
//! # The fleet runs concurrently; the coordinator owns the database
//!
//! `sync_all` hands the repos to a bounded worker pool, and the results come
//! back in **registry order** rather than completion order, so two runs over
//! the same fleet produce byte-identical summaries.
//!
//! `rusqlite::Connection` is `Send` but **not** `Sync` — it holds a
//! `RefCell` — so no reference to one can cross a thread boundary. That is a
//! compile error (E0277) rather than a runtime surprise, which is the right
//! way to learn it, and it settles the shape: **workers touch only git and
//! the filesystem.** Each one hands back the row it owes the audit trail,
//! and the coordinator writes every `sync_results` row through the single
//! [`record_result`]. There is no second place the trail is written from.
//!
//! The run row still brackets the whole thing: `open_run` before the pool,
//! `finalize_run` after it, on the coordinator, once.
//!
//! # Two rows naming one worktree do not run at once
//!
//! `repos.local_path` is **not** unique — the uniqueness constraint is on
//! `(host, owner, name)` — so a registry can legitimately hold two rows
//! pointing at one checkout. That was harmless while the run was a plain
//! `for` loop: they were serialised by accident. Under a pool it is not, and
//! two `git pull`s in one worktree fight over `.git/index.lock`. So each
//! distinct path gets one lock and a repo holds it for the length of its own
//! sync, which restores the old ordering without giving up the parallelism
//! across paths.
//!
//! Note what this is *not*: `sync` has never taken the cross-process
//! [`ro_git::RepoLock`] that `ro ship` takes. The doc comment at the top of
//! this file used to claim it did. Two `ro` processes syncing the same
//! checkout was already possible before this change and still is; what this
//! change removes is the *new* in-process race.

use anyhow::{Context, Result};
use clap::ValueEnum;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::manage::TrackedRepo;

/// Update strategy for syncing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[derive(ValueEnum)]
pub enum SyncStrategy {
    FfOnly,
    Rebase,
    Merge,
}

impl std::fmt::Display for SyncStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncStrategy::FfOnly => write!(f, "ff-only"),
            SyncStrategy::Rebase => write!(f, "rebase"),
            SyncStrategy::Merge => write!(f, "merge"),
        }
    }
}

/// The credential reference a row's sync should use: the per-repo file's
/// value if it has one, otherwise the row's.
///
/// The same precedence `ro ship` applies, and it has to be the same one —
/// `ro ship` reads the merged value through `plan_for`, so a sync that read
/// only the row would fetch with a token the push does not use, and the
/// per-repo file would be honoured by one verb and ignored by the other.
fn effective_credential_ref(repo: &TrackedRepo) -> Option<String> {
    let local = ro_config::local::RepoLocalConfig::load(Path::new(&repo.local_path))
        .ok()
        .flatten();
    let mut reference = repo.credential_ref.clone();
    if let Some(l) = &local {
        // Only the credential is read here, but `apply_to` takes all four
        // slots, so the other three are filled with throwaway `Option`s.
        let mut discard_a = None;
        let mut discard_b = None;
        let mut discard_c = None;
        l.apply_to(
            &mut discard_a,
            &mut reference,
            &mut discard_b,
            &mut discard_c,
        );
    }
    reference
}

/// Options for a sync operation.
#[derive(Debug, Clone)]
pub struct SyncOptions {
    pub strategy: SyncStrategy,
    pub autostash: bool,
    pub timeout_secs: u32,
    /// Preview only; do not clone/pull.
    pub dry_run: bool,
    /// Only clone missing repos; skip pulling existing.
    pub clone_only: bool,
    /// Only pull existing repos; skip cloning missing.
    pub pull_only: bool,
    /// Delete remote-tracking refs whose branch is gone upstream.
    ///
    /// `git remote prune`, not `git fetch --prune`. The narrower of the
    /// two on purpose: this removes local bookkeeping for branches that no
    /// longer exist and never touches the remote, so it is safe to run
    /// across a fleet without asking first.
    pub prune: bool,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            strategy: SyncStrategy::FfOnly,
            autostash: false,
            timeout_secs: 30,
            dry_run: false,
            clone_only: false,
            pull_only: false,
            prune: false,
        }
    }
}

impl SyncOptions {
    /// The deadline for one git invocation, from [`Self::timeout_secs`].
    ///
    /// `None` for a zero, and that is deliberate rather than a coercion: a
    /// zero-length deadline is not a fast git, it is a git killed before it
    /// has finished starting. The field is a `u32` with a documented
    /// "must be >= 1" in the config schema, so zero only arrives when
    /// somebody typed `--timeout 0`; the honest reading of that is "no
    /// deadline", which is what git did before this field was wired up and
    /// what a user asking for it is asking for.
    pub fn git_timeout(&self) -> Option<Duration> {
        (self.timeout_secs > 0).then(|| Duration::from_secs(self.timeout_secs as u64))
    }

    /// `RunOpts` carrying this run's deadline and nothing else.
    ///
    /// The deadline is **not** cosmetic. `RunOpts::timeout` is what makes
    /// `ro_git` spawn the child into its own process group and `killpg` the
    /// whole tree on expiry — killing only the direct child would leave the
    /// `ssh` and credential helpers it started holding the worktree lock,
    /// which stalls the fleet exactly as much as never killing anything at
    /// all, and then reports success.
    pub fn run_opts(&self) -> ro_git::mutation::RunOpts<'static> {
        ro_git::mutation::RunOpts {
            timeout: self.git_timeout(),
            ..ro_git::mutation::RunOpts::none()
        }
    }

    /// [`Self::run_opts`] plus this repo's credential, scoped to this repo's
    /// own host.
    ///
    /// The credential is resolved here rather than at the call site so the
    /// fetch, the pull **and** the clone on one row all carry the same token,
    /// and so a bad reference is reported once instead of per network call.
    ///
    /// This is the same pair `ro ship` makes — [`fetch_with_credential`] and
    /// [`push_with_credential`] — and it was missing on the sync side
    /// entirely: `sync_repo_inner` built `FetchOpts::default()` and handed
    /// `opts.run_opts()` to `fetch_in`, so `host` was `None`, no header was
    /// scoped, and **every** fetch a sync made went out anonymously. A private
    /// repo enrolled with `--credential` — the one case the flag exists for —
    /// synced with `fatal: could not read Username`, while `ro ship` over the
    /// same row succeeded. The flag was validated, stored, echoed by
    /// `ro list`, and used by exactly one verb.
    ///
    /// `Err` is a malformed or unresolvable reference, which is the user's
    /// problem to hear about and not something to answer by falling back to
    /// the machine's own credential — that would be the wrong account,
    /// silently.
    pub fn run_opts_for(&self, repo: &TrackedRepo) -> Result<crate::manage::CredentialEnv> {
        crate::manage::credential_env(&repo.clone_url, effective_credential_ref(repo).as_deref())
    }

    /// Set the deadline on a git invocation and run it.
    ///
    /// `ro_git`'s `fetch`, `pull` and `clone` take their options structs and
    /// run with `RunOpts::none()` internally, so a caller that needs the
    /// deadline enforced has to reach past them. This is that seam: the same
    /// command, the same args, the same outcome, with the caller's `RunOpts`
    /// handed to `run_in` — and `run_in` is the only thing in `ro_git` that
    /// knows how to kill a child *tree* rather than a child.
    ///
    /// Written as a free function on `SyncOptions` rather than a method on
    /// `ro_git::mutation` because the deadline is this run's and the command
    /// is git's. Pushing a `&SyncOptions` through `ro_git`'s API would put a
    /// sync concept in a git crate; leaving the three `run_in` calls inline
    /// at each site would be three copies of the same four lines.
    pub fn git(
        &self,
        cwd: Option<&Path>,
        args: &[&str],
    ) -> Result<ro_git::mutation::GitCommandResult> {
        ro_git::mutation::run_in(cwd, args, &self.run_opts())
    }
}

/// Result of syncing a single repo.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncResult {
    pub repo_id: String,
    pub action: String,
    pub status: String,
    pub duration_ms: u64,
    pub error: Option<String>,
    pub pre_oid: Option<String>,
    pub post_oid: Option<String>,
    /// What the repo is relative to its upstream, as the sync would find it.
    ///
    /// `None` for every row that is not a measurement — a clone, a skip, an
    /// error — because a row that carries a count it did not take is a row
    /// that reads as a fact. Populated by the dry run, which is the only
    /// caller that measures without acting.
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    /// Why the ahead/behind comparison could not be made, when it could not.
    ///
    /// The same reason `ro status` carries: a dry run that says
    /// "ahead=unknown" with no explanation sends the user hunting through
    /// twenty rows for the one that is unmeasured.
    pub unmeasurable_reason: Option<String>,
    /// What this invocation would actually *do* to the repo, in one line.
    ///
    /// The dry run's whole output. `would_pull` / `would_clone` was the whole
    /// of it, which is the bug: the one question a dry run is ever asked is
    /// "is there anything waiting for me", and a repo three commits behind
    /// and a repo already in line both answered `would_pull`.
    ///
    /// This is the field a consumer reads for the plan, and it used to be
    /// `None` on every row the module produces while the text went into
    /// `error` — so a reader of the documented field got nothing, and a
    /// reader of `error` got a sentence that is not an error. Both carry it
    /// now; see [`SyncResult::plan_mismatch`] for why `error` cannot simply
    /// be emptied.
    pub plan: Option<String>,
    /// The prediction this run made about itself, and where the outcome
    /// contradicted it.
    ///
    /// Every **real** run predicts its own result first, from local state
    /// only, and then compares that prediction with what actually happened
    /// ([`plans_match`]). `None` for a dry run, whose plan *is* the result,
    /// and for every real run whose prediction held. `Some(reason)` is a
    /// prediction that was wrong, which is the defect a dry run exists to
    /// prevent — reported on the row because a `tracing::warn!` is invisible
    /// to every consumer of this struct, and a disagreement nobody can read
    /// is a disagreement nobody acts on.
    ///
    /// **It does not fail the run, and the reason is not leniency.** The two
    /// cases are not symmetric. A run that refused a repo the plan called
    /// clean has already recorded `error` / `conflict` /
    /// `autostash_conflict`, so the exit code already says the repo is not in
    /// the state the user asked for; the mismatch adds information, not a
    /// verdict. A run that *succeeded* where the plan predicted failure —
    /// the plan measured a ref the fetch was about to move — is a perfectly
    /// good run over a healthy repo, and failing it would turn the tool's own
    /// staleness into a false alarm on the fleet. The exit code answers "is
    /// every repo where the user asked for it"; this field answers "did the
    /// preview tell the truth", and they are different questions.
    pub plan_mismatch: Option<String>,
    /// Why this row is not a success, when it is not a success for a reason
    /// that is **not** an error.
    ///
    /// `skipped_dirty` and `skipped_unpushed` are the two. Both used to carry
    /// their whole explanation in the `sync_results` row's `error` column and
    /// nowhere else: `SyncResult::error` was left `None` for exactly these two
    /// (neither is an error, and the field's doc says so), and the text, json
    /// and ndjson renderers all read `error`. So a plain `ro sync` printed
    /// the bare word
    ///
    /// ```text
    /// work/api action=skipped_dirty status=skipped
    /// ```
    ///
    /// and nothing else — the word, with none of the sentence that tells the
    /// user the fix is `--autostash`. The reason existed, was correct, was
    /// written to the database on every run, and reached no reader at all.
    ///
    /// A separate field rather than a second meaning on `error`, so a caller
    /// that treats "there is an error message" as "this repo failed" keeps
    /// being right: the run-level verdict reads [`status_fails_run`], which
    /// reads `status`, and `skipped` is not a failure.
    pub reason: Option<String>,
}

/// The statuses `sync_repo` can put in [`SyncResult::status`], and what each
/// one means for the run.
///
/// Written down because the run-level verdict is a decision about *this*
/// list, and a decision that is not written down is a decision that gets
/// re-made wrong. Every status the module emits is here:
///
/// | status                | meaning                                    | fails the run |
/// |-----------------------|--------------------------------------------|---------------|
/// | `success`             | the repo is where the remote says it is    | no            |
/// | `dry_run`             | nothing was asked of the repo              | no            |
/// | `skipped`             | deliberately not touched, with a reason     | no            |
/// | `error`               | git refused, or ro could not run it        | **yes**       |
/// | `autostash_conflict`  | the pull landed, the pop did not           | **yes**       |
/// | `conflict`            | the pull itself stopped on a conflict      | **yes**       |
///
/// The three that fail are the three where the repo is **not** in the state
/// the user asked for and the user has to do something about it. The three
/// that do not are the three where the repo is exactly as it should be, or
/// was never touched.
///
/// `autostash_conflict` and `conflict` are the two rows that were missing,
/// and they are missing for the same reason: the aggregation counted
/// `status == "error"` and nothing else, so a run whose only bad repo was one
/// of these exited 0 and wrote `exit_code = 0` into `runs`. A script driving
/// ro saw success. That is the lie the whole `--autostash` work exists to
/// prevent, reintroduced one layer up.
///
/// `conflict` is a failure **for the run** and nothing more. `ro ship`
/// continues from a conflicted repo, but it does so by reading the
/// on-disk state — the merge/rebase markers and unmerged index entries, via
/// `ro_git::conflict::detect` — not by matching this string, so failing the
/// run does not stop a ship that is entitled to continue. The two answers
/// are about different questions: "did this run do what was asked?" and
/// "can the pipeline go on?".
///
/// The skips are deliberately **not** failures, and this is the part that
/// must not be "fixed" by making them so. `skipped_dirty` without
/// `--autostash` is the tool refusing to throw work away, which is the
/// correct answer; `skipped_unpushed` is a branch that has never been
/// pushed, which is the normal state of a new branch; `skipped_clone` under
/// `--pull-only` is the flag being honoured. A fleet of twenty healthy repos
/// where three are dirty must exit 0, or the exit code stops meaning
/// anything and scripts start ignoring it.
pub(crate) const STATUS_SUCCESS: &str = "success";
pub(crate) const STATUS_DRY_RUN: &str = "dry_run";
pub(crate) const STATUS_SKIPPED: &str = "skipped";
pub(crate) const STATUS_ERROR: &str = "error";
pub(crate) const STATUS_AUTOSTASH_CONFLICT: &str = "autostash_conflict";
pub(crate) const STATUS_CONFLICT: &str = "conflict";

/// Whether one repo's outcome makes the whole run fail.
///
/// Kept as a function rather than an inline comparison so the rule has one
/// home and one set of tests, and so a status added later cannot be
/// forgotten here — an unknown status is treated as a failure, because a
/// status nothing has classified is not a status anyone has vouched for.
pub(crate) fn status_fails_run(status: &str) -> bool {
    match status {
        STATUS_SUCCESS | STATUS_DRY_RUN | STATUS_SKIPPED => false,
        STATUS_ERROR | STATUS_AUTOSTASH_CONFLICT | STATUS_CONFLICT => true,
        other => {
            tracing::warn!(status = %other, "an unclassified sync status is failing the run");
            true
        }
    }
}

// ── The deadline ──
//
// `--timeout <secs>` arrived as a field on `SyncOptions` and a flag on the
// CLI, and nothing ever read it. Every git call this module made went out
// with `RunOpts::none()`, whose `timeout` is `None`, and `None` means
// `Command::output()` — block until the child exits, forever. A `--timeout 5`
// sync over a fleet ran exactly as long as its slowest git, and the slowest
// git is the one waiting on a network that never answers. The flag was in
// `--help` with a unit in it, so it read as enforced, and the guarantee it
// was not enforcing is the guarantee the rest of the tool is built on: a
// wedged git call must not wedge the fleet.
//
// The deadline is enforced by `ro_git::mutation::run_in`, which already
// implements the discipline the engine uses — spawn into a fresh process
// group, poll, and on expiry `killpg` the group and reap, so the `ssh` and
// credential helpers git started die with it. `SyncOptions::run_opts` is the
// seam that hands it over, and `SyncOptions::git` is the one call site that
// uses it.
//
// `fetch`, `pull` and `clone` in `ro_git` take their options structs and
// run with `RunOpts::none()` internally, so they could not be given a
// deadline without a signature change in a crate this one does not own.
// That change landed as `fetch_in` / `pull_in` / `clone_in`: the same
// command, the same args, the same outcome, with the caller's `RunOpts`
// handed to `run_in`. The deadline now reaches every git call a sync makes
// — fetch, pull, clone, prune, and the autostash tree reads inside `pull`
// — which is the guarantee the flag was written for.
//
// The two-spell shape is deliberate. The ~20 existing callers across the
// workspace keep calling `fetch`/`pull`/`clone` and keep getting
// `RunOpts::none()`; the one caller that has a deadline to enforce asks for
// it by name. Changing the existing signature would have meant a mechanical
// edit to every call site to pass an argument none of them has any use for,
// which is how a deadline requirement decays into a default nobody notices.

/// A dry run that says a repo is fine and a real run that then refuses it.
///
/// The property the dry run exists for, stated as a function of the two
/// results rather than as a hope.
///
/// A dry run reaches it by construction for every decision the real run
/// makes **before** it talks to the network: the same code path reads the
/// worktree, the same branch, the same flags, and the plan's `status` is
/// copied into the row rather than being `dry_run` for everything. That is
/// the bug this replaces — the old dry run emitted `status = "dry_run"` for
/// all of them, and `dry_run` never fails a run, so a repo the real run was
/// going to skip for being dirty read as a clean dry run and the verdict
/// matched only by accident.
///
/// It **cannot** hold for a repo the real run refuses for a reason no local
/// read can see — a remote that is down, a URL that has stopped resolving, a
/// server that has started refusing the credential. Deciding those would mean
/// doing the fetch, and a dry run that fetches is a fetch. What the plan says
/// instead is which ref the numbers were measured against, so a user can see
/// that the answer is a local one.
///
/// The comparison is on the **verdict**, not the status string. Two different
/// non-failing statuses for the same repo are not a disagreement about
/// whether the run passes, and a repo whose upstream ref simply is not
/// resolvable locally is the case where the honest dry-run status and the
/// real-run status differ while the answer to "does this run pass" does not.
pub fn plans_match(dry_run: &SyncResult, real_run: &SyncResult) -> bool {
    status_fails_run(&dry_run.status) == status_fails_run(&real_run.status)
}

/// How long ago the local `upstream` ref was last updated, if we can tell.
///
/// "Last updated" is read from git's own reflog for the ref
/// (`git reflog show <ref> --date=unix --format=%ct`), which records the
/// moment the ref moved — i.e. the moment of the fetch that last brought it
/// level with the remote. That is exactly the freshness of the number a
/// dry run is about to report, so it is the right clock. `None` means we
/// could not read it (no reflog, unreadable ref, the reflog git call
/// failed), and the caller must then say so rather than imply the number is
/// current.
///
/// This is the seam the whole dry-run-honesty fix turns on: a dry run cannot
/// fetch (that would make it a real run), so the only honest thing it can do
/// is report *how old* the ref it measured against is.
fn tracking_ref_age(opts: &SyncOptions, local: &Path, upstream: &str) -> Option<Duration> {
    let out = opts
        .git(
            Some(local),
            &["reflog", "show", upstream, "--date=unix", "--format=%ct"],
        )
        .ok()?;
    if !out.ok() {
        return None;
    }
    let ts: i64 = out.stdout.trim().lines().next()?.trim().parse().ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    // A timestamp in the future means a skewed clock or a reflog we cannot
    // interpret; either way, do not report a negative age.
    if now < ts {
        return None;
    }
    Some(Duration::from_secs((now - ts) as u64))
}

/// A duration in the coarse, human units a plan sentence can carry.
fn human_age(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs} second(s) ago")
    } else if secs < 3_600 {
        format!("{} minute(s) ago", secs / 60)
    } else if secs < 86_400 {
        format!("{} hour(s) ago", secs / 3600)
    } else {
        format!("{} day(s) ago", secs / 86_400)
    }
}

/// The caveat every "looks fine" plan carries, because a dry run's numbers
/// come from a local ref that may be arbitrarily old.
///
/// "in line with origin/main" reads as a claim about the remote. It is not
/// one: it is a claim about the **local** `origin/main` ref, which is only as
/// fresh as the last fetch — possibly weeks. Saying so is the difference
/// between a preview and a lie. When the age is known we name it ("as last
/// fetched 3 days ago"); when it is not, we still say that a dry run cannot
/// see past the local ref, because the honest answer to "is this in line?" is
/// "in line with a ref of unknown freshness", never a bare "yes".
fn staleness_note(opts: &SyncOptions, local: &Path, upstream: &str) -> String {
    match tracking_ref_age(opts, local, upstream) {
        Some(age) => format!(
            " (that ref was last fetched {}. A dry run does not fetch, so commits \
             pushed to the remote since then are not visible here — run `ro sync` \
             for a live answer)",
            human_age(age)
        ),
        None => format!(
            " (the local {upstream} ref's last-fetch time could not be read. A dry \
             run does not fetch, so this may be stale — run `ro sync` for a live \
             answer)"
        ),
    }
}

/// What one repo's sync would do, decided without doing it.
///
/// This is the dry run's whole answer, and it is a *plan* rather than a
/// label. `would_pull` / `would_clone` was the whole of it, which is the
/// bug: the one question a dry run is ever asked is "is there anything
/// waiting for me", and a repo three commits behind and a repo already in
/// line both answered `would_pull`. A dry run that cannot tell those apart
/// is not a preview of the sync — it is a restatement of the command line.
///
/// Every branch here is a branch the real run takes, in the same order, so
/// the two cannot disagree. That is the property the dry run exists for and
/// the reason this function is written as a mirror of `sync_repo` rather
/// than as a second implementation of the same logic: a second implementation
/// is a second thing to get wrong, and the disagreement it produces is
/// invisible until someone has already run the real sync.
fn plan_repo(repo: &TrackedRepo, opts: &SyncOptions) -> Result<PlannedSync> {
    let local = Path::new(&repo.local_path);

    if !local.join(".git").exists() {
        // Not cloned. The real run clones unless `--pull-only` says
        // otherwise, and the plan says which of the two it is.
        if opts.pull_only {
            return Ok(PlannedSync {
                action: "skipped_clone".into(),
                status: STATUS_SKIPPED,
                ahead: None,
                behind: None,
                unmeasurable_reason: None,
                plan: Some(
                    "not cloned, and --pull-only skips clones — this repo would not be touched"
                        .into(),
                ),
            });
        }
        return Ok(PlannedSync {
            action: "clone".into(),
            status: STATUS_DRY_RUN,
            ahead: None,
            behind: None,
            unmeasurable_reason: None,
            plan: Some(format!(
                "clone {} into {} — there is nothing here yet",
                repo.clone_url,
                local.display()
            )),
        });
    }

    // Cloned. `--clone-only` skips pulls, and the plan says so rather than
    // leaving a row that reads as "nothing to do".
    if opts.clone_only {
        return Ok(PlannedSync {
            action: "skipped_pull".into(),
            status: STATUS_SKIPPED,
            ahead: None,
            behind: None,
            unmeasurable_reason: None,
            plan: Some(
                "already cloned, and --clone-only skips pulls — this repo would not be touched"
                    .into(),
            ),
        });
    }

    let branch = ro_git::read::current_branch(local)
        .ok()
        .flatten()
        .or(repo.branch.clone())
        .unwrap_or_else(|| "main".into());

    // A checkout with no `origin` has nothing to fetch from. The real run
    // does not fail on it — `git pull` answers "does not appear to be a git
    // repository", which the arm below recognises as a branch that has never
    // been pushed, and records a skip. The plan mirrors that status so the
    // verdicts agree, while saying the more useful thing: the remote is
    // missing, not the branch.
    match ro_git::read::has_remote(local, "origin") {
        Ok(false) => {
            return Ok(PlannedSync {
                action: "skipped_unpushed".into(),
                status: STATUS_SKIPPED,
                ahead: None,
                behind: None,
                unmeasurable_reason: None,
                plan: Some(
                    "no `origin` remote is configured, so there is nothing to fetch \
                     from — this run would skip the pull rather than fail it"
                        .into(),
                ),
            });
        }
        // Could not even list the remotes: a checkout git cannot read. The
        // real run's fetch and pull both fail on it, so the plan says so
        // rather than describing a pull that cannot happen.
        Err(e) => {
            return Ok(PlannedSync {
                action: "pull".into(),
                status: STATUS_ERROR,
                ahead: None,
                behind: None,
                unmeasurable_reason: Some(format!("cannot list remotes: {e:#}")),
                plan: Some(format!(
                    "cannot read this checkout's remotes ({e:#}) — the fetch and the pull \
                     would both fail"
                )),
            });
        }
        Ok(true) => {}
    }

    // A dirty worktree is skipped unless `--autostash` says otherwise. The
    // count is in the plan because the count is the whole reason the user
    // would want to know: "skipped" is a decision, "3 uncommitted
    // change(s)" is the information needed to make a different one.
    let dirty_count = dirty_file_count(local)?;
    if dirty_count > 0 && !opts.autostash {
        return Ok(PlannedSync {
            action: "skipped_dirty".into(),
            status: STATUS_SKIPPED,
            ahead: None,
            behind: None,
            unmeasurable_reason: None,
            plan: Some(format!(
                "skipped: {dirty_count} uncommitted change(s) — a pull would risk them, \
                 and --autostash is not set"
            )),
        });
    }

    // The ahead/behind comparison, which is the question the dry run is
    // asked. Measured against `origin/<branch>` — the same ref the real
    // pull's fetch updates — so the number is the number the sync would act
    // on, not a number about some other base.
    let upstream = format!("origin/{branch}");
    let (ahead, behind, reason) = match ro_git::read::ahead_behind(local, &upstream) {
        Ok(ab) => (Some(ab.ahead), Some(ab.behind), None),
        // Unmeasurable is reported, never guessed at. A dry run that
        // printed `behind=0` for a repo it could not measure would be the
        // one lie a dry run must never tell.
        Err(e) => (
            None,
            None,
            Some(format!("cannot measure {upstream} against HEAD: {e:#}")),
        ),
    };

    // Every branch below that reports a number measured against `upstream`
    // carries this caveat. Without it, "in line with origin/main" and "3
    // commits behind origin/main" are both claims about the **remote** that
    // are really claims about a local ref that may be weeks old, and the
    // first of those is the one a user acts on.
    let stale = staleness_note(opts, local, &upstream);

    let (status, plan) = match (ahead, behind) {
        (Some(a), Some(b)) if a == 0 && b == 0 => (
            STATUS_DRY_RUN,
            format!("nothing to do — {branch} is in line with {upstream}{stale}"),
        ),
        // Diverged. Under `--ff-only` this is not a pull that might be
        // awkward, it is a pull git refuses outright — "Not possible to
        // fast-forward, aborting" — so the plan says the run will fail
        // rather than describing a sync that will not happen. The dry run
        // and the real run then agree, which is the property the flag is
        // for; the alternative is a dry run that promises a pull and a real
        // run that exits 1.
        (Some(a), Some(b)) if a > 0 && b > 0 && matches!(opts.strategy, SyncStrategy::FfOnly) => (
            STATUS_ERROR,
            format!(
                "the pull will fail — {branch} has diverged from {upstream} \
                     ({a} ahead, {b} behind) and --ff-only refuses a non-fast-forward. \
                     Use --strategy rebase or --strategy merge, or push the local \
                     commits first{stale}"
            ),
        ),
        (Some(a), Some(b)) if a > 0 && b > 0 => (
            STATUS_DRY_RUN,
            format!(
                "pull {branch} — it has diverged from {upstream} ({a} ahead, {b} behind), \
                 so this is a {}-way sync, not a fast-forward{stale}",
                match opts.strategy {
                    SyncStrategy::Rebase => "rebase",
                    SyncStrategy::Merge => "merge",
                    SyncStrategy::FfOnly => "merge",
                }
            ),
        ),
        (Some(_), Some(b)) if b > 0 => (
            STATUS_DRY_RUN,
            format!("pull {branch} — {b} commit(s) behind {upstream} are waiting{stale}"),
        ),
        // Ahead and not behind is the case the old dry run could not see,
        // and the one a dry run most needs to be honest about: sync does
        // not push, so "would pull" over a repo with unpushed work was a
        // promise the run would not keep.
        (Some(a), Some(_)) if a > 0 => (
            STATUS_DRY_RUN,
            format!(
                "nothing to pull — {branch} is {a} commit(s) ahead of {upstream}. \
                 `ro sync` does not push, so those commits stay local until you \
                 push them yourself{stale}"
            ),
        ),
        // **Unmeasurable is not in line.** `ahead_behind` failed above, so
        // `(None, None)` arrives here and used to fall into the same `_`
        // arm as a genuinely measured `(0, 0)` — printing "nothing to do —
        // main is in line with origin/main" for a checkout that git cannot
        // compare against anything. The truth was carried only in
        // `unmeasurable_reason` and a staleness parenthetical while the
        // headline sentence still claimed a measurement nobody made.
        //
        // This is the one lie a dry run must never tell, and it is
        // reachable without anything exotic: a clone killed by `--timeout`
        // leaves a `.git` with zero objects and `HEAD=refs/heads/.invalid`,
        // which `.git exists()` accepts, and which measures `(None, None)`
        // on every subsequent run — even after the remote is healthy again.
        (None, None) => (
            STATUS_ERROR,
            format!(
                "cannot tell whether {branch} is in line with {upstream} — {} \
                 A dry run that reports a repo as in sync when it could not \
                 measure it is worse than reporting nothing, so this run is \
                 marked failed: fix the checkout, or re-clone it.",
                reason.as_deref().unwrap_or("the comparison failed")
            ),
        ),
        _ => (
            STATUS_DRY_RUN,
            format!("nothing to do — {branch} is in line with {upstream}{stale}"),
        ),
    };

    Ok(PlannedSync {
        action: "pull".into(),
        status,
        ahead,
        behind,
        unmeasurable_reason: reason,
        plan: Some(plan),
    })
}

/// What [`plan_repo`] decided, as a row.
///
/// A struct rather than a tuple because the dry run has to build a
/// [`SyncResult`] from it and a seven-field tuple is how a field gets
/// dropped on the way.
struct PlannedSync {
    action: String,
    status: &'static str,
    ahead: Option<u32>,
    behind: Option<u32>,
    unmeasurable_reason: Option<String>,
    plan: Option<String>,
}

/// Sync a single repo.
///
/// A **predict-then-verify** run. Before anything is touched, the outcome is
/// predicted from local state alone — the same [`plan_repo`] the dry run
/// uses, over the same inputs, so the prediction is by construction the
/// answer a dry run would have given. After the work is done, the prediction
/// is compared with the outcome by [`plans_match`], and any disagreement is
/// recorded on the row as [`SyncResult::plan_mismatch`].
///
/// This is the property the dry run exists for, checked by the tool against
/// itself on every real sync instead of only by a test. `plans_match` was
/// `pub` and documented as *the* invariant, and it was called from
/// `#[cfg(test)]` and nowhere else — a function that encodes the invariant
/// and cannot be reached from the binary is a comment with a test attached,
/// and a comment does not stop the next divergence.
///
/// **A disagreement warns; it does not fail the run.** The two directions
/// are not symmetric, and treating them the same would be a bug in either:
///
///  * The run **refused** a repo the plan called clean. The row already
///    says `error` / `conflict` / `autostash_conflict`, and the exit code
///    already says this repo is not where the user asked for it. The
///    mismatch adds the fact that the *preview* was wrong, which is a
///    different and useful thing to know.
///  * The run **succeeded** where the plan predicted failure — the plan
///    measured a local ref that the fetch was about to move, saw a
///    divergence that was not there, and said "this will fail". The repo is
///    healthy and fully synced. Failing the run here would mean the
///    tool's own staleness produces a red fleet board, which is how an
///    exit code stops meaning anything.
///
/// So the exit code keeps answering "is every repo where the user asked for
/// it", and `plan_mismatch` answers "did the preview tell the truth". Making
/// the second question change the first would have been the easier wiring
/// and the wrong one.
pub fn sync_repo(
    conn: &Connection,
    repo: &TrackedRepo,
    opts: &SyncOptions,
    run_id: &str,
) -> Result<SyncResult> {
    let (result, row) = run_repo(repo, opts)?;
    // Written here rather than inside `sync_repo_inner` so that the single
    // repo and the fleet share one writer — see [`record_result`].
    record_result(conn, run_id, row)?;
    Ok(result)
}

/// The audit row a finished sync owes, taken off the result itself.
///
/// Derived from the result rather than passed alongside it so a caller cannot
/// record a row that disagrees with the row it is returning: there is one
/// source, and it is the thing the user is shown. The two branches that say
/// something extra hand their row over [`sync_repo_inner`]'s `explicit_row`
/// instead, and they are the only reason that parameter exists.
fn pending_from(result: &SyncResult) -> PendingResult {
    pending(
        &result.repo_id,
        &result.action,
        &result.status,
        result.duration_ms,
        result.error.as_deref(),
        &result.pre_oid,
        &result.post_oid,
    )
}

/// Sync one repo and hand back the row it owes the audit trail.
///
/// The seam between "do the work" and "write it down". Both callers — the
/// single-repo [`sync_repo`] and the fleet's worker — go through here, so
/// there is one answer to "what row does this repo produce".
///
/// It is also where the predict-then-verify comparison lives, and that is
/// not incidental. The fleet used to call `sync_repo_inner` directly, which
/// meant a fleet run never compared its own prediction with its own outcome
/// — the invariant was checked on the one-repo path and skipped on the
/// twenty-repo path, which is the path that matters. One function, one
/// comparison, whichever caller arrived.
fn run_repo(repo: &TrackedRepo, opts: &SyncOptions) -> Result<(SyncResult, PendingResult)> {
    // A dry run's plan *is* its result; there is no second outcome to
    // compare it against, so prediction is for the real run only.
    if opts.dry_run {
        let mut explicit = None;
        let result = sync_repo_inner(&mut explicit, repo, opts)?;
        let row = explicit.unwrap_or_else(|| pending_from(&result));
        return Ok((result, row));
    }

    // Taken **before** `sync_repo_inner` runs, deliberately. The prediction
    // has to be the one a dry run would give, and a dry run sees the repo
    // exactly as it is now — before this run's own fetch has quietly moved
    // the ref the prediction is measured against. Predicting afterwards
    // would compare the outcome with itself and could never disagree.
    let predicted = plan_repo(repo, opts).ok();

    let mut explicit = None;
    let mut result = sync_repo_inner(&mut explicit, repo, opts)?;

    if let Some(plan) = &predicted {
        let predicted_row = planned_row(&repo.id, plan);
        if !plans_match(&predicted_row, &result) {
            // The plan's own sentence goes in the message, not just its
            // action and status. `pull (dry_run)` says nothing about *what*
            // was claimed; "nothing to do — main is in line with origin/main"
            // is the sentence a user would have read and believed, and it is
            // the one that has to be shown to them as the thing that turned
            // out to be wrong.
            let claimed = plan.plan.as_deref().unwrap_or("(no detail)");
            let reason = format!(
                "the preview said `{}` ({}) — {claimed} — and the run turned out to be \
                 `{}` ({}). A dry run measures a local ref that the real run's fetch then \
                 updates, so the two can disagree; a preview that misleads is worth knowing \
                 about.",
                predicted_row.action, predicted_row.status, result.action, result.status
            );
            tracing::warn!(repo = %repo.id, "{reason}");
            result.plan_mismatch = Some(reason);
        }
    }

    let row = explicit.unwrap_or_else(|| pending_from(&result));
    Ok((result, row))
}

/// A [`PlannedSync`] as the row a comparison can be made against.
///
/// Only the verdict is read by [`plans_match`], but the row is built whole
/// so that a mismatch message can name what was predicted and not just
/// "one of them failed". Durations and oids are absent because nothing has
/// happened yet: a plan that carried a post-oid would be a plan that had
/// already run.
fn planned_row(repo_id: &str, plan: &PlannedSync) -> SyncResult {
    SyncResult {
        repo_id: repo_id.to_string(),
        action: plan.action.clone(),
        status: plan.status.to_string(),
        duration_ms: 0,
        error: plan.plan.clone(),
        reason: None,
        pre_oid: None,
        post_oid: None,
        ahead: plan.ahead,
        behind: plan.behind,
        unmeasurable_reason: plan.unmeasurable_reason.clone(),
        plan: plan.plan.clone(),
        plan_mismatch: None,
    }
}

/// The sync itself. [`run_repo`] wraps this; nothing else should call it.
///
/// Takes **no** `Connection` and no `run_id`. It is the function the worker
/// pool runs, and a `Connection` cannot cross a thread boundary — so the row
/// this owes is built here and written by the caller. See the module docs.
///
/// `explicit_row` is the escape hatch for the two branches whose audit row is
/// **not** the returned result: `skipped_dirty` and `skipped_unpushed` leave
/// the returned result's `error` empty, because neither is an error and a
/// caller reading `error` should not find one. Deriving the row from the
/// result would therefore blank those two reasons out of the audit trail, so
/// those branches state their own row and everything else derives one.
///
/// The **reason** those branches return goes on [`SyncResult::reason`], which
/// is the field the text, json and ndjson renderers read for a row that is
/// not a success. `error` stays `None` — a caller that treats "there is an
/// error message" as "this repo failed" is right, because `skipped` is not a
/// failure — and the sentence still reaches every reader.
fn sync_repo_inner(
    explicit_row: &mut Option<PendingResult>,
    repo: &TrackedRepo,
    opts: &SyncOptions,
) -> Result<SyncResult> {
    let start = Instant::now();
    let local = Path::new(&repo.local_path);

    let pre_oid = if local.join(".git").exists() {
        ro_git::read::head_oid(local).ok().flatten()
    } else {
        None
    };

    // `git remote prune` removes local remote-tracking refs for branches
    // the remote no longer has. It touches only local bookkeeping, so it is
    // run before the pull rather than after: the pull refetches what is
    // still there, and anything the prune deleted was already gone upstream.
    if opts.prune && !opts.dry_run && local.join(".git").exists() {
        // The remote name is a **required** argument. `git remote prune`
        // with no name is a usage error — exit 129, "usage: git remote
        // prune [<options>] <name>" — and `run_in` returns `Ok` for a
        // non-zero exit, so the old `let _ =` discarded a command that had
        // never run. The flag existed in `--help` with a full paragraph
        // describing exactly what it did, and changed nothing.
        //
        // `origin` is the name the pull below uses, so it is the name that
        // is pruned: a repo whose remote is called something else has no
        // remote-tracking refs under `origin` to prune, and pruning a
        // different name would be pruning a remote this sync never read.
        let pruned = opts.git(Some(local), &["remote", "prune", "origin"]);
        if let Err(e) = &pruned {
            // A prune that cannot run is not a reason to fail a sync — the
            // pull is the work, and the prune is housekeeping the user asked
            // for once, not on every run. It is reported, though: silent
            // is how this flag became a no-op nobody noticed.
            tracing::warn!("git remote prune origin failed in {}: {e}", local.display());
        }
    }

    if opts.dry_run {
        // The plan is a *mirror* of the branches below, not a second
        // implementation of them, and that is the whole point of it living
        // in its own function over the same inputs. Two independent
        // "decide what would happen" paths are two things to get wrong, and
        // the disagreement is invisible until someone has already run the
        // real sync — which is the one moment a dry run cannot help.
        let planned = plan_repo(repo, opts)?;
        let duration = start.elapsed().as_millis() as u64;
        // Recorded against the run like every other row, so a dry run has an
        // entry in `sync_results` and an exit code on the run a script can
        // read. A verdict that exists only on screen is a verdict nothing
        // downstream can act on.
        return Ok(SyncResult {
            repo_id: repo.id.clone(),
            action: planned.action,
            status: planned.status.into(),
            duration_ms: duration,
            // The reason is the dry run's output. It rides in `error`
            // because that is the field the text renderer already prints
            // for any non-success row, and a dry run whose explanation
            // lives in a field nothing renders says nothing at all.
            error: planned.plan.clone(),
            pre_oid: pre_oid.clone(),
            post_oid: pre_oid,
            ahead: planned.ahead,
            behind: planned.behind,
            unmeasurable_reason: planned.unmeasurable_reason,
            // The plan is the dry run's whole answer, and it is the field a
            // consumer reads for it. It used to be `None` here while the
            // text went into `error`, so a reader of the documented field
            // got nothing and a reader of `error` got a sentence that is
            // not an error. Both carry it now.
            plan: planned.plan,
            plan_mismatch: None,
            reason: None,
        });
    }

    // Resolved **once**, before any network call, and reused by the clone,
    // the fetch and the pull below. Resolving per call site would be three
    // chances for them to disagree about which token this repo is using.
    let credential = opts.run_opts_for(repo)?;
    let credential_env = credential.env;

    if !local.join(".git").exists() {
        if opts.pull_only {
            let duration = start.elapsed().as_millis() as u64;
            return Ok(SyncResult {
                repo_id: repo.id.clone(),
                action: "skipped_clone".into(),
                status: "skipped".into(),
                duration_ms: duration,
                error: None,
                pre_oid: pre_oid.clone(),
                post_oid: None,
                ahead: None,
                behind: None,
                unmeasurable_reason: None,
                plan: None,
                plan_mismatch: None,
                reason: None,
            });
        }
        // Clone
        let clone_opts = ro_git::mutation::CloneOpts {
            // V4 dropped the cached default_branch; the tracked branch is the
            // only hint left for a clone target.
            branch: repo.branch.clone(),
            ..Default::default()
        };
        // The clone carries the credential too. It is the one git call that
        // happens when there is no checkout to read a `.ro/config.local.toml`
        // from, so the row is the only source — and `ro add` resolved the
        // same reference through the same code to make this clone in the
        // first place.
        let clone_run = ro_git::mutation::RunOpts {
            env: &credential_env,
            ..opts.run_opts()
        };
        match ro_git::mutation::clone_in(&repo.clone_url, local, &clone_opts, &clone_run) {
            Ok(outcome) => {
                if !outcome.result.ok() {
                    let duration = start.elapsed().as_millis() as u64;
                    let err_msg = outcome.result.stderr.trim().to_string();
                    return Ok(SyncResult {
                        repo_id: repo.id.clone(),
                        action: "clone".into(),
                        status: "error".into(),
                        duration_ms: duration,
                        error: Some(err_msg),
                        pre_oid,
                        post_oid: None,
                        ahead: None,
                        behind: None,
                        unmeasurable_reason: None,
                        plan: None,
                        plan_mismatch: None,
                        reason: None,
                    });
                }
                let post_oid = ro_git::read::head_oid(local).ok().flatten();
                let duration = start.elapsed().as_millis() as u64;
                Ok(SyncResult {
                    repo_id: repo.id.clone(),
                    action: "clone".into(),
                    status: "success".into(),
                    duration_ms: duration,
                    error: None,
                    pre_oid,
                    post_oid,
                    ahead: None,
                    behind: None,
                    unmeasurable_reason: None,
                    plan: None,
                    plan_mismatch: None,
                    reason: None,
                })
            }
            Err(e) => {
                let duration = start.elapsed().as_millis() as u64;
                let err_msg = format!("{e:#}");
                Ok(SyncResult {
                    repo_id: repo.id.clone(),
                    action: "clone".into(),
                    status: "error".into(),
                    duration_ms: duration,
                    error: Some(err_msg),
                    pre_oid: pre_oid.clone(),
                    post_oid: None,
                    ahead: None,
                    behind: None,
                    unmeasurable_reason: None,
                    plan: None,
                    plan_mismatch: None,
                    reason: None,
                })
            }
        }
    } else {
        if opts.clone_only {
            let duration = start.elapsed().as_millis() as u64;
            return Ok(SyncResult {
                repo_id: repo.id.clone(),
                action: "skipped_pull".into(),
                status: "skipped".into(),
                duration_ms: duration,
                error: None,
                pre_oid: pre_oid.clone(),
                post_oid: pre_oid,
                ahead: None,
                behind: None,
                unmeasurable_reason: None,
                plan: None,
                plan_mismatch: None,
                reason: None,
            });
        }
        // Fetch + pull
        // The fetch is `FetchOpts::default()`, which is every field at its
        // default, which `ro_git::mutation::fetch` renders as a bare
        // `git fetch` — verified against the real git, not assumed, because
        // a fetch that grew a `--prune` or a remote name would quietly make
        // this a different command than the one the option struct describes.
        // `FetchOpts::default()` is every field at its default, which
        // `ro_git::mutation::fetch` renders as a bare `git fetch` — verified
        // against the real git rather than assumed, because a fetch that
        // grew a `--prune` or a remote name would quietly make this a
        // different command than the option struct describes.
        let fetch_opts = ro_git::mutation::FetchOpts::default();
        // `env` is the credential; `timeout` is this run's deadline. Both in
        // one `RunOpts`, because a deadline that reaches only some of a run's
        // git calls is not a deadline and a credential that reaches only some
        // of them is not a credential.
        let fetch_run = ro_git::mutation::RunOpts {
            env: &credential_env,
            ..opts.run_opts()
        };
        if let Err(e) = ro_git::mutation::fetch_in(local, &fetch_opts, &fetch_run) {
            // A fetch that hit the deadline is a timeout, not a refusal, and
            // the two need different responses: a refusal is a wrong flag or
            // a bad URL, a timeout is a network that never answered and the
            // next repo is still worth trying. `GitError::TimedOut` is the
            // one error type in `ro_git` that says which it was, so it is
            // matched by name rather than by the words in its message.
            if let Some(ro_git::mutation::GitError::TimedOut { after, .. }) =
                e.downcast_ref::<ro_git::mutation::GitError>()
            {
                let duration = start.elapsed().as_millis() as u64;
                let err_msg = format!(
                    "git fetch did not finish within {after:?} and was killed, along with \
                     anything it had started. The next repo is unaffected."
                );
                return Ok(SyncResult {
                    repo_id: repo.id.clone(),
                    action: "fetch".into(),
                    status: "error".into(),
                    duration_ms: duration,
                    error: Some(err_msg),
                    pre_oid: pre_oid.clone(),
                    post_oid: pre_oid,
                    ahead: None,
                    behind: None,
                    unmeasurable_reason: None,
                    plan: None,
                    plan_mismatch: None,
                    reason: None,
                });
            }
            let duration = start.elapsed().as_millis() as u64;
            let err_msg = format!("{e:#}");
            return Ok(SyncResult {
                repo_id: repo.id.clone(),
                action: "fetch".into(),
                status: "error".into(),
                duration_ms: duration,
                error: Some(err_msg),
                pre_oid: pre_oid.clone(),
                post_oid: pre_oid,
                ahead: None,
                behind: None,
                unmeasurable_reason: None,
                plan: None,
                plan_mismatch: None,
                reason: None,
            });
        }

        let branch = ro_git::read::current_branch(local)
            .ok()
            .flatten()
            .or(repo.branch.clone())
            .unwrap_or_else(|| "main".into());

        let pull_strategy = match opts.strategy {
            SyncStrategy::FfOnly => ro_git::mutation::PullStrategy::FastForwardOnly,
            SyncStrategy::Rebase => ro_git::mutation::PullStrategy::Rebase,
            SyncStrategy::Merge => ro_git::mutation::PullStrategy::Merge,
        };

        let pull_opts = ro_git::mutation::PullOpts {
            branch: Some(branch.clone()),
            strategy: pull_strategy,
            autostash: opts.autostash,
            ..Default::default()
        };

        // A dirty worktree is skipped, not clobbered.
        //
        // There is deliberately no `--force` on this command. A fleet tool
        // that discards uncommitted work because someone typed a flag
        // while rushing is a tool that gets uninstalled after the first
        // accident. `--autostash` is the way to say "yes, I mean it", and
        // the skip names the count so the user can decide.
        //
        // The flag is now passed all the way down to git (it used to stop
        // here, at bypassing this skip, so git refused the merge with
        // "Your local changes ... would be overwritten" and the sync
        // reported an error for a tree the user had explicitly asked to
        // sync). Passing it is only half the fix: a pop that conflicts
        // still exits 0, which the `autostash_hold` arm below refuses to
        // report as success.
        let dirty_count = dirty_file_count(local)?;
        if dirty_count > 0 && !opts.autostash && !opts.dry_run {
            let duration = start.elapsed().as_millis() as u64;
            let detail = format!("{dirty_count} uncommitted change(s) (use --autostash)");
            // The row and the result now carry the same sentence. They did
            // not: the row's `error` column got the reason and the result
            // got `None`, and every renderer reads the result.
            *explicit_row = Some(pending(
                &repo.id,
                "skipped_dirty",
                STATUS_SKIPPED,
                duration,
                Some(&detail),
                &pre_oid,
                &pre_oid,
            ));
            return Ok(SyncResult {
                repo_id: repo.id.clone(),
                action: "skipped_dirty".into(),
                status: "skipped".into(),
                duration_ms: duration,
                error: None,
                pre_oid: pre_oid.clone(),
                post_oid: pre_oid,
                ahead: None,
                behind: None,
                unmeasurable_reason: None,
                plan: None,
                plan_mismatch: None,
                // `--autostash` is the whole remedy and the user cannot act on
                // the word `skipped_dirty` alone.
                reason: Some(detail),
            });
        }

        let pull_run = ro_git::mutation::RunOpts {
            env: &credential_env,
            ..opts.run_opts()
        };
        let pull_result = ro_git::mutation::pull_in(local, &pull_opts, &pull_run);
        let post_oid = ro_git::read::head_oid(local).ok().flatten();

        let duration = start.elapsed().as_millis() as u64;

        match pull_result {
            // A pull that exited 0 while the autostash stayed stashed.
            //
            // **This arm has to be first.** It used to sit below
            // `Ok(outcome) if outcome.result.ok()`, which matches this case
            // too — a conflicting pop exits 0 — so the arm was unreachable
            // and every conflicting autostash was filed as a clean `updated`.
            // The comment that stood here claimed the opposite ordering was
            // the requirement, which is how it survived review.
            //
            // `git pull --autostash` returns success even when the pop
            // conflicts, so without this arm a green sync row means "your
            // uncommitted work is in `stash@{0}` and your tree has conflict
            // markers". That is worse than the old honest skip: the work is
            // recoverable, but only by someone who knows to look.
            Ok(outcome) if outcome.autostash_hold.is_some() => {
                let detail = outcome.autostash_hold.clone().unwrap_or_else(|| {
                    "the autostash did not pop; your uncommitted work is in the stash".to_string()
                });
                Ok(SyncResult {
                    repo_id: repo.id.clone(),
                    action: "pull".into(),
                    status: STATUS_AUTOSTASH_CONFLICT.into(),
                    duration_ms: duration,
                    error: Some(detail),
                    pre_oid,
                    post_oid,
                    ahead: None,
                    behind: None,
                    unmeasurable_reason: None,
                    plan: None,
                    plan_mismatch: None,
                    reason: None,
                })
            }
            // `pull` returns `Ok` when the command *ran*, not when it
            // succeeded — a non-zero git exit is reported in the outcome's
            // result. Matching only on `Ok` therefore recorded a failed
            // pull as `status = success`, which is the worst possible
            // failure: a rebase that hit a conflict, or a rejected
            // non-fast-forward under `--ff-only`, was indistinguishable
            // from a clean update. The clone branch above already checks
            // this; this one did not.
            Ok(outcome) if outcome.result.ok() => {
                let action = if outcome.already_up_to_date {
                    "already_up_to_date"
                } else {
                    "updated"
                };
                Ok(SyncResult {
                    repo_id: repo.id.clone(),
                    action: action.into(),
                    status: "success".into(),
                    duration_ms: duration,
                    error: None,
                    pre_oid,
                    post_oid,
                    ahead: None,
                    behind: None,
                    unmeasurable_reason: None,
                    plan: None,
                    plan_mismatch: None,
                    reason: None,
                })
            }
            // The command ran and failed. Distinguish a conflict from a
            // plain refusal, because the two need different work: a
            // conflict is a stage `ro ship` knows how to continue from,
            // and a refusal is not.
            Ok(outcome) if outcome.conflict => {
                let stderr = outcome.result.stderr.trim();
                let detail = if stderr.is_empty() {
                    "git reported a conflict with no message"
                } else {
                    stderr
                };
                let err_msg = format!("conflicted: {detail}");
                Ok(SyncResult {
                    repo_id: repo.id.clone(),
                    action: "pull".into(),
                    status: STATUS_CONFLICT.into(),
                    duration_ms: duration,
                    error: Some(err_msg),
                    pre_oid,
                    post_oid,
                    ahead: None,
                    behind: None,
                    unmeasurable_reason: None,
                    plan: None,
                    plan_mismatch: None,
                    reason: None,
                })
            }
            Ok(outcome) => {
                // A branch that has never been pushed has no remote-tracking
                // ref, and `git pull` answers "couldn't find remote ref".
                //
                // That is the *normal* state of a branch someone just made
                // with `git checkout -b`, so recording it as a sync failure
                // fills a fleet board with red for repos that are exactly
                // as they should be. It is a skip with a reason, not an
                // error — and there is nothing to fetch, which is the point.
                let stderr = outcome.result.stderr.trim().to_string();
                if stderr.contains("couldn't find remote ref")
                    || stderr.contains("does not appear to be a git repository")
                {
                    let detail = format!(
                        "{branch} has no remote branch yet — it has never been pushed. \
                         Nothing to fetch."
                    );
                    // The row and the result carry the same sentence. They
                    // did not: the row's `error` column got the reason and the
                    // result got `None`, and every renderer reads the result.
                    *explicit_row = Some(pending(
                        &repo.id,
                        "pull",
                        "skipped_unpushed",
                        duration,
                        Some(&detail),
                        &pre_oid,
                        &post_oid,
                    ));
                    return Ok(SyncResult {
                        repo_id: repo.id.clone(),
                        action: "skipped_unpushed".into(),
                        status: "skipped".into(),
                        duration_ms: duration,
                        error: None,
                        pre_oid,
                        post_oid,
                        ahead: None,
                        behind: None,
                        unmeasurable_reason: None,
                        plan: None,
                        plan_mismatch: None,
                        // The branch is new and unpushed, which is the normal
                        // state of `git checkout -b` — the sentence says so,
                        // because the word `skipped_unpushed` alone reads as
                        // something went wrong.
                        reason: Some(detail),
                    });
                }
                let err_msg = if stderr.is_empty() {
                    format!("git pull failed with status {}", outcome.result.status)
                } else {
                    stderr
                };
                Ok(SyncResult {
                    repo_id: repo.id.clone(),
                    action: "pull".into(),
                    status: "error".into(),
                    duration_ms: duration,
                    error: Some(err_msg),
                    pre_oid,
                    post_oid,
                    ahead: None,
                    behind: None,
                    unmeasurable_reason: None,
                    plan: None,
                    plan_mismatch: None,
                    reason: None,
                })
            }
            Err(e) => {
                let err_msg = format!("{e:#}");
                Ok(SyncResult {
                    repo_id: repo.id.clone(),
                    action: "pull".into(),
                    status: "error".into(),
                    duration_ms: duration,
                    error: Some(err_msg),
                    pre_oid,
                    post_oid,
                    ahead: None,
                    behind: None,
                    unmeasurable_reason: None,
                    plan: None,
                    plan_mismatch: None,
                    reason: None,
                })
            }
        }
    }
}

/// The flags a sync run was invoked with, for the `runs` audit row.
///
/// A dry run and a real run differ in the per-repo rows but not in the run
/// row, so the run row has to say which it was — and the deadline in force
/// is the other fact a reader cannot recover afterwards. The worker bound is
/// the third: a run over one repo and a run over twenty took the same wall
/// time for the first repo, and only this says why.
fn run_args(opts: &SyncOptions, selected: &[String], parallel: usize) -> Vec<String> {
    let mut args = vec![
        format!("strategy={}", opts.strategy),
        format!("timeout_secs={}", opts.timeout_secs),
        format!("parallel={parallel}"),
    ];
    if opts.dry_run {
        args.push("dry_run".to_string());
    }
    if opts.autostash {
        args.push("autostash".to_string());
    }
    if opts.clone_only {
        args.push("clone_only".to_string());
    }
    if opts.pull_only {
        args.push("pull_only".to_string());
    }
    if opts.prune {
        args.push("prune".to_string());
    }
    if !selected.is_empty() {
        args.push(format!("repos={}", selected.join(",")));
    }
    args
}

/// Sync all tracked repos.
///
/// Opens a real `runs` row first. It used to mint a `run_id` with
/// `Uuid::new_v4()` and never insert it, so every `sync_results` write
/// violated a foreign key and — because `record_result` discarded the
/// error — every result vanished. The run row is what makes those writes
/// legal, so it is created here rather than assumed.
///
/// The run is finalised on **every** path including the failing ones. A
/// run left open means "still going", and a fleet that crashed halfway
/// would leave a run that never ends and a status that never updates.
/// Sync every repo, or just `selected` when the caller named some.
///
/// `selected` is a list of repo ids, empty meaning "all". The caller
/// resolves names to ids before calling, because a name that matches
/// nothing is a **usage** error and only the caller knows whether the user
/// named a repo: `ro sync` on an empty registry is a legitimate question
/// with the answer "nothing to do", and `ro sync nonexistent` is a typo
/// that deserves to be reported rather than absorbed.
///
/// The bound is [`SyncOptions::default_parallel`] — the shipped default for
/// `core.parallel`. See [`sync_all_bounded`] for why this function cannot
/// read the user's config for itself.
pub fn sync_all(
    conn: &Connection,
    opts: &SyncOptions,
    selected: &[String],
) -> Result<Vec<SyncResult>> {
    sync_all_bounded(conn, opts, selected, default_parallel())
}

/// How many repos a sync may touch at once when nobody said otherwise.
///
/// Read from `ro_config`'s own default rather than written out as a number,
/// so the bound cannot drift away from the one the shipped config file
/// documents and the fleet verbs use. `core.parallel` is `>= 1` by
/// validation; the `max(1)` is belt-and-braces for a caller that has not
/// been through the validator.
fn default_parallel() -> usize {
    ro_config::schema::AppConfig::default().core.parallel.max(1) as usize
}

/// Sync every selected repo, at most `parallel` at a time.
///
/// The bound is a parameter rather than something read here on purpose.
/// `ro-config`'s rule is that the CLI resolves settings once and threads
/// them down — a library function that reached for `~/.config/ro/config.toml`
/// on its own would make a unit test's behaviour depend on the developer's
/// home directory, which is how a test suite starts passing on one machine
/// and failing on another. So [`sync_all`] supplies the shipped default, and
/// the CLI passes `config.core.parallel` to say what the user actually typed.
///
/// **Order of results is registry order, not completion order.** The pool
/// writes each outcome into its own slot, and the slots are read back in the
/// order the repos were listed. A summary that reorders itself run to run is
/// worse than a slow one: a diff between two runs stops meaning "nothing
/// changed".
///
/// Two rows may name one `local_path` — uniqueness is on
/// `(host, owner, name)`, not on the checkout — so the workers share one
/// lock per distinct path and a repo holds it for its own sync. Without it,
/// two `git pull`s in one worktree race for `.git/index.lock`, which is a
/// failure mode this function did not have while it was a `for` loop.
pub fn sync_all_bounded(
    conn: &Connection,
    opts: &SyncOptions,
    selected: &[String],
    parallel: usize,
) -> Result<Vec<SyncResult>> {
    sync_all_observed(conn, opts, selected, parallel, &FleetObserver::none())
}

/// [`sync_all_bounded`], with the pool's concurrency made observable.
///
/// The observer is a parameter rather than a global because the tests that
/// use it run **concurrently in one process**: a `static` would have each
/// test's counters overwritten by whichever sibling happened to be running,
/// which is a test that fails for a reason that has nothing to do with the
/// code. `&FleetObserver::none()` is what production passes, and it costs
/// one `Option` test per repo.
fn sync_all_observed(
    conn: &Connection,
    opts: &SyncOptions,
    selected: &[String],
    parallel: usize,
    observer: &FleetObserver,
) -> Result<Vec<SyncResult>> {
    // Archived and disabled rows are excluded here rather than at each
    // call site. `list(conn, None)` returns every row, so a sync that did
    // not filter would reach into repos the user had explicitly retired —
    // and a sync that touches an archived repo is a sync that does
    // something you asked it not to do.
    let all = crate::manage::list(conn, None)?;
    let repos: Vec<_> = all
        .into_iter()
        .filter(|r| !r.archived && !r.disabled)
        .filter(|r| selected.is_empty() || selected.contains(&r.id))
        .collect();
    let parallel = parallel.max(1);
    // The run row records what the run *was*. It used to be opened with an
    // empty args slice, so every sync — dry or real, any `--timeout`, any
    // strategy — wrote `args_json = '[]'` and the two were byte-identical
    // in the audit trail. A reader could tell a dry run from a real one by
    // looking at the per-repo rows, but could not tell what flags were in
    // force, which is the question the run-level record exists to answer.
    //
    // Opened before any worker and finalised after every one of them, on
    // this thread: the run row brackets the whole fleet either way.
    let run = ro_jobs::open_run(conn, "sync", &run_args(opts, selected, parallel))
        .context("opening the sync run record")?;
    let run_id = run.id.clone();

    let outcomes = sync_fleet(&repos, opts, parallel, observer)?;
    let results: Vec<SyncResult> = outcomes.iter().map(|(r, _)| r.clone()).collect();
    // Every `sync_results` row this run owes, written here on the
    // coordinator in registry order. A failed *write* aborts the run rather
    // than being folded into a per-repo result: the audit trail is missing,
    // so every result in this run is untrustworthy, including the ones
    // already recorded.
    //
    // The rows the workers built are the ones written, **not** rows
    // re-derived from the results. Two of them — `skipped_dirty` and
    // `skipped_unpushed` — carry a reason in the row's `error` column that
    // the result they return deliberately leaves empty, and re-deriving would
    // have written a `NULL` there: a silent, unreported blanking of the
    // audit trail on exactly the two rows where the reason is the point.
    for (_, row) in &outcomes {
        record_result(conn, &run_id, row.clone())?;
    }

    // Exit code reflects the worst outcome, so a run that recorded a failure
    // is not filed as a clean one. `sync_results.status` already carries the
    // per-repo detail; this is the run-level verdict, and it is the same
    // verdict a script gets from the process exit status.
    //
    // It used to count `status == "error"` and nothing else, which meant
    // `autostash_conflict` — a real per-repo failure, with the user's work
    // in a stash — left the run at exit 0. See `status_fails_run` for the
    // rule and, more importantly, for why the skips stay out of it.
    let failed = results
        .iter()
        .filter(|r| status_fails_run(&r.status))
        .count();
    // Counted separately from `failed` because the two need different work:
    // a pull conflict is a stage `ro ship` knows how to continue from, and
    // an autostash conflict is a stash the user has to resolve by hand. A
    // single "conflicted" number would hide which.
    let conflicted = results
        .iter()
        .filter(|r| r.status == STATUS_AUTOSTASH_CONFLICT || r.status == STATUS_CONFLICT)
        .count();
    let exit_code = run_exit_code(&results);
    ro_jobs::finalize_run(conn, &run_id, exit_code).context("finalising the sync run record")?;

    tracing::info!(
        run = %run_id,
        repos = results.len(),
        failed,
        conflicted,
        "sync run complete"
    );
    Ok(results)
}

/// The process exit code for a finished sync run: 0 when every repo is in a
/// state the user asked for or was deliberately not touched, 1 otherwise.
///
/// Public because the CLI has to print this same number, and a caller that
/// had to re-derive the rule would be a second place for it to go stale.
pub fn run_exit_code(results: &[SyncResult]) -> i32 {
    if results.iter().any(|r| status_fails_run(&r.status)) {
        1
    } else {
        0
    }
}

/// Sync `repos`, at most `parallel` at a time, and return the outcomes in
/// **registry order**.
///
/// The shape is the fleet verbs' — `crates/ro/src/ship/emit.rs` — because
/// it is the same problem: bounded concurrency, deterministic output. The
/// workers touch git and the filesystem and nothing else; no `Connection`
/// crosses this boundary, which is not a style preference but the only thing
/// the type system permits (see the module docs).
///
/// Two details worth naming, because both are the way this goes quietly
/// wrong:
///
///  * **Results are placed by index, not appended.** Appending would return
///    completion order, and a summary that reorders itself between two runs
///    over the same fleet cannot be diffed.
///  * **One lock per distinct path.** Two rows naming one checkout are legal
///    (uniqueness is on `(host, owner, name)`) and used to be serialised by
///    the loop itself. Two concurrent `git pull`s in one worktree are not a
///    slower version of that; they are two writers on one index.
fn sync_fleet(
    repos: &[TrackedRepo],
    opts: &SyncOptions,
    parallel: usize,
    observer: &FleetObserver,
) -> Result<Vec<(SyncResult, PendingResult)>> {
    if repos.is_empty() {
        return Ok(Vec::new());
    }

    let locks = path_locks(repos);

    // A `try_lock` would be wrong here in the other direction: this mutex
    // guards the slots, and a poisoned one still holds every slot that was
    // written. `into_inner` keeps the results a panicking worker managed to
    // produce instead of turning its neighbours' work into a second panic.
    let outcomes: Mutex<Vec<Option<(SyncResult, PendingResult)>>> =
        Mutex::new((0..repos.len()).map(|_| None).collect());

    // Exactly `parallel` workers, each pulling the **next index** off a
    // shared cursor, rather than static chunking.
    //
    // The chunking this replaces computed `workers = min(parallel, n)` and
    // then spawned one thread per `repos.chunks(div_ceil(n, workers))` — so
    // the thread count was `ceil(n / ceil(n/p))`, not `min(p, n)`. For 12
    // repos at `p = 8` that is **6** wide, not 8: measured peak concurrency
    // was 6, while the `runs` row recorded `parallel=8`. Uniform costs hide
    // it, because the wave count is preserved; a skewed fleet pays for it and
    // the audit trail overstates the width the tool used.
    //
    // A shared cursor also makes the pool self-balancing, which static
    // chunks are not: one slow repo cannot hold up a whole chunk's worth of
    // fast ones behind it.
    let next = AtomicUsize::new(0);
    let cursor = &next;
    std::thread::scope(|scope| {
        for _ in 0..parallel.min(repos.len()) {
            let outcomes = &outcomes;
            let locks = &locks;
            scope.spawn(move || {
                loop {
                    let index = cursor.fetch_add(1, Ordering::Relaxed);
                    let Some(repo) = repos.get(index) else {
                        break;
                    };
                    let pair = sync_one(repo, opts, locks, observer);
                    let mut slots = outcomes.lock().unwrap_or_else(|e| e.into_inner());
                    // The index, not the repo id: a repo id is a UUID, and
                    // parsing one as a slot number is a silent
                    // mis-assignment.
                    slots[index] = Some(pair);
                }
            });
        }
    });

    let slots = outcomes.into_inner().unwrap_or_else(|e| e.into_inner());
    let mut results = Vec::with_capacity(repos.len());
    for (repo, slot) in repos.iter().zip(slots) {
        results.push(slot.unwrap_or_else(|| {
            let result = worker_produced_nothing(repo);
            let row = pending_from(&result);
            (result, row)
        }));
    }
    Ok(results)
}

/// Sync one repo, holding its path's lock for the whole of it.
///
/// The lock is held across the git work on purpose: two rows naming one
/// checkout have to be serialised end to end, not just at the moment they
/// happen to touch the index.
fn sync_one(
    repo: &TrackedRepo,
    opts: &SyncOptions,
    locks: &std::collections::HashMap<PathBuf, Arc<Mutex<()>>>,
    observer: &FleetObserver,
) -> (SyncResult, PendingResult) {
    let guard = match locks.get(&lock_key(Path::new(&repo.local_path))) {
        Some(lock) => lock.lock().unwrap_or_else(|e| e.into_inner()),
        // Unreachable: the map is built from the same rows through the same
        // `lock_key`, so a miss here is a real bookkeeping bug rather than a
        // spelling difference. Answering with a visible failure beats
        // `expect`, which would take down a fleet run over it.
        None => {
            return (
                SyncResult {
                    repo_id: repo.id.clone(),
                    action: "error".into(),
                    status: STATUS_ERROR.into(),
                    duration_ms: 0,
                    error: Some(format!(
                        "no lock was prepared for {}; the sync pool and the repo \
                         list disagree",
                        repo.local_path
                    )),
                    pre_oid: None,
                    post_oid: None,
                    ahead: None,
                    behind: None,
                    unmeasurable_reason: None,
                    plan: None,
                    plan_mismatch: None,
                    reason: None,
                },
                pending(
                    &repo.id,
                    "error",
                    STATUS_ERROR,
                    0,
                    Some("the sync pool and the repo list disagree"),
                    &None,
                    &None,
                ),
            );
        }
    };
    // The lock is a guard, not a `drop(guard)` at the end of the function:
    // every early return below has to release it, and there are a dozen.
    let _held = guard;

    // Entered **after** the lock, and that ordering is the whole point. A
    // worker waiting for another row's checkout is not doing git work; if
    // the observer counted it, two rows sharing a path would report a peak of
    // 2 while the second sat blocked — which is exactly the number this
    // observer exists to keep honest.
    let _observed = observer.enter();

    match run_repo(repo, opts) {
        Ok(pair) => pair,
        // A `plan_repo` failure is the only way the body gets here, and it
        // used to abort the whole fleet. In a pool there is nothing to abort
        // — the other repos have already run — so it is a row for this repo
        // and the run's exit code says what it means.
        Err(e) => {
            let err_msg = format!("{e:#}");
            (
                SyncResult {
                    repo_id: repo.id.clone(),
                    action: "error".into(),
                    status: STATUS_ERROR.into(),
                    duration_ms: 0,
                    error: Some(err_msg.clone()),
                    pre_oid: None,
                    post_oid: None,
                    ahead: None,
                    behind: None,
                    unmeasurable_reason: None,
                    plan: None,
                    plan_mismatch: None,
                    reason: None,
                },
                pending(
                    &repo.id,
                    "error",
                    STATUS_ERROR,
                    0,
                    Some(&err_msg),
                    &None,
                    &None,
                ),
            )
        }
    }
}

/// The row for a repo whose worker came back with nothing.
///
/// `std::thread::scope` re-raises a panicking worker's panic when the scope
/// closes, so this is belt-and-braces rather than a live path. It is here
/// because the alternative — an `expect` on the slot — reports "the worker
/// produced no result" as a panic about an index, which names the wrong
/// thing.
fn worker_produced_nothing(repo: &TrackedRepo) -> SyncResult {
    SyncResult {
        repo_id: repo.id.clone(),
        action: "error".into(),
        status: STATUS_ERROR.into(),
        duration_ms: 0,
        error: Some(
            "the worker for this repo produced no result; the sync did not \
             complete for it"
                .into(),
        ),
        pre_oid: None,
        post_oid: None,
        ahead: None,
        behind: None,
        unmeasurable_reason: None,
        plan: None,
        plan_mismatch: None,
        reason: None,
    }
}

/// The key a checkout is locked under: canonicalised when the path exists,
/// raw when it does not.
///
/// Canonicalisation fails for a path that does not exist yet — which is every
/// repo about to be cloned — so the raw path is the fallback, exactly as
/// `ro_git::RepoLock::acquire` does it.
///
/// **Building the map and reading it have to agree on this**, or the lock is
/// silently absent and the thing it exists to prevent happens anyway. They
/// did not: `path_locks` keyed the canonical form while `sync_one` looked up
/// the stored string. `PathBuf`'s `Hash`/`Eq` compare bytes even on Windows,
/// where the filesystem does not, so the two matched only when the stored
/// string was already spelled the way the disk spells it. On Linux `/tmp`
/// already is, so the fleet locked correctly; on Windows the drive and
/// directory casing never is, every lookup missed, and every row failed with
/// "the sync pool and the repo list disagree".
fn lock_key(raw: &Path) -> PathBuf {
    std::fs::canonicalize(raw).unwrap_or_else(|_| raw.to_path_buf())
}

/// One lock per distinct checkout, shared by every row naming it.
///
/// Keyed on a **canonicalised** path so `/tmp/x`, `/tmp/x/` and a symlink to
/// it are one checkout and not three. Canonicalisation fails for a path that
/// does not exist yet — which is every repo about to be cloned — so the raw
/// path is the fallback, exactly as `ro_git::RepoLock::acquire` does it. Two
/// rows spelling the same not-yet-created path the same way still collide,
/// which is the case that matters.
fn path_locks(repos: &[TrackedRepo]) -> std::collections::HashMap<PathBuf, Arc<Mutex<()>>> {
    let mut locks = std::collections::HashMap::new();
    for repo in repos {
        let key = lock_key(Path::new(&repo.local_path));
        locks.entry(key).or_insert_with(|| Arc::new(Mutex::new(())));
    }
    locks
}

/// A `sync_results` row that has been decided but not yet written.
///
/// Produced by a worker, written by the coordinator. Owned rather than
/// borrowed for the obvious reason: it has to outlive the frame it was
/// built in, and cross a thread boundary to get there.
#[derive(Debug, Clone)]
struct PendingResult {
    repo_id: String,
    action: String,
    status: String,
    duration_ms: u64,
    error: Option<String>,
    pre_oid: Option<String>,
    post_oid: Option<String>,
}

/// Build the row a repo's sync owes the audit trail.
///
/// Every terminal branch of `sync_repo_inner` calls this instead of writing,
/// which is what lets the write itself happen on the coordinator — see the
/// module docs on why no reference to a `Connection` can cross a thread.
#[allow(clippy::too_many_arguments)]
fn pending(
    repo_id: &str,
    action: &str,
    status: &str,
    duration_ms: u64,
    error: Option<&str>,
    pre_oid: &Option<String>,
    post_oid: &Option<String>,
) -> PendingResult {
    PendingResult {
        repo_id: repo_id.to_string(),
        action: action.to_string(),
        status: status.to_string(),
        duration_ms,
        error: error.map(str::to_string),
        pre_oid: pre_oid.clone(),
        post_oid: post_oid.clone(),
    }
}

/// Write one row to `sync_results`.
///
/// **The only place the audit trail is written.** Not "the first of two" —
/// the only one, on every path: the single-repo [`sync_repo`] and the fleet
/// [`sync_all`] both come through here. Two writers is how an audit trail
/// becomes two histories that disagree about the same run.
///
/// The error is **returned, not discarded**. This used to be
/// `let _ = conn.execute(...)`, and that one character was the whole bug:
/// `sync_results.run_id` is `NOT NULL REFERENCES runs(id)` and
/// `ro_state::open_db` turns `PRAGMA foreign_keys = ON`, so an insert with
/// a `run_id` that has no `runs` row fails every time — and the failure
/// was thrown away. The table was therefore permanently empty while every
/// caller believed it had recorded a result, and `ro status`'s
/// `last_synced_at` was permanently NULL.
///
/// Discarding an error from a write is only safe when the write is
/// genuinely optional. This one is the audit trail.
fn record_result(conn: &Connection, run_id: &str, row: PendingResult) -> Result<()> {
    conn.execute(
        "INSERT INTO sync_results (run_id, repo_id, action, status, duration_ms, error, pre_oid, post_oid)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            run_id,
            row.repo_id,
            row.action,
            row.status,
            row.duration_ms as i64,
            row.error,
            row.pre_oid,
            row.post_oid,
        ],
    )
    .with_context(|| {
        format!(
            "recording the {}/{} result for repo {} \
             under run {run_id} — is the run row missing?",
            row.action, row.status, row.repo_id
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    pub(crate) fn setup() -> (TempDir, Connection) {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        (tmp, conn)
    }

    /// Run git without depending on the machine's own configuration.
    ///
    /// A developer has `user.email` in their `~/.gitconfig`; a CI
    /// runner does not, so a fixture that commits is green on one and
    /// "Author identity unknown" on the other. The identity comes from
    /// the environment git documents for exactly this, and the config
    /// files are neutralised — `NUL` on Windows, `/dev/null` elsewhere.
    pub(crate) fn run_git(dir: &Path, args: &[&str]) -> std::process::Output {
        let mut cmd = std::process::Command::new("git");
        cmd.args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com");
        #[cfg(windows)]
        cmd.env("GIT_CONFIG_GLOBAL", "NUL")
            .env("GIT_CONFIG_SYSTEM", "NUL");
        #[cfg(not(windows))]
        cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        cmd.output().expect("git runs")
    }

    pub(crate) fn init_bare_remote(dir: &Path) -> PathBuf {
        let remote = dir.join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        run_git(&remote, &["init", "--bare", "-b", "main"]);
        remote
    }

    pub(crate) fn commit_to_remote(dir: &Path, remote: &Path, name: &str, content: &str) {
        let clone_dir = dir.join("tmp-clone");
        std::fs::create_dir_all(&clone_dir).unwrap();
        run_git(&clone_dir, &["clone", &remote.to_string_lossy(), "."]);
        run_git(&clone_dir, &["config", "user.email", "test@example.com"]);
        run_git(&clone_dir, &["config", "user.name", "Test"]);
        std::fs::write(clone_dir.join(name), content).unwrap();
        run_git(&clone_dir, &["add", "."]);
        run_git(&clone_dir, &["commit", "-m", &format!("add {name}")]);
        run_git(&clone_dir, &["push", "origin", "main"]);
        std::fs::remove_dir_all(&clone_dir).unwrap();
    }

    /// The bug this bead is named for, as a test.
    ///
    /// `sync_results.run_id` is `NOT NULL REFERENCES runs(id)`, foreign
    /// keys are on, and `sync_all` used to mint a `run_id` without ever
    /// inserting a `runs` row — so every insert failed and `let _ =`
    /// discarded the error. `sync_results` was therefore permanently
    /// empty, `ro status`'s `last_synced_at` permanently NULL, and the
    /// health scorer's failed-sync penalty permanently zero.
    ///
    /// The negative control below is what makes this assertion real.
    #[test]
    fn a_sync_run_actually_records_rows() {
        let (tmp, conn) = setup();
        let remote = init_bare_remote(tmp.path());
        commit_to_remote(tmp.path(), &remote, "a.txt", "hello");

        let local_path = tmp.path().join("local").join("proj1");
        let repo_id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES (?1, 'github.com', 'alice', 'proj1', ?2, ?3, ?4, ?5)",
            params![
                repo_id,
                remote.to_string_lossy().to_string(),
                local_path.to_string_lossy().to_string(),
                now,
                now
            ],
        )
        .unwrap();

        let results = sync_all(&conn, &SyncOptions::default(), &[]).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, "success", "the sync itself should work");

        // The whole point: the row is in the database, not just returned.
        let recorded: i64 = conn
            .query_row("SELECT COUNT(*) FROM sync_results", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            recorded, 1,
            "sync_results must not be empty; the insert was failing on a \
             foreign key and the error was being discarded"
        );

        // And the run it hangs off must exist, which is what made the
        // insert legal in the first place.
        let runs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE command = 'sync'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(runs, 1, "a sync run must create exactly one runs row");

        // A run left open means "still going". A finished run is recorded
        // as finished, or `ro status` can never show a last-synced time.
        let open: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE ended_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(open, 0, "a finished sync run must be finalised");
    }

    /// `--prune` must actually prune.
    ///
    /// `git remote prune` with **no remote name is a usage error** — exit
    /// 129, "usage: git remote prune [<options>] <name>" — and `run_in`
    /// returns `Ok` for a non-zero exit, so the old `let _ =` discarded a
    /// command that had never run. The flag was in `--help` with a full
    /// paragraph describing what it did, and changed nothing.
    #[test]
    fn prune_removes_stale_remote_tracking_refs() {
        let (tmp, conn) = setup();
        let remote = init_bare_remote(tmp.path());
        commit_to_remote(tmp.path(), &remote, "a.txt", "hello");

        let local_path = tmp.path().join("local").join("proj1");
        let repo_id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES (?1, 'github.com', 'alice', 'proj1', ?2, ?3, ?4, ?5)",
            params![
                repo_id,
                remote.to_string_lossy().to_string(),
                local_path.to_string_lossy().to_string(),
                now,
                now
            ],
        )
        .unwrap();

        // Sync once so the checkout exists and has a remote-tracking ref.
        let results = sync_all(&conn, &SyncOptions::default(), &[]).unwrap();
        assert_eq!(results[0].status, "success");

        // A stale remote-tracking ref: a branch the remote no longer has.
        // `update-ref` rather than a real push, so the fixture does not
        // depend on the remote's ref layout.
        let head = run_git(&local_path, &["rev-parse", "HEAD"]);
        let head = String::from_utf8_lossy(&head.stdout).trim().to_string();
        run_git(
            &local_path,
            &["update-ref", "refs/remotes/origin/gone", &head],
        );
        let before = run_git(&local_path, &["for-each-ref", "refs/remotes/origin/gone"]);
        assert!(
            before.status.success(),
            "the stale ref must exist before the prune"
        );

        let opts = SyncOptions {
            prune: true,
            ..Default::default()
        };
        let results = sync_all(&conn, &opts, &[]).unwrap();
        assert_eq!(results[0].status, "success");

        let after = run_git(&local_path, &["for-each-ref", "refs/remotes/origin/gone"]);
        assert!(
            after.stdout.is_empty(),
            "--prune must remove the stale remote-tracking ref; it is still there"
        );
    }

    /// `--autostash` is the flag that makes a dirty worktree syncable, and
    /// the case that needs it is the one where the remote moved **on the file
    /// the tree is dirty in**. Anything less is a false pass: if the remote
    /// only touched a different file, `git pull --ff-only` fast-forwards
    /// straight past the dirty file, leaves it dirty, and reports success —
    /// so the test passes on a build that never stashed anything and never
    /// popped anything. `git stash list` is empty in that case too, because
    /// no stash was ever created. Asserting on the pull's *exit status*
    /// alone therefore proves nothing about `--autostash` at all.
    ///
    /// Here the remote rewrites the same line the tree has uncommitted, so a
    /// pull without `--autostash` is refused by git itself:
    ///
    ///     Your local changes to the following files would be overwritten
    ///     by merge
    ///
    /// That refusal is what the flag is supposed to prevent. The run must
    /// stash, pull, and pop.
    ///
    /// Two facts about git that shape the fix, both measured here rather
    /// than assumed:
    ///
    ///  * `git pull --autostash` **exits 0 even when the pop conflicts.** On
    ///    a real tree with the remote and the worktree both rewriting the
    ///    same line, the pull fast-forwards, the pop conflicts, and git
    ///    prints `Applying autostash resulted in conflicts` and returns
    ///    success. So `status == "success"` alone does not mean the work
    ///    came back; a fix has to read the tree or the stream, not the code.
    ///  * A failed pop **keeps** the stash (`git stash list` still shows
    ///    `stash@{0}: autostash`), and the work is then reachable only
    ///    through that stash. The plan's rule — a failed pop is a per-repo
    ///    failure naming `git stash list`, never a proceed on a half-popped
    ///    tree — is therefore a real requirement, not a nicety: a silent
    ///    success here is a user's uncommitted work that nothing mentions.
    ///
    /// **What this test asserts was corrected, and why.** It used to require
    /// `status == "success"` and an empty stash — which is exactly the lie
    /// the two facts above describe. Its own scenario (both sides rewriting
    /// the same file) makes the pop conflict, so those assertions could only
    /// ever pass on a build that reported a false success. It now asserts
    /// what actually happens, and the **stash existing at all** is what
    /// proves the flag reached git: without `--autostash` on the command
    /// line git refuses the merge before it stashes anything, and there is
    /// nothing in the stash list to find.
    #[test]
    fn autostash_pulls_a_tree_that_is_dirty_in_a_file_the_remote_moved() {
        let (tmp, conn) = setup();
        let remote = init_bare_remote(tmp.path());
        commit_to_remote(tmp.path(), &remote, "a.txt", "base");

        // A real clone, so there is a real `origin` and a real worktree.
        let local_path = tmp.path().join("local").join("proj1");
        std::fs::create_dir_all(&local_path).unwrap();
        run_git(&local_path, &["clone", &remote.to_string_lossy(), "."]);
        run_git(&local_path, &["config", "user.email", "test@example.com"]);
        run_git(&local_path, &["config", "user.name", "Test"]);

        // The tree goes dirty, in the file the remote is about to change.
        std::fs::write(local_path.join("a.txt"), "LOCAL UNCOMMITTED WORK").unwrap();
        let dirty = run_git(&local_path, &["status", "--porcelain"]);
        assert!(
            dirty.status.success() && !dirty.stdout.is_empty(),
            "the worktree must actually be dirty before the sync, or this test \
             proves nothing: {}",
            String::from_utf8_lossy(&dirty.stdout)
        );

        // Now the remote moves, rewriting that exact file.
        commit_to_remote(tmp.path(), &remote, "a.txt", "REMOTE MOVED AHEAD");

        let repo_id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES (?1, 'github.com', 'alice', 'proj1', ?2, ?3, ?4, ?5)",
            params![
                repo_id,
                remote.to_string_lossy().to_string(),
                local_path.to_string_lossy().to_string(),
                now,
                now
            ],
        )
        .unwrap();

        let results = sync_all(
            &conn,
            &SyncOptions {
                autostash: true,
                ..SyncOptions::default()
            },
            &[],
        )
        .unwrap();

        // The flag reached git, so git stashed and pulled rather than
        // refusing the merge. The pop then conflicted — both sides rewrote
        // the same file — and that must be reported, not filed as success.
        assert_eq!(
            results[0].status, "autostash_conflict",
            "--autostash must let a dirty tree pull, and a pop that conflicted \
             must be reported as such rather than as a clean sync. error: {:?}",
            results[0].error
        );
        assert!(
            results[0]
                .error
                .as_deref()
                .is_some_and(|e| e.contains("git stash list")),
            "the failure must name `git stash list`, where the work now lives: {:?}",
            results[0].error
        );

        // The pull actually landed: the remote's version is in the tree,
        // underneath the conflict markers the pop left behind.
        let after = std::fs::read_to_string(local_path.join("a.txt")).unwrap();
        assert!(
            after.contains("REMOTE MOVED AHEAD"),
            "the remote's version must have landed, got: {after}"
        );
        assert!(
            after.contains("LOCAL UNCOMMITTED WORK"),
            "the local work must still be present, in the conflict: {after}"
        );

        // And the stash holds the work. This is the assertion that proves
        // the flag reached git at all: a build that never passed
        // `--autostash` was refused before stashing anything, so there
        // would be nothing here to find.
        let stashes = run_git(&local_path, &["stash", "list"]);
        assert!(
            stashes.status.success()
                && String::from_utf8_lossy(&stashes.stdout).contains("autostash"),
            "--autostash must stash what it could not pop, so the work is \
             recoverable: {}",
            String::from_utf8_lossy(&stashes.stdout)
        );
    }

    /// The flag has to reach git, not merely bypass ro's own dirty-skip.
    ///
    /// The test above asserts the run succeeded, which is the outcome the
    /// flag exists for — but it cannot distinguish "git was told to
    /// autostash" from "the pull happened to be clean". This one can,
    /// because it reads the tree git was actually handed.
    ///
    /// The remote and the worktree rewrite the **same line of the same
    /// file**, so the pop cannot apply cleanly. A build that never passes
    /// `--autostash` to git is refused before it stashes anything: the
    /// status is an error, the stash list is empty, and the local edit is
    /// still sitting uncommitted in the tree. A build that passes the flag
    /// and then reads the exit code reports success over a tree of conflict
    /// markers. Only a build that passes the flag **and** reads the tree
    /// reports the third thing, which is the truth.
    #[test]
    fn autostash_conflict_is_reported_as_a_failure_naming_the_stash() {
        let (tmp, conn) = setup();
        let remote = init_bare_remote(tmp.path());
        commit_to_remote(tmp.path(), &remote, "a.txt", "base");

        let local_path = tmp.path().join("local").join("proj1");
        std::fs::create_dir_all(&local_path).unwrap();
        run_git(&local_path, &["clone", &remote.to_string_lossy(), "."]);
        run_git(&local_path, &["config", "user.email", "test@example.com"]);
        run_git(&local_path, &["config", "user.name", "Test"]);

        // Both sides rewrite the same line, so the pop must conflict.
        std::fs::write(local_path.join("a.txt"), "LOCAL UNCOMMITTED WORK").unwrap();
        commit_to_remote(tmp.path(), &remote, "a.txt", "REMOTE MOVED AHEAD");

        let repo_id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES (?1, 'github.com', 'alice', 'proj1', ?2, ?3, ?4, ?5)",
            params![
                repo_id,
                remote.to_string_lossy().to_string(),
                local_path.to_string_lossy().to_string(),
                now,
                now
            ],
        )
        .unwrap();

        let results = sync_all(
            &conn,
            &SyncOptions {
                autostash: true,
                ..SyncOptions::default()
            },
            &[],
        )
        .unwrap();

        // Not success. `git pull --autostash` exits 0 here, so a run that
        // read the exit code would file this as a clean sync over a tree
        // holding conflict markers.
        assert_ne!(
            results[0].status, "success",
            "a conflicting autostash pop must not be recorded as a success, \
             got {:?}",
            results[0]
        );
        assert_eq!(
            results[0].status, "autostash_conflict",
            "the failure must be named for what it is, got {:?}",
            results[0]
        );

        // The message names the recovery, because the work is only reachable
        // through the stash and nothing else mentions it.
        let error = results[0]
            .error
            .as_deref()
            .expect("a failure must carry the reason");
        assert!(
            error.contains("git stash list"),
            "the message must name `git stash list`, where the work now lives: {error}"
        );
        assert!(
            error.contains("stash pop"),
            "the message must say how to get the work back: {error}"
        );

        // And the stash really does still hold the work — asserted on git's
        // own list, not on the message.
        let stashes = run_git(&local_path, &["stash", "list"]);
        assert!(
            stashes.status.success()
                && String::from_utf8_lossy(&stashes.stdout).contains("autostash"),
            "the failed pop must keep the stash, so the work is recoverable: {}",
            String::from_utf8_lossy(&stashes.stdout)
        );

        // The tree really does hold conflict markers, which is the state a
        // green sync row would have hidden.
        let after = std::fs::read_to_string(local_path.join("a.txt")).unwrap();
        assert!(
            after.contains("<<<<<<<") && after.contains(">>>>>>>"),
            "the worktree must show the conflict, got: {after}"
        );

        // The audit trail agrees with the verdict, so `ro status` does not
        // report a clean run either.
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sync_results WHERE repo_id = ?1 AND status = 'success'",
                params![repo_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            rows, 0,
            "a conflicting pop must not leave a success row in the audit trail"
        );
    }

    /// Without this, the test above would also pass on a database where
    /// nothing can be inserted at all — the assertion would be true for a
    /// reason that has nothing to do with the run row.
    #[test]
    fn the_recording_test_would_have_caught_the_old_bug() {
        let (tmp, conn) = setup();
        let remote = init_bare_remote(tmp.path());
        commit_to_remote(tmp.path(), &remote, "a.txt", "hello");
        let local_path = tmp.path().join("local").join("proj1");
        let repo_id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES (?1, 'github.com', 'alice', 'proj1', ?2, ?3, ?4, ?5)",
            params![
                repo_id,
                remote.to_string_lossy().to_string(),
                local_path.to_string_lossy().to_string(),
                now,
                now
            ],
        )
        .unwrap();

        sync_all(&conn, &SyncOptions::default(), &[]).unwrap();

        // What the old code did: a bare UUID with no `runs` row behind it.
        let orphan: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sync_results
                 WHERE run_id NOT IN (SELECT id FROM runs)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            orphan, 0,
            "no result may reference a run that does not exist — that is \
             the foreign key the old code violated on every write"
        );
    }

    /// A pull that fails is not a success.
    ///
    /// The two failure shapes are separated deliberately: a conflict is a
    /// stage `ro ship` continues from, and a plain refusal is not. Both
    /// used to land in the `success` row, which is how a fleet could look
    /// healthy while nothing was syncing.
    #[test]
    fn a_failed_pull_is_not_recorded_as_success() {
        let (tmp, conn) = setup();
        let remote = init_bare_remote(tmp.path());
        commit_to_remote(tmp.path(), &remote, "a.txt", "hello");

        // The remote moves on after the local clone diverges.
        let local_path = tmp.path().join("local").join("proj1");
        let repo_id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES (?1, 'github.com', 'alice', 'proj1', ?2, ?3, ?4, ?5)",
            params![
                repo_id,
                remote.to_string_lossy().to_string(),
                local_path.to_string_lossy().to_string(),
                now,
                now
            ],
        )
        .unwrap();

        // First sync clones.
        sync_all(&conn, &SyncOptions::default(), &[]).unwrap();

        // Both sides move, so the branch genuinely diverges and a
        // fast-forward pull has no answer.
        //
        // This used to move only the *local* side and assert the pull was
        // refused. It was refused — because `git pull` was being handed the
        // branch name where the remote goes, so every pull errored with "does
        // not appear to be a git repository". The test was reading a bug as
        // the behaviour it wanted, and passed for the wrong reason.
        //
        // Local ahead on its own is *not* a failed pull: `git pull --ff-only`
        // with a local commit and a remote that has not moved says "Already
        // up to date" and exits 0, correctly. There is nothing to fetch, and
        // an unpushed local commit is not an error.
        commit_to_remote(tmp.path(), &remote, "c.txt", "remote moved on");
        std::fs::write(local_path.join("b.txt"), "local only\n").unwrap();
        run_git(&local_path, &["add", "."]);
        run_git(&local_path, &["commit", "-m", "local commit"]);

        let results = sync_all(
            &conn,
            &SyncOptions {
                strategy: SyncStrategy::FfOnly,
                ..Default::default()
            },
            &[],
        )
        .unwrap();

        assert_ne!(
            results[0].status, "success",
            "a refused fast-forward must not be recorded as a success, got \
             {:?}",
            results[0]
        );
        assert!(
            results[0].error.is_some(),
            "a failure must carry the reason git gave"
        );

        // And nothing in the audit trail says success either.
        let success_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sync_results WHERE status = 'success' AND action = 'updated'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            success_rows, 0,
            "a refused pull must not leave an 'updated/success' row behind"
        );
    }

    #[test]
    fn sync_repo_clones_missing_repo() {
        let (tmp, conn) = setup();
        let remote = init_bare_remote(tmp.path());
        commit_to_remote(tmp.path(), &remote, "a.txt", "hello");

        let local_path = tmp.path().join("local").join("proj1");
        let repo_id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES (?1, 'github.com', 'alice', 'proj1', ?2, ?3, ?4, ?5)",
            params![
                repo_id,
                remote.to_string_lossy().to_string(),
                local_path.to_string_lossy().to_string(),
                now,
                now
            ],
        )
        .unwrap();

        let repo = TrackedRepo {
            id: repo_id,
            host: "github.com".into(),
            owner: "alice".into(),
            name: "proj1".into(),
            branch: None,
            alias: None,
            clone_url: remote.to_string_lossy().to_string(),
            local_path: local_path.to_string_lossy().to_string(),
            visibility: "unknown".into(),
            archived: false,
            disabled: false,
            credential_ref: None,
            author_ref: None,
            engine: None,
            engine_args: None,
        };

        let opts = SyncOptions::default();
        // A run row has to exist first: the write this test exercises is
        // foreign-keyed against `runs`, which is the entire bug ro-rne.10
        // is about. Minting a bare id here would fail on the constraint
        // rather than on anything the test means to check.
        let run = ro_jobs::open_run(&conn, "sync", &[]).unwrap();
        let result = sync_repo(&conn, &repo, &opts, &run.id).unwrap();
        assert_eq!(result.action, "clone");
        assert_eq!(result.status, "success");
        assert!(result.post_oid.is_some());
        assert!(local_path.join(".git").exists());
    }

    #[test]
    fn sync_all_with_empty_list() {
        let (_, conn) = setup();
        let opts = SyncOptions::default();
        let results = sync_all(&conn, &opts, &[]).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn sync_strategy_display() {
        assert_eq!(SyncStrategy::FfOnly.to_string(), "ff-only");
        assert_eq!(SyncStrategy::Rebase.to_string(), "rebase");
        assert_eq!(SyncStrategy::Merge.to_string(), "merge");
    }
}

/// How many uncommitted changes a worktree has.
///
/// `-uall` so an untracked directory is counted as its files: a directory
/// holding three new files is three changes the user could lose, and
/// "skipped: 1 uncommitted change" understates it.
fn dirty_file_count(repo: &Path) -> Result<usize> {
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain", "-z", "-uall"])
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| anyhow::anyhow!("running git status: {e}"))?;
    if !out.status.success() {
        return Ok(0);
    }
    Ok(ro_git::primitives::parse_porcelain(&String::from_utf8_lossy(&out.stdout)).len())
}

#[cfg(test)]
mod filter_and_skip_tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, Connection) {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        (tmp, conn)
    }

    fn track(conn: &Connection, owner: &str, name: &str, archived: bool, disabled: bool) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at,
                                 archived, disabled)
             VALUES (?1, 'github.com', ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8)",
            rusqlite::params![
                format!("id-{owner}-{name}"),
                owner,
                name,
                format!("https://example.com/{owner}/{name}.git"),
                format!("/nonexistent/{owner}/{name}"),
                now,
                archived as i64,
                disabled as i64
            ],
        )
        .unwrap();
    }

    /// Archived and disabled rows are **not** synced.
    ///
    /// `list(conn, None)` returns every row, so without the filter
    /// `ro sync` reached into repos the user had explicitly retired — and
    /// a sync that touches an archived repo is a sync that does something
    /// you asked it not to do.
    #[test]
    fn archived_and_disabled_repos_are_excluded() {
        let (_tmp, conn) = setup();
        track(&conn, "acme", "live", false, false);
        track(&conn, "acme", "archived", true, false);
        track(&conn, "acme", "disabled", false, true);

        // `--pull-only` means "do nothing but touch existing checkouts",
        // which is the cheapest way to observe the selection.
        let opts = SyncOptions {
            pull_only: true,
            ..Default::default()
        };
        let results = sync_all(&conn, &opts, &[]).unwrap();

        assert_eq!(
            results.len(),
            1,
            "only the live repo may be synced, got {:?}",
            results.iter().map(|r| &r.action).collect::<Vec<_>>()
        );
    }

    /// A dirty worktree is **skipped**, with the count named.
    ///
    /// There is no `--force` on this command. A fleet tool that discards
    /// uncommitted work because someone typed a flag while rushing is a
    /// tool that gets uninstalled after the first accident.
    #[test]
    fn a_dirty_worktree_is_skipped_and_says_how_many() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();

        // A real repo on a real remote, so the pull has somewhere to be a
        // no-op rather than a failure.
        let remote = tmp.path().join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        run_git(&remote, &["init", "--bare", "-q", "--initial-branch=main"]);

        let work = tmp.path().join("work");
        run_git(
            tmp.path(),
            &["clone", "-q", &remote.to_string_lossy(), "work"],
        );
        run_git(&work, &["config", "user.email", "t@e.com"]);
        run_git(&work, &["config", "user.name", "T"]);
        run_git(&work, &["commit", "-q", "--allow-empty", "-m", "init"]);
        run_git(&work, &["push", "-q", "-u", "origin", "main"]);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES ('r1', 'github.com', 'acme', 'api', ?1, ?2, ?3, ?3)",
            rusqlite::params![remote.to_string_lossy(), work.to_string_lossy(), now],
        )
        .unwrap();

        // Two uncommitted files.
        std::fs::write(work.join("a.txt"), "one\n").unwrap();
        std::fs::write(work.join("b.txt"), "two\n").unwrap();

        let results = sync_all(&conn, &SyncOptions::default(), &[]).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].action, "skipped_dirty",
            "a dirty worktree must be skipped, got {:?}",
            results[0]
        );
        assert_eq!(results[0].status, "skipped");

        // And nothing was written.
        let porcelain = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&work)
                .output()
                .unwrap()
                .stdout,
        )
        .into_owned();
        assert!(
            porcelain.contains("a.txt") && porcelain.contains("b.txt"),
            "the uncommitted work must still be there, got: {porcelain}"
        );
    }

    /// A clean worktree is not skipped — the guard must not become a
    /// blanket "never pull".
    #[test]
    fn a_clean_worktree_is_not_skipped() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let remote = tmp.path().join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        run_git(&remote, &["init", "--bare", "-q", "--initial-branch=main"]);
        let work = tmp.path().join("work");
        run_git(
            tmp.path(),
            &["clone", "-q", &remote.to_string_lossy(), "work"],
        );
        run_git(&work, &["config", "user.email", "t@e.com"]);
        run_git(&work, &["config", "user.name", "T"]);
        run_git(&work, &["commit", "-q", "--allow-empty", "-m", "init"]);
        run_git(&work, &["push", "-q", "-u", "origin", "main"]);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES ('r1', 'github.com', 'acme', 'api', ?1, ?2, ?3, ?3)",
            rusqlite::params![remote.to_string_lossy(), work.to_string_lossy(), now],
        )
        .unwrap();

        let results = sync_all(&conn, &SyncOptions::default(), &[]).unwrap();
        assert_ne!(
            results[0].action, "skipped_dirty",
            "a clean worktree must not be skipped, got {:?}",
            results[0]
        );
    }

    /// Run git, with an identity and no signing forced on the child.
    ///
    /// Both are set here rather than left to the machine. A developer's
    /// global `~/.gitconfig` supplies them locally, so a test that commits
    /// passes on a workstation and fails on a CI runner with no identity
    /// configured — "Author identity unknown" on one machine and green on
    /// another, for the same commit.
    /// Run git without depending on the machine's own configuration.
    ///
    /// A developer has `user.email` in their `~/.gitconfig`; a CI
    /// runner does not, so a fixture that commits is green on one and
    /// "Author identity unknown" on the other. The identity comes from
    /// the environment git documents for exactly this, and the config
    /// files are neutralised — `NUL` on Windows, `/dev/null` elsewhere.
    fn run_git(dir: &Path, args: &[&str]) -> std::process::Output {
        let mut cmd = std::process::Command::new("git");
        cmd.args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com");
        #[cfg(windows)]
        cmd.env("GIT_CONFIG_GLOBAL", "NUL")
            .env("GIT_CONFIG_SYSTEM", "NUL");
        #[cfg(not(windows))]
        cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        cmd.output().expect("git runs")
    }
}

/// A branch that was never pushed is a skip, not a failure.
///
/// `git checkout -b` is the normal way a branch starts, so "no remote
/// branch yet" is the state of most repos an hour after they were created.
/// Recording it as a sync error fills a fleet board with red for repos that
/// are exactly as they should be, and the message a user sees is git's
/// ("couldn't find remote ref") rather than anything they can act on.
#[cfg(test)]
mod unpushed_branch_tests {
    use super::*;

    #[test]
    fn an_unpushed_branch_is_skipped_rather_than_recorded_as_an_error() {
        let (tmp, conn) = super::tests::setup();
        let remote = super::tests::init_bare_remote(tmp.path());
        super::tests::commit_to_remote(tmp.path(), &remote, "a.txt", "hello");
        let local_path = tmp.path().join("local").join("proj1");
        let repo_id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES (?1, 'github.com', 'alice', 'proj1', ?2, ?3, ?4, ?5)",
            params![
                repo_id,
                remote.to_string_lossy().to_string(),
                local_path.to_string_lossy().to_string(),
                now,
                now
            ],
        )
        .unwrap();
        sync_all(&conn, &SyncOptions::default(), &[]).unwrap();

        // A branch made locally and never pushed — the state right after
        // `git checkout -b`.
        super::tests::run_git(&local_path, &["checkout", "-b", "fresh"]);

        let results = sync_all(&conn, &SyncOptions::default(), &[]).unwrap();
        let r = &results[0];
        assert_eq!(
            r.status, "skipped",
            "a branch with no remote yet must not be an error, got {r:?}"
        );
        assert_eq!(r.action, "skipped_unpushed");
        assert!(r.error.is_none(), "a skip carries no error: {r:?}");
        // And the reason reaches a reader. It used to live only in the
        // `sync_results` row, so the word `skipped_unpushed` was the whole
        // answer a plain `ro sync` gave — and it reads as something went
        // wrong, when the branch is simply new.
        let reason = r
            .reason
            .as_deref()
            .unwrap_or_else(|| panic!("no reason reached the reader: {r:?}"));
        assert!(
            reason.contains("never been pushed"),
            "the reason must say the branch is new, not that something failed; \
             got {reason:?}"
        );

        // And it must not be counted as a failed sync in the table either.
        let failed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sync_results WHERE repo_id = ?1 AND status = 'error'",
                params![repo_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(failed, 0, "a never-pushed branch is not a sync failure");
    }
}

/// ── The dry run as a preview ──
///
/// A dry run that cannot say whether there is anything waiting is not a
/// preview. `ro sync --dry-run` computed one bit per repo — does `.git`
/// exist — and printed `would_pull` for every checkout on the machine, so a
/// repo with three commits waiting and a repo already in line produced
/// byte-identical rows. FEATURES.md sells the flag as "show what would
/// happen"; what it showed was what command would be typed next.
///
/// The tests below drive the real code over real bare remotes, and the one
/// that matters most asserts the property the flag is *for*: a dry run that
/// says a repo is fine must not be followed by a real run that refuses it.
#[cfg(test)]
mod dry_run_preview {
    use super::tests::{commit_to_remote, init_bare_remote, run_git};
    use super::{SyncOptions, SyncResult, plans_match, run_exit_code, sync_all};
    use rusqlite::{Connection, params};
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// A fleet of three tracked repos over one bare remote, wired to real
    /// working copies:
    ///
    ///  * `behind` — three commits pushed by somebody else, not fetched
    ///  * `clean`  — exactly in line
    ///  * `dirty`  — in line, with uncommitted work in the tree
    ///  * `ahead`  — in line, with a commit that was never pushed
    ///
    /// The four are the four answers the old dry run could not give.
    struct Fleet {
        _tmp: TempDir,
        conn: Connection,
        remote: PathBuf,
        /// repo_id -> local path
        paths: Vec<(String, PathBuf)>,
    }

    fn fleet() -> Fleet {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let remote = init_bare_remote(tmp.path());
        commit_to_remote(tmp.path(), &remote, "a.txt", "base");

        let mut paths = Vec::new();
        for name in ["behind", "clean", "dirty", "diverged"] {
            let local = tmp.path().join("local").join(name);
            std::fs::create_dir_all(&local).unwrap();
            let clone = run_git(&local, &["clone", &remote.to_string_lossy(), "."]);
            assert!(
                clone.status.success(),
                "the fixture clone failed:\n{}\n{}",
                String::from_utf8_lossy(&clone.stdout),
                String::from_utf8_lossy(&clone.stderr)
            );
            run_git(&local, &["config", "user.email", "test@example.com"]);
            run_git(&local, &["config", "user.name", "Test"]);

            let repo_id = uuid::Uuid::new_v4().to_string();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            conn.execute(
                "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
                 VALUES (?1, 'github.com', 'fleet', ?2, ?3, ?4, ?5, ?5)",
                params![
                    repo_id,
                    name,
                    remote.to_string_lossy().to_string(),
                    local.to_string_lossy().to_string(),
                    now
                ],
            )
            .unwrap();
            paths.push((repo_id, local));
        }

        let f = Fleet {
            _tmp: tmp,
            conn,
            remote,
            paths,
        };

        // The remote moves on, three commits, that nobody has fetched.
        for i in 0..3 {
            commit_to_remote(
                f._tmp.path(),
                &f.remote,
                &format!("b{i}.txt"),
                &format!("remote moved {i}"),
            );
        }

        // `behind` fetches; the others do not. The fetch is the whole point:
        // `behind` is measured against the **local** remote-tracking ref, so
        // a checkout that has never fetched reports `behind=0` however far
        // the remote has moved. That is the staleness this whole change is
        // about, and a fixture that forgot it would assert that a repo
        // three commits behind reads as in line.
        run_git(&f.path_of("behind"), &["fetch", "-q", "origin"]);
        // `diverged` fetches too, and then commits — one ahead *and* three
        // behind, which `--ff-only` refuses outright. It is the case a dry
        // run can predict from local state alone, and predicting it is the
        // difference between a preview and a guess.
        run_git(&f.path_of("diverged"), &["fetch", "-q", "origin"]);

        // And the local states the old dry run could not tell apart.
        std::fs::write(f.path_of("dirty").join("a.txt"), "LOCAL UNCOMMITTED\n").unwrap();
        {
            // Fetched, then one local commit on top: one ahead **and** three
            // behind. `--ff-only` refuses that outright, so it is the case a
            // dry run can predict from local state alone and the real run
            // cannot reach any other answer.
            let diverged = f.path_of("diverged");
            std::fs::write(diverged.join("local.txt"), "mine\n").unwrap();
            run_git(&diverged, &["add", "."]);
            run_git(&diverged, &["commit", "-m", "local only"]);
        }
        f
    }

    /// A fleet where the remote has moved on and **nobody** has fetched.
    ///
    /// The state `ro status` is in during the documented daily loop, and the
    /// state a dry run is asked about: the local remote-tracking refs are
    /// stale, so every count is measured against a base that is behind the
    /// remote. The dry run has to say what it measured against rather than
    /// reporting the stale numbers as if they were current.
    fn stale_fleet() -> Fleet {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let remote = init_bare_remote(tmp.path());
        commit_to_remote(tmp.path(), &remote, "a.txt", "base");

        let mut paths = Vec::new();
        for name in ["behind", "clean", "dirty", "diverged"] {
            let local = tmp.path().join("local").join(name);
            std::fs::create_dir_all(&local).unwrap();
            let clone = run_git(&local, &["clone", &remote.to_string_lossy(), "."]);
            assert!(
                clone.status.success(),
                "the fixture clone failed:\n{}\n{}",
                String::from_utf8_lossy(&clone.stdout),
                String::from_utf8_lossy(&clone.stderr)
            );
            run_git(&local, &["config", "user.email", "test@example.com"]);
            run_git(&local, &["config", "user.name", "Test"]);

            let repo_id = uuid::Uuid::new_v4().to_string();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            conn.execute(
                "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
                 VALUES (?1, 'github.com', 'fleet', ?2, ?3, ?4, ?5, ?5)",
                params![
                    repo_id,
                    name,
                    remote.to_string_lossy().to_string(),
                    local.to_string_lossy().to_string(),
                    now
                ],
            )
            .unwrap();
            paths.push((repo_id, local));
        }

        // The remote moves on, three commits, and **no checkout fetches**.
        // That is the whole point of this fixture: the local refs are stale,
        // so `behind` reads as in line with a ref that is three commits
        // behind the remote.
        for i in 0..3 {
            commit_to_remote(
                tmp.path(),
                &remote,
                &format!("b{i}.txt"),
                &format!("remote moved {i}"),
            );
        }

        Fleet {
            _tmp: tmp,
            conn,
            remote,
            paths,
        }
    }

    impl Fleet {
        fn path_of(&self, name: &str) -> PathBuf {
            self.paths
                .iter()
                .find(|(_, p)| p.file_name().unwrap() == name)
                .map(|(_, p)| p.clone())
                .unwrap_or_else(|| panic!("no fixture repo called {name}"))
        }

        fn by_name(&self, results: &[SyncResult], name: &str) -> SyncResult {
            let id = self
                .paths
                .iter()
                .find(|(_, p)| p.file_name().unwrap() == name)
                .map(|(id, _)| id.clone())
                .unwrap();
            results
                .iter()
                .find(|r| r.repo_id == id)
                .unwrap_or_else(|| panic!("no result row for {name}"))
                .clone()
        }
    }

    /// The four checkouts, told apart.
    ///
    /// Red-first: before this, all four rows carried `action=would_pull` and
    /// a `None` reason, and no row carried a number at all. The assertion is
    /// on the *number*, because that is what a dry run is asked for and
    /// because a status word alone (`would_pull`) was the bug.
    #[test]
    fn a_dry_run_says_how_far_behind_ahead_or_dirty_each_repo_is() {
        let f = fleet();
        let dry = sync_all(
            &f.conn,
            &SyncOptions {
                dry_run: true,
                ..Default::default()
            },
            &[],
        )
        .unwrap();

        let behind = f.by_name(&dry, "behind");
        assert_eq!(
            behind.behind,
            Some(3),
            "a repo three commits behind must say three, not 'would_pull' with \
             no number on it: {behind:?}"
        );
        assert_eq!(behind.ahead, Some(0));

        let clean = f.by_name(&dry, "clean");
        assert_eq!(
            (clean.ahead, clean.behind),
            (Some(0), Some(0)),
            "a repo in line with the remote must say so: {clean:?}"
        );

        let dirty = f.by_name(&dry, "dirty");
        assert_eq!(
            dirty.action, "skipped_dirty",
            "a dirty repo is skipped, and the dry run must say it will be: {dirty:?}"
        );
        let dirty_reason = dirty.error.as_deref().unwrap_or_default();
        assert!(
            dirty_reason.contains("1 uncommitted change"),
            "the count is the information the user needs to decide about \
             --autostash, and it was absent: {dirty:?}"
        );

        // Ahead *and* behind is the case the old dry run reported as a
        // plain `would_pull`. A pull cannot fast-forward it, so the real run
        // fails and the dry run has to say so.
        let diverged_ahead = f.by_name(&dry, "diverged");
        assert_eq!(diverged_ahead.ahead, Some(1), "{diverged_ahead:?}");
        assert_eq!(diverged_ahead.behind, Some(3), "{diverged_ahead:?}");

        // Diverged under `--ff-only` is a pull git refuses. The dry run has
        // to say the run will fail, because a dry run that promises a pull
        // and a real run that exits 1 is the one thing a dry run must never
        // do — and the old implementation did it for every repo, since it
        // had no notion of a repo that would fail at all.
        let diverged = f.by_name(&dry, "diverged");
        assert_eq!(
            diverged.status, "error",
            "a diverged repo under --ff-only fails the pull, and the dry run \
             must say so: {diverged:?}"
        );
        let diverged_reason = diverged.error.as_deref().unwrap_or_default();
        assert!(
            diverged_reason.contains("will fail"),
            "the row must say the pull will fail, not describe a sync that \
             will not happen: {diverged:?}"
        );
    }

    /// The promise the flag is for.
    ///
    /// A dry run that says everything is fine and a real run that then skips
    /// three repos is the one thing a dry run must never do, and the old
    /// implementation did it by construction: every row was
    /// `status = "dry_run"`, `dry_run` never fails a run, and the dirty repos
    /// the real run skipped never appeared in either the rows or the exit
    /// code.
    ///
    /// The old code passed this test by accident — both verdicts were 0 — so
    /// the assertion is on the **per-repo** agreement, not only the run
    /// exit code, plus on the dirty repo's action matching the real run's.
    #[test]
    fn the_dry_runs_verdict_is_the_real_runs_verdict_for_every_repo() {
        let f = fleet();
        let opts = SyncOptions {
            dry_run: true,
            ..Default::default()
        };
        let dry = sync_all(&f.conn, &opts, &[]).unwrap();

        let real_opts = SyncOptions::default();
        let real = sync_all(&f.conn, &real_opts, &[]).unwrap();

        assert_eq!(
            dry.len(),
            real.len(),
            "the two runs must cover the same fleet, or the comparison below \
             is comparing different things"
        );
        for (d, r) in dry.iter().zip(real.iter()) {
            assert!(
                plans_match(d, r),
                "the dry run and the real run disagree about {}:\n  dry  = {d:?}\n  real = {r:?}",
                d.repo_id
            );
        }

        // Spelled out, because `plans_match` agreeing on two skips that
        // skipped for different reasons is still a bug.
        let dirty_real = f.by_name(&real, "dirty");
        assert_eq!(
            dirty_real.action, "skipped_dirty",
            "the real run's skip must be the one the dry run predicted"
        );
        assert_eq!(run_exit_code(&dry), run_exit_code(&real));

        // And the dirty repo must still be visible in the dry run at all —
        // a dry run that hides the repos it is about to skip has the same
        // defect as one that calls them clean.
        assert!(
            dry.iter().any(|r| r.action == "skipped_dirty"),
            "the dry run must name the repo the real run will skip"
        );
    }

    /// A repo with unpushed work must be told that sync will not push it.
    ///
    /// The old dry run said `would_pull` for this repo, which reads as a
    /// promise the run will not keep: `ro sync` pulls and never pushes, so
    /// the local commit is still there afterwards. The row has to say what
    /// the run actually does, which is nothing.
    ///
    /// The fixture leaves the remote-tracking ref where the clone put it,
    /// because that is the only way to reach "ahead and not behind" against
    /// a remote that has also moved. The assertion is on the **wording**,
    /// not on the verdict: with a ref that old the real run's fetch would
    /// find the repo diverged, and a dry run that cannot see the remote
    /// cannot promise which. That limitation is what the staleness note in
    /// the next test is for.
    #[test]
    fn an_ahead_repo_is_told_sync_will_not_push() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let remote = init_bare_remote(tmp.path());
        commit_to_remote(tmp.path(), &remote, "a.txt", "base");

        let local = tmp.path().join("local").join("ahead");
        std::fs::create_dir_all(&local).unwrap();
        let clone = run_git(&local, &["clone", &remote.to_string_lossy(), "."]);
        assert!(clone.status.success());
        run_git(&local, &["config", "user.email", "test@example.com"]);
        run_git(&local, &["config", "user.name", "Test"]);

        // One local commit, never pushed.
        std::fs::write(local.join("local.txt"), "mine\n").unwrap();
        run_git(&local, &["add", "."]);
        run_git(&local, &["commit", "-m", "local only"]);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES ('id-ahead', 'github.com', 'fleet', 'ahead', ?1, ?2, ?3, ?3)",
            params![
                remote.to_string_lossy().to_string(),
                local.to_string_lossy().to_string(),
                now
            ],
        )
        .unwrap();

        let dry = sync_all(
            &conn,
            &SyncOptions {
                dry_run: true,
                ..Default::default()
            },
            &[],
        )
        .unwrap();
        let row = &dry[0];
        assert_eq!(
            (row.ahead, row.behind),
            (Some(1), Some(0)),
            "one local commit, not pushed: {row:?}"
        );
        let reason = row.error.as_deref().unwrap_or_default();
        assert!(
            reason.contains("does not push"),
            "a dry run must not read as a promise to push; sync pulls and \
             never pushes, and the user has to know that: {row:?}"
        );
    }

    /// A dry run that cannot see the remote says what it measured against.
    ///
    /// `behind` is measured against the **local** remote-tracking ref, so a
    /// checkout that has not fetched reports `behind=0` however far the
    /// remote has moved. That is not a bug in the count — it is what the
    /// count means — and the row has to say so rather than let a stale zero
    /// read as "in line". This is the same staleness `ro status` has, and
    /// the same answer: name the ref, and the age of the ref.
    #[test]
    fn a_dry_run_over_a_stale_ref_names_the_ref_it_measured_against() {
        let f = stale_fleet();
        let dry = sync_all(
            &f.conn,
            &SyncOptions {
                dry_run: true,
                ..Default::default()
            },
            &[],
        )
        .unwrap();

        // The remote moved three commits and this checkout never fetched, so
        // the local ref says zero — which is true, and useless on its own.
        let row = f.by_name(&dry, "behind");
        assert_eq!(
            row.behind,
            Some(0),
            "measured against a ref that has not moved, the repo is not \
             behind: {row:?}"
        );

        // What makes that readable is that the row is a preview of a run
        // that fetches first, and it names the ref the number came from.
        let reason = row.error.as_deref().unwrap_or_default();
        assert!(
            reason.contains("origin/main"),
            "the plan must name the ref the counts were measured against, or \
             a `behind=0` over a stale ref is indistinguishable from a \
             repo that is genuinely in line: {row:?}"
        );
    }

    /// The headline, as a test: a dry run that says "in line" over a ref
    /// that has not been fetched is a dry run that is **wrong**, and the real
    /// run then pulls three commits anyway.
    ///
    /// This is the case the brief reproduces across `behind=1` and `behind=3`,
    /// clean and dirty, with `--autostash` and plain: the remote moved, the
    /// checkout's `origin/main` never did, so the ahead/behind comparison —
    /// which is correct about the ref and silent about its age — reads zero
    /// and the plan declares victory. The real run fetches, sees three
    /// commits, and pulls. The user trusted the dry run precisely because it
    /// was wrong.
    ///
    /// A dry run cannot fix this by fetching, because fetching is a real run.
    /// The only honest move is to qualify the claim with the age of the ref
    /// it was measured against, which is what the plan now does. The
    /// assertions:
    ///
    ///  1. the dry run's plan **carries the staleness caveat** — it says
    ///     "in line with a ref last fetched N ago", not a bare "in line";
    ///  2. the real run then **actually updates** the repo, proving the dry
    ///     run was looking at a stale ref and the two views genuinely differ;
    ///  3. the two still **agree on the verdict** (`plans_match`), because a
    ///     repo that is merely behind is not a failure either way — the dry
    ///     run's job is to stop the *lie*, not to invent a red.
    #[test]
    fn a_dry_run_never_bares_an_in_line_verdict_over_a_stale_ref() {
        let f = stale_fleet();
        let dry = sync_all(
            &f.conn,
            &SyncOptions {
                dry_run: true,
                ..Default::default()
            },
            &[],
        )
        .unwrap();

        let plan = f
            .by_name(&dry, "behind")
            .plan
            .as_deref()
            .expect("a dry-run row must carry its plan")
            .to_string();

        // The honesty marker. Without the caveat, this reads as a fact about
        // the remote; with it, it reads as a fact about a local ref of known
        // (or unknown) age. This is the assertion that fails on the old
        // implementation.
        assert!(
            plan.contains("A dry run does not fetch"),
            "a dry run that measured a local ref must say the measurement is \
             only as fresh as that ref. Got: {plan}"
        );
        // And it must not be the bare, unqualified claim the bug produced.
        assert!(
            !plan.ends_with("is in line with origin/main"),
            "the plan must not end on a bare 'in line' — it is a claim about \
             a ref that may be weeks old, and needs qualifying: {plan}"
        );

        // The real run genuinely disagrees about the *content* — it fetches,
        // finds the three commits, and updates. This is the fact the dry run
        // was measuring against a stale base for.
        let real = sync_all(&f.conn, &SyncOptions::default(), &[]).unwrap();
        let real_row = f.by_name(&real, "behind");
        assert!(
            matches!(real_row.action.as_str(), "updated" | "already_up_to_date"),
            "the real run must actually see the three commits the dry run \
             could not: {real_row:?}"
        );

        // ...but they agree on whether the run passes, which is the property
        // the dry run exists to protect.
        let dry_row = f.by_name(&dry, "behind");
        assert!(
            plans_match(&dry_row, &real_row),
            "the two must agree on the verdict even as they differ on the \
             stale-ref detail:\n  dry  = {dry_row:?}\n  real = {real_row:?}"
        );
    }

    /// `--pull-only` and `--clone-only` are two different answers for a repo
    /// that does not exist yet, and the old dry run gave both the same one
    /// (`would_clone`).
    #[test]
    fn a_dry_run_honours_pull_only_and_clone_only() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let remote = init_bare_remote(tmp.path());
        commit_to_remote(tmp.path(), &remote, "a.txt", "hello");

        // A tracked repo with no checkout, and one with a checkout.
        let missing = tmp.path().join("local").join("missing");
        let present = tmp.path().join("local").join("present");
        std::fs::create_dir_all(&present).unwrap();
        let clone = run_git(&present, &["clone", &remote.to_string_lossy(), "."]);
        assert!(clone.status.success());

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        for (name, path) in [("missing", &missing), ("present", &present)] {
            conn.execute(
                "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
                 VALUES (?1, 'github.com', 'fleet', ?2, ?3, ?4, ?5, ?5)",
                params![
                    format!("id-{name}"),
                    name,
                    remote.to_string_lossy().to_string(),
                    path.to_string_lossy().to_string(),
                    now
                ],
            )
            .unwrap();
        }

        let dry = sync_all(
            &conn,
            &SyncOptions {
                dry_run: true,
                pull_only: true,
                ..Default::default()
            },
            &[],
        )
        .unwrap();
        let skipped = dry.iter().find(|r| r.repo_id == "id-missing").unwrap();
        assert_eq!(
            skipped.action, "skipped_clone",
            "--pull-only does not clone, and the dry run has to say so: {skipped:?}"
        );
        assert_eq!(skipped.status, "skipped");
        // The real run must agree, or the run-level verdicts diverge.
        let real = sync_all(
            &conn,
            &SyncOptions {
                pull_only: true,
                ..Default::default()
            },
            &[],
        )
        .unwrap();
        let real_missing = real.iter().find(|r| r.repo_id == "id-missing").unwrap();
        assert!(
            plans_match(skipped, real_missing),
            "dry={skipped:?}\nreal={real_missing:?}"
        );

        let dry = sync_all(
            &conn,
            &SyncOptions {
                dry_run: true,
                clone_only: true,
                ..Default::default()
            },
            &[],
        )
        .unwrap();
        let present_row = dry.iter().find(|r| r.repo_id == "id-present").unwrap();
        assert_eq!(
            present_row.action, "skipped_pull",
            "--clone-only does not pull an existing checkout: {present_row:?}"
        );
    }

    /// A repo the real run will refuse must not read as clean in the dry run.
    ///
    /// This is the "dry run says fine, real run refuses" case at its most
    /// local: a repo with **no `origin` remote** cannot be fetched, and the
    /// real run records an error for it. A dry run that answered
    /// `would_pull` there was promising a fetch from a remote that does not
    /// exist, and the run-level exit codes disagreed (0 versus 1).
    /// A repo whose ahead/behind **cannot be measured** must not be
    /// reported as "in line".
    ///
    /// `ahead_behind` failed, so `(None, None)` arrived at the match and
    /// fell into the same `_` arm as a genuinely measured `(0, 0)` —
    /// printing "nothing to do — main is in line with origin/main" for a
    /// checkout git cannot compare against anything. The truth was carried
    /// only in `unmeasurable_reason` and a staleness parenthetical while the
    /// headline sentence still claimed a measurement nobody made.
    ///
    /// This is the one lie a dry run must never tell, and it is reachable
    /// without anything exotic: a clone killed by `--timeout` leaves a
    /// `.git` with zero objects and `HEAD=refs/heads/.invalid`, which
    /// `.git exists()` accepts, and which measures `(None, None)` on every
    /// subsequent run — even after the remote is healthy again.
    #[test]
    fn an_unmeasurable_repo_is_not_reported_as_in_line() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        // A checkout git can **read the config of** and cannot measure:
        // `origin` is configured, so `has_remote` answers yes and the plan
        // gets as far as the comparison — but there are no objects and
        // `HEAD` names a ref that does not exist, so `rev-list` cannot run.
        // This is the shape a clone killed by `--timeout` leaves behind: a
        // `.git` directory, zero objects, `HEAD=refs/heads/.invalid`, and
        // every "is this cloned?" check satisfied by the directory's
        // existence.
        // A checkout git **can** read the config of and **cannot** measure:
        // `origin` is configured, so `has_remote` answers yes and the plan
        // gets as far as the comparison — but the object store is empty and
        // `HEAD` names a ref that does not exist, so `rev-list` cannot run.
        //
        // This is the shape a clone killed by `--timeout` leaves behind: a
        // `.git` directory, zero objects, `HEAD=refs/heads/.invalid`, and
        // every "is this cloned?" check satisfied by the directory being
        // there. It is built with real git and then damaged, because a
        // hand-written `.git` that git refuses to recognise is refused
        // earlier, at the remotes read, and proves nothing about the arm
        // this is here for.
        let local = tmp.path().join("local").join("broken");
        std::fs::create_dir_all(&local).unwrap();
        run_git(&local, &["init", "-q", "-b", "main"]);
        run_git(&local, &["config", "user.email", "test@example.com"]);
        run_git(&local, &["config", "user.name", "Test"]);
        run_git(
            &local,
            &["remote", "add", "origin", "https://example.com/x.git"],
        );
        // The damage, and the order matters. Empty the object store and
        // point HEAD at a ref that does not exist, **before** anything is
        // committed: with a tracked file in the index the broken object
        // store reports it as changed, and the `skipped_dirty` guard fires
        // before the comparison is ever reached. An empty worktree has
        // nothing to report as changed, so the plan gets to the one
        // question this test is about.
        //
        // This is the shape a clone killed by `--timeout` leaves behind: a
        // `.git` directory, zero objects, `HEAD=refs/heads/.invalid`, and
        // every "is this cloned?" check satisfied by the directory being
        // there. `git remote` still answers — the config is readable — and
        // `git rev-list` cannot run.
        std::fs::remove_dir_all(local.join(".git").join("objects")).unwrap();
        std::fs::create_dir_all(local.join(".git").join("objects")).unwrap();
        std::fs::write(
            local.join(".git").join("HEAD"),
            "ref: refs/heads/.invalid\n",
        )
        .unwrap();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES ('id-1', 'github.com', 'fleet', 'broken', 'https://example.com/x.git', ?1, ?2, ?2)",
            params![local.to_string_lossy().to_string(), now],
        )
        .unwrap();

        let dry = sync_all(
            &conn,
            &SyncOptions {
                dry_run: true,
                ..Default::default()
            },
            &[],
        )
        .expect("a dry run over an unmeasurable repo is a report, not a failure");
        assert_eq!(dry.len(), 1, "the row is still reported");
        let row = &dry[0];
        // The verdict itself is the thing under test: "nothing to do" is
        // what the plan said for a repo it could not measure, and it is the
        // word a user reads and acts on. The rest of the sentence is allowed
        // to name the ref, because the honest form has to say which
        // comparison failed.
        assert!(
            !row.plan.as_deref().unwrap_or("").contains("nothing to do"),
            "the plan must not report a repo it could not measure as one with \
             nothing to do, got: {:?}",
            row.plan
        );
        assert_eq!(
            row.status, "error",
            "and the run must be marked as something the user has to look at"
        );
        assert!(
            row.unmeasurable_reason.is_some(),
            "with the reason carried on the row, not folded into a headline"
        );
    }

    /// The run row records what the run *was*.
    ///
    /// `sync_all` opened its `runs` row with an empty args slice, so every
    /// sync — dry or real, any `--timeout`, any strategy — wrote
    /// `args_json = '[]'` and a dry run and a real run were byte-identical in
    /// the audit trail. The per-repo rows still differ, but the run-level
    /// record could not say which kind of run it was, or what deadline was
    /// in force.
    #[test]
    fn the_run_row_records_the_flags_the_run_used() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let local = tmp.path().join("local").join("clean");
        std::fs::create_dir_all(&local).unwrap();
        run_git(&local, &["init", "-q", "-b", "main"]);
        run_git(&local, &["config", "user.email", "test@example.com"]);
        run_git(&local, &["config", "user.name", "Test"]);
        std::fs::write(local.join("a.txt"), "hello\n").unwrap();
        run_git(&local, &["add", "."]);
        run_git(&local, &["commit", "-q", "-m", "one"]);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES ('id-1', 'github.com', 'fleet', 'clean', 'https://example.com/x.git', ?1, ?2, ?2)",
            params![local.to_string_lossy().to_string(), now],
        )
        .unwrap();

        let real = sync_all(
            &conn,
            &SyncOptions {
                dry_run: false,
                timeout_secs: 7,
                ..Default::default()
            },
            &[],
        )
        .expect("the real run records");

        // The most recent run for the fleet verb, in start order.
        let latest_args = || -> String {
            conn.query_row(
                "SELECT args_json FROM runs WHERE command = 'sync' \
                 ORDER BY started_at DESC, rowid DESC LIMIT 1",
                [],
                |r| r.get::<_, String>(0),
            )
            .unwrap()
        };
        let real_args = latest_args();
        assert_ne!(real.len(), 0, "the real run recorded rows");
        assert!(
            !real_args.contains("dry_run"),
            "a real run must not be recorded as a dry one, got: {real_args}"
        );
        let dry = sync_all(
            &conn,
            &SyncOptions {
                dry_run: true,
                timeout_secs: 7,
                ..Default::default()
            },
            &[],
        )
        .expect("the dry run records");
        let dry_args = latest_args();
        assert!(
            dry_args.contains("dry_run"),
            "the run row must say this was a dry run, got: {dry_args}"
        );
        assert!(
            dry_args.contains("timeout_secs=7"),
            "the deadline in force is part of what a run was, got: {dry_args}"
        );
        assert_ne!(
            dry_args, real_args,
            "a dry run and a real run are different runs and must not be \
             indistinguishable in the audit trail"
        );
    }

    /// A repo the real run cannot fetch must not read as a clean pull.
    ///
    /// This lost its `#[test]` in an edit and sat here complete and
    /// unrun — a test that does not run is not a test, and the suite was
    /// green over it. Restored rather than deleted: the body is the case
    /// where the real run records an error and the dry run must not answer
    /// `would_pull`, which is a promise about a remote that does not exist.
    #[test]
    fn a_repo_the_real_run_cannot_fetch_is_not_reported_as_a_clean_pull() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let local = tmp.path().join("local").join("remoteless");
        std::fs::create_dir_all(&local).unwrap();
        run_git(&local, &["init", "-q", "-b", "main"]);
        run_git(&local, &["config", "user.email", "test@example.com"]);
        run_git(&local, &["config", "user.name", "Test"]);
        std::fs::write(local.join("a.txt"), "hello\n").unwrap();
        run_git(&local, &["add", "."]);
        run_git(&local, &["commit", "-q", "-m", "one"]);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES ('id-1', 'github.com', 'fleet', 'remoteless', ?1, ?2, ?3, ?3)",
            params![
                local.to_string_lossy().to_string(),
                local.to_string_lossy().to_string(),
                now
            ],
        )
        .unwrap();

        let dry = sync_all(
            &conn,
            &SyncOptions {
                dry_run: true,
                ..Default::default()
            },
            &[],
        )
        .unwrap();
        let row = &dry[0];
        assert_ne!(
            row.status, "dry_run",
            "a repo with no origin remote cannot be pulled, so the dry run \
             must not report the ordinary clean case: {row:?}"
        );
        let reason = row.error.as_deref().unwrap_or_default();
        assert!(
            reason.contains("origin"),
            "the row must name what is missing, so the fix is obvious: {row:?}"
        );

        let real = sync_all(&conn, &SyncOptions::default(), &[]).unwrap();
        let real_row = &real[0];
        assert!(
            plans_match(row, real_row),
            "the dry run and the real run must agree on a repo that cannot be \
             fetched:\n  dry  = {row:?}\n  real = {real_row:?}"
        );
        assert_eq!(run_exit_code(&dry), run_exit_code(&real));
    }

    /// A dry run writes rows, so its verdict is readable by a script.
    #[test]
    fn a_dry_run_records_its_verdict_in_the_run() {
        let f = fleet();
        let dry = sync_all(
            &f.conn,
            &SyncOptions {
                dry_run: true,
                ..Default::default()
            },
            &[],
        )
        .unwrap();
        let recorded: i64 = f
            .conn
            .query_row("SELECT COUNT(*) FROM sync_results", [], |r| r.get(0))
            .unwrap();
        assert_eq!(recorded, dry.len() as i64);
        let runs: i64 = f
            .conn
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE command = 'sync' AND ended_at IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(runs, 1, "a dry run is still a run, and it is finalised");
    }

    /// The numbers travel in the machine formats.
    ///
    /// `ro sync --format json` serialises `SyncResult` whole, so a field
    /// that is not on it is a field no script ever sees.
    #[test]
    fn the_preview_numbers_reach_json() {
        let f = fleet();
        let dry = sync_all(
            &f.conn,
            &SyncOptions {
                dry_run: true,
                ..Default::default()
            },
            &[],
        )
        .unwrap();
        let behind = f.by_name(&dry, "behind");
        let row: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&behind).unwrap()).unwrap();
        assert_eq!(row["behind"], serde_json::Value::from(3));
        assert_eq!(row["ahead"], serde_json::Value::from(0));
        assert!(
            row["error"].is_string(),
            "the reason must reach json too: {row}"
        );
        // And the documented field for it is populated, not left `None`
        // while the text rides in `error`. A consumer reading `plan` — the
        // field whose doc comment says it *is* the dry run's whole output —
        // used to get null on every row.
        assert!(
            row["plan"].is_string(),
            "`plan` is the documented field for the dry run's answer and it \
             was null on every row: {row}"
        );
        assert_eq!(
            row["plan"], row["error"],
            "the plan and the reason must be the same sentence, so a reader \
             of either field gets the whole answer: {row}"
        );
    }

    /// `plans_match` is reachable from the binary, not only from a test.
    ///
    /// Pinned by reading the source rather than by behaviour, and deliberately
    /// so. The behavioural pin is in `wedged_remote` — it observes a
    /// `plan_mismatch` actually being recorded, which only happens if
    /// production code called the function. This one is the belt to that
    /// braces: if a future refactor moves the prediction behind a flag, or
    /// deletes the call and leaves the function, the behavioural test still
    /// catches it, and if a rename breaks the wiring this catches it before
    /// the behavioural one has to.
    ///
    /// Checked against the **production** text only, because the whole bug was
    /// that this function's only call sites were below the test boundary. The
    /// boundary is the first `#[cfg(test)]` that starts an actual `mod`
    /// (indented, followed by a `mod` line) — a bare `#[cfg(test)]` inside a
    /// doc comment is prose about the very thing this test is checking, and
    /// using it as the split would have put the whole production half on the
    /// wrong side of the line.
    #[test]
    fn plans_match_is_called_from_production_code_and_not_only_from_tests() {
        let source = include_str!("sync.rs");
        // A test module opens as `#[cfg(test)]\nmod ...`; find the first such
        // pair and treat everything before it as production.
        let split = source
            .find("#[cfg(test)]\nmod ")
            .or_else(|| source.find("#[cfg(test)]\r\nmod "))
            .expect("this file has test modules; the split point must exist");
        let (production, _tests) = source.split_at(split);

        // The definition does not count as a call. Counting `plans_match(` in
        // the production half and subtracting the `pub fn` definition leaves
        // only real call sites, and the invariant is that at least one
        // survives above the boundary.
        let mentions = production.matches("plans_match(").count();
        let definition = production.matches("pub fn plans_match(").count();
        let calls = mentions - definition;
        assert!(
            calls > 0,
            "`plans_match` must be CALLED from production code, not just \
             defined there. It is defined at line ~308 but the only call is \
             below the `#[cfg(test)]` boundary — a comment with a test \
             attached, which a comment cannot enforce. (found {mentions} \
             mentions, {definition} of them the definition)"
        );
    }
}

/// The run-level verdict, and the audit trail it writes.
///
/// `sync_all` used to count `status == "error"` and nothing else, so a run
/// whose only bad repo was an `autostash_conflict` exited 0 — and wrote
/// `exit_code = 0` into `runs` alongside a `sync_results` row saying the
/// user's work was in a stash. A script driving ro saw success. That is the
/// lie the whole `--autostash` work exists to prevent, reintroduced one
/// layer up.
#[cfg(test)]
mod run_exit_code_tests {
    use super::*;

    fn result(status: &str) -> SyncResult {
        SyncResult {
            repo_id: "r1".into(),
            action: "pull".into(),
            status: status.into(),
            duration_ms: 1,
            error: None,
            pre_oid: None,
            post_oid: None,
            ahead: None,
            behind: None,
            unmeasurable_reason: None,
            plan: None,
            plan_mismatch: None,
            reason: None,
        }
    }

    /// The bug, as a unit test: the conflict is a failure, and it is not the
    /// string `error`.
    #[test]
    fn an_autostash_conflict_fails_the_run() {
        assert_eq!(
            run_exit_code(&[result(STATUS_AUTOSTASH_CONFLICT)]),
            1,
            "a run whose only bad repo is an autostash conflict must not exit 0"
        );
        assert_eq!(
            run_exit_code(&[result(STATUS_ERROR)]),
            1,
            "an error fails the run"
        );
    }

    /// The negative control. This is the assertion that stops the fix above
    /// from being "solved" by making every non-success a failure: a fleet of
    /// healthy repos where some are dirty, some unpushed, some merely
    /// simulated, is a **good** run, and it must exit 0.
    #[test]
    fn a_fleet_of_healthy_skips_exits_zero() {
        let results = vec![
            result(STATUS_SUCCESS),
            result(STATUS_SUCCESS),
            result(STATUS_SKIPPED), // skipped_dirty
            result(STATUS_SKIPPED), // skipped_unpushed
            result(STATUS_DRY_RUN), // --dry-run
            result(STATUS_SUCCESS),
        ];
        assert_eq!(
            run_exit_code(&results),
            0,
            "skips are not failures: a fleet of twenty healthy repos must not \
             exit 1, or the exit code stops meaning anything"
        );
        assert!(
            !results.iter().any(|r| status_fails_run(&r.status)),
            "no skip may be classified as a failure"
        );
    }

    /// An empty run is a success. `ro sync` on an empty registry is a
    /// legitimate question with the answer "nothing to do", not a failure.
    #[test]
    fn an_empty_run_exits_zero() {
        assert_eq!(
            run_exit_code(&[]),
            0,
            "nothing went wrong because nothing ran"
        );
    }

    /// The third failure status, and the reason it is not the string `error`.
    ///
    /// A pull that stops on a conflict is filed as `conflict`, not `error`,
    /// because the two need different work: a conflict is a stage `ro ship`
    /// knows how to continue from, and an error is not. That distinction is
    /// worth having in the audit trail — and it is exactly why the run-level
    /// aggregation, which counted only `status == "error"`, let a run whose
    /// only bad repo was a conflicted pull exit 0.
    ///
    /// Asserted at the rule rather than through a real conflicted rebase,
    /// because whether `git pull --rebase` stops on a same-line divergence is
    /// a property of the git version and of how the histories diverged, not
    /// of this code: on this machine (git 2.53) a genuinely diverged
    /// same-line rebase reports "Successfully rebased" and drops the local
    /// commit, so a fixture built on that premise would pass or fail for
    /// reasons that have nothing to do with the exit code. The rule is the
    /// thing under test, and it is the thing that was wrong.
    #[test]
    fn a_conflicted_pull_fails_the_run_even_though_it_is_not_the_string_error() {
        assert!(
            status_fails_run(STATUS_CONFLICT),
            "a conflicted pull is a failure, and it is not the string `error`"
        );
        assert_eq!(
            run_exit_code(&[result(STATUS_CONFLICT)]),
            1,
            "a run whose only bad repo is a conflicted pull must not exit 0"
        );
        // And it is still distinguishable from an autostash conflict, which
        // is the whole reason it is not filed as `error`.
        assert_ne!(
            STATUS_CONFLICT, STATUS_AUTOSTASH_CONFLICT,
            "the two conflicts must stay separate in a query"
        );
    }

    /// A status nobody has classified fails the run, loudly, rather than
    /// being waved through. A status nothing knows about is not a status
    /// anyone has vouched for, and the safe default for an unvouched status
    /// is to say so.
    #[test]
    fn an_unclassified_status_fails_the_run_rather_than_passing() {
        assert_eq!(
            run_exit_code(&[result("some_future_status")]),
            1,
            "an unknown status must not be treated as success by default"
        );
    }

    /// The end-to-end version: the conflict reaches the `runs` row, not just
    /// the return value.
    ///
    /// `finalize_run` is what writes `exit_code`, so a run recorded with 0
    /// while its results hold a conflict is an audit-trail lie — `ro status`
    /// reads `runs`, and it would report a clean run over a repo whose work
    /// is in a stash.
    #[test]
    fn a_conflicting_run_records_a_non_zero_exit_code_in_the_runs_table() {
        let (tmp, conn) = super::tests::setup();
        let remote = super::tests::init_bare_remote(tmp.path());
        super::tests::commit_to_remote(tmp.path(), &remote, "a.txt", "base");

        let local_path = tmp.path().join("local").join("proj1");
        std::fs::create_dir_all(&local_path).unwrap();
        super::tests::run_git(&local_path, &["clone", &remote.to_string_lossy(), "."]);
        super::tests::run_git(&local_path, &["config", "user.email", "test@example.com"]);
        super::tests::run_git(&local_path, &["config", "user.name", "Test"]);

        // Both sides rewrite the same line, so the pop must conflict.
        std::fs::write(local_path.join("a.txt"), "LOCAL UNCOMMITTED WORK").unwrap();
        super::tests::commit_to_remote(tmp.path(), &remote, "a.txt", "REMOTE MOVED AHEAD");

        let repo_id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES (?1, 'github.com', 'alice', 'proj1', ?2, ?3, ?4, ?5)",
            params![
                repo_id,
                remote.to_string_lossy().to_string(),
                local_path.to_string_lossy().to_string(),
                now,
                now
            ],
        )
        .unwrap();

        let results = sync_all(
            &conn,
            &SyncOptions {
                autostash: true,
                ..SyncOptions::default()
            },
            &[],
        )
        .unwrap();
        assert_eq!(
            results[0].status, "autostash_conflict",
            "the fixture must actually conflict, or this proves nothing: {:?}",
            results[0]
        );

        // The run row is the audit trail, and it must agree with the verdict.
        let recorded: i64 = conn
            .query_row(
                "SELECT exit_code FROM runs WHERE command = 'sync' ORDER BY started_at DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            recorded, 1,
            "a run whose results hold an autostash conflict must not be \
             recorded with exit_code 0 — that is the lie, in the table"
        );

        // And the per-repo row says what happened, so the two are readable
        // together.
        let row_status: String = conn
            .query_row(
                "SELECT status FROM sync_results WHERE repo_id = ?1 ORDER BY rowid DESC LIMIT 1",
                params![repo_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            row_status, "autostash_conflict",
            "the per-repo row must name the conflict: {row_status}"
        );
    }

    /// The same run-level rule, from the other side: a fleet of skips writes
    /// `exit_code = 0`, so the audit trail does not contradict the process.
    #[test]
    fn a_run_of_only_skips_records_a_zero_exit_code() {
        let (tmp, conn) = super::tests::setup();
        let remote = super::tests::init_bare_remote(tmp.path());
        super::tests::commit_to_remote(tmp.path(), &remote, "a.txt", "base");

        let local_path = tmp.path().join("local").join("proj1");
        std::fs::create_dir_all(&local_path).unwrap();
        super::tests::run_git(&local_path, &["clone", &remote.to_string_lossy(), "."]);
        super::tests::run_git(&local_path, &["config", "user.email", "test@example.com"]);
        super::tests::run_git(&local_path, &["config", "user.name", "Test"]);
        super::tests::run_git(
            &local_path,
            &["commit", "-q", "--allow-empty", "-m", "init"],
        );
        super::tests::run_git(&local_path, &["push", "-q", "-u", "origin", "main"]);

        let repo_id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES (?1, 'github.com', 'alice', 'proj1', ?2, ?3, ?4, ?5)",
            params![
                repo_id,
                remote.to_string_lossy().to_string(),
                local_path.to_string_lossy().to_string(),
                now,
                now
            ],
        )
        .unwrap();

        // Dirty, with no --autostash: the tool refuses to throw the work
        // away, which is the correct answer and not a failure.
        std::fs::write(local_path.join("a.txt"), "uncommitted\n").unwrap();
        let results = sync_all(&conn, &SyncOptions::default(), &[]).unwrap();
        assert_eq!(
            results[0].status, "skipped",
            "the fixture must skip: {:?}",
            results[0]
        );

        let recorded: i64 = conn
            .query_row(
                "SELECT exit_code FROM runs WHERE command = 'sync' ORDER BY started_at DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            recorded, 0,
            "a run of skips is a good run, and the table must say so"
        );
    }
}

/// A conflict is reported **once**, and the sync after it is clean.
///
/// This is the wedge seen from `ro sync` rather than from git: the detector
/// asked whether any line of `git stash list` contained the word
/// `autostash`, so the stash the *first* sync left behind — correctly, it is
/// the user's only route back to their work — was reported as the *second*
/// sync's. From there on every `ro sync --autostash` on that repo was a red
/// `autostash_conflict` over a healthy worktree, with no way out short of
/// the user going and dropping a stash by hand.
///
/// The discriminator is age, not wording: a stash is this pull's only if it
/// was not there before the pull started.
#[cfg(test)]
mod autostash_wedge_tests {
    use super::*;

    /// Register a real clone of `remote` at `local_path`.
    fn track_clone(conn: &Connection, remote: &Path, local_path: &Path) {
        std::fs::create_dir_all(local_path).unwrap();
        super::tests::run_git(local_path, &["clone", &remote.to_string_lossy(), "."]);
        super::tests::run_git(local_path, &["config", "user.email", "test@example.com"]);
        super::tests::run_git(local_path, &["config", "user.name", "Test"]);
        let repo_id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES (?1, 'github.com', 'alice', 'proj1', ?2, ?3, ?4, ?5)",
            params![
                repo_id,
                remote.to_string_lossy().to_string(),
                local_path.to_string_lossy().to_string(),
                now,
                now
            ],
        )
        .unwrap();
    }

    fn sync_with_autostash(conn: &Connection) -> Vec<SyncResult> {
        sync_all(
            conn,
            &SyncOptions {
                autostash: true,
                ..SyncOptions::default()
            },
            &[],
        )
        .unwrap()
    }

    /// The defect, end to end, as the user meets it.
    #[test]
    fn a_conflict_is_reported_once_and_the_next_sync_is_clean() {
        let (tmp, conn) = super::tests::setup();
        let remote = super::tests::init_bare_remote(tmp.path());
        super::tests::commit_to_remote(tmp.path(), &remote, "a.txt", "base");

        let local_path = tmp.path().join("local").join("proj1");
        track_clone(&conn, &remote, &local_path);

        // Both sides rewrite the same line, so the pop must conflict.
        std::fs::write(local_path.join("a.txt"), "LOCAL UNCOMMITTED WORK").unwrap();
        super::tests::commit_to_remote(tmp.path(), &remote, "a.txt", "REMOTE MOVED AHEAD");

        // Sync #1: the conflict, reported once.
        let first = sync_with_autostash(&conn);
        assert_eq!(
            first[0].status, "autostash_conflict",
            "the pop conflicts here, so this sync must say so: {:?}",
            first[0]
        );
        let held = super::tests::run_git(&local_path, &["stash", "list", "--format=%H"]);
        assert_eq!(
            String::from_utf8_lossy(&held.stdout).lines().count(),
            1,
            "a failed pop keeps the user's work in a stash, and that is the \
             whole point: {}",
            String::from_utf8_lossy(&held.stdout)
        );

        // The user resolves the conflict markers and commits the resolution.
        // The stash is still there — it is their work, and nothing has any
        // business dropping it for them.
        super::tests::run_git(&local_path, &["checkout", "--theirs", "a.txt"]);
        super::tests::run_git(&local_path, &["add", "a.txt"]);
        super::tests::run_git(
            &local_path,
            &["commit", "-q", "-m", "resolved: keep the local work"],
        );
        let still_there = super::tests::run_git(&local_path, &["stash", "list", "--format=%H"]);
        assert_eq!(
            String::from_utf8_lossy(&still_there.stdout).lines().count(),
            1,
            "the stash must survive the resolution, or the user has lost work"
        );

        // Sync #2: the tree is clean and the pull is a no-op. It stashes
        // nothing and pops nothing, so the stash above is not its stash.
        let second = sync_with_autostash(&conn);
        assert_ne!(
            second[0].status, "autostash_conflict",
            "a leftover autostash from an EARLIER sync must not be reported \
             as this one's — that wedges the repo permanently, with no way \
             out short of dropping the stash by hand. Got {:?}",
            second[0]
        );
        assert_eq!(
            second[0].status, "success",
            "and a clean sync over a clean tree is a success: {:?}",
            second[0]
        );

        // The run agrees: a clean sync is a run that exits 0, in the table
        // as well as on the wire.
        assert_eq!(
            run_exit_code(&second),
            0,
            "the second sync is a good run and must not be reported as failed"
        );
    }

    /// The other half of the same defect, from `ro sync`.
    ///
    /// A stash the user made by hand is not this pull's stash, even when
    /// its message says `autostash` — which is a thing people write, because
    /// it is the word in the flag they just used. A substring match cannot
    /// tell the two apart, and invents a conflict on a sync that
    /// fast-forwarded perfectly.
    #[test]
    fn a_stash_the_user_made_is_not_reported_as_a_conflict() {
        let (tmp, conn) = super::tests::setup();
        let remote = super::tests::init_bare_remote(tmp.path());
        super::tests::commit_to_remote(tmp.path(), &remote, "a.txt", "base");

        let local_path = tmp.path().join("local").join("proj1");
        track_clone(&conn, &remote, &local_path);
        std::fs::write(local_path.join("a.txt"), "LOCAL UNCOMMITTED WORK").unwrap();

        // The user's own stash, made by hand, before the sync.
        super::tests::run_git(
            &local_path,
            &["stash", "push", "-m", "autostash of the report rewrite"],
        );

        // The remote moves, so the sync has real work to do.
        super::tests::commit_to_remote(tmp.path(), &remote, "a.txt", "REMOTE MOVED AHEAD");

        let results = sync_with_autostash(&conn);
        assert_eq!(
            results[0].status, "success",
            "this is an ordinary fast-forward over a clean tree; a stash the \
             user made is not this pull's stash, whatever it is called: {:?}",
            results[0]
        );
        assert_eq!(
            std::fs::read_to_string(local_path.join("a.txt")).unwrap(),
            "REMOTE MOVED AHEAD",
            "the remote's version must have landed"
        );
        assert_eq!(
            run_exit_code(&results),
            0,
            "and a clean sync over a clean tree is not a failure"
        );
    }
}

/// ── The deadline ──
#[cfg(test)]
mod deadline {
    use super::*;
    use tempfile::TempDir;

    /// The one git call sync makes directly is given the run's deadline.
    ///
    /// `--timeout <secs>` arrived as a field on `SyncOptions` and a flag on
    /// the CLI and **nothing read it**: every git call went out with
    /// `RunOpts::none()`, whose `timeout` is `None`, and `None` means
    /// `Command::output()` — wait for the child, forever. A `--timeout 5`
    /// sync over a fleet ran exactly as long as its slowest git, and the
    /// slowest git is the one waiting on a network that never answers.
    ///
    /// Measured, not assumed. `git remote prune` against a listener that
    /// accepts the connection and never answers it runs 30 seconds and is
    /// killed by the outer `timeout`, so without the deadline it is a
    /// wedged fleet; with `--timeout 5` the same call is killed at five
    /// seconds and the run moves on to the next repo. That is the guarantee
    /// the flag promises, and the reason the discipline is the engine's
    /// rather than a `Child::kill` on the direct child: git spawns `ssh` and
    /// credential helpers of its own, and killing only the direct child
    /// leaves those holding the worktree lock, which stalls the fleet exactly
    /// as much as never killing anything — and then reports success.
    #[test]
    fn the_deadline_is_passed_to_the_git_call_sync_makes() {
        let opts = SyncOptions {
            timeout_secs: 5,
            ..Default::default()
        };
        assert_eq!(
            opts.run_opts().timeout,
            Some(Duration::from_secs(5)),
            "`--timeout 5` must reach git as a five-second deadline"
        );

        // Zero means "no deadline", not "kill it immediately": a
        // zero-length deadline is not a fast git, and the only way zero
        // arrives is a user who typed `--timeout 0`.
        let zero = SyncOptions {
            timeout_secs: 0,
            ..Default::default()
        };
        assert_eq!(
            zero.git_timeout(),
            None,
            "a zero timeout is no deadline, not an instant kill"
        );
    }

    /// A hanging git is killed at the deadline, and the run continues.
    ///
    /// This is the property the whole flag exists for, asserted through the
    /// same `run_in` that `ro_git` uses, with a shim in the place of the real
    /// binary. `sleep 600` is the shape of a git call waiting on a network
    /// that never answers: not a crash, not a refusal, a child that simply
    /// never returns.
    ///
    /// The kill is on the **tree**, which is why the shim leaves a marker
    /// file: a grandchild that outlived the kill would still be able to
    /// touch the worktree, and a timeout that returns while its grandchildren
    /// live is worse than no timeout — the fleet stalls anyway and now also
    /// reports success.
    #[test]
    fn a_hanging_git_is_killed_at_the_timeout_and_the_run_continues() {
        let tmp = TempDir::new().unwrap();
        let marker = tmp.path().join("grandchild-survived");
        // Two scripts rather than one on Windows, because the shape under test
        // is a **tree**: a direct child that hangs, and a grandchild that would
        // perform its side effect if it outlived the kill. A single `.cmd`
        // cannot express that, because `cmd.exe` has no equivalent of `sh`'s
        // background `&`.
        //
        // The Unix fixture was a `#!/bin/sh` script named without an
        // extension, which Windows refuses to execute at all — `os error 193,
        // "%1 is not a valid Win32 application"`. The test then asserted on a
        // spawn error rather than on a timeout, and reported the deadline as
        // broken when the deadline had never been given a process to fire
        // against. Rust's `Command` runs a `.cmd` through `cmd.exe`, so naming
        // it is enough to make the fixture real here.
        //
        // `ping -n 25` is the Windows stand-in for `sleep`: it blocks for
        // roughly 25 seconds with no output, which is far longer than the
        // 500 ms deadline and the 3 s the marker is watched for.
        #[cfg(windows)]
        let shim = {
            let grandchild = tmp.path().join("grandchild.cmd");
            std::fs::write(
                &grandchild,
                format!(
                    "@echo off\r\nping -n 25 127.0.0.1 > nul\r\necho survived> \"{}\"\r\n",
                    marker.display()
                ),
            )
            .unwrap();
            let shim = tmp.path().join("git-hang.cmd");
            std::fs::write(
                &shim,
                format!(
                    "@echo off\r\nstart \"\" /b \"{}\"\r\nping -n 25 127.0.0.1 > nul\r\n",
                    grandchild.display()
                ),
            )
            .unwrap();
            shim
        };
        #[cfg(not(windows))]
        let shim = {
            let shim = tmp.path().join("git-hang");
            std::fs::write(&shim, "#!/bin/sh\nsleep 600\ntouch \"$MARKER\"\n").unwrap();
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            shim
        };
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let started = Instant::now();
        let err = ro_git::mutation::run_in(
            Some(&repo),
            &["remote", "prune", "origin"],
            &ro_git::mutation::RunOpts {
                env: &[],
                timeout: Some(Duration::from_millis(500)),
                program: Some(shim.to_str().expect("a UTF-8 shim path")),
            },
        )
        .expect_err("a hanging git must not return Ok");
        let elapsed = started.elapsed();

        // `TimedOut` specifically — "an error" would also be a shim that
        // failed to start, and that is not the thing under test.
        let timed_out = err
            .downcast_ref::<ro_git::mutation::GitError>()
            .is_some_and(|g| matches!(g, ro_git::mutation::GitError::TimedOut { .. }));
        assert!(
            timed_out,
            "a shim that never returns must be reported as a timeout, got {err:#}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the deadline did not fire promptly: {elapsed:?}"
        );
        assert!(
            elapsed >= Duration::from_millis(400),
            "it returned before the deadline: {elapsed:?}"
        );
        // The control that makes the marker meaningful: the marker is written
        // by the *sleeping* process as it exits, so a marker that never
        // appears is evidence the kill reached the child, not evidence the
        // shim never ran at all.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            assert!(
                !marker.exists(),
                "a grandchild outlived the deadline and performed its side effect, so the \
                 kill reached the direct child but not the tree"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The deadline is a floor, not a ceiling.
    ///
    /// A `RunOpts::timeout` of `None` is what git did before the field
    /// existed, and a caller that asks for no deadline must not be given one
    /// by accident: the whole cost of the flag is that it is opt-in per
    /// invocation, and quietly turning it on for everyone is a behaviour
    /// change wearing a bug's clothes.
    #[test]
    fn a_run_that_does_not_ask_for_a_deadline_gets_none() {
        // The default is thirty seconds, which is the same value the CLI
        // fills in, so this is the normal case and it must carry the
        // deadline rather than silently dropping it.
        assert_eq!(
            SyncOptions::default().git_timeout(),
            Some(Duration::from_secs(30)),
            "the default timeout reaches git"
        );
        assert_eq!(
            ro_git::mutation::RunOpts::none().timeout,
            None,
            "a caller that asks for no deadline must not be handed one"
        );
    }
}

/// ── A remote that never answers ──
#[cfg(test)]
mod wedged_remote {
    use super::*;
    use tempfile::TempDir;

    /// A TCP listener     that completes the handshake and then never answers.
    ///
    /// This is the shape of the failure `--timeout` exists for: a server that
    /// accepts the connection and says nothing, so the client waits for bytes
    /// that never come. Measured against a real one of these, `git fetch` runs
    /// until something **outside** the process kills it — 50 seconds and exit
    /// 124 in the original report — which is the entire bug: a flag documented
    /// as a deadline that did not fire.
    ///
    /// The socket is deliberately **not** closed on accept. A closed socket is a
    /// connection reset, which git reports promptly and which ro already handles
    /// correctly; the hang is the thing under test, so the connection stays open
    /// and silent for [`BLACK_HOLE_SECS`].
    ///
    /// The hold is bounded rather than infinite so that a build where the
    /// deadline is *not* working still terminates this test: the socket then
    /// closes of its own accord, git errors out, and the elapsed-time assertion
    /// below fails on a real number instead of hanging the suite forever.
    const BLACK_HOLE_SECS: u64 = 15;

    pub(super) struct BlackHoleServer {
        port: u16,
    }

    impl BlackHoleServer {
        pub(super) fn start() -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")
                .expect("a loopback listener is bindable");
            let port = listener
                .local_addr()
                .expect("a bound listener has an address")
                .port();
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    // One holder thread per accepted connection. `_held` keeps
                    // the socket alive for the length of the sleep; dropping it
                    // would be the reset this fixture is built to avoid.
                    std::thread::spawn(move || {
                        let _held = stream;
                        std::thread::sleep(Duration::from_secs(BLACK_HOLE_SECS));
                    });
                }
            });
            Self { port }
        }

        /// A `git://` URL pointing at this listener. `git://` rather than
        /// `https://` because the fixture must not depend on a TLS stack, and
        /// because git's protocol handshake is exactly where it waits.
        pub(super) fn url(&self) -> String {
            format!("git://127.0.0.1:{}/wedged.git", self.port)
        }
    }

    /// A real fetch against a remote that never answers is cut off at the
    /// deadline — and the repo after it still syncs.
    ///
    /// Red-first, and red the way the bug was: `--timeout 3` was enforced on
    /// exactly one git call in a whole sync, and that one was `git remote prune`
    /// — a command that touches nothing but local bookkeeping. The three calls
    /// that actually talk to a network (`fetch`, `pull`, `clone`) went out with
    /// `RunOpts::none()`, so the flag was a no-op for the failure it was written
    /// for, and a wedged remote wedged the fleet.
    ///
    /// Asserted end to end through [`sync_all`], over a real bare remote and a
    /// real TCP listener, because the bug was never in `run_in` — that has killed
    /// hung children correctly all along. The bug was that the sync's fetch never
    /// handed it a deadline to enforce. A test of `run_in` alone, the way the
    /// suite already had one, passes on a build where the flag does nothing.
    ///
    /// The three assertions are the three halves of the guarantee:
    ///
    ///  1. it returns in about the timeout, not in about forever;
    ///  2. the wedged repo is an **error** — a timeout is not a success and not
    ///     a silent skip;
    ///  3. **the next repo in the fleet still runs.** A deadline that kills the
    ///     wedged git and then takes the run down with it is not a deadline, it
    ///     is a fleet-wide hang with a tidier error message.
    #[test]
    fn a_wedged_remote_is_cut_off_at_the_deadline_and_the_next_repo_still_runs() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let black_hole = BlackHoleServer::start();

        let remote = super::tests::init_bare_remote(tmp.path());
        super::tests::commit_to_remote(tmp.path(), &remote, "a.txt", "base");

        // The wedged repo: an ordinary checkout, with `origin` repointed at a
        // server that accepts and never answers. Everything before the fetch —
        // the prediction, the dirty check, the ahead/behind read — succeeds
        // locally, so the fetch is the first thing to touch the network and the
        // first thing to have to be killed.
        let wedged = tmp.path().join("local").join("wedged");
        std::fs::create_dir_all(&wedged).unwrap();
        let clone = super::tests::run_git(&wedged, &["clone", &remote.to_string_lossy(), "."]);
        assert!(
            clone.status.success(),
            "the wedged fixture must be a real checkout: {}",
            String::from_utf8_lossy(&clone.stderr)
        );
        let repoint =
            super::tests::run_git(&wedged, &["remote", "set-url", "origin", &black_hole.url()]);
        assert!(
            repoint.status.success(),
            "the origin repoint failed: {}",
            String::from_utf8_lossy(&repoint.stderr)
        );

        // The healthy repo: the same real remote, left alone. This is the one
        // that proves the run moved on rather than aborting.
        let healthy = tmp.path().join("local").join("healthy");
        std::fs::create_dir_all(&healthy).unwrap();
        let clone = super::tests::run_git(&healthy, &["clone", &remote.to_string_lossy(), "."]);
        assert!(
            clone.status.success(),
            "the healthy fixture must be a real checkout: {}",
            String::from_utf8_lossy(&clone.stderr)
        );

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        for (id, name, path) in [
            ("id-wedged", "wedged", &wedged),
            ("id-healthy", "healthy", &healthy),
        ] {
            conn.execute(
                "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
                 VALUES (?1, 'github.com', 'fleet', ?2, ?3, ?4, ?5, ?5)",
                params![
                    id,
                    name,
                    remote.to_string_lossy().to_string(),
                    path.to_string_lossy().to_string(),
                    now
                ],
            )
            .unwrap();
        }

        let started = Instant::now();
        let results = sync_all(
            &conn,
            &SyncOptions {
                timeout_secs: 3,
                ..Default::default()
            },
            &[],
        )
        .unwrap();
        let elapsed = started.elapsed();

        let wedged_row = results
            .iter()
            .find(|r| r.repo_id == "id-wedged")
            .expect("the wedged repo has a row");
        let healthy_row = results
            .iter()
            .find(|r| r.repo_id == "id-healthy")
            .expect("the healthy repo has a row");

        // 1. About the deadline, and nowhere near the hold the fixture keeps the
        //    silent connection open for. On a build where the deadline is not
        //    reaching the fetch, this run cannot finish before the socket is
        //    closed for it, so this number is the bug and not a flake.
        assert!(
            elapsed < Duration::from_secs(9),
            "the run took {elapsed:?} against a remote that never answers. The \
             deadline is not reaching the fetch: without it this call blocks until \
             something outside ro kills it."
        );

        // 2. A timeout is an error, and says what happened. Not a skip — a
        //    "skipped" row here would be indistinguishable from a healthy repo.
        assert_eq!(
            wedged_row.status, "error",
            "a fetch that ran out of time is an error, got {wedged_row:?}"
        );
        let why = wedged_row.error.as_deref().unwrap_or_default();
        assert!(
            why.contains("did not finish within") || why.contains("killed"),
            "the row must say the git was killed at the deadline, so the user can \
             tell a wedged network from a bad URL. Got: {why}"
        );

        // 3. The fleet moved on. This is the assertion that makes the other two
        //    worth having: `--timeout` exists so one bad repo cannot stop the
        //    other nineteen.
        assert_eq!(
            healthy_row.status, "success",
            "the repo after the wedged one must still sync; a timeout that takes \
             the run down with it is not a timeout. Got {healthy_row:?}"
        );

        // And the run-level verdict agrees: a repo we could not reach is not a
        // good run, and a script driving ro has to be able to see that.
        assert_eq!(
            run_exit_code(&results),
            1,
            "a fleet containing a repo the deadline cut off must not exit 0"
        );
    }

    /// The prediction mismatch this run recorded, and that it did not fail on.
    ///
    /// Two properties in one, because they are the same decision seen from both
    /// sides.
    ///
    /// **It is recorded.** The prediction for the wedged repo was made from
    /// local state, before the fetch: its `origin/main` had not moved, so the
    /// plan said the repo was in line and the run would pass. The run then failed
    /// it. `plans_match` is the function that encodes exactly this invariant, and
    /// until this change it was `pub`, documented as *the* property, and called
    /// from `#[cfg(test)]` and nowhere else — a function that cannot be reached
    /// from the binary is a comment with a test attached, and a comment does not
    /// stop the next divergence. A `Some` here is that call being reached from
    /// production code, observed as behaviour rather than by reading the source.
    ///
    /// **It does not fail the run**, and the asymmetry is the point. This repo
    /// *was* an error, so the exit code is already 1 and the user already knows.
    /// The reverse case is the one that would make a naive wiring actively
    /// harmful: a repo the plan wrongly predicted would fail but that synced
    /// perfectly is a healthy repo, and failing the run for it would turn the
    /// tool's own staleness into a red fleet board — which is how an exit code
    /// stops meaning anything and starts being ignored.
    #[test]
    fn a_prediction_that_did_not_hold_is_reported_without_failing_the_run() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let black_hole = BlackHoleServer::start();

        let remote = super::tests::init_bare_remote(tmp.path());
        super::tests::commit_to_remote(tmp.path(), &remote, "a.txt", "base");

        let wedged = tmp.path().join("local").join("wedged");
        std::fs::create_dir_all(&wedged).unwrap();
        let clone = super::tests::run_git(&wedged, &["clone", &remote.to_string_lossy(), "."]);
        assert!(clone.status.success());
        let repoint =
            super::tests::run_git(&wedged, &["remote", "set-url", "origin", &black_hole.url()]);
        assert!(repoint.status.success());

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES ('id-wedged', 'github.com', 'fleet', 'wedged', ?1, ?2, ?3, ?3)",
            params![
                remote.to_string_lossy().to_string(),
                wedged.to_string_lossy().to_string(),
                now
            ],
        )
        .unwrap();

        let results = sync_all(
            &conn,
            &SyncOptions {
                timeout_secs: 3,
                ..Default::default()
            },
            &[],
        )
        .unwrap();
        let row = &results[0];

        assert!(
            row.plan_mismatch.is_some(),
            "the plan said this repo was in line and the run could not reach it, so \
             the prediction was wrong. `plans_match` must be reached from \
             production code for that to be noticed at all — it used to be called \
             only from tests. Got {row:?}"
        );
        let why = row.plan_mismatch.as_deref().unwrap_or_default();
        assert!(
            why.contains("in line") && why.contains("error"),
            "the mismatch must name what was predicted and what happened, so it is \
             actionable. Got: {why}"
        );
        // And the decision that makes this safe to leave on by default: the run
        // is not failed *because of the mismatch*. It fails on the error, which
        // is a real fact about the repo.
        assert_eq!(
            row.status, "error",
            "the mismatch must not rewrite what actually happened to the repo"
        );
        assert_eq!(
            run_exit_code(&results),
            1,
            "the run fails because the repo could not be synced, not because the \
             preview was wrong"
        );
    }
}

/// The fleet runs concurrently, and the audit trail stays in one piece.
///
/// `sync_all` was a plain `for` loop over the registry, which made the
/// README's claim that `ro sync` is parallel false. These tests are what make
/// it true — and, more importantly, what keep it true: a pool that returns
/// results in completion order, or writes its own rows, or lets two repos
/// share a worktree, is a pool that has to be unwound later.
///
/// # What is asserted, and what deliberately is not
///
/// **Not timing.** A test that asserts "it was faster" passes on a build that
/// is merely less slow, and fails on a machine that is merely busy. What is
/// asserted instead is *order of completion*: a shared counter the observer
/// increments, read back after the run. Two repos that finished in the order
/// they were started is a fact; "the wall clock went down" is a hope.
///
/// The one place a duration does appear is the wedged-remote test, and there
/// it is a bound in the other direction — a remote that never answers must
/// not be waited on — which is the assertion that cannot be satisfied by
/// accident.
///
/// # Why the observer is in-process and not a `PATH` shim
///
/// The obvious instrument is a fake `git` on `PATH` that logs its own pid.
/// It was tried, and it is the wrong tool here: `PATH` is process-global,
/// the suite shares one process, and a shim installed for one test is
/// visible to every fixture another test builds *while it is installed* —
/// which is how a test that never touches `git` ends up failing on
/// `cannot open /tmp/.tmpXXXX/git`. The testkit's `TestEnv` serialises the
/// swap, but only for the duration of one call, and a fleet run is exactly
/// the call that has to hold it.
///
/// So the observer is a thread-local counter the sync itself increments.
/// It observes the real thing — how many git calls are in flight at once —
/// without intercepting any of them.
#[cfg(test)]
mod parallel_fleet {
    use super::tests::{commit_to_remote, init_bare_remote, run_git};
    use super::*;
    use super::{FleetObserver, sync_all_observed};
    use tempfile::TempDir;

    /// A fleet of `n` rows, each pointing at its own checkout of `remote`.
    ///
    /// Every repo is already cloned and in line, so a sync is a fetch and a
    /// pull against a local bare remote — fast, and identical for all of them.
    /// That is the point: the only thing that differs between them is *when*
    /// they run.
    ///
    /// `git clone` and nothing hand-rolled. The sync decides "already up to
    /// date" by comparing against `origin/<branch>`, and a fixture built with
    /// `git init` + `git remote add` + `git fetch` does not have that ref —
    /// so a hand-built fixture reports the repo as **unmeasurable**, the dry
    /// run marks it failed, and every assertion about it is about the
    /// fixture rather than about concurrency. A fixture that differs from a
    /// real clone in a way the code reads proves nothing.
    fn fleet(tmp: &TempDir, conn: &Connection, n: usize) -> Vec<String> {
        let remote = init_bare_remote(tmp.path());
        commit_to_remote(tmp.path(), &remote, "a.txt", "base");

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let mut ids = Vec::new();
        for i in 0..n {
            let id = format!("id-{i}");
            let local = tmp.path().join("local").join(format!("repo{i}"));
            std::fs::create_dir_all(&local).unwrap();
            let clone = run_git(&local, &["clone", &remote.to_string_lossy(), "."]);
            assert!(
                clone.status.success(),
                "fixture {i} must clone: {}",
                String::from_utf8_lossy(&clone.stderr)
            );
            // The clone has to have landed on `main` for `origin/main` to be
            // the ref the sync compares against. `init_bare_remote` makes the
            // bare repo's branch `main`, and a clone follows the remote's HEAD
            // — asserted rather than assumed, because a fixture that is
            // quietly on `master` fails as "unmeasurable", which reads like
            // a concurrency bug and is not one.
            let branch = String::from_utf8_lossy(
                &run_git(&local, &["rev-parse", "--abbrev-ref", "HEAD"]).stdout,
            )
            .trim()
            .to_string();
            assert_eq!(
                branch, "main",
                "fixture {i} must be on main, like a real clone of this remote"
            );
            conn.execute(
                "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
                 VALUES (?1, 'github.com', 'fleet', ?2, ?3, ?4, ?5, ?5)",
                params![
                    id,
                    format!("repo{i}"),
                    remote.to_string_lossy().to_string(),
                    local.to_string_lossy().to_string(),
                    now,
                ],
            )
            .unwrap();
            ids.push(id);
        }
        ids
    }

    /// A fleet of `n` rows that are **not cloned yet**, so each sync is a
    /// clone. Used where the assertion is about the clone path specifically.
    fn uncloned_fleet(tmp: &TempDir, conn: &Connection, n: usize) -> Vec<String> {
        let remote = init_bare_remote(tmp.path());
        commit_to_remote(tmp.path(), &remote, "a.txt", "base");

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let mut ids = Vec::new();
        for i in 0..n {
            let id = format!("id-{i}");
            let local = tmp.path().join("local").join(format!("repo{i}"));
            std::fs::create_dir_all(&local).unwrap();
            conn.execute(
                "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
                 VALUES (?1, 'github.com', 'fleet', ?2, ?3, ?4, ?5, ?5)",
                params![
                    id,
                    format!("repo{i}"),
                    remote.to_string_lossy().to_string(),
                    local.to_string_lossy().to_string(),
                    now,
                ],
            )
            .unwrap();
            ids.push(id);
        }
        ids
    }

    /// A fleet of `n` rows all naming **one** checkout.
    ///
    /// Legal — uniqueness is on `(host, owner, name)`, not on `local_path` —
    /// and harmless while the run was a loop. Under a pool it is two `git
    /// pull`s in one worktree, which is the failure mode the shared lock
    /// exists to prevent.
    fn shared_path_fleet(tmp: &TempDir, conn: &Connection, n: usize) -> Vec<String> {
        let remote = init_bare_remote(tmp.path());
        commit_to_remote(tmp.path(), &remote, "a.txt", "base");

        let shared = tmp.path().join("local").join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        let clone = run_git(&shared, &["clone", &remote.to_string_lossy(), "."]);
        assert!(
            clone.status.success(),
            "the shared fixture must clone: {}",
            String::from_utf8_lossy(&clone.stderr)
        );

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let mut ids = Vec::new();
        for i in 0..n {
            let id = format!("id-{i}");
            conn.execute(
                "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
                 VALUES (?1, 'github.com', 'fleet', ?2, ?3, ?4, ?5, ?5)",
                params![
                    id,
                    format!("shared{i}"),
                    remote.to_string_lossy().to_string(),
                    shared.to_string_lossy().to_string(),
                    now,
                ],
            )
            .unwrap();
            ids.push(id);
        }
        ids
    }

    /// The repos a run actually touched, in the order the registry listed
    /// them — which is the order `manage::list` returns, `ORDER BY owner,
    /// name`.
    fn registry_order(conn: &Connection) -> Vec<String> {
        crate::manage::list(conn, None)
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect()
    }

    /// Run a fleet with the concurrency observed, and hand the observer back
    /// so the caller can read the counters.
    ///
    /// Returning them together is what keeps the observer's lifetime honest:
    /// it cannot outlive the run it measured, and a test cannot read a
    /// counter that belonged to some other test's fleet.
    fn observed(conn: &Connection, parallel: usize) -> (Vec<SyncResult>, FleetObserver) {
        let observer = FleetObserver::watching();
        let results = sync_all_observed(conn, &SyncOptions::default(), &[], parallel, &observer)
            .expect("the fleet syncs");
        (results, observer)
    }

    /// A fleet of repos syncs **concurrently**, not one at a time.
    ///
    /// Asserted on the shared counter's peak, not on the wall clock. A
    /// sequential loop can only ever have one repo in flight, so a peak of 2
    /// or more is a statement about concurrency that no amount of "the
    /// machine was slow" can produce — which is the failure mode a timing
    /// assertion has.
    ///
    /// The control is the test beside it: the same fleet at a bound of one
    /// peaks at exactly 1. Between them the counter is shown to be capable
    /// of reporting a serial run, so the parallel number means something.
    #[test]
    fn a_fleet_of_repos_syncs_concurrently() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let ids = fleet(&tmp, &conn, 4);

        let (results, observer) = observed(&conn, 4);
        assert_eq!(results.len(), ids.len(), "every repo has a row");
        for r in &results {
            assert_eq!(
                r.status, "success",
                "a clean checkout against a local remote syncs: {r:?}"
            );
        }

        assert_eq!(
            observer.total(),
            ids.len(),
            "each repo was synced exactly once"
        );
        assert!(
            observer.peak() >= 2,
            "the fleet ran {} repos with a peak of {} in flight. A sequential \
             loop cannot exceed 1, so this is the assertion that the pool \
             exists — and it is a count, not a duration, so a slow machine \
             cannot fake it either way.",
            ids.len(),
            observer.peak()
        );
    }

    /// A loopback HTTP server that answers **401 to everything** and records
    /// the `Authorization` header each request carried.
    ///
    /// The 401 is the point: an unauthenticated fetch **fails**, so "the
    /// credential reached the remote" and "the fetch was answered" are the
    /// same observation rather than two that can disagree.
    struct DemandingRemote {
        /// Kept alive: dropping the `TempDir` deletes the bare repo the
        /// server is serving while a fetch may still be in flight.
        _tmp: tempfile::TempDir,
        seen: std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl DemandingRemote {
        fn start() -> (Self, String) {
            use std::io::{Read, Write};
            use std::net::TcpListener;

            let tmp = tempfile::TempDir::new().unwrap();
            let bare = tmp.path().join("remote.git");
            std::fs::create_dir_all(&bare).unwrap();
            run_git(&bare, &["init", "--bare", "-q", "--initial-branch=main"]);

            let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port is bindable");
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let seen_thread = seen.clone();
            let stop_thread = stop.clone();
            let _worker = std::thread::spawn(move || {
                while !stop_thread.load(std::sync::atomic::Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let _ = stream.set_nonblocking(false);
                            let _ =
                                stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
                            let mut chunk = [0u8; 8192];
                            let Ok(n) = stream.read(&mut chunk) else {
                                continue;
                            };
                            let head = String::from_utf8_lossy(&chunk[..n]).into_owned();
                            let header = head
                                .lines()
                                .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                                .and_then(|l| l.split_once(':'))
                                .map(|(_, v)| v.trim().to_string());
                            seen_thread.lock().unwrap().push(header);
                            let _ = stream.write_all(
                                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n",
                            );
                            let _ = stream.flush();
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(_) => break,
                    }
                }
            });
            (
                Self {
                    _tmp: tmp,
                    seen,
                    stop,
                },
                format!("http://127.0.0.1:{port}"),
            )
        }

        /// Every request the server answered, with the header it carried.
        ///
        /// Sets the stop flag first so the accept loop exits rather than
        /// outliving the test and holding the port.
        fn headers(&self) -> Vec<Option<String>> {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            self.seen.lock().unwrap().clone()
        }
    }

    /// `ro sync` fetches with the row's credential.
    ///
    /// The flag was validated, stored on the row, echoed by `ro list`, and
    /// used by exactly one verb: `ro ship`. `sync_repo_inner` built
    /// `FetchOpts::default()` — `host: None` — and handed `opts.run_opts()` to
    /// `fetch_in`, which carries a **deadline** and no credential. So every
    /// fetch a sync made went out anonymously, and a private repo enrolled
    /// with `--credential` synced with `fatal: could not read Username` while
    /// `ro ship` over the same row succeeded.
    ///
    /// Observed against a server that answers 401 to everything, so the
    /// assertion is not "a header was configured" but "the header reached the
    /// wire".
    #[test]
    fn sync_fetches_with_the_rows_credential() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        run_git(&work, &["init", "-q", "--initial-branch=main"]);
        run_git(&work, &["config", "user.email", "t@e.com"]);
        run_git(&work, &["config", "user.name", "T"]);
        std::fs::write(work.join("a.txt"), "base\n").unwrap();
        run_git(&work, &["add", "-A"]);
        run_git(&work, &["commit", "-q", "-m", "base"]);

        let (remote, url) = DemandingRemote::start();
        run_git(
            &work,
            &["remote", "add", "origin", &format!("{url}/acme/api.git")],
        );

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, credential_ref, added_at, updated_at)
             VALUES ('r1', 'example.invalid', 'acme', 'api', ?1, ?2, 'env:RO_SYNC_CRED_TEST', ?3, ?3)",
            params![
                format!("{url}/acme/api.git"),
                work.to_string_lossy().to_string(),
                now
            ],
        )
        .unwrap();

        unsafe {
            std::env::set_var("RO_SYNC_CRED_TEST", "ghp_sync_marker");
        }
        let results = sync_all_bounded(&conn, &SyncOptions::default(), &[], 1).unwrap();
        unsafe {
            std::env::remove_var("RO_SYNC_CRED_TEST");
        }

        let headers = remote.headers();
        assert!(
            !headers.is_empty(),
            "the remote must have been asked for something, got nothing"
        );
        for header in &headers {
            let header = header.as_deref().expect(
                "a row with a credential must authenticate its sync fetch; the \
                 request went out anonymously",
            );
            assert!(
                // Base64 of `x-access-token:ghp_sync_marker`.
                header.contains("eC1hY2Nlc3MtdG9rZW46Z2hwX3N5bmNfbWFya2Vy"),
                "the row's own credential must be on the wire, got: {header}"
            );
        }
        // And the honest failure: the server refuses, so the row says so.
        assert_eq!(
            results[0].status, "error",
            "a remote that answered 401 is an error row: {results:?}"
        );
    }

    /// The pool is as wide as the number it records.
    ///
    /// The chunking this replaces computed `workers = min(parallel, n)` and
    /// then spawned one thread per `repos.chunks(div_ceil(n, workers))` — so
    /// the width was `ceil(n / ceil(n/p))`, not `min(p, n)`. For 12 repos at
    /// `p = 8` that is **6**, while the `runs` row recorded `parallel=8`: a
    /// number the user never chose, written into the audit trail, for a pool
    /// half the width they asked for.
    ///
    /// `p = 3` over 7 repos is the smallest case where the two disagree: the
    /// old shape gave `ceil(7 / ceil(7/3)) = ceil(7/3) = 3`… which happens
    /// to match. `p = 4` over 9 is the first that differs: `ceil(9/3) = 3`
    /// threads against a recorded `parallel=4`.
    #[test]
    fn the_pool_is_as_wide_as_the_number_it_records() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let ids = fleet(&tmp, &conn, 9);

        let (_results, observer) = observed(&conn, 4);
        assert!(
            observer.peak() >= 4,
            "four workers were asked for over 9 repos and the peak was {}. \
             Static chunking makes the width ceil(n / ceil(n/p)), which is \
             not min(p, n) — the runs row then overstates the pool.",
            observer.peak()
        );
        assert_eq!(observer.total(), ids.len(), "every repo still ran once");
    }

    /// A bound of one never has two repos in flight.
    ///
    /// The control for [`a_fleet_of_repos_syncs_concurrently`], and the
    /// `core.parallel = 1` case in the same breath: the setting that says "one
    /// at a time" has to mean it, or the setting is a suggestion.
    #[test]
    fn a_bound_of_one_never_has_two_repos_in_flight() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let ids = fleet(&tmp, &conn, 4);

        let (results, observer) = observed(&conn, 1);
        assert_eq!(results.len(), ids.len(), "the whole fleet ran");
        for r in &results {
            assert_eq!(r.status, "success", "every repo synced: {r:?}");
        }

        assert_eq!(observer.total(), ids.len(), "every repo still ran");
        assert_eq!(
            observer.peak(),
            1,
            "core.parallel = 1 means one repo at a time. This is also the \
             control that shows the counter can report a serial run at all — \
             without it, a peak of 2 elsewhere would only prove the counter \
             counts."
        );
    }

    /// A bound below the fleet size is honoured.
    ///
    /// Six repos at a bound of two: the peak must not exceed two, and every
    /// repo must still run. A pool that ignores its bound is a pool that
    /// opens every checkout at once, which is what the bound exists to stop.
    #[test]
    fn a_bound_below_the_fleet_size_is_honoured() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let ids = fleet(&tmp, &conn, 6);

        let (results, observer) = observed(&conn, 2);
        assert_eq!(results.len(), ids.len(), "every repo ran");
        for r in &results {
            assert_eq!(r.status, "success", "every repo synced: {r:?}");
        }

        assert_eq!(observer.total(), ids.len(), "six syncs ran");
        assert!(
            observer.peak() <= 2,
            "six repos at a bound of two reached a peak of {} in flight. The \
             bound is not a suggestion: each worker holds a checkout open, and \
             a fleet run that opens all of them at once starves itself.",
            observer.peak()
        );
    }

    /// Every repo in the fleet is synced exactly once.
    ///
    /// The count the brief asked for, read off the shared counter. This is
    /// what catches a pool that drops a repo (too few) or runs one twice (too
    /// many) — neither of which the result rows can see, because a dropped
    /// repo still has a row if the pool wrote one for it.
    #[test]
    fn every_repo_in_the_fleet_is_synced_exactly_once() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let n = 5;
        let ids = fleet(&tmp, &conn, n);

        let (results, observer) = observed(&conn, 4);
        assert_eq!(results.len(), ids.len(), "every repo has a row");

        assert_eq!(
            observer.total(),
            n,
            "each repo's sync ran exactly once — the pool dropped or \
             duplicated one"
        );
        let mut seen = results
            .iter()
            .map(|r| r.repo_id.clone())
            .collect::<Vec<_>>();
        seen.sort();
        seen.dedup();
        assert_eq!(
            seen.len(),
            n,
            "one row per repo, no repo represented twice: {seen:?}"
        );
    }

    /// Results come back in **registry order**, not completion order.
    ///
    /// The property that makes a parallel run diffable against a serial one.
    /// A summary that reorders itself between two runs over the same fleet is
    /// worse than a slow one: "what changed" stops being answerable.
    #[test]
    fn results_come_back_in_registry_order_not_completion_order() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let ids = fleet(&tmp, &conn, 6);
        let expected = registry_order(&conn);
        assert_eq!(
            expected, ids,
            "the fixture is listed in the order it was inserted"
        );

        // A bound of 1 is the old sequential path, and it is the control: the
        // order must be registry order there too, or the assertion below is
        // not measuring what it claims to.
        let serial = sync_all_bounded(&conn, &SyncOptions::default(), &[], 1).unwrap();
        assert_eq!(
            serial.iter().map(|r| r.repo_id.clone()).collect::<Vec<_>>(),
            expected,
            "a serial run is in registry order"
        );

        let parallel = sync_all_bounded(&conn, &SyncOptions::default(), &[], 4).unwrap();
        assert_eq!(
            parallel
                .iter()
                .map(|r| r.repo_id.clone())
                .collect::<Vec<_>>(),
            expected,
            "a parallel run must be in registry order too — completion order \
             would make the summary unreadable run to run"
        );
    }

    /// The `runs` row is written exactly once, with the right exit code.
    ///
    /// `open_run` / `finalize_run` bracket the whole fleet. A pool that
    /// opened a run per worker would leave N-1 of them unfinalised — "still
    /// going" forever — and `ro status` reads `last_synced_at` off the
    /// finished one.
    #[test]
    fn the_run_row_is_written_exactly_once_with_the_right_exit_code() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        fleet(&tmp, &conn, 5);

        let results = sync_all_bounded(&conn, &SyncOptions::default(), &[], 4).unwrap();
        assert_eq!(results.len(), 5, "the fleet ran");

        let runs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE command = 'sync'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(runs, 1, "a fleet of five repos is one sync run, not five");

        let open: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE command = 'sync' AND ended_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(open, 0, "the run is finalised, not left 'still going'");

        let (exit_code, rows): (i64, i64) = conn
            .query_row(
                "SELECT exit_code, (SELECT COUNT(*) FROM sync_results sr \
                 WHERE sr.run_id = runs.id) \
                 FROM runs WHERE command = 'sync' ORDER BY started_at DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            exit_code,
            run_exit_code(&results) as i64,
            "the run row's exit code is the verdict the CLI would print"
        );
        assert_eq!(
            rows, 5,
            "every repo in the fleet has a row under the one run"
        );
    }

    /// Two repos sharing a path do not corrupt each other.
    ///
    /// `repos.local_path` is not unique, so a registry can hold two rows for
    /// one checkout. That was serialised by the loop; under a pool it is two
    /// `git pull`s racing on one `.git/index.lock`. The shared lock is what
    /// restores the ordering.
    #[test]
    fn two_repos_sharing_a_path_do_not_corrupt_each_other() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let ids = shared_path_fleet(&tmp, &conn, 3);

        let results = sync_all_bounded(&conn, &SyncOptions::default(), &[], 3).unwrap();
        assert_eq!(results.len(), 3, "all three rows ran");
        for (id, r) in ids.iter().zip(&results) {
            assert_eq!(
                r.status, "success",
                "repo {id} synced against the shared checkout: {r:?}"
            );
        }

        // The checkout is still a repo, and still in line with the remote.
        // A lost update here would show as a fetch that never happened, or a
        // worktree left dirty by a pull that was interrupted.
        let shared = tmp.path().join("local").join("shared");
        let status = run_git(&shared, &["status", "--porcelain"]);
        assert!(
            status.stdout.is_empty(),
            "the shared worktree is clean after all three rows synced: {}",
            String::from_utf8_lossy(&status.stdout)
        );
        let head = run_git(&shared, &["rev-parse", "HEAD"]);
        let remote_head = run_git(&shared, &["rev-parse", "origin/main"]);
        assert_eq!(
            String::from_utf8_lossy(&head.stdout).trim(),
            String::from_utf8_lossy(&remote_head.stdout).trim(),
            "the shared checkout is in line with its remote"
        );

        // And the audit trail has one row per repo, not one per path — the
        // lock serialises the work, it does not collapse the rows.
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM sync_results", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 3, "each of the three rows recorded its own result");
    }

    /// The audit row keeps the reason a repo was skipped, which the returned
    /// result deliberately does not carry.
    ///
    /// `skipped_dirty` and `skipped_unpushed` put their explanation in the
    /// row's `error` column and leave `SyncResult::error` empty — neither is
    /// an error, and a caller reading `error` should not find one. Those are
    /// the only two rows that are not derivable from the result they return.
    ///
    /// This test exists because the pool broke that and **nothing noticed**:
    /// the coordinator was re-deriving every row from its result, which wrote
    /// a `NULL` where the reason used to be — on exactly the two rows where
    /// the reason is the whole point. A green suite, a quieter audit trail,
    /// and no failure anywhere. The assertion is on the column, not on the
    /// status, because the status was always right.
    #[test]
    fn a_skipped_row_keeps_its_reason_in_the_audit_trail() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let remote = init_bare_remote(tmp.path());
        commit_to_remote(tmp.path(), &remote, "a.txt", "base");

        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let clone = run_git(&work, &["clone", &remote.to_string_lossy(), "."]);
        assert!(clone.status.success(), "the fixture clones");

        // Uncommitted work, so the sync skips rather than clobbering it.
        std::fs::write(work.join("dirty.txt"), "unsaved work\n").unwrap();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES ('r1', 'github.com', 'fleet', 'dirty', ?1, ?2, ?3, ?3)",
            params![
                remote.to_string_lossy().to_string(),
                work.to_string_lossy().to_string(),
                now
            ],
        )
        .unwrap();

        let results = sync_all_bounded(&conn, &SyncOptions::default(), &[], 1).unwrap();
        assert_eq!(results[0].action, "skipped_dirty", "the repo is skipped");

        // The reason reaches a **reader**, not only the database.
        //
        // The row has always carried it; the returned result did not, and
        // every renderer — text, json, ndjson — reads the result. So a plain
        // `ro sync` printed the word `skipped_dirty` and nothing else, while
        // a query over `sync_results` showed "(use --autostash)". The action
        // the user is told to take existed and was shown to nobody.
        let reason = results[0]
            .reason
            .as_deref()
            .unwrap_or_else(|| panic!("no reason reached the reader: {:?}", results[0]));
        assert!(
            reason.contains("--autostash"),
            "the reason must name the flag that changes the outcome; got {reason:?}"
        );
        assert!(
            results[0].error.is_none(),
            "a skip is not an error; `error` must stay empty: {:?}",
            results[0]
        );

        // The row is where the reason has always survived too. A reader of
        // `ro status` or a query over `sync_results` sees this string and
        // knows what to do about it.
        let recorded: Option<String> = conn
            .query_row(
                "SELECT error FROM sync_results WHERE repo_id = 'r1'",
                [],
                |r| r.get(0),
            )
            .expect("the row was written");
        let recorded = recorded.unwrap_or_default();
        assert!(
            recorded.contains("uncommitted change") && recorded.contains("--autostash"),
            "the audit row must keep the reason a repo was skipped — it is the \
             only place that reason survives, and the result deliberately does \
             not carry it. Got: {recorded:?}"
        );
    }

    /// Two rows naming one checkout are **serialised**, not merely both
    /// allowed to run.
    ///
    /// The test above asserts the outcome is right; this one asserts the
    /// ordering, which is the thing the lock actually buys. Two `git pull`s in
    /// one worktree usually *both succeed* — git's own index lock serialises
    /// them at the last moment — so an outcome-only test passes on a build
    /// with no lock at all. That is why this asserts on the observer: the
    /// peak for a shared path must be 1, because the second row cannot start
    /// until the first has released the lock.
    #[test]
    fn two_rows_sharing_a_path_are_serialised_not_merely_both_allowed() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let ids = shared_path_fleet(&tmp, &conn, 2);

        let (results, observer) = observed(&conn, 2);
        assert_eq!(results.len(), ids.len(), "both rows ran");
        for r in &results {
            assert_eq!(r.status, "success", "both rows synced: {r:?}");
        }

        assert_eq!(observer.total(), ids.len(), "both rows were synced");
        assert_eq!(
            observer.peak(),
            1,
            "two rows naming one checkout reached a peak of {} in flight. The \
             shared lock is what makes them serial: without it, both start at \
             once and the second waits on git's own index lock instead of on \
             ro's, which is a race that usually wins and occasionally does not.",
            observer.peak()
        );
    }

    /// A fleet of repos that are not cloned yet clones them all.
    ///
    /// The clone path is the one where a pool pays most — a clone is the
    /// sync's most expensive git call — and the one where a bug in the pool
    /// would be most visible, because a half-written checkout poisons every
    /// later run.
    #[test]
    fn a_fleet_of_missing_repos_is_cloned_concurrently() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let ids = uncloned_fleet(&tmp, &conn, 4);

        let results = sync_all_bounded(&conn, &SyncOptions::default(), &[], 4).unwrap();
        assert_eq!(results.len(), 4, "every repo has a row");
        for (id, r) in ids.iter().zip(&results) {
            assert_eq!(r.action, "clone", "repo {id} was cloned: {r:?}");
            assert_eq!(r.status, "success", "repo {id} cloned cleanly: {r:?}");
            assert!(
                r.post_oid.is_some(),
                "a clone records the oid it landed: {r:?}"
            );
        }

        // Each destination is a real checkout, not a directory with a
        // partial `.git` — the state a clone killed mid-flight leaves behind,
        // which every later "is this repo cloned?" check accepts as cloned.
        for i in 0..4 {
            let local = tmp.path().join("local").join(format!("repo{i}"));
            let head = run_git(&local, &["rev-parse", "HEAD"]);
            assert!(
                head.status.success(),
                "repo{i} is a real checkout after the clone: {}",
                String::from_utf8_lossy(&head.stderr)
            );
        }
    }

    /// A dry run over a fleet records a row per repo and changes nothing.
    ///
    /// The dry run goes down the same path as the real one — same pool, same
    /// order, same audit trail — because a preview that took a different path
    /// than the run it previews would be a preview of something else.
    #[test]
    fn a_dry_run_over_a_fleet_records_every_repo_and_changes_nothing() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let ids = fleet(&tmp, &conn, 4);

        let opts = SyncOptions {
            dry_run: true,
            ..Default::default()
        };
        let results = sync_all_bounded(&conn, &opts, &[], 4).unwrap();
        assert_eq!(results.len(), ids.len(), "every repo has a row");
        for r in &results {
            assert_eq!(
                r.status, STATUS_DRY_RUN,
                "a dry run's row says dry_run: {r:?}"
            );
        }

        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM sync_results", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            rows,
            ids.len() as i64,
            "a dry run is recorded like any other run, so a script can read it"
        );

        // And it changed nothing: the checkout is exactly as it was.
        let local = tmp.path().join("local").join("repo0");
        let status = run_git(&local, &["status", "--porcelain"]);
        assert!(
            status.stdout.is_empty(),
            "a dry run leaves the worktree alone: {}",
            String::from_utf8_lossy(&status.stdout)
        );
    }

    /// The bound is honoured: a fleet larger than the bound does not run all
    /// of them at once.
    ///
    /// The bound exists because each worker holds a checkout open, and a fleet
    /// run that opens all twenty at once starves itself. Asserted on the
    /// number of git calls in flight rather than on the wall clock.
    #[test]
    fn the_bound_is_honoured_a_fleet_larger_than_the_bound_does_not_run_at_once() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let ids = fleet(&tmp, &conn, 6);

        let (results, observer) = observed(&conn, 2);
        assert_eq!(results.len(), ids.len(), "the whole fleet ran");
        for r in &results {
            assert_eq!(r.status, "success", "every repo synced: {r:?}");
        }

        assert_eq!(observer.total(), 6, "every repo in the fleet was synced");
        assert!(
            observer.peak() <= 2,
            "six repos at a bound of two reached a peak of {} in flight. The \
             bound is not a suggestion: each worker holds a checkout open, and \
             a fleet run that opens all of them at once starves itself.",
            observer.peak()
        );
    }

    /// A repo that fails does not take the fleet down with it.
    ///
    /// The pool collects a row per repo rather than aborting on the first
    /// error, which is what `--timeout` already depended on for the wedged
    /// remote and what a fleet of twenty needs for every other kind of
    /// failure.
    #[test]
    fn a_failing_repo_does_not_take_the_fleet_down() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let remote = init_bare_remote(tmp.path());
        commit_to_remote(tmp.path(), &remote, "a.txt", "base");

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // One repo whose origin is a URL that does not resolve to anything.
        // It fails at the fetch, locally, with no network involved.
        let broken = tmp.path().join("local").join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        let clone = run_git(&broken, &["clone", &remote.to_string_lossy(), "."]);
        assert!(clone.status.success(), "the fixture clones");
        run_git(
            &broken,
            &[
                "remote",
                "set-url",
                "origin",
                "https://127.0.0.1:1/nope.git",
            ],
        );

        // And three healthy ones around it.
        for (id, name, path) in [
            ("id-broken", "aaa-broken", &broken),
            ("id-ok1", "bbb-ok1", &tmp.path().join("local").join("ok1")),
            ("id-ok2", "ccc-ok2", &tmp.path().join("local").join("ok2")),
            ("id-ok3", "ddd-ok3", &tmp.path().join("local").join("ok3")),
        ] {
            if !path.join(".git").exists() {
                std::fs::create_dir_all(path).unwrap();
                let c = run_git(path, &["clone", &remote.to_string_lossy(), "."]);
                assert!(c.status.success(), "{name} clones");
            }
            conn.execute(
                "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
                 VALUES (?1, 'github.com', 'fleet', ?2, ?3, ?4, ?5, ?5)",
                params![
                    id,
                    name,
                    remote.to_string_lossy().to_string(),
                    path.to_string_lossy().to_string(),
                    now,
                ],
            )
            .unwrap();
        }

        let results = sync_all_bounded(&conn, &SyncOptions::default(), &[], 4).unwrap();
        assert_eq!(results.len(), 4, "the whole fleet has a row");

        let broken_row = results
            .iter()
            .find(|r| r.repo_id == "id-broken")
            .expect("the broken repo has a row");
        assert_eq!(
            broken_row.status, "error",
            "a repo that cannot reach its origin is an error: {broken_row:?}"
        );

        // The three around it still ran. This is the assertion: a pool that
        // aborted on the first error would have stopped at the broken repo,
        // and the other three would have no rows at all.
        for id in ["id-ok1", "id-ok2", "id-ok3"] {
            let row = results
                .iter()
                .find(|r| r.repo_id == id)
                .unwrap_or_else(|| panic!("{id} has a row"));
            assert_eq!(
                row.status, "success",
                "{id} synced despite the broken repo: {row:?}"
            );
        }

        // And the run's exit code says the fleet is not clean, which is the
        // verdict a script reads.
        assert_eq!(
            run_exit_code(&results),
            1,
            "a fleet with a failing repo does not exit 0"
        );
    }

    /// The predict-then-verify comparison runs on the fleet path too.
    ///
    /// The fleet used to call `sync_repo_inner` directly, skipping the
    /// comparison `sync_repo` makes — so the invariant was checked on the
    /// one-repo path and silently skipped on the twenty-repo path. This is
    /// the test that caught that, and it stays because the two paths are one
    /// function again and this is what proves it.
    #[test]
    fn the_fleet_compares_its_prediction_with_its_outcome() {
        let tmp = TempDir::new().unwrap();
        let conn = ro_state::open_memory().unwrap();
        let black_hole = super::wedged_remote::BlackHoleServer::start();

        let remote = init_bare_remote(tmp.path());
        commit_to_remote(tmp.path(), &remote, "a.txt", "base");

        let wedged = tmp.path().join("local").join("wedged");
        std::fs::create_dir_all(&wedged).unwrap();
        let clone = run_git(&wedged, &["clone", &remote.to_string_lossy(), "."]);
        assert!(clone.status.success(), "the fixture clones");
        run_git(&wedged, &["remote", "set-url", "origin", &black_hole.url()]);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO repos (id, host, owner, name, clone_url, local_path, added_at, updated_at)
             VALUES ('id-wedged', 'github.com', 'fleet', 'wedged', ?1, ?2, ?3, ?3)",
            params![
                remote.to_string_lossy().to_string(),
                wedged.to_string_lossy().to_string(),
                now,
            ],
        )
        .unwrap();

        let results = sync_all_bounded(
            &conn,
            &SyncOptions {
                timeout_secs: 3,
                ..Default::default()
            },
            &[],
            2,
        )
        .unwrap();
        let row = results
            .iter()
            .find(|r| r.repo_id == "id-wedged")
            .expect("the wedged repo has a row");

        assert!(
            row.plan_mismatch.is_some(),
            "the fleet must compare its prediction with its outcome, exactly as \
             the single-repo path does. Got {row:?}"
        );
    }
}

/// How many repo syncs are in flight right now, the most there have
/// been, and how many ran in all.
///
/// This is the shared counter the brief asked for, and it is shared
/// rather than per-thread because "in flight at the same time" is a fact
/// about every thread at once: a per-thread counter reports 1 for a
/// perfectly parallel pool, which is the wrong answer rather than a
/// conservative one.
///
/// A **parameter**, not a `static`. The tests that read it run
/// concurrently in one process, so a global would have each test's
/// numbers overwritten by whichever sibling was running — a test that
/// fails for a reason having nothing to do with the code.
/// [`FleetObserver::none`] is what production passes, and it costs one
/// `Option` test per repo.
#[derive(Debug, Default)]
struct FleetObserver {
    counts: Option<std::sync::Arc<FleetCounts>>,
}

#[derive(Debug, Default)]
struct FleetCounts {
    in_flight: std::sync::atomic::AtomicUsize,
    peak: std::sync::atomic::AtomicUsize,
    total: std::sync::atomic::AtomicUsize,
}

impl FleetObserver {
    /// A run nobody is watching.
    fn none() -> Self {
        Self { counts: None }
    }

    /// A run being watched. What the parallel-fleet tests install.
    #[cfg(test)]
    fn watching() -> Self {
        Self {
            counts: Some(std::sync::Arc::new(FleetCounts::default())),
        }
    }

    /// Record one repo's sync starting, and return a guard that records
    /// it ending.
    fn enter(&self) -> FleetRun<'_> {
        match &self.counts {
            Some(counts) => {
                let now = counts
                    .in_flight
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    + 1;
                counts
                    .total
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                counts
                    .peak
                    .fetch_max(now, std::sync::atomic::Ordering::SeqCst);
                FleetRun {
                    counts: Some(counts.clone()),
                    _owner: std::marker::PhantomData,
                }
            }
            None => FleetRun {
                counts: None,
                _owner: std::marker::PhantomData,
            },
        }
    }

    /// The most syncs ever in flight at the same instant. `0` when
    /// nobody is watching.
    #[cfg(test)]
    fn peak(&self) -> usize {
        self.counts
            .as_ref()
            .map(|c| c.peak.load(std::sync::atomic::Ordering::SeqCst))
            .unwrap_or(0)
    }

    /// How many syncs ran, however many at a time.
    #[cfg(test)]
    fn total(&self) -> usize {
        self.counts
            .as_ref()
            .map(|c| c.total.load(std::sync::atomic::Ordering::SeqCst))
            .unwrap_or(0)
    }
}

/// One repo's sync, in flight. Counts itself down on drop.
///
/// A guard rather than an explicit call at the end, because `sync_one`
/// has a dozen early returns and the one that forgets would leave the
/// counter permanently raised — turning every later measurement into a
/// fiction.
struct FleetRun<'a> {
    counts: Option<std::sync::Arc<FleetCounts>>,
    _owner: std::marker::PhantomData<&'a ()>,
}

impl Drop for FleetRun<'_> {
    fn drop(&mut self) {
        if let Some(counts) = &self.counts {
            counts
                .in_flight
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}
