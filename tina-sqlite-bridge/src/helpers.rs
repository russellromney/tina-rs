//! Caller-side helpers: type aliases, raw and typed call shorthands,
//! row accessors, and outcome classification.
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
//! - [`send_request`] is the **raw, full-truth** path: takes a
//!   [`SqliteRequest`], reply is
//!   `CallOutcome<Result<SqliteResponse, SqliteError>>`. Use it when
//!   you want the response enum visible at the call site, or when one
//!   helper must accept either request shape.
//! - [`execute_call`] / [`query_call`] are **typed shorthands** that
//!   take their args directly (sql + params, plus `max_rows` for
//!   queries). The reply projects the response enum away:
//!   `execute_call` is `CallOutcome<Result<u64, SqliteError>>`;
//!   `query_call` is `CallOutcome<Result<SqliteRows, SqliteError>>`.
//!   Mismatched request shape is impossible at the type level.

use std::time::Duration;

use tina::CallAddress;
use tina::Effect;
use tina::Isolate;
use tina_runtime::bridge::{BridgeFatal, BridgeOutcomeClass, BridgeRetryable, BridgeUnavailable};
use tina_runtime::{CallOutcome, IsolateCall, RuntimeCall, call_typed};

use crate::types::{SqliteRequest, SqliteResponse, SqliteValue};
use crate::worker::SqliteMsg;
use crate::{SqliteError, SqliteProtocolError};

/// Worker reply type. Same as the inner `Result` carried inside a
/// [`CallOutcome`].
pub type SqliteResult = Result<SqliteResponse, SqliteError>;

/// Tina address shape for a registered SQLite worker. Use this in
/// isolate fields:
///
/// ```ignore
/// struct App { db: SqliteAddress, ... }
/// ```
///
/// Compile rail: the bridge request lane is callable, not sendable.
///
/// ```compile_fail
/// use tina_sqlite_bridge::{SqliteAddress, SqliteMsg};
///
/// fn wants_send(_addr: tina::SendAddress<SqliteMsg>) {}
/// fn wrong(addr: SqliteAddress) {
///     wants_send(addr);
/// }
/// ```
pub type SqliteAddress = CallAddress<SqliteMsg, SqliteResult>;

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

impl SqliteRows {
    /// Number of buffered rows.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// `true` iff no rows were returned.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Borrow row `i`. None if out of range.
    pub fn row(&self, i: usize) -> Option<Row<'_>> {
        let cells = self.rows.get(i)?;
        Some(Row {
            cells,
            columns: &self.columns,
        })
    }

    /// Iterate rows.
    pub fn iter(&self) -> impl Iterator<Item = Row<'_>> {
        self.rows.iter().map(move |cells| Row {
            cells,
            columns: &self.columns,
        })
    }
}

/// Borrowed view of one row plus its column names.
#[derive(Debug, Clone, Copy)]
pub struct Row<'a> {
    cells: &'a [SqliteValue],
    columns: &'a [String],
}

impl<'a> Row<'a> {
    /// Number of columns.
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// `true` iff no columns.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Borrow cell `idx`. None if out of range.
    pub fn col(&self, idx: usize) -> Option<&'a SqliteValue> {
        self.cells.get(idx)
    }

    /// Borrow the cell whose column has this name. None if no such
    /// column. Linear scan; fine for small result rows.
    pub fn by_name(&self, name: &str) -> Option<&'a SqliteValue> {
        self.columns
            .iter()
            .position(|c| c == name)
            .and_then(|i| self.cells.get(i))
    }
}

// ---------------------------------------------------------------------------
// Raw path: full-truth IsolateCall.
// ---------------------------------------------------------------------------

/// Submit one SQLite request through a registered worker.
///
/// Thin wrapper over `tina_runtime::call_typed(addr, SqliteMsg::Request(req),
/// timeout)`. The reply preserves the response enum and both error
/// layers.
pub fn send_request(
    addr: SqliteAddress,
    request: SqliteRequest,
    timeout: Duration,
) -> IsolateCall<SqliteMsg, SqliteResult> {
    call_typed(addr, SqliteMsg::Request(request), timeout)
}

// ---------------------------------------------------------------------------
// Typed shorthands: request shape known at the call site.
// ---------------------------------------------------------------------------

/// Prepared `Execute` call. Use [`Self::then`] to fold it into a
/// continuation message of your isolate's message type.
pub struct ExecuteCall {
    inner: IsolateCall<SqliteMsg, SqliteResult>,
}

impl std::fmt::Debug for ExecuteCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecuteCall").finish_non_exhaustive()
    }
}

