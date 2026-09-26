//! Durable job execution for ro.
//!
//! Jobs, runs, run events, failure classification.
//! Every mutating command creates a run record.

pub mod events;
pub mod failure;
pub mod run;

// Re-exports for ergonomic public API
pub use events::{EventLevel, RunEvent, append_event, events_for_run, list_events, recent_events};
pub use failure::{
    FailureClass, FailureRecord, classify_exit, clear_failures, get_failure, list_failures,
    record_failure,
};
pub use run::{RunRecord, finalize_run, get_run, open_run, open_runs, recent_runs};
