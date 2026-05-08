#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! `tower::Service` adapter for Tina bridge handles.
//!
//! Wraps a [`tina_tokio_bridge::BridgeHandle`] in a value that
//! implements [`tower_service::Service`], so Tower-shaped HTTP stacks
//! (Axum, Hyper, Tower middleware) can call into a Tina service the
//! same way they call any other backend.
//!
//! # Pressure rule
//!
//! ```text
//! Tina Full     -> Service future returns Err(BridgeError::Full)
//! Tina Closed   -> Service future returns Err(BridgeError::Closed)
//! Tina Timeout  -> Service future returns Err(BridgeError::Timeout)
//! ```
//!
//! `poll_ready` is **never** `Pending`. It returns `Ready(Ok(()))` when
//! the bridge is open and `Ready(Err(Closed))` when it has been closed.
//! Admission backpressure surfaces on the `call`-side future as
//! `Err(Full)`, never as `Pending`. This is deliberate: Tina ingress
//! cannot back-press a Tower readiness check without an unbounded
//! wait, and the plan rule is that bridges show pressure rather than
//! hide it.
//!
//! # Use
//!
//! ```no_run
//! use std::time::Duration;
//! use tina_tokio_bridge::BridgeHandle;
//! use tina_tower_bridge::{Service, TinaService, TinaTowerService};
//!
//! # fn build_handle() -> BridgeHandle<u32, u32, tina::SingleShard, tina_runtime::DefaultThreadedMailboxFactory> { unimplemented!() }
//! # async fn driver() {
//! let handle: BridgeHandle<u32, u32, _, _> = build_handle();
//! let mut svc: TinaService<u32, u32> = TinaTowerService::new(handle);
//! let response = svc.call(7).await.expect("svc call");
//! # let _ = response;
//! # }
//! ```
//!
//! # Cancellation
//!
//! Dropping the call future *before* it is first polled never starts
//! work. After the future is polled once, the bridge has begun the
//! `try_send_and_observe` admission; after that, dropping the future
//! cannot guarantee that no work runs. The Tina handler may still
//! execute and its reply is discarded by the bridge's metrics path.
//!
//! # Preserved Tina guarantees
//!
//! - **Bounded ingress**: every `call` goes through the underlying
//!   bridge handle's `try_send_and_observe`; admission is bounded by
//!   the Tina mailbox capacity. `Full` is caller-visible.
//! - **Visible failures**: every error path is a typed
//!   [`tina_tokio_bridge::BridgeError`] variant.
//! - **Synchronous handlers**: the wrapped Tina isolate's handler runs
//!   in one synchronous turn. The Service does not introduce its own
//!   async work or queues.
//! - **No hidden Service queue**: this crate adds nothing between
//!   Tower and the bridge handle. `Service::call` is a thin shell
//!   around `BridgeHandle::call`.
//!
//! # Weakened Tina guarantees
//!
//! - **Deterministic replay**: Tower's executor is not observed by
//!   `tina-sim`. Replay parity is best-effort at the bridge boundary
//!   only.
//! - **Tower readiness backpressure**: `poll_ready` does not reflect
//!   Tina mailbox occupancy or in-flight count. Readiness only signals
//!   "bridge open" vs "closed". A user expecting Tower's "ask if
//!   ready, then admit one slot" reservation idiom must remember that
//!   admission still races on the `call` future.
//!
//! # Request id and trace context
//!
//! The Service is generic over the request type `M`. Trace ids,
//! correlator headers, and `tracing` span context are caller metadata
//! and travel inside `M`. The bridge does not introduce a global
//! current-span or a thread-local — there is no hidden context across
//! the boundary. If the caller wants spans to follow the request,
//! attach them to `M` (e.g. as a typed field) and read them inside
//! the Tina handler.
//!
//! # Tower middleware safety
//!
//! Most Tower layers compose cleanly. A few introduce a second
//! buffer or cap on top of Tina's bounded ingress; pick one source
//! of truth for overload and document the choice.
//!
//! - `tower::buffer::Buffer` takes an explicit bound (`Buffer::new(svc, bound)`),
//!   so the buffer's channel is bounded — no unbounded queue is
//!   introduced. The thing to remember is that requests waiting in
//!   the Buffer channel sit *outside* Tina's pressure surface; they
//!   do not show up in `BridgeMetrics`. Pick the `bound` deliberately
//!   so total in-flight (Buffer + Tina ingress) stays bounded.
//! - `tower::load_shed::LoadShed` is fine — it sheds when the inner
//!   Service is `Pending`, which we never are.
//! - `tower::limit::ConcurrencyLimit` adds an *outer* in-flight cap
//!   on top of Tina's bounded mailbox ingress. Two caps now exist:
//!   the Tower Semaphore and the Tina mailbox capacity. Pick which
//!   one owns overload semantics — typically set the inner mailbox
//!   roomy and let `ConcurrencyLimit` shed first, or skip
//!   `ConcurrencyLimit` and let Tina ingress surface `Full`.
//! - `tower::timeout::Timeout` is fine — its outer timeout cancels
//!   our future on expiry. The cancellation maps to the
//!   drop-after-admission case: bridge counts a dropped responder.
//!
//! # Cloning and `&mut self`
//!
//! Tower's `Service::call` takes `&mut self`. Cloning a
//! [`TinaTowerService`] clones the bridge handle (cheap `Arc` clone)
//! — it does **not** clone the Tina service state behind the bridge.
//! Two clones share one isolate.
//!
//! In Axum / WebSocket handlers, clone per async task or per split
//! half when you need independent callers. Axum's `State<S>` extracts
//! the value, not `&mut S`, so handlers that call the service
//! directly need a one-line rebind:
//!
//! ```no_run
//! # use axum::extract::State;
//! # use axum::http::StatusCode;
//! # use tina_tokio_bridge::BridgeError;
//! # use tina_tower_bridge::{Service, TinaService};
//! async fn handler(State(svc): State<TinaService<u32, u32>>) -> Result<String, StatusCode> {
//!     let mut svc = svc;
//!     match svc.call(7).await {
//!         Ok(v) => Ok(v.to_string()),
//!         Err(BridgeError::Full | BridgeError::Closed) => Err(StatusCode::SERVICE_UNAVAILABLE),
//!         Err(BridgeError::Timeout) => Err(StatusCode::GATEWAY_TIMEOUT),
//!     }
//! }
//! ```
//!
//! For per-connection fan-out (e.g. WebSocket reader/writer split),
//! clone the service so each task owns an independent `&mut Service`:
//!
//! ```no_run
//! # use tina_tower_bridge::{Service, TinaService};
//! # async fn fan_out(svc: TinaService<u8, u8>) -> Result<(), tina_tokio_bridge::BridgeError> {
//! let mut sub_svc = svc.clone();
//! sub_svc.call(0).await?;
//!
//! let mut publish_svc = svc.clone();
//! publish_svc.call(1).await?;
//! # Ok(()) }
//! ```
//!
//! See [`tina_tokio_bridge`] for the underlying bridge contract.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tina::Shard;
use tina_runtime::MailboxFactory;
use tina_tokio_bridge::{BridgeError, BridgeHandle, BridgeMessage, BridgeRequest};
#[cfg(feature = "tracing")]
use tracing::{Level, event};

