//! Tokio-vs-Tina graceful drain.
//!
//! A server-shaped worker accepts up to [`QUEUE_CAPACITY`] queued
//! jobs and processes them at one per [`JOB_WORK_MS`]. The driver
//! fires a burst of [`BURST_JOBS`] submissions, then asks for
//! shutdown. A graceful drain means: no new admissions after
//! shutdown, but every already-admitted job runs to completion.
//!
//! What we are looking at:
//!
//! - **Tokio**: bounded `mpsc` + a consumer task with `tokio::select!`
//!   on `recv()` vs a `oneshot` shutdown receiver. After shutdown,
//!   the consumer keeps draining the channel until empty, then exits.
//!   Outstanding work shape: "is the queue empty *and* is the
//!   shutdown signal set?" — composed in user code.
//! - **Tina**: shutdown is a normal message in the same bounded
//!   mailbox the jobs use. The worker handles `Submit / Tick(reply) /
//!   Drain`. After `Drain` arrives the worker stops admitting via a
//!   bool; once `pending == 0`, `stop_with(report)` publishes the
//!   typed final value.
//!
//! Both sides produce the same [`Report`]: every admitted job must
//! finish; `items_full` (admission rejected at producer) is nonzero
//! because the burst exceeds the mailbox; `shutdown_observed = true`.

pub mod tina_impl;
pub mod tokio_impl;

/// Burst size from the producer.
pub const BURST_JOBS: u32 = 16;

/// Capacity of the worker's mailbox / channel. The burst exceeds
/// this so admission rejection is visible.
pub const QUEUE_CAPACITY: usize = 4;

/// Per-job processing time. Drives the rate at which the worker
/// drains.
pub const JOB_WORK_MS: u64 = 5;

/// What each side observed end-to-end.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// Submissions the worker accepted into its queue.
    pub items_admitted: u32,
    /// Submissions rejected at the producer because the queue was
    /// full.
    pub items_full: u32,
    /// Jobs the worker fully processed (drained).
    pub items_processed: u32,
    /// Whether the worker observed the shutdown signal.
    pub shutdown_observed: bool,
    /// Whether the run finished cleanly.
    pub exit_clean: bool,
}

/// Asserts the structural invariants both sides must satisfy. Exact
/// admit/full split is timing-sensitive; the assertions check shape.
pub fn assert_report_invariants(side: &str, report: &Report) {
    assert_eq!(
        report.items_admitted + report.items_full,
        BURST_JOBS,
        "{side}: every burst job should be accounted for, got {report:?}",
    );
    assert!(
        report.items_full > 0,
        "{side}: expected at least one admission rejection, got {report:?}",
    );
    assert_eq!(
        report.items_processed, report.items_admitted,
        "{side}: drain must process every admitted job, got {report:?}",
    );
    assert!(
        report.shutdown_observed,
        "{side}: worker should have observed shutdown, got {report:?}",
    );
    assert!(
        report.exit_clean,
        "{side}: expected exit_clean, got {report:?}"
    );
}
