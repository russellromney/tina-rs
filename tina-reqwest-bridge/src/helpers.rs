//! Caller-side helpers: type aliases, send shorthand, opt-in flatten.
//!
//! The reqwest bridge has two error layers:
//!
//! - **Bridge delivery**: `CallOutcome::Full | Closed | Timeout` is what
//!   the runtime says about the IsolateCall itself — could it reach the
//!   worker, did the worker stop, did the IsolateCall deadline elapse.
//! - **Worker outcome**: [`crate::ReqwestError`] is what the worker says
//!   about the request it accepted — bad URL, body too large, reqwest
//!   transport failure, response cap exceeded.
//!
//! Keeping the two layers visibly distinct is the bridge's whole job.
//! The default reply shape preserves them:
//!
//! ```ignore
//! AppMsg::HttpReturned(ReqwestCallOutcome)
//! // CallOutcome::Replied(Ok(response))
//! // CallOutcome::Replied(Err(ReqwestError::...))
//! // CallOutcome::Full | Closed | Timeout
//! ```
//!
//! [`flatten_outcome`] is opt-in and explicitly for app edges that do
//! not need to distinguish "Tina could not deliver the call" from
//! "worker received it and produced an error." It maps both layers
//! into a single [`ReqwestCallError`] enum that still names which
//! layer failed:
//!
//! ```ignore
//! match flatten_outcome(outcome) {
//!     Ok(response) => ...,
//!     Err(ReqwestCallError::Bridge(BridgeFailure::Timeout)) => ...,
//!     Err(ReqwestCallError::Worker(ReqwestError::ResponseTooLarge)) => ...,
//! }
//! ```
//!
//! Use the raw shape unless your app edge code is shorter and clearer
//! with the flat one. The bridge will never lie about which world
//! failed.

use std::time::Duration;

use http::status::StatusCode;
use tina::Address;
use tina_runtime::{CallOutcome, IsolateCall, call};

use crate::types::{ReqwestError, ReqwestRequest, ReqwestResponse};
use crate::worker::ReqwestMsg;

/// Worker reply type. Same as the inner `Result` carried inside a
/// [`CallOutcome`] — the worker's typed outcome before the IsolateCall
/// layer is considered.
pub type ReqwestResult = Result<ReqwestResponse, ReqwestError>;

/// Tina address shape for a registered reqwest worker. Use this in
/// isolate fields:
///
/// ```ignore
/// struct App { http: ReqwestAddress, ... }
/// ```
pub type ReqwestAddress = Address<ReqwestMsg, ReqwestResult>;

/// Full reply shape returned by the runtime when a Tina caller does
/// `call(addr, ReqwestMsg::Send(...), timeout).then(...)` against a
/// reqwest worker. Preserves the bridge-delivery / worker-outcome
/// layering.
pub type ReqwestCallOutcome = CallOutcome<ReqwestResult>;

/// Issue one outbound HTTP request.
///
/// Thin wrapper over `tina_runtime::call(addr, ReqwestMsg::Send(req),
/// timeout)`. No hidden retry, no hidden timeout, no queue. The
/// returned [`IsolateCall`] still needs `.then(...)` to produce an
/// `Effect`; the user picks how to translate the outcome into their
/// own message.
///
/// ```ignore
/// send_request(self.http, ReqwestRequest::get(&url), timeout)
///     .then(AppMsg::HttpReturned)
/// ```
pub fn send_request(
    addr: ReqwestAddress,
    request: ReqwestRequest,
    timeout: Duration,
) -> IsolateCall<ReqwestMsg, ReqwestResult> {
    call(addr, ReqwestMsg::Send(request), timeout)
}

