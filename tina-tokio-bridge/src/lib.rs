#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

//! Narrow Tokio/Tower bridge for Tina services.
//!
//! The bridge does not make Tina handlers async and does not add a hidden
//! executor queue. Tokio code sends one bounded request into a
//! [`ThreadedRuntime`], then
//! waits for a oneshot response with an explicit timeout.
//!
//! ```compile_fail
//! use std::rc::Rc;
//! fn must_be_send<T: Send>() {}
//! must_be_send::<tina_tokio_bridge::BridgeRequest<Rc<u8>, ()>>();
//! ```
//!
//! ```compile_fail
//! let (sender, _receiver) = tokio::sync::oneshot::channel::<u8>();
//! let request = tina_tokio_bridge::BridgeRequest::new((), sender);
//! let (_message, responder) = request.into_parts();
//! responder.respond("wrong response type");
//! ```
//!
//! # Direct `BridgeRequest` vs wrapper enum
//!
//! The simplest bridge-hosted isolate takes [`BridgeRequest<M, R>`]
//! directly as its message type. One Tina message = one bridge call;
//! the handler unpacks `(request, responder)` and replies. Use this
//! shape when the isolate has nothing to do but respond:
//!
//! ```ignore
//! #[tina::isolate(message = BridgeRequest<CounterRequest, CounterReply>)]
//! impl Counter {
//!     fn handle(
//!         &mut self,
//!         msg: BridgeRequest<CounterRequest, CounterReply>,
//!         _ctx: &mut Context<'_, SingleShard, Self::Reply>,
//!     ) -> Effect<Self> {
//!         let (request, responder) = msg.into_parts();
//!         self.value += 1;
//!         let _ = responder.respond(CounterReply { value: self.value });
//!         noop()
//!     }
//! }
//! ```
//!
//! As soon as the isolate needs to do anything *between* "request
//! arrived" and "respond" — sleep on a timer, do an outbound runtime
//! call, talk to a peer isolate, schedule retries — the direct
//! `BridgeRequest` shape stops being enough. The handler must return
//! a runtime call ([`tina_runtime::sleep`], `tcp_*`, etc.) and resume
//! later when the runtime delivers a continuation message. That
//! continuation has to be the same isolate's message type, which
//! means wrapping `BridgeRequest` in a custom enum:
//!
//! ```ignore
//! #[derive(Debug)]
//! enum CounterMsg {
//!     /// Inbound bridge call. Carries the request + the responder
//!     /// the handler must answer.
//!     Request(BridgeRequest<CounterRequest, CounterReply>),
//!     /// Per-tick continuation: timer fired, return to work.
//!     Tick(Result<(), tina_runtime::CallError>),
//!     /// Outbound runtime-call completion. Carries the responder
//!     /// captured from the original Request so the handler can
//!     /// answer once the work finishes.
//!     Done(Result<Vec<u8>, tina_runtime::CallError>, BridgeResponder<CounterReply>),
//! }
//!
//! impl From<BridgeRequest<CounterRequest, CounterReply>> for CounterMsg {
//!     fn from(value: BridgeRequest<CounterRequest, CounterReply>) -> Self {
//!         Self::Request(value)
//!     }
//! }
//!
//! impl BridgeMessage for CounterMsg {
//!     fn bridge_cancelled(&self) -> bool {
//!         match self {
//!             Self::Request(req) => req.bridge_cancelled(),
//!             // Continuations are not subject to caller cancellation
//!             // — they were scheduled by the bridge's own runtime
//!             // calls.
//!             Self::Tick(_) | Self::Done(_, _) => false,
//!         }
//!     }
//! }
//! ```
//!
//! Two rules for the wrapper:
//!
//! 1. The wrapper enum must implement `From<BridgeRequest<M, R>>` so
//!    [`BridgeHost::register_bridge`] and the [`BridgeHandle`]
//!    machinery can lift inbound calls into your message type.
//! 2. The wrapper must impl [`BridgeMessage`] and forward
//!    `bridge_cancelled()` to the underlying [`BridgeRequest`] when
//!    the variant carries one. Internal continuations should return
//!    `false`.
//!
//! Carry the [`BridgeResponder<R>`] across continuation messages
//! (e.g. inside a `Done(...)` variant) when the response cannot be
//! answered until the runtime call returns. The responder is
//! `Send`-bounded and survives across handler turns.
//!
//! When in doubt, start with the direct `BridgeRequest` shape and
//! convert to the wrapper enum only when an internal continuation
//! actually appears.

