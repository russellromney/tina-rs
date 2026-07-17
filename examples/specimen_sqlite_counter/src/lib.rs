//! Tokio-vs-Tina SQLite counter.
//!
//! Both sides keep a single-row counter in a SQLite database file,
//! increment it [`INCREMENTS`] times, then read the final value back.
//! The point is to compare how each runtime accommodates a sync
//! C-extension library (rusqlite) against its visible-pressure
//! contract.
//!
//! - **Tokio**: rusqlite is sync. The recommended pattern is
//!   `tokio::task::spawn_blocking(...)` for each query. Each call
//!   moves a clone of the connection into a blocking pool thread.
//! - **Tina**: a root isolate drives `tina-sqlite-bridge` via
//!   `execute_call` / `query_call`, privately accumulates query and
//!   update metrics, and publishes them once through `stop_with`.
//!   The host claims `observe_result` before start. Point-in-time
//!   database inspection uses the bridge's existing typed query
//!   request; there is no result mutex or poll loop.
//!
//! Both sides start from a fresh temporary database, run the script,
//! and produce the same [`Report`].
//!
//! This specimen does **not** prove DB readiness. It surfaces the
//! shape of the gap between Tina's bounded-runtime story and a sync
//! C-library where the call-into-the-library is the work.

pub mod tina_demo;
pub mod tina_impl;
pub mod tokio_impl;

/// Number of increment operations the script issues.
pub const INCREMENTS: u32 = 50;

/// What each side observed end-to-end.
///
/// Query and update metrics live on the report so the host reads them
/// from the terminal observation path rather than a shared slot.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// Final counter value read from the database.
    pub final_value: u64,
    /// Successful UPDATE steps completed.
    pub updates_ok: u64,
    /// Successful SELECT finalize steps completed.
    pub queries_ok: u64,
    /// Total `rows_changed` observed across successful updates.
    pub rows_changed: u64,
    /// Whether each side reached the end of `run` cleanly.
    pub exit_clean: bool,
}

/// Expected counts after running the script. Both sides should land
/// on `final_value = INCREMENTS` with matching query/update metrics.
pub fn expected_report() -> Report {
    Report {
        final_value: u64::from(INCREMENTS),
        updates_ok: u64::from(INCREMENTS),
        queries_ok: 1,
        rows_changed: u64::from(INCREMENTS),
        exit_clean: true,
    }
}