/// Why the bridge could not deliver an IsolateCall to the worker. Used
/// only in the flattened error shape — see [`ReqwestCallError`].
///
/// These three correspond 1:1 with [`CallOutcome::Full`],
/// [`CallOutcome::Closed`], and [`CallOutcome::Timeout`]. They are
/// **not** the same as [`ReqwestError::Full`] /
/// [`ReqwestError::Closed`] / [`ReqwestError::Timeout`]. The
/// outer layer means "the runtime never delivered the request to the
/// worker"; the inner layer means "the worker accepted and produced
/// this outcome."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeFailure {
    /// Target mailbox was full or admission was rejected. The worker
    /// never saw the request.
    Full,
    /// Target isolate is closed or stale. The worker never saw the
    /// request.
    Closed,
    /// IsolateCall deadline elapsed before the worker replied. The
    /// worker may have seen the request and may still be processing
    /// it; this is a "stopped waiting" signal at the runtime layer.
    Timeout,
    /// Target isolate rejected the call without a worker reply.
    Rejected(tina::CallRejectedReason),
}

impl std::fmt::Display for BridgeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => f.write_str("reqwest bridge: ingress full"),
            Self::Closed => f.write_str("reqwest bridge: target closed"),
            Self::Timeout => f.write_str("reqwest bridge: call timed out"),
            Self::Rejected(reason) => write!(f, "reqwest bridge: call rejected: {reason:?}"),
        }
    }
}

/// Flattened error type produced by [`flatten_outcome`].
///
/// Distinguishes which layer failed:
///
/// - [`ReqwestCallError::Bridge`]: the runtime could not deliver the
///   call, or the IsolateCall deadline elapsed. The worker's request
///   either never started or was abandoned by the runtime.
/// - [`ReqwestCallError::Worker`]: the worker received the request and
///   produced a typed error outcome (bad URL, body too large, reqwest
///   transport failure, etc.).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReqwestCallError {
    /// IsolateCall delivery failed at the runtime / bridge layer.
    Bridge(BridgeFailure),
    /// Worker accepted the request and produced a typed outcome.
    Worker(ReqwestError),
}

impl std::fmt::Display for ReqwestCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Delegate to the inner Display; each layer's own Display is
        // self-naming ("reqwest bridge: ..." for BridgeFailure,
        // "reqwest worker: ..." for ReqwestError) so we do not double
        // up on prefixes.
        match self {
            Self::Bridge(b) => std::fmt::Display::fmt(b, f),
            Self::Worker(e) => std::fmt::Display::fmt(e, f),
        }
    }
}

impl std::error::Error for ReqwestCallError {}

impl From<ReqwestError> for ReqwestCallError {
    fn from(e: ReqwestError) -> Self {
        Self::Worker(e)
    }
}

impl From<BridgeFailure> for ReqwestCallError {
    fn from(b: BridgeFailure) -> Self {
        Self::Bridge(b)
    }
}

/// Opt-in flatten of a [`ReqwestCallOutcome`] into a single
/// `Result<ReqwestResponse, ReqwestCallError>`.
///
/// **The default path keeps the layers separate.** Use this only when
/// the call site is genuinely an application edge that does not need
/// to distinguish "Tina could not deliver the call" from "worker
/// received it and produced an error." The flat error type still
/// names which layer failed via [`ReqwestCallError::Bridge`] vs
/// [`ReqwestCallError::Worker`] — the layering is preserved, only
/// the pattern-match shape changes.
///
/// ```ignore
/// AppMsg::HttpReturned(outcome) => {
///     match flatten_outcome(outcome) {
///         Ok(response) => { ... }
///         Err(ReqwestCallError::Bridge(_)) => { ... }
///         Err(ReqwestCallError::Worker(_)) => { ... }
///     }
/// }
/// ```
pub fn flatten_outcome(outcome: ReqwestCallOutcome) -> Result<ReqwestResponse, ReqwestCallError> {
    match outcome {
        CallOutcome::Replied(Ok(response)) => Ok(response),
        CallOutcome::Replied(Err(err)) => Err(ReqwestCallError::Worker(err)),
        CallOutcome::Full => Err(ReqwestCallError::Bridge(BridgeFailure::Full)),
        CallOutcome::Closed => Err(ReqwestCallError::Bridge(BridgeFailure::Closed)),
        CallOutcome::Timeout => Err(ReqwestCallError::Bridge(BridgeFailure::Timeout)),
        CallOutcome::Rejected(reason) => {
            Err(ReqwestCallError::Bridge(BridgeFailure::Rejected(reason)))
        }
    }
}

