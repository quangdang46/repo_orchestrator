//! Git operations for ro.
//!
//! Read queries use `gix` (pure Rust, fast). Mutations use shell-out to
//! `git` for full ref/refspec/credential support. All mutations acquire
//! an `fs4` lock first.

pub mod conflict;
pub mod lock;
pub mod mutation;
pub mod primitives;
pub mod read;
pub mod status;

pub use conflict::{ConflictOp, ConflictState, ConflictedFile};
pub use lock::RepoLock;
pub use mutation::{
    CloneOpts, CloneOutcome, FetchOpts, GitCommandResult, GitError, PullOpts, PullOutcome,
    PullStrategy, PushOpts, RunOpts, run_in,
};
pub use status::{AheadBehind, RepoStatus};
