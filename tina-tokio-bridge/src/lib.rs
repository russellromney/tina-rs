#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

//! Narrow Tokio/Tower bridge for Tina services.
//!
//! The bridge does not make Tina handlers async and does not add a hidden
//! executor queue. Tokio code sends one bounded request into a
//! [`BetelgeuseBackedRuntime`], then
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

use std::convert::Infallible;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use tina::{Address, Isolate, Outbound as TinaOutbound, Shard};
use tina_runtime::{
    BetelgeuseBackedControlError, BetelgeuseBackedRuntime, BetelgeuseBackedRuntimeConfig,
    BetelgeuseBackedSendObservedError, BetelgeuseBackedTrySendError, CallError, MailboxFactory,
    RuntimeEvent, SendRejectedReason,
};
use tokio::sync::oneshot;
use tower_service::Service;

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
        attempts: usize,
        /// Delay between attempts.
        delay: Duration,
    },
}

impl BridgeBackpressure {
    /// Reject overload immediately.
    pub const fn reject() -> Self {
        Self::Reject
    }

    /// Retry overload with an explicit delay and retry count.
    pub const fn retry(attempts: usize, delay: Duration) -> Self {
        Self::Retry { attempts, delay }
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

impl From<BetelgeuseBackedTrySendError> for BridgeError {
    fn from(error: BetelgeuseBackedTrySendError) -> Self {
        match error {
            BetelgeuseBackedTrySendError::IngressFull => Self::Full,
            BetelgeuseBackedTrySendError::WorkerStopped => Self::Closed,
        }
    }
}

impl From<BetelgeuseBackedSendObservedError> for BridgeError {
    fn from(error: BetelgeuseBackedSendObservedError) -> Self {
        match error {
            BetelgeuseBackedSendObservedError::IngressFull
            | BetelgeuseBackedSendObservedError::MailboxFull => Self::Full,
            BetelgeuseBackedSendObservedError::MailboxClosed
            | BetelgeuseBackedSendObservedError::WorkerStopped => Self::Closed,
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

impl From<BridgeError> for SendRejectedReason {
    fn from(error: BridgeError) -> Self {
        match error {
            BridgeError::Full => Self::Full,
            BridgeError::Closed | BridgeError::Timeout => Self::Closed,
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
        }
    }

    fn with_metrics(message: M, sender: oneshot::Sender<R>, state: Arc<BridgeState>) -> Self {
        Self {
            message,
            responder: BridgeResponder {
                sender: Some(sender),
                state: Some(state),
            },
        }
    }

    /// Splits the bridge request into the user message and responder.
    pub fn into_parts(self) -> (M, BridgeResponder<R>) {
        (self.message, self.responder)
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
        }
        result
    }
}

/// Owns one Betelgeuse-backed Tina runtime for bridge-hosted services.
pub struct BridgeHost<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    runtime: Arc<BetelgeuseBackedRuntime<S, F>>,
}

impl<S, F> BridgeHost<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    /// Starts a bridge host over one shard-owned Tina runtime.
    pub fn new(shard: S, mailbox_factory: F, config: BetelgeuseBackedRuntimeConfig) -> Self {
        Self {
            runtime: Arc::new(BetelgeuseBackedRuntime::with_config(
                shard,
                mailbox_factory,
                config,
            )),
        }
    }

    /// Returns the hosted Tina runtime.
    pub fn runtime(&self) -> &Arc<BetelgeuseBackedRuntime<S, F>> {
        &self.runtime
    }

    /// Registers one bridge-facing Tina service and returns its bridge handle.
    pub fn register_bridge<I, M, R, Outbound>(
        &self,
        isolate: I,
        mailbox_capacity: usize,
        timeout: Duration,
    ) -> Result<BridgeHandle<M, R, S, F, I::Reply>, BetelgeuseBackedControlError>
    where
        I: Isolate<
                Message = BridgeRequest<M, R>,
                Shard = S,
                Send = TinaOutbound<Outbound>,
                Spawn = Infallible,
                Call = Infallible,
            > + Send
            + 'static,
        I::Reply: Send + 'static,
        M: Send + 'static,
        R: Send + 'static,
        Outbound: 'static,
    {
        let address = self
            .runtime
            .register_with_capacity::<I, Outbound>(isolate, mailbox_capacity)?;
        Ok(BridgeHandle::new(
            Arc::clone(&self.runtime),
            address,
            timeout,
        ))
    }

    /// Shuts down the hosted runtime once all bridge handles have been dropped.
    pub fn shutdown(self) -> Result<Vec<RuntimeEvent>, BridgeShutdownError<S, F>> {
        let runtime = Arc::try_unwrap(self.runtime).map_err(BridgeShutdownError::StillShared)?;
        runtime.shutdown().map_err(BridgeShutdownError::Runtime)
    }
}

/// Error returned by [`BridgeHost::shutdown`].
pub enum BridgeShutdownError<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    /// Some bridge handles still hold the runtime.
    StillShared(Arc<BetelgeuseBackedRuntime<S, F>>),
    /// The hosted runtime failed during shutdown.
    Runtime(BetelgeuseBackedControlError),
}

impl<S, F> std::fmt::Debug for BridgeShutdownError<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StillShared(_) => formatter.write_str("BridgeShutdownError::StillShared"),
            Self::Runtime(error) => formatter.debug_tuple("Runtime").field(error).finish(),
        }
    }
}