use std::convert::Infallible;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tina::{Address, Isolate, Outbound as TinaOutbound, Shard};
use tina_runtime::{
    CallError, IntoErasedCall, LocalSystem, MailboxFactory, RuntimeEvent, SendRejectedReason,
    ThreadedRuntime, ThreadedRuntimeConfig, ThreadedRuntimeError, ThreadedSendObservedError,
    ThreadedTrySendError,
};
use tokio::sync::oneshot;
#[cfg(feature = "tracing")]
use tracing::{Level, event};

/// `tracing` target for per-call events on the bridge.
#[cfg(feature = "tracing")]
const TRACE_TARGET_CALL: &str = "tina_tokio.bridge.call";

/// `tracing` target for bridge lifecycle events (close).
#[cfg(feature = "tracing")]
const TRACE_TARGET_BRIDGE: &str = "tina_tokio.bridge";

#[cfg(feature = "tracing")]
fn bridge_error_reason(error: BridgeError) -> &'static str {
    match error {
        BridgeError::Full => "Full",
        BridgeError::Closed => "Closed",
        BridgeError::Timeout => "Timeout",
    }
}

/// Error returned by a Tokio-to-Tina bridge call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeError {
    /// The bounded Tina worker ingress queue is full.
    Full,
    /// Tina worker or responder was closed before a response arrived.
    Closed,
    /// The caller's explicit bridge timeout elapsed.
    Timeout,
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => f.write_str("tina bridge: bounded ingress full"),
            Self::Closed => f.write_str("tina bridge: worker or responder closed"),
            Self::Timeout => f.write_str("tina bridge: call timed out"),
        }
    }
}

impl std::error::Error for BridgeError {}

impl BridgeError {
    /// Converts only the bridge errors that are true send-rejection reasons.
    pub fn send_rejected_reason(self) -> Option<SendRejectedReason> {
        match self {
            Self::Full => Some(SendRejectedReason::Full),
            Self::Closed => Some(SendRejectedReason::Closed),
            Self::Timeout => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_error_send_rejection_mapping_keeps_timeout_distinct() {
        assert_eq!(
            BridgeError::Full.send_rejected_reason(),
            Some(SendRejectedReason::Full)
        );
        assert_eq!(
            BridgeError::Closed.send_rejected_reason(),
            Some(SendRejectedReason::Closed)
        );
        assert_eq!(BridgeError::Timeout.send_rejected_reason(), None);
        assert_eq!(CallError::from(BridgeError::Timeout), CallError::Timeout);
    }
}

/// Capability status for the Tokio bridge compared with Tina core semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityStatus {
    /// The bridge preserves this Tina guarantee.
    Preserved,
    /// The bridge keeps a useful but weaker form of this guarantee.
    Weakened,
    /// The bridge does not claim this capability.
    NotClaimed,
}

/// Public bridge capability table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeCapabilityTable {
    /// Bounded ingress from Tokio into Tina.
    pub bounded_ingress: CapabilityStatus,
    /// Synchronous Tina isolate handlers.
    pub synchronous_handlers: CapabilityStatus,
    /// Explicit `Full` / `Closed` / `Timeout` outcomes.
    pub visible_failures: CapabilityStatus,
    /// Byte-for-byte deterministic replay under Tokio.
    pub deterministic_replay: CapabilityStatus,
    /// Tokio/Tower hidden queue behavior.
    pub tokio_scheduler_control: CapabilityStatus,
}

/// Capabilities preserved and weakened by this bridge.
pub const BRIDGE_CAPABILITIES: BridgeCapabilityTable = BridgeCapabilityTable {
    bounded_ingress: CapabilityStatus::Preserved,
    synchronous_handlers: CapabilityStatus::Preserved,
    visible_failures: CapabilityStatus::Preserved,
    deterministic_replay: CapabilityStatus::Weakened,
    tokio_scheduler_control: CapabilityStatus::NotClaimed,
};

/// Explicit bridge backpressure policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeBackpressure {
    /// Return [`BridgeError::Full`] immediately when bounded Tina ingress or
    /// the target mailbox is full.
    Reject,
    /// Retry `Full` with an explicit delay and bounded retry count.
    Retry {
        /// Number of additional attempts after the first failed attempt.
        max_retries: usize,
        /// Delay between attempts.
        delay: Duration,
    },
    /// Retry `Full` with a bounded retry count and total deadline.
    RetryWithin {
        /// Number of additional attempts after the first failed attempt.
        max_retries: usize,
        /// Delay between attempts.
        delay: Duration,
        /// Total deadline for all attempts and retry delays.
        total_timeout: Duration,
    },
}

impl BridgeBackpressure {
    /// Reject overload immediately.
    pub const fn reject() -> Self {
        Self::Reject
    }

