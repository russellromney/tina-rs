//! Caller-side helpers: type aliases, raw and typed call shorthands,
//! and outcome classification.
//!
//! The bridge has two error layers:
//!
//! - **Bridge delivery**: `CallOutcome::Full | Closed | Timeout` is what
//!   the runtime says about the IsolateCall — could it reach the
//!   worker, did the IsolateCall deadline elapse.
//! - **Worker outcome**: [`crate::PgError`] is what the worker says
//!   about the request it accepted, including SQLx pool acquire
//!   timeout, sqlx errors, decode failures.
//!
//! The two layers stay distinct in the default reply shape:
//!
//! ```ignore
//! AppMsg::DbDone(PgCallOutcome)
//! // CallOutcome::Replied(Ok(PgResponse::Executed { .. }))
//! // CallOutcome::Replied(Err(PgError::PoolAcquireTimeout))
//! // CallOutcome::Full | Closed | Timeout
//! ```
//!
//! # Two paths
//!
//! - [`send_request`] is the **raw, full-truth** path: takes a
//!   [`PgRequest`], reply preserves the response enum and both error
//!   layers.
//! - [`execute_call`] / [`fetch_one_call`] are **typed shorthands**
//!   that take their args directly. The reply projects the response
//!   enum away: `execute_call` is
//!   `CallOutcome<Result<u64, PgError>>`; `fetch_one_call` is
//!   `CallOutcome<Result<Option<PgRow>, PgError>>` — `None` for
//!   zero rows, `Some(row)` for exactly one. `TooManyRows` stays in
//!   the inner `Err`.

use std::time::Duration;

use tina::Address;
use tina::Effect;
use tina::Isolate;
use tina_runtime::bridge::{BridgeFatal, BridgeOutcomeClass, BridgeRetryable, BridgeUnavailable};
use tina_runtime::{CallOutcome, IsolateCall, RuntimeCall, call};

use crate::types::{PgError, PgRequest, PgResponse, PgRow, PgStep, PgTransactionOutcome};
use crate::worker::PgMsg;

/// Worker reply type. Same as the inner `Result` carried inside a
/// [`CallOutcome`].
pub type PgResult = Result<PgResponse, PgError>;

/// Tina address shape for a registered Postgres worker. Use this in
/// isolate fields:
///
/// ```ignore
/// struct App { db: PgAddress, ... }
/// ```
pub type PgAddress = Address<PgMsg, PgResult>;

/// Full reply shape: `CallOutcome<Result<PgResponse, PgError>>`.
pub type PgCallOutcome = CallOutcome<PgResult>;

/// Reply shape from [`execute_call`].
pub type PgExecutedOutcome = CallOutcome<Result<u64, PgError>>;

/// Reply shape from [`fetch_one_call`]. `Ok(None)` is `NoRows`;
/// `Ok(Some(row))` is one row. `TooManyRows` lands in `Err`.
pub type PgFetchOneOutcome = CallOutcome<Result<Option<PgRow>, PgError>>;

/// Reply shape from [`fetch_many_call`]. `Ok((rows, truncated))`
/// gives the buffered rows and whether the bridge stopped pulling
/// at its cap.
pub type PgFetchManyOutcome = CallOutcome<Result<PgRows, PgError>>;

/// Reply shape from [`transaction_call`]. The inner `Ok` carries a
/// [`PgTransactionOutcome`] so callers see commit/rollback truth
/// without losing the per-step record.
pub type PgTransactionCallOutcome = CallOutcome<Result<PgTransactionOutcome, PgError>>;

/// Buffered query result. Carried inside [`PgFetchManyOutcome`].
#[derive(Debug, Clone, PartialEq)]
pub struct PgRows {
    /// Buffered rows, in result order.
    pub rows: Vec<PgRow>,
    /// `true` if Postgres had more rows on the wire and the bridge
    /// stopped pulling at its effective cap.
    pub truncated: bool,
}

impl PgRows {
    /// Number of buffered rows.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// `true` iff no rows were returned.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Iterate buffered rows.
    pub fn iter(&self) -> impl Iterator<Item = &PgRow> {
        self.rows.iter()
    }
}

