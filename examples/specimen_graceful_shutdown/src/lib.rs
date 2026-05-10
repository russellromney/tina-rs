//! Tina-vs-Tokio: graceful shutdown after SIGINT. A producer pushes
//! work items at intervals; a consumer drains them. SIGINT arrives
//! partway through; the producer should stop pushing new items, but
//! every item already produced should be processed before exit.
//!
//! Read [`tokio_impl`] and [`tina_impl`] top-to-bottom; the README
//! compares feel.
//!
//! There is no `both` mode in `main.rs`: each side installs its own
//! process-wide signal handler, and running them in the same
//! process means one side's handler fires during the other's run.
//! `cargo test` already runs each smoke test in its own process,
//! which gives the same isolation.

pub mod tina_impl;
pub mod tokio_impl;

/// How many items the producer plans to push.
pub const TOTAL_PLANNED_ITEMS: u32 = 12;

/// Interval between produced items.
pub const ITEM_INTERVAL_MS: u64 = 5;

/// When the simulated SIGINT fires (relative to start).
pub const SIGNAL_AFTER_MS: u64 = 28;

/// What each side observed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub items_produced: u32,
    pub items_processed: u32,
    pub signal_received: bool,
    pub items_remaining_in_queue_at_exit: u32,
    pub exit_clean: bool,
}