    /// Retry overload with an explicit delay and maximum retry count.
    ///
    /// `max_retries` counts retries after the first attempt, so
    /// `retry(2, delay)` can make at most three attempts.
    pub const fn retry(max_retries: usize, delay: Duration) -> Self {
        Self::Retry { max_retries, delay }
    }

    /// Retry overload with an explicit delay, maximum retry count, and total
    /// deadline for the whole policy.
    pub const fn retry_within(
        max_retries: usize,
        delay: Duration,
        total_timeout: Duration,
    ) -> Self {
        Self::RetryWithin {
            max_retries,
            delay,
            total_timeout,
        }
    }
}

/// Bridge health state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeHealth {
    /// The bridge accepts new calls. Bounded queue admission is still checked
    /// when a call is submitted.
    Accepting,
    /// The bridge has been closed and rejects new calls.
    Closed,
}

/// Snapshot of bridge counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BridgeMetricsSnapshot {
    /// Calls submitted by Tokio/Tower code.
    pub attempts: u64,
    /// Requests accepted into the target Tina mailbox.
    pub accepted: u64,
    /// Requests rejected because worker ingress or target mailbox was full.
    pub full: u64,
    /// Requests rejected because the bridge, worker, mailbox, or responder was
    /// closed.
    pub closed: u64,
    /// Requests that timed out while waiting for Tina.
    pub timeout: u64,
    /// Requests that received a response successfully.
    pub responses: u64,
    /// Tina tried to respond after the Tokio caller had gone away.
    pub dropped_responses: u64,
}

#[derive(Debug, Default)]
struct BridgeMetrics {
    attempts: AtomicU64,
    accepted: AtomicU64,
    full: AtomicU64,
    closed: AtomicU64,
    timeout: AtomicU64,
    responses: AtomicU64,
    dropped_responses: AtomicU64,
}