impl ExecuteCall {
    /// Turn this prepared call into one continuation message.
    #[deprecated(since = "0.1.0", note = "use `.then(...)` for ordinary continuations")]
    pub fn reply<I, F, M>(self, translator: F) -> Effect<I>
    where
        I: Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(SqliteExecutedOutcome) -> M + 'static,
        M: 'static,
    {
        self.then(translator)
    }

    /// Turn this prepared call into one continuation message.
    pub fn then<I, F, M>(self, translator: F) -> Effect<I>
    where
        I: Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(SqliteExecutedOutcome) -> M + 'static,
        M: 'static,
    {
        self.inner
            .then(move |raw| translator(project_executed(raw)))
    }
}

/// Prepared `QueryRows` call.
pub struct QueryCall {
    inner: IsolateCall<SqliteMsg, SqliteResult>,
}

impl std::fmt::Debug for QueryCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryCall").finish_non_exhaustive()
    }
}

impl QueryCall {
    /// Turn this prepared call into one continuation message.
    #[deprecated(since = "0.1.0", note = "use `.then(...)` for ordinary continuations")]
    pub fn reply<I, F, M>(self, translator: F) -> Effect<I>
    where
        I: Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(SqliteRowsOutcome) -> M + 'static,
        M: 'static,
    {
        self.then(translator)
    }

    /// Turn this prepared call into one continuation message.
    pub fn then<I, F, M>(self, translator: F) -> Effect<I>
    where
        I: Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(SqliteRowsOutcome) -> M + 'static,
        M: 'static,
    {
        self.inner.then(move |raw| translator(project_rows(raw)))
    }
}

/// Build an `Execute` call with the typed reply shape
/// `CallOutcome<Result<u64, SqliteError>>`. Same admission, same
/// failure surface; only the response enum is projected away.
///
/// ```ignore
/// AppMsg::Start => execute_call(
///     self.db,
///     "UPDATE counter SET value = value + 1 WHERE id = ?",
///     vec![self.id.into()],
///     Duration::from_secs(2),
/// )
/// .then(AppMsg::Updated),
///
/// AppMsg::Updated(outcome) => match outcome {
///     CallOutcome::Replied(Ok(rows_changed)) => { ... }
///     CallOutcome::Replied(Err(e)) => { ... }
///     CallOutcome::Full | CallOutcome::Closed | CallOutcome::Timeout => { ... }
/// }
/// ```
pub fn execute_call(
    addr: SqliteAddress,
    sql: impl Into<String>,
    params: Vec<SqliteValue>,
    timeout: Duration,
) -> ExecuteCall {
    ExecuteCall {
        inner: send_request(
            addr,
            SqliteRequest::Execute {
                sql: sql.into(),
                params,
            },
            timeout,
        ),
    }
}

/// Build a `QueryRows` call with the typed reply shape
/// `CallOutcome<Result<SqliteRows, SqliteError>>`.
pub fn query_call(
    addr: SqliteAddress,
    sql: impl Into<String>,
    params: Vec<SqliteValue>,
    max_rows: usize,
    timeout: Duration,
) -> QueryCall {
    QueryCall {
        inner: send_request(
            addr,
            SqliteRequest::QueryRows {
                sql: sql.into(),
                params,
                max_rows,
            },
            timeout,
        ),
    }
}

fn project_executed(outcome: SqliteCallOutcome) -> SqliteExecutedOutcome {
    match outcome {
        CallOutcome::Replied(Ok(SqliteResponse::Executed { rows_changed })) => {
            CallOutcome::Replied(Ok(rows_changed))
        }
        // Unreachable in practice: `execute_call` only ever sends
        // `SqliteRequest::Execute`, and the worker's `run_request`
        // matches request shape to response shape. Surface
        // a typed protocol failure rather than panic if a future code
        // change breaks that invariant.
        CallOutcome::Replied(Ok(SqliteResponse::Rows { .. })) => CallOutcome::Replied(Err(
            SqliteError::Protocol(SqliteProtocolError::ExecuteReturnedRows),
        )),
        CallOutcome::Replied(Err(e)) => CallOutcome::Replied(Err(e)),
        CallOutcome::Full => CallOutcome::Full,
        CallOutcome::Closed => CallOutcome::Closed,
        CallOutcome::Timeout => CallOutcome::Timeout,
        CallOutcome::Rejected(reason) => CallOutcome::Rejected(reason),
    }
}

