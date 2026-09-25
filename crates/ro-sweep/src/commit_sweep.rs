//! Multi-repo commit sweep.
//!
//! Port of ru's `commit-sweep`: instead of one catch-all commit per repo,
//! changed files are grouped into logical buckets (source / test / doc /
//! config) and each bucket becomes its own conventional commit.
//!
//! Purely deterministic — classification and message generation are rule-based,
//! with no AI or LLM involved.

use anyhow::Result;
use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use ro_git::mutation::{self, PushOpts};

/// Branches that are never committed to or pushed by a sweep.
const PROTECTED_BRANCHES: [&str; 4] = ["main", "master", "production", "staging"];

/// Branches starting with this prefix are protected.
const PROTECTED_PREFIX: &str = "release/";

/// Logical grouping for a changed file. One commit is produced per bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Bucket {
    Source,
    Test,
    Doc,
    Config,
}

impl Bucket {
    /// Classify a repo-relative path into a bucket.
    ///
    /// Port of ru's `cs_classify_file`. Order matters: test beats doc beats
    /// config, and anything unrecognised is source.
    pub fn classify(path: &str) -> Bucket {
        let base = path.rsplit('/').next().unwrap_or(path);

        if is_test(path, base) {
            return Bucket::Test;
        }
        if is_doc(path, base) {
            return Bucket::Doc;
        }
        if is_config(path, base) {
            return Bucket::Config;
        }
        Bucket::Source
    }

    /// Conventional-commit prefix for this bucket.
    ///
    /// Port of ru's `cs_commit_prefix`. Only the `Source` bucket varies by
    /// status code: a new file is a feature, a rename is a refactor, a
    /// deletion is a chore, anything else is a fix.
    pub fn prefix(self, status: &str) -> &'static str {
        match self {
            Bucket::Test => "test",
            Bucket::Doc => "docs",
            Bucket::Config => "chore",
            Bucket::Source => match status.as_bytes().first().copied() {
                Some(b'A') | Some(b'?') => "feat",
                Some(b'R') => "refactor",
                Some(b'D') => "chore",
                _ => "fix",
            },
        }
    }
}

fn is_test(path: &str, base: &str) -> bool {
    const PATH_PATTERNS: [&str; 7] = [
        "test/", "tests/", "spec/", "_test.", "_test/", ".test.", ".spec.",
    ];
    if PATH_PATTERNS.iter().any(|p| path.contains(p)) {
        return true;
    }
    if base.starts_with("test_") {
        return true;
    }
    const BASE_SUFFIXES: [&str; 5] = [
        "_test.rs", "_test.go", "_test.py", "_test.js", "_test.ts",
    ];
    BASE_SUFFIXES.iter().any(|s| base.ends_with(s))
}

fn is_doc(path: &str, base: &str) -> bool {
    const PATH_PREFIXES: [&str; 3] = ["docs/", "doc/", "documentation/"];
    if PATH_PREFIXES.iter().any(|p| path.starts_with(p)) {
        return true;
    }
    const EXTENSIONS: [&str; 4] = [".md", ".rst", ".txt", ".adoc"];
    if EXTENSIONS.iter().any(|e| base.ends_with(e)) {
        return true;
    }
    matches!(
        base,
        "CHANGELOG" | "CHANGES" | "AUTHORS" | "CONTRIBUTORS"
    )
}

fn is_config(path: &str, base: &str) -> bool {
    const PATH_MARKERS: [&str; 8] = [
        ".github/",
        ".circleci/",
        ".travis",
        "Makefile",
        "Justfile",
        "Dockerfile",
        "docker-compose",
        ".dockerignore",
    ];
    if PATH_MARKERS.iter().any(|m| path.contains(m)) {
        return true;
    }
    if path.starts_with(".gitlab-ci") {
        return true;
    }
    const EXTENSIONS: [&str; 7] = [".toml", ".yaml", ".yml", ".json", ".lock", ".cfg", ".ini"];
    if EXTENSIONS.iter().any(|e| base.ends_with(e)) {
        return true;
    }
    base.ends_with(".conf")
        || matches!(
            base,
            ".gitignore" | ".editorconfig"
        )
        || base.starts_with(".prettierrc")
        || base.starts_with(".eslintrc")
}

/// The top-level directory a file lives in, used as the conventional-commit
/// scope. Files at the repository root get the scope `root`.
///
/// Port of ru's `cs_extract_scope`.
fn scope_of(path: &str) -> String {
    match path.split_once('/') {
        Some((top, _)) => top.to_string(),
        None => "root".to_string(),
    }
}

