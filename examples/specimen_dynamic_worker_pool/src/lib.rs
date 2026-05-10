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
//! - **Tina**: a `Coordinator` isolate spawns N child `Worker`
//!   isolates via `spawn(ChildDefinition::new(...))` with a bootstrap
//!   `Compute` message. Each child sends its partial sum back to the
//!   coordinator via `send(parent_addr, CoordMsg::WorkerDone(sum))`.
//!   When the coordinator has heard from every child it
//!   `stop_with(report)`.
//!
//! What this teaches:
//!
//! - Dynamic spawn ergonomics. The Tokio shape is the standard
//!   `JoinSet` pattern; the Tina shape uses `ChildDefinition` plus a
//!   parent-address handshake.
//! - Partial-failure aggregation as the join semantic. Both sides
//!   produce one `Report`; an unfinished worker would surface as a
//!   missing partial in the aggregate. (Real partial failure beyond
//!   "we got every reply" is sketched in the README.)

pub mod tina_impl;
pub mod tokio_impl;

/// Workers spawned by the coordinator.
pub const WORKER_COUNT: u32 = 4;

/// Fixed workload. Length must be divisible by [`WORKER_COUNT`].
pub const WORK_VALUES: [u64; 16] = [
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];

/// What each side observed end-to-end.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// Number of workers the coordinator received a result from.
    pub results_collected: u32,
    /// Sum of every worker's partial sum.
    pub total_sum: u64,
    /// Whether the run finished cleanly.
    pub exit_clean: bool,
}

/// Expected `Report` under [`WORKER_COUNT`] and [`WORK_VALUES`].
pub fn expected_report() -> Report {
    Report {
        results_collected: WORKER_COUNT,
        total_sum: WORK_VALUES.iter().sum(),
        exit_clean: true,
    }
}