/// `tracing` target for per-call events at the tower layer.
///
/// Tower events sit *above* the underlying tina-tokio bridge events
/// (`tina_tokio.bridge.call`). Filter on this target for the
/// service-layer view; filter on the inner target for transport truth.
#[cfg(feature = "tracing")]
const TRACE_TARGET_CALL: &str = "tina_tower.bridge.call";

/// `tracing` target for tower-layer lifecycle events.
#[cfg(feature = "tracing")]
const TRACE_TARGET_BRIDGE: &str = "tina_tower.bridge";

#[cfg(feature = "tracing")]
fn bridge_error_reason(error: BridgeError) -> &'static str {
    match error {
        BridgeError::Full => "Full",
        BridgeError::Closed => "Closed",
        BridgeError::Timeout => "Timeout",
    }
}

/// `tower::Service` wrapper around a [`BridgeHandle`].
///
/// Cloneable; each clone shares the same bridge handle and metrics.
pub struct TinaTowerService<M, R, S, F, AR = (), TM = BridgeRequest<M, R>>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    handle: BridgeHandle<M, R, S, F, AR, TM>,
    per_call_timeout: Option<Duration>,
}

impl<M, R, S, F, AR, TM> Clone for TinaTowerService<M, R, S, F, AR, TM>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    fn clone(&self) -> Self {
        Self {
            handle: self.handle.clone(),
            per_call_timeout: self.per_call_timeout,
        }
    }
}