impl BridgeMetrics {
    fn snapshot(&self) -> BridgeMetricsSnapshot {
        BridgeMetricsSnapshot {
            attempts: self.attempts.load(Ordering::Relaxed),
            accepted: self.accepted.load(Ordering::Relaxed),
            full: self.full.load(Ordering::Relaxed),
            closed: self.closed.load(Ordering::Relaxed),
            timeout: self.timeout.load(Ordering::Relaxed),
            responses: self.responses.load(Ordering::Relaxed),
            dropped_responses: self.dropped_responses.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Default)]
struct BridgeState {
    closed: AtomicBool,
    metrics: BridgeMetrics,
}

impl BridgeState {
    fn close(&self) {
        #[cfg(feature = "tracing")]
        {
            // swap so we can suppress the trace event on idempotent re-close.
            let was_closed = self.closed.swap(true, Ordering::AcqRel);
            if !was_closed {
                event!(target: TRACE_TARGET_BRIDGE, Level::DEBUG, kind = "close");
            }
        }
        #[cfg(not(feature = "tracing"))]
        self.closed.store(true, Ordering::Release);
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

enum BridgeWaitOutcome<R> {
    Response(R),
    ObservedError(BridgeError),
    ObserveChannelClosed,
    ReplyClosed,
}

struct BridgeCancellation {
    cancelled: Arc<AtomicBool>,
    armed: bool,
}

impl BridgeCancellation {
    fn new(cancelled: Arc<AtomicBool>) -> Self {
        Self {
            cancelled,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for BridgeCancellation {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled.store(true, Ordering::Release);
        }
    }
}

/// Message types accepted by a bridge handle.
///
/// Bridge-hosted services can use a larger service enum instead of accepting
/// [`BridgeRequest`] directly. Implement this trait by returning whether the
/// wrapped bridge request has been cancelled; runtime completion messages
/// should normally return `false`.
pub trait BridgeMessage {
    /// Returns whether this message wraps a cancelled bridge request.
    fn bridge_cancelled(&self) -> bool;
}

struct BridgeGuard<I> {
    inner: I,
}

impl<I> BridgeGuard<I> {
    fn new(inner: I) -> Self {
        Self { inner }
    }
}

impl<I> std::fmt::Debug for BridgeGuard<I>
where
    I: std::fmt::Debug,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BridgeGuard")
            .field("inner", &self.inner)
            .finish()
    }
}

impl<I> Isolate for BridgeGuard<I>
where
    I: Isolate<SpawnObserved = Infallible, SpawnObservedRemote = Infallible>,
    I::Message: BridgeMessage,
{
    type Message = I::Message;
    type Reply = I::Reply;
    type Send = I::Send;
    type Spawn = I::Spawn;
    type SpawnObserved = std::convert::Infallible;
    type Call = I::Call;
    type Fact = I::Fact;
    type Shard = I::Shard;

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut tina::Context<'_, Self::Shard, Self::Reply>,
    ) -> tina::Effect<Self> {
        if msg.bridge_cancelled() {
            return tina::noop();
        }

        remap_effect(self.inner.handle(msg, ctx))
    }
}

fn remap_effect<I>(effect: tina::Effect<I>) -> tina::Effect<BridgeGuard<I>>
where
    I: Isolate<SpawnObserved = Infallible, SpawnObservedRemote = Infallible>,
    I::Message: BridgeMessage,
{
    match effect {
        tina::Effect::Noop => tina::Effect::Noop,
        tina::Effect::Reply(reply) => tina::Effect::Reply(reply),
        tina::Effect::Reject(reason) => tina::Effect::Reject(reason),
        tina::Effect::Send(send) => tina::Effect::Send(send),
        tina::Effect::Spawn(spawn) => tina::Effect::Spawn(spawn),
        tina::Effect::SpawnObserved(spawn) => match spawn {},
        tina::Effect::SpawnObservedOn(spawn) => match spawn {},
        tina::Effect::Stop => tina::Effect::Stop,
        tina::Effect::Fail => tina::Effect::Fail,
        tina::Effect::StopWith(result) => tina::Effect::StopWith(result),
        tina::Effect::RestartChildren => tina::Effect::RestartChildren,
        tina::Effect::StopChildren => tina::Effect::StopChildren,
        tina::Effect::Call(call) => tina::Effect::Call(call),
        tina::Effect::Batch(effects) => {
            tina::Effect::Batch(effects.into_iter().map(remap_effect).collect())
        }
        tina::Effect::ReplyTo(slot, reply) => tina::Effect::ReplyTo(slot, reply),
        tina::Effect::Fact(fact) => tina::Effect::Fact(fact),
    }
}

impl From<ThreadedTrySendError> for BridgeError {
    fn from(error: ThreadedTrySendError) -> Self {
        match error {
            ThreadedTrySendError::IngressFull => Self::Full,
            ThreadedTrySendError::WorkerStopped => Self::Closed,
        }
    }
}

impl From<ThreadedSendObservedError> for BridgeError {
    fn from(error: ThreadedSendObservedError) -> Self {
        match error {
            ThreadedSendObservedError::IngressFull | ThreadedSendObservedError::MailboxFull => {
                Self::Full
            }
            ThreadedSendObservedError::MailboxClosed | ThreadedSendObservedError::WorkerStopped => {
                Self::Closed
            }
        }
    }
}

impl From<BridgeError> for CallError {
    fn from(error: BridgeError) -> Self {
        match error {
            BridgeError::Full => Self::TargetFull,
            BridgeError::Closed => Self::TargetClosed,
            BridgeError::Timeout => Self::Timeout,
        }
    }
}

/// Message payload sent from Tokio code into a Tina isolate.
///
/// A Tina handler receives this as its normal message, splits it into the user
/// request plus responder, and replies synchronously from the handler turn.
#[derive(Debug)]
pub struct BridgeRequest<M, R> {
    message: M,
    responder: BridgeResponder<R>,
    cancelled: Arc<AtomicBool>,
}

impl<M, R> BridgeRequest<M, R> {
    /// Creates one bridge request around a user message and oneshot responder.
    pub fn new(message: M, sender: oneshot::Sender<R>) -> Self {
        Self {
            message,
            responder: BridgeResponder {
                sender: Some(sender),
                state: None,
            },
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    fn with_metrics(
        message: M,
        sender: oneshot::Sender<R>,
        state: Arc<BridgeState>,
        cancelled: Arc<AtomicBool>,
    ) -> Self {
        Self {
            message,
            responder: BridgeResponder {
                sender: Some(sender),
                state: Some(state),
            },
            cancelled,
        }
    }

    /// Returns whether the Tokio caller has cancelled or timed out.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Borrows the user request without consuming the bridge responder.
    pub fn request(&self) -> &M {
        &self.message
    }

    /// Splits the bridge request into the user message and responder.
    pub fn into_parts(self) -> (M, BridgeResponder<R>) {
        (self.message, self.responder)
    }
}

impl<M, R> BridgeMessage for BridgeRequest<M, R> {
    fn bridge_cancelled(&self) -> bool {
        self.is_cancelled()
    }
}

/// One response handle owned by the Tina handler turn.
#[derive(Debug)]
pub struct BridgeResponder<R> {
    sender: Option<oneshot::Sender<R>>,
    state: Option<Arc<BridgeState>>,
}

impl<R> BridgeResponder<R> {
    /// Returns whether the Tokio caller has already gone away.
    pub fn is_closed(&self) -> bool {
        match &self.sender {
            Some(sender) => sender.is_closed(),
            None => true,
        }
    }

    /// Sends the response back to the Tokio caller.
    ///
    /// Returns the response when the Tokio caller has already timed out or
    /// dropped the request.
    pub fn respond(mut self, response: R) -> Result<(), R> {
        let result = self
            .sender
            .take()
            .expect("bridge responder is consumed once")
            .send(response);
        if result.is_err() {
            if let Some(state) = &self.state {
                state
                    .metrics
                    .dropped_responses
                    .fetch_add(1, Ordering::Relaxed);
            }
            #[cfg(feature = "tracing")]
            event!(
                target: TRACE_TARGET_CALL,
                Level::WARN,
                kind = "dropped_response",
                reason = "CallerClosed",
            );
        }
        result
    }
}

/// Bridge handle returned when a [`BridgeHost`] registers an isolate.
pub type RegisteredBridgeHandle<M, R, S, F, I> =
    BridgeHandle<M, R, S, F, <I as Isolate>::Reply, <I as Isolate>::Message>;

/// Owns one threaded Tina runtime for bridge-hosted services.
pub struct BridgeHost<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    runtime: Option<Arc<ThreadedRuntime<S, F>>>,
}

impl<S, F> BridgeHost<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    /// Starts a bridge host over one shard-owned Tina runtime.
    pub fn new(shard: S, mailbox_factory: F, config: ThreadedRuntimeConfig) -> Self {
        Self {
            runtime: Some(Arc::new(ThreadedRuntime::with_config(
                shard,
                mailbox_factory,
                config,
            ))),
        }
    }