// ---------------------------------------------------------------------------
// Raw path: full-truth IsolateCall.
// ---------------------------------------------------------------------------

/// Submit one Postgres request through a registered worker.
///
/// Thin wrapper over `tina_runtime::call(addr, PgMsg::Send(req),
/// timeout)`. The reply preserves the response enum and both error
/// layers.
pub fn send_request(
    addr: PgAddress,
    request: PgRequest,
    timeout: Duration,
) -> IsolateCall<PgMsg, PgResult> {
    call(addr, PgMsg::Send(request), timeout)
}

// ---------------------------------------------------------------------------
// Typed shorthands.
// ---------------------------------------------------------------------------

/// Prepared `Execute` call. Use [`Self::then`] to fold into the
/// continuation message of your isolate.
pub struct ExecuteCall {
    inner: IsolateCall<PgMsg, PgResult>,
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
        I: Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(PgExecutedOutcome) -> M + 'static,
        M: 'static,
    {
        self.then(translator)
    }

    /// Turn this prepared call into one continuation message.
    pub fn then<I, F, M>(self, translator: F) -> Effect<I>
    where
        I: Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(PgExecutedOutcome) -> M + 'static,
        M: 'static,
    {
        self.inner
            .then(move |raw| translator(project_executed(raw)))
    }
}

/// Prepared `FetchMany` call.
pub struct FetchManyCall {
    inner: IsolateCall<PgMsg, PgResult>,
}

impl std::fmt::Debug for FetchManyCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FetchManyCall").finish_non_exhaustive()
    }
}

impl FetchManyCall {
    /// Turn this prepared call into one continuation message.
    #[deprecated(since = "0.1.0", note = "use `.then(...)` for ordinary continuations")]
    pub fn reply<I, F, M>(self, translator: F) -> Effect<I>
    where
        I: Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(PgFetchManyOutcome) -> M + 'static,
        M: 'static,
    {
        self.then(translator)
    }

    /// Turn this prepared call into one continuation message.
    pub fn then<I, F, M>(self, translator: F) -> Effect<I>
    where
        I: Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(PgFetchManyOutcome) -> M + 'static,
        M: 'static,
    {
        self.inner
            .then(move |raw| translator(project_fetch_many(raw)))
    }
}

/// Prepared `FetchOne` call.
pub struct FetchOneCall {
    inner: IsolateCall<PgMsg, PgResult>,
}

impl std::fmt::Debug for FetchOneCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FetchOneCall").finish_non_exhaustive()
    }
}

impl FetchOneCall {
    /// Turn this prepared call into one continuation message.
    #[deprecated(since = "0.1.0", note = "use `.then(...)` for ordinary continuations")]
    pub fn reply<I, F, M>(self, translator: F) -> Effect<I>
    where
        I: Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(PgFetchOneOutcome) -> M + 'static,
        M: 'static,
    {
        self.then(translator)
    }

    /// Turn this prepared call into one continuation message.
    pub fn then<I, F, M>(self, translator: F) -> Effect<I>
    where
        I: Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(PgFetchOneOutcome) -> M + 'static,
        M: 'static,
    {
        self.inner
            .then(move |raw| translator(project_fetch_one(raw)))
    }
}

/// Build an `Execute` call with the typed reply shape
/// `CallOutcome<Result<u64, PgError>>`.
pub fn execute_call(
    addr: PgAddress,
    sql: impl Into<String>,
    params: Vec<crate::types::PgValue>,
    timeout: Duration,
) -> ExecuteCall {
    ExecuteCall {
        inner: send_request(
            addr,
            PgRequest::Execute {
                sql: sql.into(),
                params,
            },
            timeout,
        ),
    }
}

/// Build a `FetchOne` call with the typed reply shape
/// `CallOutcome<Result<Option<PgRow>, PgError>>`.
pub fn fetch_one_call(
    addr: PgAddress,
    sql: impl Into<String>,
    params: Vec<crate::types::PgValue>,
    timeout: Duration,
) -> FetchOneCall {
    FetchOneCall {
        inner: send_request(
            addr,
            PgRequest::FetchOne {
                sql: sql.into(),
                params,
            },
            timeout,
        ),
    }
}

