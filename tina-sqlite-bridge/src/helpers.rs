//! Caller-side helpers: type aliases, raw and typed call shorthands,
//! and outcome classification.
//!
//! The bridge has two error layers:
//!
//! - **Bridge delivery**: `CallOutcome::Full | Closed | Timeout` is what
//!   the runtime says about the IsolateCall — could it reach the worker,
//!   did the IsolateCall deadline elapse.
//! - **Worker outcome**: [`crate::SqliteError`] is what the worker says
//!   about the request it accepted.
//!
//! Both layers stay distinct in the default reply shape:
//!
//! ```ignore
//! AppMsg::DbDone(SqliteCallOutcome)
//! // CallOutcome::Replied(Ok(SqliteResponse::Executed { .. }))
//! // CallOutcome::Replied(Err(SqliteError::Constraint(_)))
//! // CallOutcome::Full | Closed | Timeout
//! ```
//!
//! # Two paths
//!
//! - [`send_request`] is the **raw, full-truth** path: the reply is
//!   `CallOutcome<Result<SqliteResponse, SqliteError>>`. Use it when
//!   you want the response enum visible at the call site.
//! - [`execute_call`] / [`query_call`] are **typed shorthands** that
//!   project the response enum away when the request shape already
//!   says which arm to expect. `execute_call` reply is
//!   `CallOutcome<Result<u64, SqliteError>>`; `query_call` is
//!   `CallOutcome<Result<SqliteRows, SqliteError>>`. If the worker
//!   somehow returns the wrong arm (e.g. SQL with a row-producing
//!   statement passed to `execute_call`), the projection reports
//!   [`SqliteError::Internal`] — the bridge does not lie about
//!   shape.

use std::time::Duration;

use tina::Address;
use tina::Effect;
use tina::Isolate;
use tina_runtime::{CallOutcome, IsolateCall, RuntimeCall, call};

use crate::SqliteError;
use crate::types::{SqliteRequest, SqliteResponse, SqliteValue};
use crate::worker::SqliteMsg;

/// Worker reply type. Same as the inner `Result` carried inside a
/// [`CallOutcome`].
pub type SqliteResult = Result<SqliteResponse, SqliteError>;

/// Tina address shape for a registered SQLite worker. Use this in
/// isolate fields:
///
/// ```ignore
/// struct App { db: SqliteAddress, ... }
/// ```
pub type SqliteAddress = Address<SqliteMsg, SqliteResult>;

/// Full reply shape: `CallOutcome<Result<SqliteResponse, SqliteError>>`.
pub type SqliteCallOutcome = CallOutcome<SqliteResult>;

/// Reply shape from [`execute_call`].
pub type SqliteExecutedOutcome = CallOutcome<Result<u64, SqliteError>>;

/// Reply shape from [`query_call`].
pub type SqliteRowsOutcome = CallOutcome<Result<SqliteRows, SqliteError>>;

/// Buffered query result. Carried inside [`SqliteRowsOutcome`].
#[derive(Debug, Clone, PartialEq)]
pub struct SqliteRows {
    /// Column names in result order.
    pub columns: Vec<String>,
    /// Buffered rows. Cells match column order.
    pub rows: Vec<Vec<SqliteValue>>,
}

// ---------------------------------------------------------------------------
// Raw path: full-truth IsolateCall.
// ---------------------------------------------------------------------------

/// Submit one SQLite request through a registered worker.
///
/// Thin wrapper over `tina_runtime::call(addr, SqliteMsg::Request(req),
/// timeout)`. The reply preserves the response enum and both error
/// layers.
pub fn send_request(
    addr: SqliteAddress,
    request: SqliteRequest,
    timeout: Duration,
) -> IsolateCall<SqliteMsg, SqliteResult> {
    call(addr, SqliteMsg::Request(request), timeout)
}

// ---------------------------------------------------------------------------
// Typed shorthands: project the response enum away.
// ---------------------------------------------------------------------------

/// Prepared `Execute` call. Use [`Self::reply`] to fold it into a
/// continuation message of your isolate's message type.
pub struct ExecuteCall {
    inner: IsolateCall<SqliteMsg, SqliteResult>,
}

