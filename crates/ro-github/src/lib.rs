#![recursion_limit = "512"]

//! GitHub API client for ro.
//!
//! Auth discovery, repo lookup, issue/PR/check queries.
//! Rate-limit handling with backoff.

pub mod auth;
pub mod checks;
pub mod client;
pub mod issues;

pub use auth::{AuthToken, build_client, discover_token};
pub use checks::{
    CheckConclusion, CheckResult, CheckSource, CheckStatus, list_check_runs, list_workflow_runs,
};
pub use client::{RepoMetadata, RepoVisibility, fetch_repo};
pub use issues::{IssueOrPr, ItemKind, ListOpts, list_issues, list_pulls};