/// Build a `FetchMany` call with the typed reply shape
/// `CallOutcome<Result<PgRows, PgError>>`. Effective row cap is
/// `min(max_rows, PgConfig::max_response_rows)`.
pub fn fetch_many_call(
    addr: PgAddress,
    sql: impl Into<String>,
    params: Vec<crate::types::PgValue>,
    max_rows: usize,
    timeout: Duration,
) -> FetchManyCall {
    FetchManyCall {
        inner: send_request(
            addr,
            PgRequest::FetchMany {
                sql: sql.into(),
                params,
                max_rows,
            },
            timeout,
        ),
    }
}

/// Prepared transaction call.
pub struct TransactionCall {
    inner: IsolateCall<PgMsg, PgResult>,
}

impl std::fmt::Debug for TransactionCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransactionCall").finish_non_exhaustive()
    }
}

impl TransactionCall {
    /// Turn this prepared call into one continuation message.
    #[deprecated(since = "0.1.0", note = "use `.then(...)` for ordinary continuations")]
    pub fn reply<I, F, M>(self, translator: F) -> Effect<I>
    where
        I: Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(PgTransactionCallOutcome) -> M + 'static,
        M: 'static,
    {
        self.then(translator)
    }

    /// Turn this prepared call into one continuation message.
    pub fn then<I, F, M>(self, translator: F) -> Effect<I>
    where
        I: Isolate<Message = M, Call = RuntimeCall<M>>,
        F: FnOnce(PgTransactionCallOutcome) -> M + 'static,
        M: 'static,
    {
        self.inner
            .then(move |raw| translator(project_transaction(raw)))
    }
}

/// Build a `Transaction` call. The inner `Ok` carries a
/// [`PgTransactionOutcome`] which itself names commit vs rollback.
/// `Err` covers admission/timeout failures, COMMIT failures, and
/// pool errors before the script ran.
pub fn transaction_call(addr: PgAddress, steps: Vec<PgStep>, timeout: Duration) -> TransactionCall {
    TransactionCall {
        inner: send_request(addr, PgRequest::Transaction { steps }, timeout),
    }
}

fn project_executed(outcome: PgCallOutcome) -> PgExecutedOutcome {
    match outcome {
        CallOutcome::Replied(Ok(PgResponse::Executed { rows_affected })) => {
            CallOutcome::Replied(Ok(rows_affected))
        }
        // Worker invariant: `Execute` only ever produces `Executed`.
        // Surface `Internal` rather than panic if a future change
        // breaks that invariant.
        CallOutcome::Replied(Ok(_)) => CallOutcome::Replied(Err(PgError::Internal(
            "execute_call: worker returned non-Executed response".into(),
        ))),
        CallOutcome::Replied(Err(e)) => CallOutcome::Replied(Err(e)),
        CallOutcome::Full => CallOutcome::Full,
        CallOutcome::Closed => CallOutcome::Closed,
        CallOutcome::Timeout => CallOutcome::Timeout,
        CallOutcome::Rejected(reason) => CallOutcome::Rejected(reason),
    }
}

fn project_fetch_one(outcome: PgCallOutcome) -> PgFetchOneOutcome {
    match outcome {
        CallOutcome::Replied(Ok(PgResponse::Row(row))) => CallOutcome::Replied(Ok(Some(row))),
        CallOutcome::Replied(Ok(PgResponse::NoRows)) => CallOutcome::Replied(Ok(None)),
        // Same invariant note as `project_executed`.
        CallOutcome::Replied(Ok(_)) => CallOutcome::Replied(Err(PgError::Internal(
            "fetch_one_call: worker returned non-FetchOne response".into(),
        ))),
        CallOutcome::Replied(Err(e)) => CallOutcome::Replied(Err(e)),
        CallOutcome::Full => CallOutcome::Full,
        CallOutcome::Closed => CallOutcome::Closed,
        CallOutcome::Timeout => CallOutcome::Timeout,
        CallOutcome::Rejected(reason) => CallOutcome::Rejected(reason),
    }
}

