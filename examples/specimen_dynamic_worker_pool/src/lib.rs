//! Tokio-vs-Tina dynamic worker pool with join-all aggregation.
//!
//! A coordinator dynamically spawns [`WORKER_COUNT`] workers, gives
//! each a disjoint slice of [`WORK_VALUES`], and joins their partial
//! sums into one total. The script is fixed so both sides produce
//! the same `Report`.
//!
//! - **Tokio**: `tokio::task::JoinSet`. The coordinator spawns N
//!   tasks, then loops on `join_next()` until every task has
//!   reported.
//! - **Tina**: a `Coordinator` observes N child spawns, then calls each
//!   request-only worker. Each exhaustive call outcome settles one child, so
//!   premature child termination cannot strand the aggregate.
//!
//! What this teaches:
//!
//! - Dynamic spawn ergonomics. The Tokio shape is the standard
//!   `JoinSet` pattern; the Tina shape uses `spawn_observed` plus typed calls.
//! - Partial-failure aggregation as the join semantic. Both sides produce one
//!   `Report`; Tina preserves every spawn and call terminal outcome.

pub mod tina_impl;
pub mod tokio_impl;

/// Workers spawned by the coordinator.
pub const WORKER_COUNT: u32 = 4;

/// Fixed workload. Length must be divisible by [`WORKER_COUNT`].
pub const WORK_VALUES: [u64; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

/// What each side observed end-to-end.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// Number of workers the coordinator received a result from.
    pub results_collected: u32,
    /// Sum of every worker's partial sum.
    pub total_sum: u64,
    /// Child construction requested an invalid zero-capacity mailbox.
    pub spawn_zero_capacity: u32,
    /// Cross-shard child construction could not reach its destination.
    pub spawn_destination_unavailable: u32,
    /// A future non-exhaustive spawn rejection reason.
    pub spawn_other: u32,
    /// A child request could not enter its bounded mailbox.
    pub call_full: u32,
    /// A child stopped before producing its typed reply.
    pub call_closed: u32,
    /// The child request exceeded its mandatory timeout.
    pub call_timeout: u32,
    /// The child address belonged to another system incarnation.
    pub rejected_foreign_system: u32,
    /// The worker returned without settling request authority.
    pub rejected_reply_abandoned: u32,
    /// The worker panicked before settling request authority.
    pub rejected_handler_panicked: u32,
    /// The worker did not support the request message shape.
    pub rejected_unsupported_message: u32,
    /// Whether the run finished cleanly.
    pub exit_clean: bool,
}

/// Expected `Report` under [`WORKER_COUNT`] and [`WORK_VALUES`].
pub fn expected_report() -> Report {
    Report {
        results_collected: WORKER_COUNT,
        total_sum: WORK_VALUES.iter().sum(),
        exit_clean: true,
        ..Report::default()
    }
}

/// Every spawned worker must settle into exactly one visible terminal bucket.
pub fn assert_tina_report_accounted(report: &Report) {
    let settled = report.results_collected
        + report.spawn_zero_capacity
        + report.spawn_destination_unavailable
        + report.spawn_other
        + report.call_full
        + report.call_closed
        + report.call_timeout
        + report.rejected_foreign_system
        + report.rejected_reply_abandoned
        + report.rejected_handler_panicked
        + report.rejected_unsupported_message;
    assert_eq!(settled, WORKER_COUNT, "unsettled worker: {report:?}");
    assert!(
        report.exit_clean,
        "coordinator did not exit cleanly: {report:?}"
    );
}