    /// Builds a bridge host from the canonical local app owner.
    pub fn from_app(app: LocalSystem<S, F>) -> Self {
        Self {
            runtime: Some(Arc::new(app.into_threaded_runtime())),
        }
    }

    /// Returns the hosted Tina runtime.
    pub fn runtime(&self) -> &Arc<ThreadedRuntime<S, F>> {
        self.runtime
            .as_ref()
            .expect("bridge host runtime is unavailable after shutdown")
    }

    /// Registers one bridge-facing Tina service and returns its bridge handle.
    pub fn register_bridge<I, M, R, Outbound>(
        &self,
        isolate: I,
        mailbox_capacity: usize,
        timeout: Duration,
    ) -> Result<RegisteredBridgeHandle<M, R, S, F, I>, ThreadedRuntimeError>
    where
        I: Isolate<
                Shard = S,
                Send = TinaOutbound<Outbound>,
                Spawn = Infallible,
                SpawnObserved = Infallible,
                SpawnObservedRemote = Infallible,
            > + Send
            + 'static,
        I::Message: BridgeMessage + From<BridgeRequest<M, R>> + Send + 'static,
        I::Reply: Send + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: tina_runtime::IntoRuntimeFact + 'static,
        M: Send + 'static,
        R: Send + 'static,
        Outbound: 'static,
    {
        let address = self
            .runtime
            .as_ref()
            .expect("bridge host runtime is unavailable after shutdown")
            .register_with_capacity::<BridgeGuard<I>, Outbound>(
                BridgeGuard::new(isolate),
                mailbox_capacity,
            )?;
        Ok(BridgeHandle::with_message(
            Arc::clone(
                self.runtime
                    .as_ref()
                    .expect("bridge host runtime is unavailable after shutdown"),
            ),
            address,
            timeout,
            I::Message::from,
        ))
    }

    /// Attempts to shut down the hosted runtime.
    ///
    /// If bridge handles still exist, the host is left intact so callers can
    /// drop handles and retry.
    pub fn try_shutdown(&mut self) -> Result<Vec<RuntimeEvent>, BridgeShutdownError> {
        let Some(runtime) = self.runtime.take() else {
            return Ok(Vec::new());
        };

        match Arc::try_unwrap(runtime) {
            Ok(runtime) => runtime.shutdown().map_err(BridgeShutdownError::Runtime),
            Err(runtime) => {
                self.runtime = Some(runtime);
                Err(BridgeShutdownError::StillShared)
            }
        }
    }

    /// Shuts down the hosted runtime once all bridge handles have been dropped.
    pub fn shutdown(mut self) -> Result<Vec<RuntimeEvent>, BridgeShutdownError> {
        self.try_shutdown()
    }

    /// Number of `BridgeHandle` clones (or other `Arc<ThreadedRuntime>`
    /// holders) currently sharing the hosted runtime, beyond the host's own
    /// reference.
    ///
    /// Use this to observe drain progress before calling
    /// [`drain_and_shutdown`](Self::drain_and_shutdown). When this returns
    /// `0`, [`shutdown`](Self::shutdown) and [`try_shutdown`](Self::try_shutdown)
    /// will succeed.
    pub fn pending_handles(&self) -> usize {
        self.runtime
            .as_ref()
            .map(|runtime| Arc::strong_count(runtime).saturating_sub(1))
            .unwrap_or(0)
    }

