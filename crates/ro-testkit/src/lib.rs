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

pub mod capture;
pub mod remote;
pub mod shim;
pub mod worktree;

pub use capture::{Captured, contains_pat};
pub use remote::{BareRemote, RemotePair};
pub use shim::{FakeBinary, TestEnv, path_lock};
pub use worktree::Worktree;