impl ExecuteCall {
    /// Turn this prepared call into one continuation message.
    pub fn reply<I, F, M>(self, translator: F) -> Effect<I>
    where
        I: Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(SqliteExecutedOutcome) -> M + 'static,
        M: 'static,
    {
        self.inner
            .reply(move |raw| translator(project_executed(raw)))
    }
}

/// Prepared `QueryRows` call.
pub struct QueryCall {
    inner: IsolateCall<SqliteMsg, SqliteResult>,
}

impl QueryCall {
    /// Turn this prepared call into one continuation message.
    pub fn reply<I, F, M>(self, translator: F) -> Effect<I>
    where
        I: Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(SqliteRowsOutcome) -> M + 'static,
        M: 'static,
    {
        self.inner.reply(move |raw| translator(project_rows(raw)))
    }
}

/// Build an `Execute` call with the typed reply shape
/// `CallOutcome<Result<u64, SqliteError>>`. Same admission, same
/// failure surface; only the response enum is projected away.
///
/// ```ignore
/// AppMsg::Start => execute_call(
///     self.db,
///     SqliteRequest::execute("UPDATE counter SET value = value + 1")
///         .param(self.id),
///     Duration::from_secs(2),
/// )
/// .reply(AppMsg::Updated),
///
/// AppMsg::Updated(outcome) => match outcome {
///     CallOutcome::Replied(Ok(rows_changed)) => { ... }
///     CallOutcome::Replied(Err(e)) => { ... }
///     CallOutcome::Full | CallOutcome::Closed | CallOutcome::Timeout => { ... }
/// }
/// ```
pub fn execute_call(addr: SqliteAddress, request: SqliteRequest, timeout: Duration) -> ExecuteCall {
    ExecuteCall {
        inner: send_request(addr, request, timeout),
    }
}

/// Build a `QueryRows` call with the typed reply shape
/// `CallOutcome<Result<SqliteRows, SqliteError>>`.
pub fn query_call(addr: SqliteAddress, request: SqliteRequest, timeout: Duration) -> QueryCall {
    QueryCall {
        inner: send_request(addr, request, timeout),
    }
}

fn project_executed(outcome: SqliteCallOutcome) -> SqliteExecutedOutcome {
    match outcome {
        CallOutcome::Replied(Ok(SqliteResponse::Executed { rows_changed })) => {
            CallOutcome::Replied(Ok(rows_changed))
        }
        CallOutcome::Replied(Ok(SqliteResponse::Rows { .. })) => CallOutcome::Replied(Err(
            SqliteError::Internal("execute_call: worker returned Rows response".into()),
        )),
        CallOutcome::Replied(Err(e)) => CallOutcome::Replied(Err(e)),
        CallOutcome::Full => CallOutcome::Full,
        CallOutcome::Closed => CallOutcome::Closed,
        CallOutcome::Timeout => CallOutcome::Timeout,
    }
}

fn project_rows(outcome: SqliteCallOutcome) -> SqliteRowsOutcome {
    match outcome {
        CallOutcome::Replied(Ok(SqliteResponse::Rows { columns, rows })) => {
            CallOutcome::Replied(Ok(SqliteRows { columns, rows }))
        }
        CallOutcome::Replied(Ok(SqliteResponse::Executed { .. })) => CallOutcome::Replied(Err(
            SqliteError::Internal("query_call: worker returned Executed response".into()),
        )),
        CallOutcome::Replied(Err(e)) => CallOutcome::Replied(Err(e)),
        CallOutcome::Full => CallOutcome::Full,
        CallOutcome::Closed => CallOutcome::Closed,
        CallOutcome::Timeout => CallOutcome::Timeout,
    }
}

// ---------------------------------------------------------------------------
// Outcome classification: Succeeded / Transient / Fatal.
// ---------------------------------------------------------------------------

/// Three-way classification of a [`SqliteCallOutcome`].
///
/// Caller-side retry loops typically only care about three buckets:
/// did the call succeed, was the failure transient (worth retrying),
/// or fatal? Match against this.
///
/// **The classifier does not retry.** It does not know your idempotency
/// rules, your retry budget, or your backoff. It just labels each
/// outcome.
///
/// # Default policy
///
/// - **Worker `Busy`**: `Transient(Busy)`. SQLite asked us to back off.
/// - **Worker `Timeout`**: `Transient(WorkerTimeout)`.
/// - **Bridge `Timeout`**: `Transient(BridgeTimeout)`.
/// - Everything else is `Fatal(...)`. Retrying without changing the
///   request, the database state, or the bridge config will reproduce.
#[derive(Debug, Clone)]
pub enum SqliteOutcomeClass {
    /// Worker accepted and produced a successful response.
    Succeeded(SqliteResponse),
    /// Failure that retrying the same request might fix.
    Transient(SqliteTransientReason),
    /// Failure that retrying the same request will not fix.
    Fatal(SqliteFatalReason),
}

