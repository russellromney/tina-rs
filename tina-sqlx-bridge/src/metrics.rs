//! Bridge metrics. Plain atomic counters; no time-series, no labels.
//!
//! Counters are **worker-terminal**: they describe what the bridge
//! observed about each operation it admitted. They do not describe the
//! caller's view. If the caller's IsolateCall deadline elapsed
//! (`CallOutcome::Timeout`) the bridge does not see it; the runtime
//! drops the eventual reply as `CallReplyRejected` and that truth lives
//! in the trace, not here.
//!
//! `late_results` is a related but distinct signal: the *bridge*
//! per-attempt timeout fired ([`crate::PgError::Timeout`]) and the
//! spawned task later completed anyway. That counter says "Postgres
//! did the work even though we stopped waiting."

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Snapshot of bridge counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PgMetrics {
    /// Admitted into the bridge (a SQLx future was spawned).
    pub admitted: u64,
    /// Rejected at admission: `max_in_flight` saturated.
    pub full: u64,
    /// Rejected at admission: bridge closed.
    pub closed: u64,
    /// Rejected at admission: request validation (params, empty sql).
    pub invalid: u64,
    /// Bridge per-attempt deadline fired before the spawned task
    /// completed. Distinct from `pool_acquire_timeouts`.
    pub timeouts: u64,
    /// SQLx pool returned `PoolTimedOut` for an admitted request.
    pub pool_acquire_timeouts: u64,
    /// SQLx pool was closed when an admitted request asked for a
    /// connection.
    pub pool_closed: u64,
    /// SQLx returned a non-pool error (`Database`, `Io`, `Tls`,
    /// `Configuration`, `Decode`, etc.).
    pub sqlx_errors: u64,
    /// `Execute` produced a successful `rows_affected`.
    pub responses_executed: u64,
    /// `FetchOne` produced exactly one row.
    pub responses_row: u64,
    /// `FetchOne` produced zero rows.
    pub responses_no_rows: u64,
    /// `FetchOne` matched more than one row.
    pub too_many_rows: u64,
    /// `FetchMany` produced a (possibly empty) row buffer.
    pub responses_rows: u64,
    /// `FetchMany` had to truncate at the row cap. Subset of
    /// `responses_rows`.
    pub responses_truncated: u64,
    /// Cumulative count of rows actually returned to callers across
    /// all successful `FetchOne` and `FetchMany` responses.
    pub rows_returned: u64,
    /// Transaction scripts that committed.
    pub transactions_committed: u64,
    /// Transaction scripts that rolled back because a step failed.
    /// Does not count COMMITs that themselves failed (those land as
    /// `sqlx_errors`).
    pub transactions_rolled_back: u64,
    /// Number of `pg_cancel_backend(pid)` cancellations the bridge
    /// fired against the sidecar pool. Counts the *attempt*, not
    /// whether Postgres honored it.
    pub db_cancels_sent: u64,
    /// Row decode failed.
    pub decode_errors: u64,
    /// Worker terminal that landed after the bridge surfaced
    /// `Timeout`. Worker-side truth; not the same as caller-observed
    /// `CallOutcome::Timeout`.
    pub late_results: u64,
    /// Current in-flight count.
    pub in_flight_current: u64,
    /// Highest `in_flight_current` ever observed.
    pub in_flight_high_water: u64,
}

#[derive(Debug, Default)]
pub(crate) struct MetricsInner {
    pub(crate) admitted: AtomicU64,
    pub(crate) full: AtomicU64,
    pub(crate) closed: AtomicU64,
    pub(crate) invalid: AtomicU64,
    pub(crate) timeouts: AtomicU64,
    pub(crate) pool_acquire_timeouts: AtomicU64,
    pub(crate) pool_closed: AtomicU64,
    pub(crate) sqlx_errors: AtomicU64,
    pub(crate) responses_executed: AtomicU64,
    pub(crate) responses_row: AtomicU64,
    pub(crate) responses_no_rows: AtomicU64,
    pub(crate) too_many_rows: AtomicU64,
    pub(crate) responses_rows: AtomicU64,
    pub(crate) responses_truncated: AtomicU64,
    pub(crate) rows_returned: AtomicU64,
    pub(crate) transactions_committed: AtomicU64,
    pub(crate) transactions_rolled_back: AtomicU64,
    pub(crate) db_cancels_sent: AtomicU64,
    pub(crate) decode_errors: AtomicU64,
    pub(crate) late_results: AtomicU64,
    pub(crate) in_flight_current: AtomicU64,
    pub(crate) in_flight_high_water: AtomicU64,
}

impl MetricsInner {
    pub(crate) fn snapshot(&self) -> PgMetrics {
        PgMetrics {
            admitted: self.admitted.load(Ordering::Relaxed),
            full: self.full.load(Ordering::Relaxed),
            closed: self.closed.load(Ordering::Relaxed),
            invalid: self.invalid.load(Ordering::Relaxed),
            timeouts: self.timeouts.load(Ordering::Relaxed),
            pool_acquire_timeouts: self.pool_acquire_timeouts.load(Ordering::Relaxed),
            pool_closed: self.pool_closed.load(Ordering::Relaxed),
            sqlx_errors: self.sqlx_errors.load(Ordering::Relaxed),
            responses_executed: self.responses_executed.load(Ordering::Relaxed),
            responses_row: self.responses_row.load(Ordering::Relaxed),
            responses_no_rows: self.responses_no_rows.load(Ordering::Relaxed),
            too_many_rows: self.too_many_rows.load(Ordering::Relaxed),
            responses_rows: self.responses_rows.load(Ordering::Relaxed),
            responses_truncated: self.responses_truncated.load(Ordering::Relaxed),
            rows_returned: self.rows_returned.load(Ordering::Relaxed),
            transactions_committed: self.transactions_committed.load(Ordering::Relaxed),
            transactions_rolled_back: self.transactions_rolled_back.load(Ordering::Relaxed),
            db_cancels_sent: self.db_cancels_sent.load(Ordering::Relaxed),
            decode_errors: self.decode_errors.load(Ordering::Relaxed),
            late_results: self.late_results.load(Ordering::Relaxed),
            in_flight_current: self.in_flight_current.load(Ordering::Relaxed),
            in_flight_high_water: self.in_flight_high_water.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn set_in_flight(&self, current: u64) {
        self.in_flight_current.store(current, Ordering::Relaxed);
    }

    pub(crate) fn note_in_flight(&self, current: u64) {
        let mut prev = self.in_flight_high_water.load(Ordering::Relaxed);
        while current > prev {
            match self.in_flight_high_water.compare_exchange(
                prev,
                current,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => prev = observed,
            }
        }
    }
}

/// Cloneable handle for inspecting bridge metrics from outside the
/// hosting Tina runtime.
#[derive(Debug, Clone)]
pub struct PgMetricsHandle {
    pub(crate) inner: Arc<MetricsInner>,
}

impl PgMetricsHandle {
    /// Returns a fresh snapshot of counter values.
    pub fn snapshot(&self) -> PgMetrics {
        self.inner.snapshot()
    }
}