/// Extract a ticket id from a branch name, e.g. `feature/bd-123` or
/// `fix/PROJ-456`. Port of ru's `cs_extract_task_id`.
fn task_id_of(branch: &str) -> Option<String> {
    let re_bd = regex::Regex::new(r"(bd[-_][a-zA-Z0-9_]+)").ok()?;
    if let Some(m) = re_bd.captures(branch) {
        return Some(m[1].to_string());
    }
    let re_ticket = regex::Regex::new(r"([A-Z]+-[0-9]+)").ok()?;
    re_ticket
        .captures(branch)
        .map(|m| m[1].to_string())
}

/// True when the branch must not be committed to or pushed by a sweep.
pub fn is_protected_branch(branch: &str) -> bool {
    PROTECTED_BRANCHES.contains(&branch) || branch.starts_with(PROTECTED_PREFIX)
}

/// One entry of `git status --porcelain=v1 -z`.
struct StatusEntry {
    code: String,
    path: String,
}

/// Parse NUL-delimited porcelain output.
///
/// Renames and copies are two NUL-terminated fields: `XY <old>\0<new>\0`. The
/// destination is the meaningful path, so it wins. Ported from the same
/// NUL-safe loop in ru's `cs_process_repo`.
fn parse_porcelain(raw: &str) -> Vec<StatusEntry> {
    let mut fields = raw.split('\0').filter(|f| !f.is_empty());
    let mut out = Vec::new();

    while let Some(entry) = fields.next() {
        // Every entry is "XY<space>path" — three leading bytes are ASCII, so
        // slicing is safe even when the path itself is not.
        if entry.len() < 3 || !entry.is_char_boundary(2) || !entry.is_char_boundary(3) {
            continue;
        }
        let code = entry[..2].replace(' ', "");
        let mut path = entry[3..].to_string();

        if code.starts_with('R') || code.starts_with('C') {
            if let Some(dest) = fields.next() {
                path = dest.to_string();
            }
        }
        out.push(StatusEntry { code, path });
    }
    out
}

/// Read the worktree status for a repo.
///
/// `-uall` expands untracked directories into individual files. Without it
/// git collapses an untracked directory into a single `dir/` entry, which
/// would commit the whole tree in one opaque step.
fn read_status(repo: &Path) -> Result<Vec<StatusEntry>> {
    let out = mutation::run(repo, &["status", "--porcelain=v1", "-z", "-uall"])?;
    if out.status != 0 {
        anyhow::bail!("git status failed: {}", out.stderr.trim());
    }
    Ok(parse_porcelain(&out.stdout))
}

/// Files already staged by the user, kept as their own group when
/// `respect_staging` is set.
fn read_staged(repo: &Path) -> Result<Vec<String>> {
    let out = mutation::run(repo, &["diff", "--cached", "--name-only", "-z"])?;
    if out.status != 0 {
        return Ok(Vec::new());
    }
    Ok(out
        .stdout
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect())
}

/// A commit that will be (or was) made for one bucket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedCommit {
    pub bucket: Bucket,
    pub message: String,
    pub files: Vec<String>,
}

/// Why a repo was excluded from the sweep.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum SkipReason {
    /// Worktree is clean.
    Clean,
    /// On a branch the sweep refuses to touch.
    ProtectedBranch { branch: String },
    /// Every changed file was denied or already staged elsewhere.
    NothingCommittable,
}

/// The full commit plan for one repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoPlan {
    pub repo_id: String,
    pub branch: String,
    pub commits: Vec<PlannedCommit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<SkipReason>,
    /// Non-fatal notes, e.g. that a protected branch was force-enabled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl RepoPlan {
    /// Total files across every planned commit.
    pub fn file_count(&self) -> usize {
        self.commits.iter().map(|c| c.files.len()).sum()
    }
}

/// Options controlling a sweep run.
#[derive(Debug, Clone, Default)]
pub struct SweepOptions {
    /// Actually create the commits. When false, plan only.
    pub execute: bool,
    /// Keep manually staged files as a separate `wip` commit.
    pub respect_staging: bool,
    /// Push after a successful commit.
    pub push: bool,
    /// Remote to push to; defaults to the repo's `origin`.
    pub push_remote: Option<String>,
    /// Use `--force-with-lease` instead of a plain push.
    pub force_with_lease: bool,
    /// Commit on protected branches (`main`, `master`, `release/*`, ...)
    /// instead of skipping them. Off by default.
    pub allow_protected: bool,
}