fn project_fetch_many(outcome: PgCallOutcome) -> PgFetchManyOutcome {
    match outcome {
        CallOutcome::Replied(Ok(PgResponse::Rows { rows, truncated })) => {
            CallOutcome::Replied(Ok(PgRows { rows, truncated }))
        }
        CallOutcome::Replied(Ok(_)) => CallOutcome::Replied(Err(PgError::Internal(
            "fetch_many_call: worker returned non-FetchMany response".into(),
        ))),
        CallOutcome::Replied(Err(e)) => CallOutcome::Replied(Err(e)),
        CallOutcome::Full => CallOutcome::Full,
        CallOutcome::Closed => CallOutcome::Closed,
        CallOutcome::Timeout => CallOutcome::Timeout,
        CallOutcome::Rejected(reason) => CallOutcome::Rejected(reason),
    }
}

fn project_transaction(outcome: PgCallOutcome) -> PgTransactionCallOutcome {
    match outcome {
        CallOutcome::Replied(Ok(PgResponse::Transaction(tx))) => CallOutcome::Replied(Ok(tx)),
        CallOutcome::Replied(Ok(_)) => CallOutcome::Replied(Err(PgError::Internal(
            "transaction_call: worker returned non-Transaction response".into(),
        ))),
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

/// Three-way classification of a Postgres call outcome.
///
/// Generic over the success carrier `T`:
///
/// - On the raw path, `T = PgResponse`.
/// - On `execute_call`, `T = u64` (rows_affected).
/// - On `fetch_one_call`, `T = Option<PgRow>` (`None` means `NoRows`).
///
/// **The classifier does not retry.** It does not know your
/// idempotency rules, your retry budget, or your backoff. It just
/// labels each outcome.
///
/// # Default policy
///
/// - **Worker `Timeout`**: `Transient(WorkerTimeout)` — the bridge
///   per-attempt deadline elapsed.
/// - **Worker `PoolAcquireTimeout`**: `Transient(PoolAcquireTimeout)`
///   — SQLx couldn't acquire a connection; widening the pool or
///   retrying may help.
/// - **Bridge `Timeout`**: `Transient(BridgeTimeout)` — the caller's
///   IsolateCall deadline elapsed.
/// - **Bridge `Full`**, **Bridge `Closed`**: `Fatal(BridgeFull)` /
///   `Fatal(BridgeClosed)` — the caller decides whether to back off.
///   Marked fatal because retrying immediately reproduces.
/// - Everything else (decode error, sqlx error, invalid request,
///   too-many-rows) is `Fatal(...)`. Retrying without changing the
///   request, the database state, or the bridge config will reproduce.
#[derive(Debug, Clone)]
pub enum PgOutcomeClass<T> {
    /// Worker accepted and produced a successful response.
    Succeeded(T),
    /// Failure that retrying the same request might fix.
    Transient(PgTransientReason),
    /// Failure that retrying the same request will not fix.
    Fatal(PgFatalReason),
}

/// Why the classifier judged the outcome retryable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PgTransientReason {
    /// Bridge per-attempt deadline elapsed.
    WorkerTimeout,
    /// SQLx pool could not acquire a connection within its
    /// `acquire_timeout`.
    PoolAcquireTimeout,
    /// IsolateCall deadline elapsed before the worker replied.
    BridgeTimeout,
}

/// Why the classifier judged the outcome non-retryable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PgFatalReason {
    /// Worker rejected admission: `max_in_flight` saturated.
    Full,
    /// Worker has been closed.
    Closed,
    /// Request rejected by validation.
    InvalidRequest(String),
    /// `FetchOne` matched more than one row.
    TooManyRows,
    /// Row decoding failed.
    Decode(String),
    /// SQLx pool was closed.
    PoolClosed,
    /// Catch-all SQLx error.
    Sqlx(String),
    /// Bridge invariant failed.
    Internal(String),
    /// Bridge ingress mailbox full (runtime layer).
    BridgeFull,
    /// Bridge target closed or stale (runtime layer).
    BridgeClosed,
    /// Bridge target rejected the call without a worker reply.
    BridgeRejected(tina::CallRejectedReason),
}