/// Cloneable bounded bridge handle for Tokio/Tower callers.
pub struct BridgeHandle<M, R, S, F, AR = ()>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    runtime: Arc<BetelgeuseBackedRuntime<S, F>>,
    address: Address<BridgeRequest<M, R>, AR>,
    timeout: Duration,
    state: Arc<BridgeState>,
    marker: PhantomData<fn(M, R, AR)>,
}

impl<M, R, S, F, AR> Clone for BridgeHandle<M, R, S, F, AR>
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
            marker: PhantomData,
        }
    }
}

impl<M, R, S, F, AR> BridgeHandle<M, R, S, F, AR>
where
    M: Send + 'static,
    R: Send + 'static,
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
    AR: Send + 'static,
{
    /// Builds a bridge handle to an already-registered Tina isolate.
    pub fn new(
        runtime: Arc<BetelgeuseBackedRuntime<S, F>>,
        address: Address<BridgeRequest<M, R>, AR>,
        timeout: Duration,
    ) -> Self {
        Self {
            runtime,
            address,
            timeout,
            state: Arc::new(BridgeState::default()),
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

    /// Sends one request with an explicit overload policy and timeout.
    pub async fn call_with_policy(
        &self,
        message: M,
        policy: BridgeBackpressure,
        timeout: Duration,
    ) -> Result<R, BridgeError>
    where
        M: Clone,
    {
        let (mut remaining, delay) = match policy {
            BridgeBackpressure::Reject => (0, Duration::ZERO),
            BridgeBackpressure::Retry { attempts, delay } => (attempts, delay),
        };
        loop {
            match self.call_once(message.clone(), timeout).await {
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
            return Err(BridgeError::Closed);
        }

        let (reply_tx, reply_rx) = oneshot::channel();
        let (observed_tx, observed_rx) = oneshot::channel();
        let state = Arc::clone(&self.state);
        self.runtime
            .try_send_and_observe_with(
                self.address,
                BridgeRequest::with_metrics(message, reply_tx, Arc::clone(&self.state)),
                move |result| {
                    let result = result.map_err(BridgeError::from);
                    match result {
                        Ok(()) => {
                            state.metrics.accepted.fetch_add(1, Ordering::Relaxed);
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
                return Err(BridgeError::Timeout);
            }
        };

        match outcome {
            BridgeWaitOutcome::Response(response) => {
                self.state.metrics.responses.fetch_add(1, Ordering::Relaxed);
                Ok(response)
            }
            BridgeWaitOutcome::ObservedError(error) => Err(error),
            BridgeWaitOutcome::ObserveChannelClosed | BridgeWaitOutcome::ReplyClosed => {
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
    }
}

impl<M, R, S, F, AR> Service<M> for BridgeHandle<M, R, S, F, AR>
where
    M: Send + 'static,
    R: Send + 'static,
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
    AR: Send + 'static,
{
    type Response = R;
    type Error = BridgeError;
    type Future = Pin<Box<dyn Future<Output = Result<R, BridgeError>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.state.is_closed() {
            Poll::Ready(Err(BridgeError::Closed))
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn call(&mut self, request: M) -> Self::Future {
        let handle = self.clone();
        Box::pin(async move { BridgeHandle::call(&handle, request).await })
    }
}
