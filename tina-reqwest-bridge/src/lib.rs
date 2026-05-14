#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! Bounded outbound HTTP worker around `reqwest`, for Tina services.
//!
//! Adoption bridge. Not native Tina HTTP. Tokio and reqwest own
//! outbound sockets and TLS; Tina owns bounded ingress, visible
//! pressure, and per-request timeout/cap policy.
//!
//! # Use
//!
//! Build a worker, register it on a Tina runtime, then `send_request`
//! from any Tina handler:
//!
//! ```no_run
//! use std::convert::Infallible;
//! use std::time::Duration;
//!
//! use tina::prelude::*;
//! use tina_reqwest_bridge::{
//!     ReqwestAddress, ReqwestCallOutcome, ReqwestRequest, send_request,
//! };
//! use tina_runtime::RuntimeCall;
//!
//! enum AppMsg {
//!     Start,
//!     HttpReturned(ReqwestCallOutcome),
//! }
//!
//! struct App {
//!     http: ReqwestAddress,
//! }
//!
//! impl Isolate for App {
//!     tina::isolate_types! {
//!         message: AppMsg,
//!         reply: (),
//!         send: tina::Outbound<Infallible>,
//!         spawn: Infallible,
//!         call: RuntimeCall<AppMsg>,
//!         shard: SingleShard,
//!     }
//!
//!     fn handle(&mut self, msg: AppMsg, _ctx: &mut Context<'_, SingleShard, Self::Reply>) -> Effect<Self> {
//!         match msg {
//!             AppMsg::Start => send_request(
//!                 self.http,
//!                 ReqwestRequest::get("http://127.0.0.1:0/"),
//!                 Duration::from_secs(2),
//!             )
//!             .then(AppMsg::HttpReturned),
//!             AppMsg::HttpReturned(_) => stop(),
//!         }
//!     }
//! }
//! ```
//!
//! The reply shape preserves both error layers separately:
//!
//! - outer [`tina_runtime::CallOutcome::Full`] / `Closed` / `Timeout` is
//!   *bridge-delivery* truth (could the IsolateCall reach the worker);
//! - inner [`ReqwestError`] is *worker-outcome* truth (did reqwest
//!   succeed, time out, hit a body cap, etc.).
//!
//! See [`flatten_outcome`] for an opt-in helper that collapses both
//! layers into a single `Result<ReqwestResponse, ReqwestCallError>`
//! while still naming which layer failed. The default path keeps the
//! layers distinct.
//!
//! # Preserved Tina guarantees
//!
//! - Bounded ingress: mailbox capacity caps queued sends; past the
//!   cap the caller observes `CallError::TargetFull`.
//! - Visible failure: every failure mode is a typed
//!   [`ReqwestError`] variant. Nothing is silently absorbed.
//! - Bounded in-flight: `max_in_flight` caps concurrent reqwest tasks.
//! - Bounded body sizes: request and response bodies are capped.
//! - Synchronous handlers: the worker isolate handles each message in
//!   one synchronous turn. Async work runs on Tokio via spawn; the
//!   shard thread does not block.
//!
//! # Weakened Tina guarantees
//!
//! - Deterministic replay: reqwest IO is not deterministic and is not
//!   observed by `tina-sim`. Replay parity is best-effort at the
//!   bridge boundary only.
//! - Cancellation precision: once the spawned reqwest task starts on
//!   Tokio, the bridge can abort the task but cannot guarantee that
//!   bytes are not already on the wire. Late results are dropped at
//!   the runtime layer (visible as `CallReplyRejected` in the
//!   trace), not counted in bridge metrics.
//! - Polling latency: the bridge wakes via Tina's `sleep` to
//!   re-check the result channel each `poll_interval`. That is
//!   visible chatter in the trace and adds bounded latency to
//!   completion.
//!
//! # Pressure rule
//!
//! ```text
//! mailbox full  -> CallError::TargetFull (Tina ingress)
//! max_in_flight -> ReqwestError::Full
//! per-request   -> ReqwestError::Timeout
//! body too big  -> ReqwestError::RequestTooLarge / ResponseTooLarge
//! reqwest fail  -> ReqwestError::Reqwest(reason)
//! worker closed -> ReqwestError::Closed
//! ```
//!
//! # Bridge vs native
//!
//! `tina-http` provides a native outbound HTTP/1.1 client driven by
//! Tina's own `tcp_*` runtime calls. That path is deterministic under
//! `tina-sim`, has no Tokio dependency, and owns every byte. Use it
//! when those properties matter.
//!
//! `tina-reqwest-bridge` wraps mature reqwest. It speaks HTTPS, HTTP/2,
//! redirects, and connection reuse out of the box. Use it when those
//! features matter more than determinism. The native client is the
//! Tina-owned path; this crate is the adoption bridge.
//!
//! # Dropped-caller rule
//!
//! - Before admission: no work starts. The mailbox or `max_in_flight`
//!   cap rejects synchronously.
//! - After admission, before reqwest accepts: the worker has spawned
//!   the reqwest task. The per-attempt timeout aborts it; the bridge
//!   surfaces [`ReqwestError::Timeout`].
//! - After reqwest accepts: same — timeout aborts the task. Bytes
//!   already on the wire stay on the wire. The bridge does not observe
//!   the Tina caller's `IsolateCall` deadline directly; if the caller
//!   has already given up, the worker's eventual Reply is dropped by
//!   the runtime as `CallReplyRejected` and is visible in the trace,
//!   not in bridge metrics.
//!
//! # Close
//!
//! [`ReqwestCloser::close`] is a graceful drain: new sends reply
//! [`ReqwestError::Closed`], in-flight requests run to natural
//! completion (or hit their per-attempt timeout). To force-cancel
//! in-flight work, drop the hosting Tina runtime.
//!
//! # Retry
//!
//! See [`RetryPolicy`]. Default is no retry. When configured, retries
//! are bounded, fire only on transport-level failures
//! ([`ReqwestError::Timeout`] and [`ReqwestError::Reqwest`]), and each
//! attempt resets its own per-attempt clock. [`ReqwestError::Full`] is
//! never retried by the worker — it is the explicit pressure signal
//! and the caller decides what to do.

mod helpers;
mod metrics;
mod types;
mod worker;

pub use helpers::{
    BridgeFailure, ReqwestAddress, ReqwestCallError, ReqwestCallOutcome, ReqwestFatalReason,
    ReqwestOutcomeClass, ReqwestOutcomeExt, ReqwestResult, ReqwestTransientReason, flatten_outcome,
    send_request,
};
pub use metrics::{ReqwestMetrics, ReqwestMetricsHandle};
pub use types::{
    RedirectPolicy, ReqwestConfig, ReqwestConfigError, ReqwestError, ReqwestRequest,
    ReqwestResponse, RetryPolicy,
};
pub use worker::{InstallError, InstalledReqwestBridge, ReqwestCloser, ReqwestMsg, ReqwestWorker};
