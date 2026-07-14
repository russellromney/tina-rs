//! Tokio-vs-Tina: a scatter/gather coordinator service.
//!
//! Many concurrent client queries land at one coordinator. For each
//! query the coordinator fans out to N workers, gathers per-target
//! results, and replies once with the aggregate. Workers reply out
//! of order; the coordinator must route each aggregate to the right
//! original caller.
//!
//! Read [`tokio_impl`] and [`tina_impl`] top-to-bottom; the README
//! compares feel.

pub mod tina_impl;
pub mod tokio_impl;

/// Number of worker shards behind the coordinator.
pub const WORKERS: usize = 4;
/// Number of concurrent client queries the coordinator must handle.
pub const CLIENTS: usize = 6;
/// Cap on in-flight client queries the coordinator admits at once.
pub const MAX_IN_FLIGHT: usize = 8;

/// One query = one client payload. Each worker contributes
/// `payload + worker_id` so the expected aggregate is deterministic.
pub fn expected_aggregate(payload: u64) -> u64 {
    let mut sum = 0u64;
    for w in 0..WORKERS as u64 {
        sum = sum.wrapping_add(payload.wrapping_add(w));
    }
    sum
}

/// Per-side report. Both sides drive the same shape: every client
/// query must return the same aggregate the lib expects.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub clients: usize,
    pub workers: usize,
    /// Number of queries that returned the expected aggregate.
    pub aggregates_correct: usize,
    /// Number of queries whose returned aggregate did not match.
    pub aggregates_wrong: usize,
    /// Number of queries that failed (timeout / closed / full).
    pub failed: usize,
    /// True when the run drained cleanly with no leaked tasks/isolates.
    pub exit_clean: bool,
    /// Tina-only outer coordinator and start-admission terminals.
    pub tina_terminals: TinaTerminalReport,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TinaTerminalReport {
    pub coordinator_full: usize,
    pub coordinator_mailbox_full: usize,
    pub coordinator_closed: usize,
    pub coordinator_timeout: usize,
    pub coordinator_rejected: Vec<tina::CallRejectedReason>,
    pub start_rejected: Vec<tina_runtime::ScatterGatherStartError<usize>>,
    pub incorrect_aggregates: Vec<IncorrectAggregate>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncorrectAggregate {
    pub expected_targets: usize,
    pub targets: Vec<TargetResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetResult {
    pub expected_target: usize,
    pub actual_target: usize,
    pub outcome: TargetOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetOutcome {
    Replied {
        worker: usize,
        value: u64,
        expected_value: u64,
    },
    Full,
    Closed,
    Timeout,
    Rejected(tina::CallRejectedReason),
    AggregateTimeout,
    MissingShard,
}
