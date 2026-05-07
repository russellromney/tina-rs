//! Bridge metrics. Plain atomic counters; no time-series, no labels.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Snapshot of bridge counters.
///
/// `worker_*` counters describe what the blocking worker observed.
/// Admission counters describe what the bridge rejected before the
/// worker saw the request. `late_results` counts worker outcomes that
/// landed after the bridge had already surfaced
/// [`crate::SqliteError::Timeout`] — also visible in the runtime trace
/// as `CallReplyRejected`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SqliteMetrics {
    /// Admitted into the worker thread.
    pub admitted: u64,
    /// Rejected at admission: `max_in_flight` saturated.
    pub full: u64,
    /// Rejected at admission: worker closed.
    pub closed: u64,
    /// Rejected at admission: request validation (params, empty sql).
    pub invalid: u64,
    /// Bridge per-attempt deadline fired before the worker replied.
    /// Worker thread keeps running regardless.
    pub timeouts: u64,
    /// Worker terminal: `Execute` ok.
    pub worker_executed: u64,
    /// Worker terminal: `QueryRows` ok.
    pub worker_rows: u64,
    /// Worker terminal: row buffer over cap.
    pub worker_response_too_large: u64,
    /// Worker terminal: `SQLITE_BUSY` / `SQLITE_LOCKED`.
    pub worker_busy: u64,
    /// Worker terminal: constraint violation.
    pub worker_constraint: u64,
    /// Worker terminal: I/O-class SQLite error.
    pub worker_io: u64,
    /// Worker terminal: catch-all SQLite error.
    pub worker_sqlite: u64,
    /// Worker terminal that arrived after the bridge surfaced Timeout.
    pub late_results: u64,
    /// Current in-flight count. `0` or `1`.
    pub current_in_flight: u64,
    /// Highest `current_in_flight` ever observed.
    pub in_flight_high_water: u64,
}

#[derive(Debug, Default)]
pub(crate) struct MetricsInner {
    pub(crate) admitted: AtomicU64,
    pub(crate) full: AtomicU64,
    pub(crate) closed: AtomicU64,
    pub(crate) invalid: AtomicU64,
    pub(crate) timeouts: AtomicU64,
    pub(crate) worker_executed: AtomicU64,
    pub(crate) worker_rows: AtomicU64,
    pub(crate) worker_response_too_large: AtomicU64,
    pub(crate) worker_busy: AtomicU64,
    pub(crate) worker_constraint: AtomicU64,
    pub(crate) worker_io: AtomicU64,
    pub(crate) worker_sqlite: AtomicU64,
    pub(crate) late_results: AtomicU64,
    pub(crate) current_in_flight: AtomicU64,
    pub(crate) in_flight_high_water: AtomicU64,
}

impl MetricsInner {
    pub(crate) fn snapshot(&self) -> SqliteMetrics {
        SqliteMetrics {
            admitted: self.admitted.load(Ordering::Relaxed),
            full: self.full.load(Ordering::Relaxed),
            closed: self.closed.load(Ordering::Relaxed),
            invalid: self.invalid.load(Ordering::Relaxed),
            timeouts: self.timeouts.load(Ordering::Relaxed),
            worker_executed: self.worker_executed.load(Ordering::Relaxed),
            worker_rows: self.worker_rows.load(Ordering::Relaxed),
            worker_response_too_large: self.worker_response_too_large.load(Ordering::Relaxed),
            worker_busy: self.worker_busy.load(Ordering::Relaxed),
            worker_constraint: self.worker_constraint.load(Ordering::Relaxed),
            worker_io: self.worker_io.load(Ordering::Relaxed),
            worker_sqlite: self.worker_sqlite.load(Ordering::Relaxed),
            late_results: self.late_results.load(Ordering::Relaxed),
            current_in_flight: self.current_in_flight.load(Ordering::Relaxed),
            in_flight_high_water: self.in_flight_high_water.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn set_in_flight(&self, current: u64) {
        self.current_in_flight.store(current, Ordering::Relaxed);
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
pub struct SqliteMetricsHandle {
    pub(crate) inner: Arc<MetricsInner>,
}

impl SqliteMetricsHandle {
    /// Returns a fresh snapshot of counter values.
    pub fn snapshot(&self) -> SqliteMetrics {
        self.inner.snapshot()
    }
}