/// Build the commit plan for a single repo without mutating anything.
pub fn plan_repo(repo: &Path, repo_id: &str, opts: &SweepOptions) -> Result<RepoPlan> {
    let branch = current_branch(repo)?;
    let mut warnings: Vec<String> = Vec::new();

    if is_protected_branch(&branch) {
        if !opts.allow_protected {
            return Ok(RepoPlan {
                repo_id: repo_id.to_string(),
                branch: branch.clone(),
                commits: Vec::new(),
                skipped: Some(SkipReason::ProtectedBranch { branch }),
                warnings,
            });
        }
        warnings.push(format!(
            "protected branch '{branch}' — committing because --allow-protected was set"
        ));
    }

    let entries = read_status(repo)?;
    if entries.is_empty() {
        return Ok(RepoPlan {
            repo_id: repo_id.to_string(),
            branch,
            commits: Vec::new(),
            skipped: Some(SkipReason::Clean),
            warnings,
        });
    }

    let staged = if opts.respect_staging {
        read_staged(repo)?
    } else {
        Vec::new()
    };
    let denylist = crate::Denylist::new_default()?;

    // git does not descend into a nested repository, so it reports the whole
    // thing as a single `dir/` entry. Committing that would add a broken
    // gitlink, so skip it and say so.
    let mut nested: Vec<String> = Vec::new();
    let entries: Vec<StatusEntry> = entries
        .into_iter()
        .filter(|e| {
            if e.path.ends_with('/') {
                nested.push(e.path.trim_end_matches('/').to_string());
                false
            } else {
                true
            }
        })
        .collect();
    for dir in &nested {
        warnings.push(format!(
            "skipped nested repository or untracked directory: {dir}/"
        ));
    }

    let mut commits: Vec<PlannedCommit> = Vec::new();
    let mut by_bucket: BTreeMap<Bucket, (Vec<String>, String)> = BTreeMap::new();

    if !staged.is_empty() {
        let kept: Vec<String> = staged
            .iter()
            .filter(|p| !denylist.is_denied(p.as_str()))
            .cloned()
            .collect();
        if !kept.is_empty() {
            commits.push(PlannedCommit {
                bucket: Bucket::Source,
                message: format!("wip: manually staged changes ({} file(s))", kept.len()),
                files: kept,
            });
        }
    }

    for entry in entries {
        if opts.respect_staging && staged.contains(&entry.path) {
            continue;
        }
        if denylist.is_denied(&entry.path) {
            continue;
        }
        let bucket = Bucket::classify(&entry.path);
        let slot = by_bucket.entry(bucket).or_default();
        slot.0.push(entry.path);
        if slot.1.is_empty() {
            slot.1 = entry.code;
        }
    }

    let task = task_id_of(&branch);
    for (bucket, (files, code)) in by_bucket {
        if files.is_empty() {
            continue;
        }
        let scope = scope_of(&files[0]);
        let mut message = format!("{}({}): update {} {}", bucket.prefix(&code), scope, scope, bucket_name(bucket));
        if let Some(t) = &task {
            message.push_str(&format!(" ({t})"));
        }
        commits.push(PlannedCommit {
            bucket,
            message,
            files,
        });
    }

    let skipped = if commits.is_empty() {
        Some(SkipReason::NothingCommittable)
    } else {
        None
    };

    Ok(RepoPlan {
        repo_id: repo_id.to_string(),
        branch,
        commits,
        skipped,
        warnings,
    })
}

fn bucket_name(bucket: Bucket) -> &'static str {
    match bucket {
        Bucket::Source => "source",
        Bucket::Test => "test",
        Bucket::Doc => "doc",
        Bucket::Config => "config",
    }
}

/// Result of applying a plan to one repo.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RepoOutcome {
    pub repo_id: String,
    pub committed: u32,
    pub failed: u32,
    pub pushed: bool,
    pub push_error: Option<String>,
}

/// Verify a push is safe to attempt.
///
/// The three gates, in order: the caller must have opted in, the branch must
/// not be protected, and the remote must actually exist. Push is never
/// attempted if any gate fails.
pub fn verify_push_safe(repo: &Path, branch: &str, remote: &str) -> Result<()> {
    if is_protected_branch(branch) {
        anyhow::bail!("refusing to push protected branch '{branch}'");
    }
    let out = mutation::run(repo, &["remote", "get-url", remote])?;
    if out.status != 0 {
        anyhow::bail!("remote '{remote}' not configured");
    }
    Ok(())
}