fn project_rows(outcome: SqliteCallOutcome) -> SqliteRowsOutcome {
    match outcome {
        CallOutcome::Replied(Ok(SqliteResponse::Rows { columns, rows })) => {
            CallOutcome::Replied(Ok(SqliteRows { columns, rows }))
        }
        // See `project_executed` for the same invariant note.
        CallOutcome::Replied(Ok(SqliteResponse::Executed { .. })) => CallOutcome::Replied(Err(
            SqliteError::Protocol(SqliteProtocolError::QueryReturnedExecuted),
        )),
        CallOutcome::Replied(Err(e)) => CallOutcome::Replied(Err(e)),
        CallOutcome::Full => CallOutcome::Full,
        CallOutcome::Closed => CallOutcome::Closed,
        CallOutcome::Timeout => CallOutcome::Timeout,
        CallOutcome::Rejected(reason) => CallOutcome::Rejected(reason),
    }
}

// ---------------------------------------------------------------------------
// Outcome classification: Succeeded / Transient / Fatal.
// ---------------------------------------------------------------------------

/// Three-way classification of a SQLite call outcome.
///
/// Generic over the success carrier `T`:
///
/// - On the raw path, `T = SqliteResponse`.
/// - On `execute_call`, `T = u64` (rows_changed).
/// - On `query_call`, `T = SqliteRows`.
///
/// Caller-side retry loops typically only care about three buckets:
/// did the call succeed, was the failure transient (worth retrying),
/// or fatal? Match against this.
///
/// **The classifier does not retry.** It does not know your
/// idempotency rules, your retry budget, or your backoff. It just
/// labels each outcome.
///
/// # Default policy
///
/// - **Worker `Busy`**: `Transient(Busy)`. SQLite asked us to back off.
/// - **Worker `Timeout`**: `Transient(WorkerTimeout)`.
/// - **Bridge `Timeout`**: `Transient(BridgeTimeout)`.
/// - Everything else is `Fatal(...)`. Retrying without changing the
///   request, the database state, or the bridge config will reproduce.
#[derive(Debug, Clone)]
pub enum SqliteOutcomeClass<T> {
    /// Worker accepted and produced a successful response.
    Succeeded(T),
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
    /// Typed helper response-shape invariant failed.
    Protocol(SqliteProtocolError),
    /// Runtime rejected the bridge call before an application reply.
    Rejected(tina::CallRejectedReason),
    /// Bridge ingress mailbox full (runtime layer).
    BridgeFull,
    /// Bridge target closed or stale (runtime layer).
    BridgeClosed,
}

/// Extension trait that adds [`Self::classify`] to the various
/// SQLite call outcome shapes.
pub trait SqliteOutcomeExt {
    /// Successful payload type carried by [`SqliteOutcomeClass::Succeeded`].
    type Success;
    /// Classify the outcome into Succeeded / Transient / Fatal.
    fn classify(self) -> SqliteOutcomeClass<Self::Success>;
    /// Project the outcome into the shared bridge classifier
    /// vocabulary ([`BridgeOutcomeClass`]). The typed success payload
    /// is dropped; use [`Self::classify`] when you want both the typed
    /// payload and the verbose error info.
    fn bridge_class(&self) -> BridgeOutcomeClass;
}

impl SqliteOutcomeExt for SqliteCallOutcome {
    type Success = SqliteResponse;
    fn classify(self) -> SqliteOutcomeClass<SqliteResponse> {
        classify_inner(self, |resp| resp)
    }
    fn bridge_class(&self) -> BridgeOutcomeClass {
        sqlite_bridge_class(self)
    }
}

impl SqliteOutcomeExt for SqliteExecutedOutcome {
    type Success = u64;
    fn classify(self) -> SqliteOutcomeClass<u64> {
        classify_inner(self, |rows_changed| rows_changed)
    }
    fn bridge_class(&self) -> BridgeOutcomeClass {
        sqlite_bridge_class(self)
    }
}

impl SqliteOutcomeExt for SqliteRowsOutcome {
    type Success = SqliteRows;
    fn classify(self) -> SqliteOutcomeClass<SqliteRows> {
        classify_inner(self, |rows| rows)
    }
    fn bridge_class(&self) -> BridgeOutcomeClass {
        sqlite_bridge_class(self)
    }
}

