//! Run records — the audit trail `ro sync` leaves behind.
//!
//! This crate used to be a durable job executor: `jobs`, `run_events`, and a
//! `failures` table with its own classification vocabulary. Measured before
//! the removal, seven of the nine exported functions had **zero** callers
//! outside this crate:
//!
//! ```text
//!   append_event      0        get_failure       0
//!   events_for_run    0        list_failures     0
//!   list_events       0        record_failure    0
//!   recent_events     0        classify_exit     0
//!                       clear_failures   0
//! ```
//!
//! What survived is the part with a live caller: `ro sync` opens a run at the
//! start of a batch and finalises it with the fleet's exit code, so a run row
//! exists for every sync. What went is everything that read those rows back —
//! which, with no `ro run` verb, nothing did.
//!
//! The failure vocabulary did not go with it. `FailureClass` lives in
//! `ro-core` and is used by the engine layer, which would otherwise have
//! reached into this crate for a type and pulled `ro-engine -> ro-jobs ->
//! ro-state -> rusqlite` into the graph just to spell an error discriminant.

pub mod run;

pub use run::{RunRecord, finalize_run, open_run};