/// Why the classifier judged the outcome retryable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqliteTransientReason {
    /// `SQLITE_BUSY` / `SQLITE_LOCKED`. SQLite asked us to back off.
    Busy,
    /// Worker per-attempt deadline elapsed.
    WorkerTimeout,
    /// IsolateCall deadline elapsed before the worker replied.
    BridgeTimeout,
}

/// Why the classifier judged the outcome non-retryable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqliteFatalReason {
    /// Worker rejected admission: `max_in_flight` saturated.
    Full,
    /// Worker has been closed.
    Closed,
    /// Request rejected by validation.
    InvalidRequest(String),
    /// Buffered query response exceeded the configured row cap.
    ResponseTooLarge,
    /// SQLite constraint violation.
    Constraint(String),
    /// I/O-class SQLite error.
    Io(String),
    /// Catch-all SQLite error.
    Sqlite(String),
    /// Bridge invariant failed.
    Internal(String),
    /// Bridge ingress mailbox full (runtime layer).
    BridgeFull,
    /// Bridge target closed or stale (runtime layer).
    BridgeClosed,
}

/// Extension trait that adds [`Self::classify`] to
/// [`SqliteCallOutcome`].
pub trait SqliteOutcomeExt {
    /// Classify the outcome into Succeeded / Transient / Fatal.
    fn classify(self) -> SqliteOutcomeClass;
}

impl SqliteOutcomeExt for SqliteCallOutcome {
    fn classify(self) -> SqliteOutcomeClass {
        match self {
            CallOutcome::Replied(Ok(resp)) => SqliteOutcomeClass::Succeeded(resp),
            CallOutcome::Replied(Err(SqliteError::Busy)) => {
                SqliteOutcomeClass::Transient(SqliteTransientReason::Busy)
            }
            CallOutcome::Replied(Err(SqliteError::Timeout)) => {
                SqliteOutcomeClass::Transient(SqliteTransientReason::WorkerTimeout)
            }
            CallOutcome::Replied(Err(SqliteError::Full)) => {
                SqliteOutcomeClass::Fatal(SqliteFatalReason::Full)
            }
            CallOutcome::Replied(Err(SqliteError::Closed)) => {
                SqliteOutcomeClass::Fatal(SqliteFatalReason::Closed)
            }
            CallOutcome::Replied(Err(SqliteError::InvalidRequest(msg))) => {
                SqliteOutcomeClass::Fatal(SqliteFatalReason::InvalidRequest(msg))
            }
            CallOutcome::Replied(Err(SqliteError::ResponseTooLarge)) => {
                SqliteOutcomeClass::Fatal(SqliteFatalReason::ResponseTooLarge)
            }
            CallOutcome::Replied(Err(SqliteError::Constraint(msg))) => {
                SqliteOutcomeClass::Fatal(SqliteFatalReason::Constraint(msg))
            }
            CallOutcome::Replied(Err(SqliteError::Io(msg))) => {
                SqliteOutcomeClass::Fatal(SqliteFatalReason::Io(msg))
            }
            CallOutcome::Replied(Err(SqliteError::Sqlite(msg))) => {
                SqliteOutcomeClass::Fatal(SqliteFatalReason::Sqlite(msg))
            }
            CallOutcome::Replied(Err(SqliteError::Internal(msg))) => {
                SqliteOutcomeClass::Fatal(SqliteFatalReason::Internal(msg))
            }
            CallOutcome::Timeout => {
                SqliteOutcomeClass::Transient(SqliteTransientReason::BridgeTimeout)
            }
            CallOutcome::Full => SqliteOutcomeClass::Fatal(SqliteFatalReason::BridgeFull),
            CallOutcome::Closed => SqliteOutcomeClass::Fatal(SqliteFatalReason::BridgeClosed),
        }
    }
}