    /// One-call drain + shutdown that replaces the `Arc::try_unwrap`
    /// dance Specimen examples used to write.
    ///
    /// Polls until every `BridgeHandle` clone has been dropped or
    /// `drain_timeout` elapses. If the drain completes, consumes the
    /// `Arc<ThreadedRuntime>` and shuts down. If handles remain, the host is
    /// left intact so callers can drop more handles and retry.
    ///
    /// `drain_timeout` is a real wall-clock timeout. The poll cadence is
    /// short (1ms) so tests do not need to cooperate with anything other
    /// than dropping their `BridgeHandle` clones. If the host itself never
    /// holds an extra clone of the runtime Arc, the drain is bounded by
    /// when callers drop their handles.
    pub fn drain_and_shutdown(
        &mut self,
        drain_timeout: Duration,
    ) -> Result<BridgeShutdownReport, BridgeShutdownError> {
        let deadline = std::time::Instant::now() + drain_timeout;
        loop {
            if self.pending_handles() == 0 {
                break;
            }
            if std::time::Instant::now() >= deadline {
                let pending = self.pending_handles();
                // We deliberately do not call try_shutdown here; the host is
                // still live so callers can drop more handles and retry.
                return Ok(BridgeShutdownReport {
                    trace: Vec::new(),
                    outstanding_handles_at_shutdown: pending,
                    drained_within_timeout: false,
                });
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let trace = self.try_shutdown()?;
        Ok(BridgeShutdownReport {
            trace,
            outstanding_handles_at_shutdown: 0,
            drained_within_timeout: true,
        })
    }

    /// Async drain + shutdown for callers already running on Tokio.
    ///
    /// This has the same contract as [`drain_and_shutdown`](Self::drain_and_shutdown)
    /// but uses `tokio::time::sleep` between polls so an Axum/Tokio shutdown
    /// task does not park a runtime worker thread while waiting for bridge
    /// handles to drop.
    pub async fn drain_and_shutdown_async(
        &mut self,
        drain_timeout: Duration,
    ) -> Result<BridgeShutdownReport, BridgeShutdownError> {
        let deadline = tokio::time::Instant::now() + drain_timeout;
        loop {
            if self.pending_handles() == 0 {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                let pending = self.pending_handles();
                return Ok(BridgeShutdownReport {
                    trace: Vec::new(),
                    outstanding_handles_at_shutdown: pending,
                    drained_within_timeout: false,
                });
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let trace = self.try_shutdown()?;
        Ok(BridgeShutdownReport {
            trace,
            outstanding_handles_at_shutdown: 0,
            drained_within_timeout: true,
        })
    }
}

/// Outcome of [`BridgeHost::drain_and_shutdown`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeShutdownReport {
    /// Trace recorded by the hosted Tina runtime up to shutdown.
    ///
    /// Empty when [`drained_within_timeout`](Self::drained_within_timeout) is
    /// `false`, because the runtime is deliberately left running so the host
    /// can retry after callers drop outstanding handles.
    pub trace: Vec<RuntimeEvent>,
    /// Number of `BridgeHandle` clones still alive when drain finished. When
    /// `drained_within_timeout` is `true` this is `0` and shutdown completed.
    pub outstanding_handles_at_shutdown: usize,
    /// Whether every bridge handle had been dropped before
    /// `drain_timeout` elapsed.
    pub drained_within_timeout: bool,
}

/// Error returned by [`BridgeHost::shutdown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeShutdownError {
    /// Some bridge handles still hold the runtime.
    StillShared,
    /// The hosted runtime failed during shutdown.
    Runtime(ThreadedRuntimeError),
}

/// Cloneable bounded bridge handle for Tokio/Tower callers.
pub struct BridgeHandle<M, R, S, F, AR = (), TM = BridgeRequest<M, R>>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    runtime: Arc<ThreadedRuntime<S, F>>,
    address: Address<TM, AR>,
    timeout: Duration,
    state: Arc<BridgeState>,
    into_message: Arc<dyn Fn(BridgeRequest<M, R>) -> TM + Send + Sync>,
    marker: PhantomData<fn(M, R, AR, TM)>,
}

impl<M, R, S, F, AR, TM> Clone for BridgeHandle<M, R, S, F, AR, TM>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    fn clone(&self) -> Self {
        Self {
            runtime: Arc::clone(&self.runtime),
            address: self.address,
            timeout: self.timeout,
            state: Arc::clone(&self.state),
            into_message: Arc::clone(&self.into_message),
            marker: PhantomData,
        }
    }
}

impl<M, R, S, F, AR> BridgeHandle<M, R, S, F, AR, BridgeRequest<M, R>>
where
    M: Send + 'static,
    R: Send + 'static,
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
    AR: Send + 'static,
{
    /// Builds a bridge handle to an already-registered Tina isolate.
    pub fn new(
        runtime: Arc<ThreadedRuntime<S, F>>,
        address: Address<BridgeRequest<M, R>, AR>,
        timeout: Duration,
    ) -> Self {
        Self {
            runtime,
            address,
            timeout,
            state: Arc::new(BridgeState::default()),
            into_message: Arc::new(|request| request),
            marker: PhantomData,
        }
    }
}

impl<M, R, S, F, AR, TM> BridgeHandle<M, R, S, F, AR, TM>
where
    M: Send + 'static,
    R: Send + 'static,
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
    AR: Send + 'static,
    TM: BridgeMessage + Send + 'static,
{
    /// Builds a bridge handle that maps bridge requests into a larger Tina
    /// service message type.
    pub fn with_message(
        runtime: Arc<ThreadedRuntime<S, F>>,
        address: Address<TM, AR>,
        timeout: Duration,
        into_message: impl Fn(BridgeRequest<M, R>) -> TM + Send + Sync + 'static,
    ) -> Self {
        Self {
            runtime,
            address,
            timeout,
            state: Arc::new(BridgeState::default()),
            into_message: Arc::new(into_message),
            marker: PhantomData,
        }
    }

    /// Closes this bridge handle and all clones derived from it.
    pub fn close(&self) {
        self.state.close();
    }

    /// Returns current bridge health.
    pub fn health(&self) -> BridgeHealth {
        if self.state.is_closed() {
            BridgeHealth::Closed
        } else {
            BridgeHealth::Accepting
        }
    }

    /// Returns a metrics snapshot for this bridge handle and its clones.
    pub fn metrics(&self) -> BridgeMetricsSnapshot {
        self.state.metrics.snapshot()
    }

    /// Sends one bounded request into Tina and waits for the handler response.
    pub async fn call(&self, message: M) -> Result<R, BridgeError> {
        self.call_once(message, self.timeout).await
    }

    /// Sends one request with a per-call timeout.
    pub async fn call_with_timeout(&self, message: M, timeout: Duration) -> Result<R, BridgeError> {
        self.call_once(message, timeout).await
    }

    /// Sends one request with an explicit overload policy and per-attempt
    /// timeout.
    pub async fn call_with_policy(
        &self,
        message: M,
        policy: BridgeBackpressure,
        per_attempt_timeout: Duration,
    ) -> Result<R, BridgeError>
    where
        M: Clone,
    {
        match policy {
            BridgeBackpressure::Reject => self.call_once(message, per_attempt_timeout).await,
            BridgeBackpressure::Retry { max_retries, delay } => {
                self.call_with_retry(message, max_retries, delay, per_attempt_timeout)
                    .await
            }
            BridgeBackpressure::RetryWithin {
                max_retries,
                delay,
                total_timeout,
            } => {
                match tokio::time::timeout(
                    total_timeout,
                    self.call_with_retry(message, max_retries, delay, per_attempt_timeout),
                )
                .await
                {
                    Ok(outcome) => outcome,
                    Err(_) => {
                        self.record_error(BridgeError::Timeout);
                        // record_error_on skips Timeout because the
                        // per-attempt site emits it with elapsed_ms
                        // (see call_once). The RetryWithin total
                        // budget has its own timeout site, so emit
                        // here too with the policy's total budget
                        // as elapsed_ms.
                        #[cfg(feature = "tracing")]
                        event!(
                            target: TRACE_TARGET_CALL,
                            Level::WARN,
                            kind = "timeout",
                            reason = "Timeout",
                            scope = "retry_within_total",
                            elapsed_ms = total_timeout.as_millis() as u64,
                        );
                        Err(BridgeError::Timeout)
                    }
                }
            }
        }
    }

    async fn call_with_retry(
        &self,
        message: M,
        mut remaining: usize,
        delay: Duration,
        per_attempt_timeout: Duration,
    ) -> Result<R, BridgeError>
    where
        M: Clone,
    {
        loop {
            match self.call_once(message.clone(), per_attempt_timeout).await {
                Err(BridgeError::Full) if remaining > 0 => {
                    remaining -= 1;
                    tokio::time::sleep(delay).await;
                }
                outcome => return outcome,
            }
        }
    }

    async fn call_once(&self, message: M, timeout: Duration) -> Result<R, BridgeError> {
        self.state.metrics.attempts.fetch_add(1, Ordering::Relaxed);
        if self.state.is_closed() {
            self.state.metrics.closed.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "tracing")]
            event!(
                target: TRACE_TARGET_CALL,
                Level::WARN,
                kind = "admission_rejected",
                reason = "Closed",
            );
            return Err(BridgeError::Closed);
        }

