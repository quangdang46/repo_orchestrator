//! Shared test fixtures for the ro workspace.
//!
//! Almost every test in PLAN needs a fake `gh`, a fake agent engine, or a
//! bare remote, and several need the *same* ones. Building those ad hoc per
//! test is how a suite ends up green for the wrong reason: each test's
//! fixture answers slightly differently, and the difference is invisible
//! because nothing compares them.
//!
//! So they live here, once.
//!
//! # What is deliberately not here
//!
//! No dependency on any workspace crate. A testkit that imported `ro-github`
//! could not be used to fake `gh` for `ro-github`'s own tests without a
//! dependency cycle, and "just this once" would be a lie the first time two
//! crates needed different fixtures.
//!
//! # One invariant, because it is load-bearing
//!
//! Fixtures spawn `git` by **absolute path**, resolved once by
//! [`git_path`]. They never spell the bare name. `PATH` is process-global
//! and [`shim::TestEnv`] rewrites it deliberately; a bare name is resolved
//! against whatever it happens to be at spawn time, and the whole suite
//! shares one process. That combination is the fixture flake this crate
//! used to have — see the resolver's doc comment for the failure, for the two
//! fixes that do not work, and for the named exception: the agent-engine shim
//! *bodies* still shell out to a bare `git`, which resolves through `PATH` at
//! shim runtime, outside the resolver's reach.

pub mod capture;
pub mod remote;
pub mod shim;
pub mod worktree;

pub use capture::{Captured, contains_pat};
pub use remote::{BareRemote, RemotePair};
pub use shim::{FakeBinary, TestEnv, git_path, path_lock};
pub use worktree::{Worktree, registered_path};