/// Extension trait that adds [`Self::classify`] to the various
/// Postgres call outcome shapes.
pub trait PgOutcomeExt {
    /// Successful payload type carried by [`PgOutcomeClass::Succeeded`].
    type Success;
    /// Classify the outcome into Succeeded / Transient / Fatal.
    fn classify(self) -> PgOutcomeClass<Self::Success>;
    /// Project the outcome into the shared bridge classifier
    /// vocabulary ([`BridgeOutcomeClass`]). The typed success payload
    /// is dropped; use [`Self::classify`] when you want both the typed
    /// payload and the verbose error info.
    fn bridge_class(&self) -> BridgeOutcomeClass;
}

impl PgOutcomeExt for PgCallOutcome {
    type Success = PgResponse;
    fn classify(self) -> PgOutcomeClass<PgResponse> {
        classify_inner(self, |resp| resp)
    }
    fn bridge_class(&self) -> BridgeOutcomeClass {
        bridge_class(self)
    }
}

impl PgOutcomeExt for PgExecutedOutcome {
    type Success = u64;
    fn classify(self) -> PgOutcomeClass<u64> {
        classify_inner(self, |rows_affected| rows_affected)
    }
    fn bridge_class(&self) -> BridgeOutcomeClass {
        bridge_class(self)
    }
}

impl PgOutcomeExt for PgFetchOneOutcome {
    type Success = Option<PgRow>;
    fn classify(self) -> PgOutcomeClass<Option<PgRow>> {
        classify_inner(self, |row| row)
    }
    fn bridge_class(&self) -> BridgeOutcomeClass {
        bridge_class(self)
    }
}

impl PgOutcomeExt for PgFetchManyOutcome {
    type Success = PgRows;
    fn classify(self) -> PgOutcomeClass<PgRows> {
        classify_inner(self, |rows| rows)
    }
    fn bridge_class(&self) -> BridgeOutcomeClass {
        bridge_class(self)
    }
}

impl PgOutcomeExt for PgTransactionCallOutcome {
    type Success = PgTransactionOutcome;
    fn classify(self) -> PgOutcomeClass<PgTransactionOutcome> {
        classify_inner(self, |tx| tx)
    }
    fn bridge_class(&self) -> BridgeOutcomeClass {
        bridge_class(self)
    }
}

fn classify_inner<R, T>(
    outcome: CallOutcome<Result<R, PgError>>,
    project: impl FnOnce(R) -> T,
) -> PgOutcomeClass<T> {
    match outcome {
        CallOutcome::Replied(Ok(payload)) => PgOutcomeClass::Succeeded(project(payload)),
        CallOutcome::Replied(Err(PgError::Timeout)) => {
            PgOutcomeClass::Transient(PgTransientReason::WorkerTimeout)
        }
        CallOutcome::Replied(Err(PgError::PoolAcquireTimeout)) => {
            PgOutcomeClass::Transient(PgTransientReason::PoolAcquireTimeout)
        }
        CallOutcome::Replied(Err(PgError::Full)) => PgOutcomeClass::Fatal(PgFatalReason::Full),
        CallOutcome::Replied(Err(PgError::Closed)) => PgOutcomeClass::Fatal(PgFatalReason::Closed),
        CallOutcome::Replied(Err(PgError::PoolClosed)) => {
            PgOutcomeClass::Fatal(PgFatalReason::PoolClosed)
        }
        CallOutcome::Replied(Err(PgError::InvalidRequest(msg))) => {
            PgOutcomeClass::Fatal(PgFatalReason::InvalidRequest(msg))
        }
        CallOutcome::Replied(Err(PgError::TooManyRows)) => {
            PgOutcomeClass::Fatal(PgFatalReason::TooManyRows)
        }
        CallOutcome::Replied(Err(PgError::Decode(msg))) => {
            PgOutcomeClass::Fatal(PgFatalReason::Decode(msg))
        }
        CallOutcome::Replied(Err(PgError::Sqlx(msg))) => {
            PgOutcomeClass::Fatal(PgFatalReason::Sqlx(msg))
        }
        CallOutcome::Replied(Err(PgError::Internal(msg))) => {
            PgOutcomeClass::Fatal(PgFatalReason::Internal(msg))
        }
        CallOutcome::Timeout => PgOutcomeClass::Transient(PgTransientReason::BridgeTimeout),
        CallOutcome::Full => PgOutcomeClass::Fatal(PgFatalReason::BridgeFull),
        CallOutcome::Closed => PgOutcomeClass::Fatal(PgFatalReason::BridgeClosed),
        CallOutcome::Rejected(reason) => {
            PgOutcomeClass::Fatal(PgFatalReason::BridgeRejected(reason))
        }
    }
}