// ---------- Classification ----------

/// Three-way classification of a [`ReqwestCallOutcome`].
///
/// Caller-side retry loops typically only care about three buckets:
/// did the call succeed, was the failure transient (worth retrying),
/// or was it fatal? The typed multi-arm match against the layered
/// `ReqwestCallOutcome` still works — this is opt-in sugar that
/// preserves both layers internally and names *why* via the typed
/// reason payloads.
///
/// **The classifier does not retry.** It does not know your idempotency
/// rules, your retry budget, or your backoff. It just labels each
/// outcome.
///
/// # Default policy
///
/// - **5xx upstream:** `Transient(UpstreamServer { status })`. 5xx is
///   "server is unhappy, try again". If your service has stricter
///   rules (e.g. treat `501 Not Implemented` as fatal), match the
///   reason and downgrade.
/// - **non-5xx non-2xx:** `Fatal(UpstreamClient { status })`. 4xx is
///   the client's fault and retrying without changing the request will
///   reproduce the failure.
/// - **bridge `Timeout`:** `Transient(BridgeTimeout)`. The bridge gave
///   up waiting; the worker may have completed but we did not see it.
/// - **worker `Timeout`:** `Transient(WorkerTimeout)`. The reqwest
///   attempt exceeded its per-attempt deadline and the bridge aborted
///   the spawned Tokio task. Upstream side effects may already have
///   happened; retry only when the request is safe to repeat.
/// - **worker `Reqwest`:** `Transient(WorkerTransport)`. Connect / IO
///   class errors are typically retryable.
/// - **bridge `Full` / `Closed`, worker `Full` / `Closed`,
///   `RequestTooLarge`, `ResponseTooLarge`, `InvalidRequest`,
///   `Internal`:** fatal — retrying without changing pressure, the
///   request, or the bridge itself will reproduce them.
#[derive(Debug, Clone)]
pub enum ReqwestOutcomeClass {
    /// Upstream returned a 2xx success.
    Succeeded(ReqwestResponse),
    /// Failure that retrying the same request might fix.
    Transient(ReqwestTransientReason),
    /// Failure that retrying the same request will not fix.
    Fatal(ReqwestFatalReason),
}

/// Why the classifier judged the outcome retryable.
///
/// `Debug` formatting includes the underlying detail (status code,
/// transport message) so `tracing::warn!("transient: {reason:?}")` is
/// useful without re-matching the raw outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReqwestTransientReason {
    /// Upstream returned a 5xx status.
    UpstreamServer {
        /// The 5xx status code returned by the upstream.
        status: StatusCode,
    },
    /// IsolateCall deadline elapsed before the worker replied. The
    /// worker may still be processing; the bridge stopped waiting.
    BridgeTimeout,
    /// The reqwest worker's per-attempt timeout elapsed and the bridge
    /// aborted the spawned Tokio task. Upstream side effects may
    /// already have happened.
    WorkerTimeout,
    /// reqwest returned a transport-class error (connect / IO / decode).
    /// Carries the underlying message; treat the string as opaque
    /// (reqwest message text can change between releases).
    WorkerTransport(String),
}

/// Why the classifier judged the outcome non-retryable.
///
/// `Debug` formatting includes the underlying detail (status code,
/// validation message) so log lines stay useful without re-matching
/// the raw outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReqwestFatalReason {
    /// Upstream returned a non-2xx non-5xx status (e.g. 4xx).
    UpstreamClient {
        /// The non-2xx non-5xx status code returned by the upstream.
        status: StatusCode,
    },
    /// Bridge ingress rejected admission: bounded mailbox full.
    BridgeFull,
    /// Bridge ingress rejected admission: target stale or closed.
    BridgeClosed,
    /// Bridge target rejected the call without a worker reply.
    BridgeRejected(tina::CallRejectedReason),
    /// Worker rejected at the `max_in_flight` cap.
    WorkerFull,
    /// Worker has been closed.
    WorkerClosed,
    /// Request body exceeded the configured cap.
    RequestTooLarge,
    /// Response body exceeded the configured cap.
    ResponseTooLarge,
    /// Request validation failed (bad URL, invalid method, etc.).
    /// Carries the underlying validation message.
    InvalidRequest(String),
    /// Bridge worker invariant failed internally. Carries an opaque
    /// diagnostic message.
    WorkerInternal(String),
}

