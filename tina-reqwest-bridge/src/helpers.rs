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
/// `call(addr, ReqwestMsg::Send(...), timeout).reply(...)` against a
/// reqwest worker. Preserves the bridge-delivery / worker-outcome
/// layering.
pub type ReqwestCallOutcome = CallOutcome<ReqwestResult>;

/// Issue one outbound HTTP request.
///
/// Thin wrapper over `tina_runtime::call(addr, ReqwestMsg::Send(req),
/// timeout)`. No hidden retry, no hidden timeout, no queue. The
/// returned [`IsolateCall`] still needs `.reply(...)` to produce an
/// `Effect`; the user picks how to translate the outcome into their
/// own message.
///
/// ```ignore
/// send_request(self.http, ReqwestRequest::get(&url), timeout)
///     .reply(AppMsg::HttpReturned)
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
}

impl std::fmt::Display for BridgeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => f.write_str("bridge ingress full"),
            Self::Closed => f.write_str("bridge target closed"),
            Self::Timeout => f.write_str("bridge call timed out"),
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
        match self {
            Self::Bridge(b) => write!(f, "reqwest bridge: {b}"),
            Self::Worker(e) => write!(f, "reqwest worker: {e}"),
        }
    }
}

impl std::error::Error for ReqwestCallError {}

impl From<ReqwestError> for ReqwestCallError {
    fn from(e: ReqwestError) -> Self {
        Self::Worker(e)
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
    }
}