        let cancelled = Arc::new(AtomicBool::new(false));
        let mut cancellation = BridgeCancellation::new(Arc::clone(&cancelled));
        let (reply_tx, reply_rx) = oneshot::channel();
        let (observed_tx, observed_rx) = oneshot::channel();
        let state = Arc::clone(&self.state);
        let preflight_cancelled = Arc::new(AtomicBool::new(false));
        let preflight_cancelled_for_worker = Arc::clone(&preflight_cancelled);
        let request =
            BridgeRequest::with_metrics(message, reply_tx, Arc::clone(&self.state), cancelled);
        let message = (self.into_message)(request);
        self.runtime
            .try_send_and_observe_with_preflight(
                self.address,
                message,
                move |message| {
                    if message.bridge_cancelled() {
                        preflight_cancelled_for_worker.store(true, Ordering::Release);
                        Some(ThreadedSendObservedError::MailboxClosed)
                    } else {
                        None
                    }
                },
                move |result| {
                    let skipped_cancelled =
                        matches!(result, Err(ThreadedSendObservedError::MailboxClosed))
                            && preflight_cancelled.load(Ordering::Acquire);
                    if skipped_cancelled {
                        let _ = observed_tx.send(Err(BridgeError::Timeout));
                        return;
                    }

                    let result = result.map_err(BridgeError::from);
                    match result {
                        Ok(()) => {
                            state.metrics.accepted.fetch_add(1, Ordering::Relaxed);
                            #[cfg(feature = "tracing")]
                            event!(
                                target: TRACE_TARGET_CALL,
                                Level::DEBUG,
                                kind = "admitted",
                            );
                            let _ = observed_tx.send(Ok(()));
                        }
                        Err(error) => {
                            Self::record_error_on(&state, error);
                            let _ = observed_tx.send(Err(error));
                        }
                    }
                },
            )
            .map_err(|error| {
                let error = BridgeError::from(error);
                self.record_error(error);
                error
            })?;