/// Execute a plan: one commit per bucket, then optionally push.
pub fn apply_repo(repo: &Path, plan: &RepoPlan, opts: &SweepOptions) -> RepoOutcome {
    let mut outcome = RepoOutcome {
        repo_id: plan.repo_id.clone(),
        ..Default::default()
    };

    for planned in &plan.commits {
        let files: Vec<std::path::PathBuf> =
            planned.files.iter().map(std::path::PathBuf::from).collect();
        match mutation::commit(repo, &files, &planned.message) {
            Ok(_) => outcome.committed += 1,
            Err(e) => {
                outcome.failed += 1;
                tracing::warn!(
                    repo = %plan.repo_id,
                    error = %e,
                    "commit failed for bucket"
                );
            }
        }
    }

    if opts.push && outcome.failed == 0 && outcome.committed > 0 {
        let remote = opts.push_remote.as_deref().unwrap_or("origin");
        match verify_push_safe(repo, &plan.branch, remote) {
            Ok(()) => {
                let popts = PushOpts {
                    remote: Some(remote.to_string()),
                    branch: Some(plan.branch.clone()),
                    force_with_lease: opts.force_with_lease,
                    set_upstream: false,
                    tags: false,
                };
                match mutation::push(repo, &popts) {
                    Ok(res) if res.status == 0 => outcome.pushed = true,
                    Ok(res) => {
                        outcome.push_error = Some(format!(
                            "push rejected: {}",
                            first_line(&res.stderr)
                        ));
                    }
                    Err(e) => outcome.push_error = Some(format!("push failed: {e}")),
                }
            }
            Err(e) => outcome.push_error = Some(e.to_string()),
        }
    }

    outcome
}

fn first_line(s: &str) -> String {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("unknown")
        .trim()
        .to_string()
}

