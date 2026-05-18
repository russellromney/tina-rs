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

/// Pressure report in pool vocabulary.
///
/// `SqliteWorker` with `max_in_flight = 1` is the pool: one
/// connection, one lane, serial admission. This report maps bridge
/// metrics into the same vocabulary as
/// [`tina::pool::PoolPressureReport`] so the serial shape is
/// copyable and comparable.
///
/// `waiters` is always `0` because the bridge does not queue
/// callers — it replies `SqliteError::Full` immediately when
/// `max_in_flight` is saturated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SqlitePressureReport {
    /// Configured `max_in_flight` — pinned to `1` in first form.
    pub capacity: usize,
    /// Slots currently free (`capacity - leased`).
    pub available: usize,
    /// Operations currently in flight (`0` or `1`).
    pub leased: usize,
    /// Callers parked waiting for a slot. Always `0`.
    pub waiters: usize,
    /// Max waiters. Always `0`.
    pub max_waiters: usize,
    /// Cumulative `SqliteError::Full` outcomes.
    pub full_count: u64,
    /// Cumulative `SqliteError::Closed` outcomes.
    pub closed_count: u64,
    /// Cumulative bridge per-attempt timeouts (`SqliteError::Timeout`).
    pub timeout_count: u64,
    /// Cumulative SQLite `SQLITE_BUSY` / `SQLITE_LOCKED`.
    pub busy_count: u64,
    /// Cumulative constraint violations.
    pub constraint_count: u64,
    /// Cumulative late results — worker terminal after bridge timeout.
    pub late_result_count: u64,
    /// Highest in-flight count ever observed (`0` or `1`).
    pub high_water: u64,
}

impl SqlitePressureReport {
    /// `true` when the single slot is in use.
    pub fn is_full(&self) -> bool {
        self.leased >= self.capacity
    }
}

/// Stable surface name for the SQLite bridge pressure surface.
pub const SQLITE_BRIDGE_SURFACE: &str = "sqlite.bridge";

impl From<SqlitePressureReport> for tina_runtime::bridge::BridgePressure {
    fn from(r: SqlitePressureReport) -> Self {
        // SQLite admission is serial: capacity is always 1. This still
        // surfaces honestly through the shared `BridgePressure` shape;
        // dashboards see `capacity=1, current=0|1` instead of
        // "unavailable" or "absent".
        //
        // worker_terminal_count reuses late_result_count — the bridge
        // does not separately expose total worker terminations.
        tina_runtime::bridge::BridgePressure::measured(
            SQLITE_BRIDGE_SURFACE,
            r.capacity,
            r.leased,
            r.high_water,
            r.full_count,
            r.timeout_count,
            r.closed_count,
            r.late_result_count,
            r.late_result_count,
        )
        .expect("sqlite bridge surface name is a valid bridge name")
    }
}

/// Cloneable handle for inspecting bridge metrics from outside the
/// hosting Tina runtime.
#[derive(Debug, Clone)]
pub struct SqliteMetricsHandle {
    pub(crate) inner: Arc<MetricsInner>,
    pub(crate) capacity: usize,
}

impl SqliteMetricsHandle {
    /// Returns a fresh snapshot of counter values.
    pub fn snapshot(&self) -> SqliteMetrics {
        self.inner.snapshot()
    }

    /// Returns a pressure report mapped into pool vocabulary.
    ///
    /// `capacity` is the `max_in_flight` value captured at install time.
    pub fn pressure_report(&self) -> SqlitePressureReport {
        let m = self.inner.snapshot();
        let capacity = self.capacity;
        let leased = m.current_in_flight.min(capacity as u64) as usize;
        SqlitePressureReport {
            capacity,
            available: capacity.saturating_sub(leased),
            leased,
            waiters: 0,
            max_waiters: 0,
            full_count: m.full,
            closed_count: m.closed,
            timeout_count: m.timeouts,
            busy_count: m.worker_busy,
            constraint_count: m.worker_constraint,
            late_result_count: m.late_results,
            high_water: m.in_flight_high_water,
        }
    }
}
