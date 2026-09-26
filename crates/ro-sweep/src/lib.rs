//! The safety net: what must be checked before ro writes a commit.
//!
//! `denylist`, `secret_scan` and `quality_gates` are the three checks, and
//! they are here rather than in `ro-engine` on purpose — an engine is
//! handed a worktree, and these are the checks that run *before* an engine
//! is ever handed one. `ro-engine` owns what to do; this crate owns what
//! must be true first.
//!
//! The crate keeps its name. Renaming it to `ro-safety` would be churn
//! that buys nothing and breaks the workspace path in a dozen places.

pub mod denylist;
pub mod quality_gates;
pub mod secret_scan;

pub use denylist::{DEFAULT_DENYLIST, Denylist};
pub use quality_gates::{
    Ecosystem, Gate, GateResult, GateStatus, any_failed, detect, run_all, run_gate,
};
pub use secret_scan::{
    SecretFinding, SecretScanMode, scan_file, scan_files, scan_text, should_block,
};