/// Bridge-vocabulary projection for SQLite call outcomes.
///
/// Rules upheld at this layer (same as every other bridge):
///
/// - `CallOutcome::Closed` and `SqliteError::Closed` are
///   [`BridgeUnavailable::BridgeClosed`], not retry fog. A closed
///   handle stays closed.
/// - `SqliteError::Busy` is [`BridgeRetryable::ServiceThrottled`] —
///   SQLite asked us to back off; the caller may retry under their
///   own idempotency story.
/// - Constraint violations are [`BridgeFatal::InvalidRequest`].
/// - Generic SQLite/io errors carry no retryable metadata at this
///   layer, so they are [`BridgeFatal::SdkUnknown`].
pub fn sqlite_bridge_class<T>(outcome: &CallOutcome<Result<T, SqliteError>>) -> BridgeOutcomeClass {
    match outcome {
        CallOutcome::Replied(Ok(_)) => BridgeOutcomeClass::Succeeded,
        CallOutcome::Replied(Err(SqliteError::Busy)) => {
            BridgeOutcomeClass::Retryable(BridgeRetryable::ServiceThrottled)
        }
        CallOutcome::Replied(Err(SqliteError::Timeout)) => {
            BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeTimeout)
        }
        CallOutcome::Replied(Err(SqliteError::Full)) => {
            BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeFull)
        }
        CallOutcome::Replied(Err(SqliteError::Closed)) => {
            BridgeOutcomeClass::Unavailable(BridgeUnavailable::BridgeClosed)
        }
        CallOutcome::Replied(Err(SqliteError::InvalidRequest(_))) => {
            BridgeOutcomeClass::Fatal(BridgeFatal::InvalidRequest)
        }
        CallOutcome::Replied(Err(SqliteError::ResponseTooLarge)) => {
            BridgeOutcomeClass::Fatal(BridgeFatal::TooLarge)
        }
        CallOutcome::Replied(Err(SqliteError::Constraint(_))) => {
            BridgeOutcomeClass::Fatal(BridgeFatal::InvalidRequest)
        }
        CallOutcome::Replied(Err(SqliteError::Io(_)))
        | CallOutcome::Replied(Err(SqliteError::Sqlite(_))) => {
            BridgeOutcomeClass::Fatal(BridgeFatal::SdkUnknown)
        }
        CallOutcome::Replied(Err(SqliteError::Internal(_))) => {
            BridgeOutcomeClass::Fatal(BridgeFatal::Internal)
        }
        CallOutcome::Replied(Err(SqliteError::Protocol(_))) => {
            BridgeOutcomeClass::Fatal(BridgeFatal::Internal)
        }
        CallOutcome::Timeout => BridgeOutcomeClass::Retryable(BridgeRetryable::CallerTimeout),
        CallOutcome::Full => BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeFull),
        CallOutcome::Closed => BridgeOutcomeClass::Unavailable(BridgeUnavailable::BridgeClosed),
        CallOutcome::Rejected(_) => BridgeOutcomeClass::Fatal(BridgeFatal::InvalidRequest),
    }
}

fn classify_inner<R, T>(
    outcome: CallOutcome<Result<R, SqliteError>>,
    project: impl FnOnce(R) -> T,
) -> SqliteOutcomeClass<T> {
    match outcome {
        CallOutcome::Replied(Ok(payload)) => SqliteOutcomeClass::Succeeded(project(payload)),
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
        CallOutcome::Replied(Err(SqliteError::Protocol(error))) => {
            SqliteOutcomeClass::Fatal(SqliteFatalReason::Protocol(error))
        }
        CallOutcome::Timeout => SqliteOutcomeClass::Transient(SqliteTransientReason::BridgeTimeout),
        CallOutcome::Full => SqliteOutcomeClass::Fatal(SqliteFatalReason::BridgeFull),
        CallOutcome::Closed => SqliteOutcomeClass::Fatal(SqliteFatalReason::BridgeClosed),
        CallOutcome::Rejected(reason) => {
            SqliteOutcomeClass::Fatal(SqliteFatalReason::Rejected(reason))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execute_projection_preserves_the_exact_response_shape_violation() {
        let outcome = project_executed(CallOutcome::Replied(Ok(SqliteResponse::Rows {
            columns: vec!["value".into()],
            rows: vec![vec![SqliteValue::Integer(1)]],
        })));
        assert_eq!(
            outcome,
            CallOutcome::Replied(Err(SqliteError::Protocol(
                SqliteProtocolError::ExecuteReturnedRows
            )))
        );
    }

    #[test]
    fn query_projection_preserves_the_exact_response_shape_violation() {
        let outcome = project_rows(CallOutcome::Replied(Ok(SqliteResponse::Executed {
            rows_changed: 1,
        })));
        assert_eq!(
            outcome,
            CallOutcome::Replied(Err(SqliteError::Protocol(
                SqliteProtocolError::QueryReturnedExecuted
            )))
        );
    }

    #[test]
    fn classifier_preserves_the_runtime_rejection_reason() {
        let outcome: SqliteExecutedOutcome =
            CallOutcome::Rejected(tina::CallRejectedReason::ReplyAbandoned);
        assert!(matches!(
            outcome.classify(),
            SqliteOutcomeClass::Fatal(SqliteFatalReason::Rejected(
                tina::CallRejectedReason::ReplyAbandoned
            ))
        ));
    }
}
