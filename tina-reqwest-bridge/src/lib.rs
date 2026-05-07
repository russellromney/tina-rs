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
//! Build a worker, register it on a Tina runtime, then `call(...)` it
//! from any Tina handler:
//!
//! ```no_run
//! use std::convert::Infallible;
//! use std::time::Duration;
//!
//! use tina::prelude::*;
//! use tina_reqwest_bridge::{
//!     ReqwestConfig, ReqwestError, ReqwestMsg, ReqwestRequest, ReqwestResponse,
//!     ReqwestWorker,
//! };
//! use tina_runtime::{
//!     DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime, ThreadedRuntimeConfig, call,
//! };
//!
//! enum AppMsg {
//!     Start,
//!     HttpReturned(Result<ReqwestResponse, ReqwestError>),
//! }
//!
//! struct App {
//!     http: Address<ReqwestMsg, Result<ReqwestResponse, ReqwestError>>,
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
//!     fn handle(&mut self, msg: AppMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
//!         match msg {
//!             AppMsg::Start => call(
//!                 self.http,
//!                 ReqwestMsg::Send(ReqwestRequest::get("http://127.0.0.1:0/")),
//!                 Duration::from_secs(2),
//!             )
//!             .reply(AppMsg::HttpReturned),
//!             AppMsg::HttpReturned(_) => stop(),
//!         }
//!     }
//! }
//! ```
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
//!   bytes are not already on the wire. Late results are discarded
//!   and counted in metrics.
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

mod metrics;
mod types;
mod worker;

pub use metrics::{ReqwestMetrics, ReqwestMetricsHandle};
pub use types::{
    RedirectPolicy, ReqwestConfig, ReqwestError, ReqwestRequest, ReqwestResponse, RetryPolicy,
};
pub use worker::{ReqwestCloser, ReqwestMsg, ReqwestWorker};