/// Current branch name, or `HEAD` when detached.
pub fn current_branch(repo: &Path) -> Result<String> {
    let out = mutation::run(repo, &["symbolic-ref", "--short", "HEAD"])?;
    if out.status == 0 {
        let b = out.stdout.trim();
        if !b.is_empty() {
            return Ok(b.to_string());
        }
    }
    Ok("HEAD".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_path_and_extension() {
        assert_eq!(Bucket::classify("src/main.rs"), Bucket::Source);
        assert_eq!(Bucket::classify("crates/foo/src/lib.rs"), Bucket::Source);
        assert_eq!(Bucket::classify("tests/integration.rs"), Bucket::Test);
        assert_eq!(Bucket::classify("src/parser_test.rs"), Bucket::Test);
        assert_eq!(Bucket::classify("app/foo.test.ts"), Bucket::Test);
        assert_eq!(Bucket::classify("README.md"), Bucket::Doc);
        assert_eq!(Bucket::classify("docs/guide.md"), Bucket::Doc);
        assert_eq!(Bucket::classify("CHANGELOG"), Bucket::Doc);
        assert_eq!(Bucket::classify("Cargo.toml"), Bucket::Config);
        assert_eq!(Bucket::classify(".github/workflows/ci.yml"), Bucket::Config);
        assert_eq!(Bucket::classify("Makefile"), Bucket::Config);
    }

    #[test]
    fn source_prefix_depends_on_status() {
        assert_eq!(Bucket::Source.prefix("A"), "feat");
        assert_eq!(Bucket::Source.prefix("??"), "feat");
        assert_eq!(Bucket::Source.prefix("R"), "refactor");
        assert_eq!(Bucket::Source.prefix("D"), "chore");
        assert_eq!(Bucket::Source.prefix("M"), "fix");
    }

    #[test]
    fn fixed_buckets_ignore_status() {
        assert_eq!(Bucket::Test.prefix("A"), "test");
        assert_eq!(Bucket::Doc.prefix("A"), "docs");
        assert_eq!(Bucket::Config.prefix("A"), "chore");
    }

    #[test]
    fn scope_is_top_level_dir_or_root() {
        assert_eq!(scope_of("crates/ro/src/main.rs"), "crates");
        assert_eq!(scope_of("README.md"), "root");
    }

    #[test]
    fn task_id_extracted_from_branch() {
        assert_eq!(task_id_of("feature/bd-123"), Some("bd-123".to_string()));
        assert_eq!(task_id_of("fix/bd_4f2a"), Some("bd_4f2a".to_string()));
        assert_eq!(task_id_of("feat/PROJ-456"), Some("PROJ-456".to_string()));
        assert_eq!(task_id_of("main"), None);
    }

    #[test]
    fn protected_branches_detected() {
        for b in ["main", "master", "production", "staging", "release/1.0"] {
            assert!(is_protected_branch(b), "{b} should be protected");
        }
        for b in ["feature/x", "fix/y", "maintenance"] {
            assert!(!is_protected_branch(b), "{b} should not be protected");
        }
    }

    #[test]
    fn parses_porcelain_including_renames() {
        let raw = " M src/a.rs\0?? new.txt\0R  old.rs\0crates/new.rs\0";
        let entries = parse_porcelain(raw);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].code, "M");
        assert_eq!(entries[0].path, "src/a.rs");
        assert_eq!(entries[1].code, "??");
        assert_eq!(entries[1].path, "new.txt");
        // rename takes the destination path
        assert_eq!(entries[2].code, "R");
        assert_eq!(entries[2].path, "crates/new.rs");
    }

    #[test]
    fn plan_is_dry_run_and_groups_buckets() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = dir.path();
        mutation::run(repo, &["init", "-q", "-b", "feature/x"]).unwrap();
        mutation::run(repo, &["config", "user.email", "t@example.com"]).unwrap();
        mutation::run(repo, &["config", "user.name", "T"]).unwrap();
        mutation::run(repo, &["config", "commit.gpgSign", "false"]).unwrap();

        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::create_dir_all(repo.join("tests")).unwrap();
        std::fs::write(repo.join("src/lib.rs"), b"fn a() {}\n").unwrap();
        std::fs::write(repo.join("tests/t.rs"), b"#[test]\n").unwrap();
        std::fs::write(repo.join("README.md"), b"# hi\n").unwrap();

        let opts = SweepOptions::default();
        let plan = plan_repo(repo, "o/r", &opts).unwrap();

        assert!(plan.skipped.is_none());
        assert_eq!(plan.commits.len(), 3, "source + test + doc");

        // dry run must not have created any commit
        let log = mutation::run(repo, &["rev-list", "--count", "HEAD"]).unwrap();
        assert_ne!(log.status, 0, "no commit should exist after planning");

        let source = plan
            .commits
            .iter()
            .find(|c| c.bucket == Bucket::Source)
            .unwrap();
        assert!(source.message.starts_with("feat(src):"), "{}", source.message);
    }

    #[test]
    fn protected_branch_yields_no_commits() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = dir.path();
        mutation::run(repo, &["init", "-q", "-b", "main"]).unwrap();
        std::fs::write(repo.join("a.txt"), b"x\n").unwrap();

        let plan = plan_repo(repo, "o/r", &SweepOptions::default()).unwrap();
        assert!(plan.commits.is_empty());
        assert!(matches!(
            plan.skipped,
            Some(SkipReason::ProtectedBranch { .. })
        ));
    }

    #[test]
    fn clean_repo_is_skipped() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = dir.path();
        mutation::run(repo, &["init", "-q", "-b", "feature/x"]).unwrap();

        let plan = plan_repo(repo, "o/r", &SweepOptions::default()).unwrap();
        assert!(matches!(plan.skipped, Some(SkipReason::Clean)));
    }

    #[test]
    fn apply_creates_one_commit_per_bucket() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = dir.path();
        mutation::run(repo, &["init", "-q", "-b", "feature/x"]).unwrap();
        mutation::run(repo, &["config", "user.email", "t@example.com"]).unwrap();
        mutation::run(repo, &["config", "user.name", "T"]).unwrap();
        mutation::run(repo, &["config", "commit.gpgSign", "false"]).unwrap();

        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::create_dir_all(repo.join("tests")).unwrap();
        std::fs::write(repo.join("src/lib.rs"), b"fn a() {}\n").unwrap();
        std::fs::write(repo.join("tests/t.rs"), b"#[test]\n").unwrap();

        let opts = SweepOptions {
            execute: true,
            ..Default::default()
        };
        let plan = plan_repo(repo, "o/r", &opts).unwrap();
        let outcome = apply_repo(repo, &plan, &opts);

        assert_eq!(outcome.committed, 2);
        assert_eq!(outcome.failed, 0);
        assert!(!outcome.pushed, "no push requested");

        let count = mutation::run(repo, &["rev-list", "--count", "HEAD"]).unwrap();
        assert_eq!(count.stdout.trim(), "2");
    }
}