impl<M, R, S, F, AR, TM> TinaTowerService<M, R, S, F, AR, TM>
where
    M: Send + 'static,
    R: Send + 'static,
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
    AR: Send + 'static,
    TM: BridgeMessage + Send + 'static,
{
    /// Wraps `handle`. The Service uses the bridge handle's default
    /// timeout for every `call`.
    pub fn new(handle: BridgeHandle<M, R, S, F, AR, TM>) -> Self {
        Self {
            handle,
            per_call_timeout: None,
        }
    }

    /// Sets a per-`call` timeout that overrides the bridge handle's
    /// default. Returns the same service for chaining.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.per_call_timeout = Some(timeout);
        self
    }

    /// Borrows the underlying bridge handle.
    pub fn handle(&self) -> &BridgeHandle<M, R, S, F, AR, TM> {
        &self.handle
    }

    /// Marks the underlying bridge handle (and every clone) as closed.
    /// After close, `poll_ready` returns `Ready(Err(Closed))`.
    pub fn close(&self) {
        // Underlying bridge emits its own close event on
        // `tina_tokio.bridge`. We add a tower-layer marker so
        // operators filtering by tower target see the lifecycle too.
        #[cfg(feature = "tracing")]
        event!(target: TRACE_TARGET_BRIDGE, Level::DEBUG, kind = "close");
        self.handle.close();
    }

    /// Whether the underlying bridge handle has been closed.
    pub fn is_closed(&self) -> bool {
        matches!(
            self.handle.health(),
            tina_tokio_bridge::BridgeHealth::Closed
        )
    }
}

impl<M, R, S, F, AR, TM> Service<M> for TinaTowerService<M, R, S, F, AR, TM>
where
    M: Send + 'static,
    R: Send + 'static,
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
    AR: Send + 'static,
    TM: BridgeMessage + Send + 'static,
{
    type Response = R;
    type Error = BridgeError;
    type Future = Pin<Box<dyn Future<Output = Result<R, BridgeError>> + Send>>;

    /// Returns `Ready(Ok)` when the bridge is open, `Ready(Err(Closed))`
    /// once it has been closed. Never returns `Pending`.
    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self.handle.health() {
            tina_tokio_bridge::BridgeHealth::Accepting => Poll::Ready(Ok(())),
            tina_tokio_bridge::BridgeHealth::Closed => Poll::Ready(Err(BridgeError::Closed)),
        }
    }

    fn call(&mut self, request: M) -> Self::Future {
        let handle = self.handle.clone();
        let per_call = self.per_call_timeout;
        #[cfg(feature = "tracing")]
        event!(target: TRACE_TARGET_CALL, Level::DEBUG, kind = "tower_call");
        Box::pin(async move {
            let result = match per_call {
                Some(timeout) => handle.call_with_timeout(request, timeout).await,
                None => handle.call(request).await,
            };
            #[cfg(feature = "tracing")]
            match &result {
                Ok(_) => event!(
                    target: TRACE_TARGET_CALL,
                    Level::DEBUG,
                    kind = "tower_response",
                ),
                Err(error) => {
                    let reason = bridge_error_reason(*error);
                    event!(
                        target: TRACE_TARGET_CALL,
                        Level::WARN,
                        kind = "tower_error",
                        reason,
                    );
                }
            }
            result
        })
    }
}

/// Re-export of [`tower_service::Service`] so callers do not need a
/// direct `tower-service` dep.
pub use tower_service::Service;

/// Specimen-facing alias for the common Tower service shape: single
/// shard, default threaded mailbox factory, no extra address reply
/// type. Most Tina specimens want this rather than the full
/// [`TinaTowerService`] with six explicit generics.
pub type TinaService<M, R> =
    TinaTowerService<M, R, tina::SingleShard, tina_runtime::DefaultThreadedMailboxFactory, ()>;