/// Bridge-vocabulary projection: maps a Postgres call outcome into the
/// shared [`BridgeOutcomeClass`] vocabulary so dashboards, retry
/// helpers, and replay suites can grep one set of names across every
/// bridge.
///
/// The Postgres bridge keeps its richer typed outcome via
/// [`PgOutcomeClass`] for callers that want the typed success payload
/// or the verbose error info. This projection drops the payload and
/// returns the four-arm classification.
///
/// Rules upheld at this layer:
///
/// - `CallOutcome::Closed` and `PgError::PoolClosed` are
///   [`BridgeUnavailable`], not retry fog. A closed bridge or pool
///   needs a new handle.
/// - The generic `PgError::Sqlx(_)` wrapper has no retryable metadata
///   at this layer, so it is [`BridgeFatal::SdkUnknown`] — never
///   blindly retryable.
pub fn bridge_class<T>(outcome: &CallOutcome<Result<T, PgError>>) -> BridgeOutcomeClass {
    match outcome {
        CallOutcome::Replied(Ok(_)) => BridgeOutcomeClass::Succeeded,
        CallOutcome::Replied(Err(PgError::Timeout)) => {
            BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeTimeout)
        }
        CallOutcome::Replied(Err(PgError::PoolAcquireTimeout)) => {
            BridgeOutcomeClass::Retryable(BridgeRetryable::PoolAcquireTimeout)
        }
        CallOutcome::Replied(Err(PgError::Full)) => {
            BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeFull)
        }
        CallOutcome::Replied(Err(PgError::Closed)) => {
            BridgeOutcomeClass::Unavailable(BridgeUnavailable::BridgeClosed)
        }
        CallOutcome::Replied(Err(PgError::PoolClosed)) => {
            BridgeOutcomeClass::Unavailable(BridgeUnavailable::PoolClosed)
        }
        CallOutcome::Replied(Err(PgError::InvalidRequest(_))) => {
            BridgeOutcomeClass::Fatal(BridgeFatal::InvalidRequest)
        }
        CallOutcome::Replied(Err(PgError::TooManyRows)) => {
            BridgeOutcomeClass::Fatal(BridgeFatal::InvalidRequest)
        }
        CallOutcome::Replied(Err(PgError::Decode(_))) => {
            BridgeOutcomeClass::Fatal(BridgeFatal::Decode)
        }
        CallOutcome::Replied(Err(PgError::Sqlx(_))) => {
            BridgeOutcomeClass::Fatal(BridgeFatal::SdkUnknown)
        }
        CallOutcome::Replied(Err(PgError::Internal(_))) => {
            BridgeOutcomeClass::Fatal(BridgeFatal::Internal)
        }
        CallOutcome::Timeout => BridgeOutcomeClass::Retryable(BridgeRetryable::CallerTimeout),
        CallOutcome::Full => BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeFull),
        CallOutcome::Closed => BridgeOutcomeClass::Unavailable(BridgeUnavailable::BridgeClosed),
        CallOutcome::Rejected(_) => BridgeOutcomeClass::Fatal(BridgeFatal::InvalidRequest),
    }
}
