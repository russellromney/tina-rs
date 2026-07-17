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
//! - **Tina**: a root isolate drives `tina-sqlite-bridge` via
//!   `execute_call` / `query_call`, privately accumulates query and
//!   update metrics, and publishes them once through `stop_with`.
//!   The host claims `observe_result` before start. Point-in-time
//!   database inspection uses the bridge's existing typed query
//!   request; there is no result mutex or poll loop.
//!
//! Both sides start from a fresh temporary database, run the script,
//! and produce the same [`Report`].
//!
//! This specimen does **not** prove DB readiness. It surfaces the
//! shape of the gap between Tina's bounded-runtime story and a sync
//! C-library where the call-into-the-library is the work.

pub mod tina_demo;
pub mod tina_impl;
pub mod tokio_impl;

/// Number of increment operations the script issues.
pub const INCREMENTS: u32 = 50;

/// What each side observed end-to-end.
///
/// Query and update metrics live on the report so the host reads them
/// from the terminal observation path rather than a shared slot.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// Final counter value read from the database.
    pub final_value: u64,
    /// Successful UPDATE steps completed.
    pub updates_ok: u64,
    /// Successful SELECT finalize steps completed.
    pub queries_ok: u64,
    /// Total `rows_changed` observed across successful updates.
    pub rows_changed: u64,
    /// Whether each side reached the end of `run` cleanly.
    pub exit_clean: bool,
}

/// Terminal failure produced by the Tina counter isolate.
///
/// A failed script never returns a [`Report`]. The database/worker layer,
/// runtime delivery layer, and counter protocol layer remain separate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CounterFailure {
    /// The bridge replied with a typed SQLite/worker failure.
    Sqlite(tina_sqlite_bridge::SqliteError),
    /// The runtime could not complete the call to the bridge.
    Delivery(BridgeDeliveryFailure),
    /// The bridge succeeded, but the reply violated the counter protocol.
    Protocol(CounterProtocolFailure),
}

/// Runtime-layer terminal for one bridge call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeDeliveryFailure {
    /// The bridge isolate mailbox could not admit the call.
    Full,
    /// The bridge isolate was closed, stale, or unavailable.
    Closed,
    /// The isolate-call deadline elapsed.
    Timeout,
    /// The runtime rejected the call without an application reply.
    Rejected(tina::CallRejectedReason),
}

/// Counter-specific protocol violation after a successful bridge reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CounterProtocolFailure {
    /// One UPDATE changed a number of rows other than one.
    UnexpectedRowsChanged {
        /// Expected rows changed.
        expected: u64,
        /// Actual rows changed.
        actual: u64,
    },
    /// The final query returned a number of rows other than one.
    UnexpectedRowCount {
        /// Actual row count.
        actual: usize,
    },
    /// The final query's only row returned a number of columns other than one.
    UnexpectedColumnCount {
        /// Actual column count.
        actual: usize,
    },
    /// The final value had the wrong SQLite value kind.
    UnexpectedValueKind {
        /// Actual SQLite value kind.
        actual: SqliteValueKind,
    },
    /// SQLite returned a negative value for the unsigned counter.
    NegativeFinalValue {
        /// Signed value returned by SQLite.
        actual: i64,
    },
}

/// Stable value-kind vocabulary used by [`CounterProtocolFailure`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqliteValueKind {
    Null,
    Integer,
    Real,
    Text,
    Blob,
}

impl std::fmt::Display for CounterFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "sqlite failure: {error}"),
            Self::Delivery(error) => write!(formatter, "bridge delivery failure: {error:?}"),
            Self::Protocol(error) => write!(formatter, "counter protocol failure: {error:?}"),
        }
    }
}

impl std::error::Error for CounterFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Delivery(_) | Self::Protocol(_) => None,
        }
    }
}

/// Expected counts after running the script. Both sides should land
/// on `final_value = INCREMENTS` with matching query/update metrics.
pub fn expected_report() -> Report {
    Report {
        final_value: u64::from(INCREMENTS),
        updates_ok: u64::from(INCREMENTS),
        queries_ok: 1,
        rows_changed: u64::from(INCREMENTS),
        exit_clean: true,
    }
}
