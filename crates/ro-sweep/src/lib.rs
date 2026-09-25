//! Sweep operations for ro.
//!
//! sweep commit: scan denylist → secret scan → quality gates → commit/push
//! sweep agent: AI-driven sweep with plan/apply cycle

pub mod agent;
pub mod commit;
pub mod commit_sweep;
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
