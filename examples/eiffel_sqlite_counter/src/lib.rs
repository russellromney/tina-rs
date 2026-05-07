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
//! - **Tina**: there is no `tina-sqlx-bridge` (yet — see ROADMAP
//!   phase 055 "native database first form"). The honest first-form
//!   shape is a single connection isolate that owns a `rusqlite::
//!   Connection` and runs each query *inside* the synchronous
//!   handler. SQLite operations are fast and bounded; for a single-
//!   shard runtime this is acceptable as an adoption shape, but the
//!   isolate's handler does block while the query runs (no `sleep`,
//!   no reply translator gives this back as a `RuntimeCall`).
//!
//! Both sides start from a fresh database, run the script, and
//! produce the same [`Report`].
//!
//! This specimen does **not** prove DB readiness. It surfaces the
//! shape of the gap between Tina's bounded-runtime story and a sync
//! C-library where the call-into-the-library is the work.

pub mod tina_impl;
pub mod tokio_impl;

/// Number of increment operations the script issues.
pub const INCREMENTS: u32 = 50;

/// What each side observed end-to-end.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// Final counter value read from the database.
    pub final_value: u64,
    /// Whether each side reached the end of `run` cleanly.
    pub exit_clean: bool,
}

/// Expected counts after running the script. Both sides should land
/// on `final_value = INCREMENTS`.
pub fn expected_report() -> Report {
    Report {
        final_value: u64::from(INCREMENTS),
        exit_clean: true,
    }
}