/// Extension trait that adds [`Self::classify`] to
/// [`ReqwestCallOutcome`].
///
/// Use:
///
/// ```ignore
/// use tina_reqwest_bridge::ReqwestOutcomeExt;
///
/// match outcome.classify() {
///     ReqwestOutcomeClass::Succeeded(resp) => finish_ok(resp),
///     ReqwestOutcomeClass::Transient(_) if budget_left() => retry(),
///     ReqwestOutcomeClass::Transient(_) | ReqwestOutcomeClass::Fatal(_) => fail(),
/// }
/// ```
pub trait ReqwestOutcomeExt {
    /// Classify the outcome into Succeeded / Transient / Fatal.
    fn classify(self) -> ReqwestOutcomeClass;
}

impl ReqwestOutcomeExt for ReqwestCallOutcome {
    fn classify(self) -> ReqwestOutcomeClass {
        match self {
            CallOutcome::Replied(Ok(response)) => {
                if response.status.is_success() {
                    ReqwestOutcomeClass::Succeeded(response)
                } else if response.status.is_server_error() {
                    ReqwestOutcomeClass::Transient(ReqwestTransientReason::UpstreamServer {
                        status: response.status,
                    })
                } else {
                    ReqwestOutcomeClass::Fatal(ReqwestFatalReason::UpstreamClient {
                        status: response.status,
                    })
                }
            }
            CallOutcome::Replied(Err(ReqwestError::Timeout)) => {
                ReqwestOutcomeClass::Transient(ReqwestTransientReason::WorkerTimeout)
            }
            CallOutcome::Replied(Err(ReqwestError::Reqwest(msg))) => {
                ReqwestOutcomeClass::Transient(ReqwestTransientReason::WorkerTransport(msg))
            }
            CallOutcome::Replied(Err(ReqwestError::Internal(msg))) => {
                ReqwestOutcomeClass::Fatal(ReqwestFatalReason::WorkerInternal(msg))
            }
            CallOutcome::Replied(Err(ReqwestError::Full)) => {
                ReqwestOutcomeClass::Fatal(ReqwestFatalReason::WorkerFull)
            }
            CallOutcome::Replied(Err(ReqwestError::Closed)) => {
                ReqwestOutcomeClass::Fatal(ReqwestFatalReason::WorkerClosed)
            }
            CallOutcome::Replied(Err(ReqwestError::RequestTooLarge)) => {
                ReqwestOutcomeClass::Fatal(ReqwestFatalReason::RequestTooLarge)
            }
            CallOutcome::Replied(Err(ReqwestError::ResponseTooLarge)) => {
                ReqwestOutcomeClass::Fatal(ReqwestFatalReason::ResponseTooLarge)
            }
            CallOutcome::Replied(Err(ReqwestError::InvalidRequest(msg))) => {
                ReqwestOutcomeClass::Fatal(ReqwestFatalReason::InvalidRequest(msg))
            }
            CallOutcome::Timeout => {
                ReqwestOutcomeClass::Transient(ReqwestTransientReason::BridgeTimeout)
            }
            CallOutcome::Full => ReqwestOutcomeClass::Fatal(ReqwestFatalReason::BridgeFull),
            CallOutcome::Closed => ReqwestOutcomeClass::Fatal(ReqwestFatalReason::BridgeClosed),
            CallOutcome::Rejected(reason) => {
                ReqwestOutcomeClass::Fatal(ReqwestFatalReason::BridgeRejected(reason))
            }
        }
    }
}
