//! Bridge metrics. Plain atomic counters; no time-series, no labels.
//!
//! Counters are **worker-terminal**: they describe what the bridge
//! observed about each operation it admitted. They do not describe the
//! caller's view. If the caller's IsolateCall deadline elapsed
//! (`CallOutcome::Timeout`) the bridge does not see it; the runtime
//! drops the eventual reply as `CallReplyRejected` and that truth lives
//! in the trace, not here.
//!
//! `caller_waiting_current`, `external_in_flight_current`, and
//! `late_in_flight_current` split the timeout truth: Tina can stop
//! waiting while SQLx/Postgres is still physically running.
//! `late_results` counts those late physical terminals.

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
    /// Does not count COMMITs that themselves failed.
    pub transactions_rolled_back: u64,
    /// Transaction scripts whose steps completed but whose COMMIT
    /// result was ambiguous.
    pub transactions_commit_ambiguous: u64,
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
    /// Current callers still waiting for a bridge reply.
    pub caller_waiting_current: u64,
    /// Current SQLx work still physically in flight. This is the
    /// value enforced by `max_in_flight`.
    pub external_in_flight_current: u64,
    /// Current SQLx work still running after the bridge already
    /// surfaced [`crate::PgError::Timeout`] to the caller.
    pub late_in_flight_current: u64,
    /// Current in-flight count. Deprecated alias for
    /// `external_in_flight_current`.
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
    pub(crate) transactions_commit_ambiguous: AtomicU64,
    pub(crate) db_cancels_sent: AtomicU64,
    pub(crate) decode_errors: AtomicU64,
    pub(crate) late_results: AtomicU64,
    pub(crate) caller_waiting_current: AtomicU64,
    pub(crate) external_in_flight_current: AtomicU64,
    pub(crate) late_in_flight_current: AtomicU64,
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
            transactions_commit_ambiguous: self
                .transactions_commit_ambiguous
                .load(Ordering::Relaxed),
            db_cancels_sent: self.db_cancels_sent.load(Ordering::Relaxed),
            decode_errors: self.decode_errors.load(Ordering::Relaxed),
            late_results: self.late_results.load(Ordering::Relaxed),
            caller_waiting_current: self.caller_waiting_current.load(Ordering::Relaxed),
            external_in_flight_current: self.external_in_flight_current.load(Ordering::Relaxed),
            late_in_flight_current: self.late_in_flight_current.load(Ordering::Relaxed),
            in_flight_current: self.in_flight_current.load(Ordering::Relaxed),
            in_flight_high_water: self.in_flight_high_water.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn set_in_flight(&self, current: u64) {
        self.external_in_flight_current
            .store(current, Ordering::Relaxed);
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

    pub(crate) fn note_caller_waiting_admitted(&self) {
        self.caller_waiting_current.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_caller_waiting_terminal(&self) {
        self.caller_waiting_current.fetch_sub(1, Ordering::Relaxed);
    }

    pub(crate) fn note_caller_timed_out_but_external_running(&self) {
        self.caller_waiting_current.fetch_sub(1, Ordering::Relaxed);
        self.late_in_flight_current.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_late_external_terminal(&self) {
        self.late_in_flight_current.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Pressure report in pool vocabulary.
///
/// `PgWorker` with `max_in_flight = N` already acts as the pool:
/// SQLx owns the connections; the bridge bounds admitted query work
/// above that pool. This report maps bridge metrics into the same
/// vocabulary as [`tina::pool::PoolPressureReport`] so callers can
/// observe capacity, current load, and high-water mark through one
/// typed surface.
///
/// `waiters` is the number of admitted callers still waiting for a
/// bridge reply. Late external work after a bridge timeout remains in
/// `leased` until SQLx reaches terminal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PgPressureReport {
    /// Configured `max_in_flight` — the bridge's pool capacity.
    pub capacity: usize,
    /// Slots currently free (`capacity - leased`).
    pub available: usize,
    /// Operations currently in flight.
    pub leased: usize,
    /// Admitted callers currently waiting for a reply.
    pub waiters: usize,
    /// Max waiters the bridge accepts. Always `0`; present for shape
    /// parity.
    pub max_waiters: usize,
    /// Cumulative `PgError::Full` outcomes.
    pub full_count: u64,
    /// Cumulative `PgError::Closed` outcomes.
    pub closed_count: u64,
    /// Cumulative bridge per-attempt timeouts (`PgError::Timeout`).
    pub timeout_count: u64,
    /// Cumulative SQLx pool acquire timeouts
    /// (`PgError::PoolAcquireTimeout`).
    pub pool_acquire_timeout_count: u64,
    /// Cumulative SQL errors (`PgError::Sqlx`).
    pub sql_error_count: u64,
    /// Cumulative decode errors (`PgError::Decode`).
    pub decode_error_count: u64,
    /// Cumulative late results — worker terminal after bridge timeout.
    pub late_result_count: u64,
    /// Highest in-flight count ever observed.
    pub high_water: u64,
}

impl PgPressureReport {
    /// `true` when every slot is in use.
    pub fn is_full(&self) -> bool {
        self.leased >= self.capacity
    }
}

/// Stable surface name for the Postgres bridge pressure surface.
pub const PG_BRIDGE_SURFACE: &str = "pg.bridge";

impl From<PgPressureReport> for tina_runtime::bridge::BridgePressure {
    fn from(r: PgPressureReport) -> Self {
        // worker_terminal_count: bridge measures `late_results`
        // (worker reached terminal after the caller gave up). Use that
        // conservative value; total worker terminations (including
        // ones the caller did observe) are not exposed separately at
        // this layer.
        tina_runtime::bridge::BridgePressure::measured(
            PG_BRIDGE_SURFACE,
            r.capacity,
            r.leased,
            r.high_water,
            r.full_count,
            r.timeout_count.saturating_add(r.pool_acquire_timeout_count),
            r.closed_count,
            r.late_result_count,
            r.late_result_count,
        )
        .expect("pg bridge surface name is a valid bridge name")
    }
}

/// Cloneable handle for inspecting bridge metrics from outside the
/// hosting Tina runtime.
#[derive(Debug, Clone)]
pub struct PgMetricsHandle {
    pub(crate) inner: Arc<MetricsInner>,
    pub(crate) capacity: usize,
}

impl PgMetricsHandle {
    /// Returns a fresh snapshot of counter values.
    pub fn snapshot(&self) -> PgMetrics {
        self.inner.snapshot()
    }

    /// Returns a pressure report mapped into pool vocabulary.
    ///
    /// `capacity` is the `max_in_flight` value captured at install time.
    pub fn pressure_report(&self) -> PgPressureReport {
        let m = self.inner.snapshot();
        let capacity = self.capacity;
        let leased = m.external_in_flight_current.min(capacity as u64) as usize;
        PgPressureReport {
            capacity,
            available: capacity.saturating_sub(leased),
            leased,
            waiters: m.caller_waiting_current.min(usize::MAX as u64) as usize,
            max_waiters: 0,
            full_count: m.full,
            closed_count: m.closed,
            timeout_count: m.timeouts,
            pool_acquire_timeout_count: m.pool_acquire_timeouts,
            sql_error_count: m.sqlx_errors,
            decode_error_count: m.decode_errors,
            late_result_count: m.late_results,
            high_water: m.in_flight_high_water,
        }
    }
}