        let outcome = match tokio::time::timeout(timeout, async {
            match observed_rx.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return BridgeWaitOutcome::ObservedError(error),
                Err(_) => return BridgeWaitOutcome::ObserveChannelClosed,
            }

            match reply_rx.await {
                Ok(response) => BridgeWaitOutcome::Response(response),
                Err(_) => BridgeWaitOutcome::ReplyClosed,
            }
        })
        .await
        {
            Ok(outcome) => outcome,
            Err(_) => {
                self.record_error(BridgeError::Timeout);
                #[cfg(feature = "tracing")]
                event!(
                    target: TRACE_TARGET_CALL,
                    Level::WARN,
                    kind = "timeout",
                    reason = "Timeout",
                    scope = "per_attempt",
                    elapsed_ms = timeout.as_millis() as u64,
                );
                return Err(BridgeError::Timeout);
            }
        };

        match outcome {
            BridgeWaitOutcome::Response(response) => {
                cancellation.disarm();
                self.state.metrics.responses.fetch_add(1, Ordering::Relaxed);
                #[cfg(feature = "tracing")]
                event!(
                    target: TRACE_TARGET_CALL,
                    Level::DEBUG,
                    kind = "replied",
                    outcome = "response",
                );
                Ok(response)
            }
            BridgeWaitOutcome::ObservedError(error) => {
                cancellation.disarm();
                Err(error)
            }
            BridgeWaitOutcome::ObserveChannelClosed | BridgeWaitOutcome::ReplyClosed => {
                cancellation.disarm();
                self.record_error(BridgeError::Closed);
                Err(BridgeError::Closed)
            }
        }
    }

    fn record_error(&self, error: BridgeError) {
        Self::record_error_on(&self.state, error);
    }

    fn record_error_on(state: &BridgeState, error: BridgeError) {
        match error {
            BridgeError::Full => {
                state.metrics.full.fetch_add(1, Ordering::Relaxed);
            }
            BridgeError::Closed => {
                state.metrics.closed.fetch_add(1, Ordering::Relaxed);
            }
            BridgeError::Timeout => {
                state.metrics.timeout.fetch_add(1, Ordering::Relaxed);
            }
        }
        // Bridge timeouts emit at the timeout site so they carry
        // elapsed_ms; here we cover Full and Closed admission paths
        // (record_error_on is the single record-side mutator).
        #[cfg(feature = "tracing")]
        if !matches!(error, BridgeError::Timeout) {
            event!(
                target: TRACE_TARGET_CALL,
                Level::WARN,
                kind = "admission_rejected",
                reason = bridge_error_reason(error),
            );
        }
    }
}
