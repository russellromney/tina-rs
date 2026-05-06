#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

//! Tokio async-bridge for `tina-rpc` clients.
//!
//! Edge adapter. Lives outside `tina-rpc` because the plan rule
//! is loud: "Tokio `async` convenience must live in bridge crates,
//! not the Tina-native core."
//!
//! What ships:
//!
//! - [`BridgeClient`] — pairs a [`tina_rpc::Client`] address with a
//!   shim isolate that demuxes [`tina_rpc::ClientResultMsg`] back to
//!   per-call [`tokio::sync::oneshot`] receivers.
//! - [`BridgeClient::call`] — typed `await` over the framed RPC.
//!   Caller passes a `request_fn` (use macro-generated
//!   `*_request` helpers) and a `decoder` (matching
//!   `*_decode_reply`); the bridge handles correlator, demux,
//!   outcome mapping.
//! - [`BridgeError`] — typed mapping of every
//!   [`tina_rpc::ClientResult`] into an `await`able error.
//! - [`RetryPolicy`] / [`call_with_retry`] — opt-in, bounded,
//!   trace-visible retry. No retry by default.
//!
//! # Stays explicit
//!
//! - Capacity: the `Client` still owns `max_in_flight`. Past it,
//!   the bridge surfaces [`BridgeError::Full`] — never queues.
//! - Deadline: per-call `Duration` rides on the `ClientRequest`.
//! - Retry: opt-in via [`call_with_retry`]; bare [`BridgeClient::call`]
//!   never retries.
//!
//! > macro may hide bytes. macro may not hide backpressure.
//! > bridge may hide await. bridge may not hide overload.

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Semaphore, oneshot};

use tina::{Address, Context, Effect, Isolate, Outbound};
use tina_rpc::{
    ClientMsg, ClientRequest, ClientResult, ClientResultMsg, EncodingError, FrameError,
};
use tina_runtime::{
    MailboxFactory, RuntimeCall, ThreadedRuntime, ThreadedSendObservedError, ThreadedTrySendError,
};

#[cfg(feature = "tracing")]
use tracing::{Instrument, debug, debug_span, field, warn};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Outcome of one bridged RPC call.
///
/// Every variant of [`tina_rpc::ClientResult`] (the per-request
/// notification the underlying client emits) plus the bridge-side
/// failure modes (encoding, runtime ingress) maps onto exactly one
/// `BridgeError` variant. There is no silent absorption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeError {
    /// `tina_rpc::Encoding::encode` rejected the typed request, or
    /// `tina_rpc::Encoding::decode` rejected the typed reply.
    Encoding(EncodingError),
    /// Server returned a wire `Error(code)` frame.
    Server(FrameError),
    /// Per-request deadline elapsed; client's local timeout fired.
    Timeout,
    /// Connection closed before a reply arrived.
    ConnectionClosed,
    /// Client's idle timeout fired.
    Idle,
    /// In-flight cap on the underlying client was at capacity.
    Full,
    /// Client refused to encode the request (oversize past
    /// `max_frame_size`, etc.).
    LocalEncodeFailed,
    /// Underlying TCP / runtime IO error closed the connection
    /// while this request was pending.
    IoError(tina_runtime::CallError),
    /// Bridge ingress to the `Client` isolate failed (mailbox
    /// closed or full).
    ClientUnavailable,
    /// The shim isolate that demuxes replies has stopped before
    /// this call's correlator could be matched.
    ShimGone,
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Encoding(err) => write!(f, "rpc bridge encoding error: {err}"),
            Self::Server(code) => write!(f, "rpc bridge server error: {code:?}"),
            Self::Timeout => write!(f, "rpc bridge call timed out (client deadline)"),
            Self::ConnectionClosed => write!(f, "rpc bridge connection closed"),
            Self::Idle => write!(f, "rpc bridge connection idle-closed"),
            Self::Full => write!(f, "rpc bridge in-flight cap full"),
            Self::LocalEncodeFailed => write!(f, "rpc bridge could not encode request locally"),
            Self::IoError(err) => write!(f, "rpc bridge IO error: {err:?}"),
            Self::ClientUnavailable => write!(f, "rpc bridge client mailbox unavailable"),
            Self::ShimGone => write!(f, "rpc bridge reply shim isolate stopped"),
        }
    }
}

impl std::error::Error for BridgeError {}

impl BridgeError {
    /// True when this outcome is a candidate for [`call_with_retry`]
    /// under the conventional retry policy: capacity / transient
    /// faults only. Closed and IO errors are not retried (they need
    /// a fresh client) and timeout is opt-in per
    /// [`RetryPolicy::on_timeout`].
    fn is_retryable_full(&self) -> bool {
        matches!(self, Self::Full)
    }

    fn is_retryable_timeout(&self) -> bool {
        matches!(self, Self::Timeout)
    }
}

fn map_client_result(result: ClientResult) -> Result<Vec<u8>, BridgeError> {
    match result {
        ClientResult::Ok(bytes) => Ok(bytes),
        ClientResult::ServerError(code) => Err(BridgeError::Server(code)),
        ClientResult::Timeout => Err(BridgeError::Timeout),
        ClientResult::ConnectionClosed => Err(BridgeError::ConnectionClosed),
        ClientResult::Idle => Err(BridgeError::Idle),
        ClientResult::Full => Err(BridgeError::Full),
        ClientResult::LocalEncodeFailed => Err(BridgeError::LocalEncodeFailed),
        ClientResult::IoError(err) => Err(BridgeError::IoError(err)),
    }
}

// ---------------------------------------------------------------------------
// Shim isolate: receives ClientResultMsg and forwards via oneshot
// ---------------------------------------------------------------------------

/// Pending entries hold `Some(tx)` while a future is awaiting. When
/// the future is cancelled the [`CancelGuard`] flips it to `None` so
/// the shim still sees the correlator was admitted (and releases the
/// admission slot) without trying a dead `oneshot::Sender`.
type PendingMap = HashMap<u64, Option<oneshot::Sender<ClientResult>>>;

/// Shim isolate registered with the runtime once per [`BridgeClient`].
/// Its only job is to receive [`ClientResultMsg`] from the
/// `tina_rpc::Client`, route the per-correlator `oneshot::Sender`
/// back to the awaiting Tokio task, and release one admission slot
/// for the call that just completed.
struct ReplyShim<S>
where
    S: tina::Shard,
{
    pending: Arc<Mutex<PendingMap>>,
    slots: Arc<Semaphore>,
    _shard: std::marker::PhantomData<S>,
}

impl<S> Isolate for ReplyShim<S>
where
    S: tina::Shard,
{
    type Message = ClientResultMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<ClientResultMsg>;
    type Shard = S;

    fn handle(&mut self, msg: ClientResultMsg, _ctx: &mut Context<'_, S>) -> Effect<Self> {
        let entry = {
            let mut pending = self
                .pending
                .lock()
                .expect("BridgeClient pending map mutex poisoned");
            pending.remove(&msg.correlator)
        };
        match entry {
            Some(Some(tx)) => {
                // Live awaiter. The receiver may already be dropped;
                // a send error is a noop.
                let _ = tx.send(msg.result);
                self.slots.add_permits(1);
            }
            Some(None) => {
                // Awaiter was cancelled before the reply landed.
                // Release the admission slot so a queued caller can
                // proceed.
                self.slots.add_permits(1);
            }
            None => {
                // Stray: not an admitted correlator. Drop without
                // releasing — no slot was reserved on its behalf.
            }
        }
        Effect::Noop
    }
}

// ---------------------------------------------------------------------------
// Bridge client
// ---------------------------------------------------------------------------

/// Tokio async-bridge over a `tina_rpc::Client`.
///
/// Construct with [`BridgeClient::new`], passing the live
/// [`ThreadedRuntime`], the client isolate's `Address<ClientMsg>`,
/// the shard id the bridge isolate will live on, and the bridge
/// shim's mailbox capacity. Then `await` typed calls via
/// [`BridgeClient::call`].
///
/// `Clone` is intentional and cheap — every clone shares the same
/// shim isolate and pending-correlator map.
pub struct BridgeClient<S>
where
    S: tina::Shard,
{
    inner: Arc<BridgeClientInner<S>>,
}

impl<S> Clone for BridgeClient<S>
where
    S: tina::Shard,
{
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// Closure that submits a `ClientMsg` to the runtime and reports
/// both the ingress outcome (return value) and the target-mailbox
/// outcome (via the observer, fired on the runtime worker thread).
/// The bridge needs both sides — `ThreadedRuntime::try_send` only
/// reports ingress; the worker-thread `let _ = ...` would silently
/// drop a target-side `Full`/`Closed` and hang the awaiting future.
type SendAndObserve = Box<
    dyn Fn(
            Address<ClientMsg>,
            ClientMsg,
            Box<dyn FnOnce(Result<(), ThreadedSendObservedError>) + Send + 'static>,
        ) -> Result<(), ThreadedTrySendError>
        + Send
        + Sync,
>;

struct BridgeClientInner<S>
where
    S: tina::Shard,
{
    client_addr: Address<ClientMsg>,
    shim_addr: Address<ClientResultMsg>,
    pending: Arc<Mutex<PendingMap>>,
    /// Bounded admission counter. One permit = one guaranteed reply
    /// slot in the shim's mailbox. Acquired synchronously in `call`;
    /// released by the shim when the reply lands or by the observer
    /// when ingress fails.
    slots: Arc<Semaphore>,
    next_correlator: AtomicU64,
    send_and_observe: SendAndObserve,
    _shard: std::marker::PhantomData<S>,
}

impl<S> BridgeClient<S>
where
    S: tina::Shard + Send + 'static,
{
    /// Builds a new bridge over `client_addr`. Registers a tiny
    /// shim isolate with the runtime to receive
    /// [`ClientResultMsg`] notifications and demux them back to
    /// per-call awaiters.
    ///
    /// `max_in_flight` is the bridge's hard cap on simultaneously
    /// in-flight calls. Past it, [`BridgeClient::call`] returns
    /// [`BridgeError::Full`] **synchronously**, before any
    /// `ClientMsg::Request` hits the runtime. The shim isolate's
    /// mailbox is sized to match — a successful admission therefore
    /// guarantees a reply slot, so the shim never silently drops a
    /// `ClientResultMsg` and an awaiter never hangs on lost replies.
    /// Must be `> 0`.
    ///
    /// `runtime` is taken as `Arc<ThreadedRuntime<...>>` because
    /// the bridge's reply demux closure must outlive any single
    /// borrow — `Arc` makes that explicit. Wrap your existing
    /// runtime once at the bridge edge:
    /// `Arc::new(ThreadedRuntime::with_config(...))`.
    pub fn new<F>(
        runtime: Arc<ThreadedRuntime<S, F>>,
        client_addr: Address<ClientMsg>,
        max_in_flight: usize,
    ) -> Result<Self, BridgeError>
    where
        F: MailboxFactory + Send + Clone + 'static,
    {
        assert!(
            max_in_flight > 0,
            "BridgeClient::new requires max_in_flight > 0",
        );
        let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));
        let slots = Arc::new(Semaphore::new(max_in_flight));
        let shim = ReplyShim::<S> {
            pending: Arc::clone(&pending),
            slots: Arc::clone(&slots),
            _shard: std::marker::PhantomData,
        };
        let shim_addr = runtime
            .register_with_capacity::<ReplyShim<S>, Infallible>(shim, max_in_flight)
            .map_err(|_| BridgeError::ClientUnavailable)?;
        // We keep the runtime in a closure rather than carrying
        // `<S, F>` through `BridgeClient`'s public type. The
        // closure erases the runtime's generics at the API
        // boundary.
        let runtime_for_send = Arc::clone(&runtime);
        let send_and_observe: SendAndObserve = Box::new(move |addr, msg, observer| {
            runtime_for_send.try_send_and_observe_with(addr, msg, observer)
        });
        Ok(Self {
            inner: Arc::new(BridgeClientInner {
                client_addr,
                shim_addr,
                pending,
                slots,
                next_correlator: AtomicU64::new(1),
                send_and_observe,
                _shard: std::marker::PhantomData,
            }),
        })
    }

    /// Awaits one typed RPC call.
    ///
    /// `request_fn(correlator, reply_to)` runs synchronously to
    /// produce a [`ClientRequest`]. Use the macro-generated
    /// `<Client>::name_request(...)` helpers; they take exactly
    /// these two arguments alongside the user-typed args.
    ///
    /// `decoder(bytes)` runs after a successful wire reply to
    /// decode the typed return value. Use the matching
    /// `<Client>::name_decode_reply(...)`.
    ///
    /// # What this hides and what it does not
    ///
    /// Hidden: correlator allocation, reply demux, the
    /// `ClientResultMsg` → typed-result wiring.
    ///
    /// Not hidden: capacity (a saturated client returns
    /// [`BridgeError::Full`]), deadline (lives on the user's
    /// `ClientRequest`), encoding (the user's `request_fn` /
    /// `decoder` enforce the typed shape).
    pub async fn call<R, D, T>(&self, request_fn: R, decoder: D) -> Result<T, BridgeError>
    where
        R: FnOnce(u64, Address<ClientResultMsg>) -> Result<ClientRequest, EncodingError>,
        D: FnOnce(&[u8]) -> Result<T, EncodingError>,
    {
        // Bounded admission. Synchronous refusal at saturation is the
        // single guarantee that prevents "shim mailbox full → reply
        // dropped → awaiter hangs forever." Past `max_in_flight`,
        // `BridgeError::Full` returns immediately, never sends.
        let permit = match Arc::clone(&self.inner.slots).try_acquire_owned() {
            Ok(p) => p,
            Err(_) => return Err(BridgeError::Full),
        };
        // We "forget" the permit — the slot is held until a shim
        // notification (or observer / abort path) explicitly returns
        // it via `add_permits(1)`. This guarantees the live
        // permit-count tracks "calls whose reply slot is still
        // reserved," not "futures still polling."
        permit.forget();

        let correlator = self.inner.next_correlator.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel::<ClientResult>();

        // Insert into the pending map BEFORE sending, so a fast
        // reply path can find the slot.
        {
            let mut pending = self
                .inner
                .pending
                .lock()
                .expect("BridgeClient pending map mutex poisoned");
            pending.insert(correlator, Some(tx));
        }

        let request = match request_fn(correlator, self.inner.shim_addr) {
            Ok(req) => req,
            Err(err) => {
                self.abort_admitted(correlator);
                return Err(BridgeError::Encoding(err));
            }
        };

        #[cfg(feature = "tracing")]
        let span = debug_span!(
            "tina_rpc.bridge.call",
            service = %request.service,
            method = %request.method,
            correlator = correlator,
            // Declared at construction so the later `record` writes
            // into a known field; tracing's `record` is a noop on
            // undeclared fields.
            result_kind = field::Empty,
        );
        #[cfg(feature = "tracing")]
        debug!(
            service = %request.service,
            method = %request.method,
            correlator = correlator,
            "rpc bridge dispatch"
        );

        // Submit through `try_send_and_observe_with` so a target-side
        // `Full`/`Closed` is surfaced back to the awaiter rather than
        // silently dropped on the worker thread. When the observer
        // fires `Err`, the message never reached the `Client`, so the
        // shim will never see this correlator — the observer must
        // therefore release the admission slot itself.
        let pending_for_observer = Arc::clone(&self.inner.pending);
        let slots_for_observer = Arc::clone(&self.inner.slots);
        let observer: Box<dyn FnOnce(Result<(), ThreadedSendObservedError>) + Send + 'static> =
            Box::new(move |observed| {
                if let Err(err) = observed {
                    let mapped = match err {
                        ThreadedSendObservedError::MailboxFull => ClientResult::Full,
                        ThreadedSendObservedError::MailboxClosed => ClientResult::ConnectionClosed,
                        ThreadedSendObservedError::IngressFull
                        | ThreadedSendObservedError::WorkerStopped => {
                            ClientResult::IoError(tina_runtime::CallError::Io)
                        }
                    };
                    let entry = pending_for_observer
                        .lock()
                        .ok()
                        .and_then(|mut p| p.remove(&correlator));
                    if let Some(Some(tx)) = entry {
                        let _ = tx.send(mapped);
                    }
                    slots_for_observer.add_permits(1);
                }
            });

        let send_outcome = (self.inner.send_and_observe)(
            self.inner.client_addr,
            ClientMsg::Request(request),
            observer,
        );
        if let Err(err) = send_outcome {
            self.abort_admitted(correlator);
            #[cfg(feature = "tracing")]
            warn!(error = ?err, "rpc bridge ingress failed");
            let _ = err;
            return Err(BridgeError::ClientUnavailable);
        }

        // Drop guard for cancellation: if the awaiting future is
        // dropped, leave a `None` marker in the pending map so the
        // shim still releases the admission slot when the eventual
        // reply lands. We do *not* release the slot from the guard —
        // the reply is still in flight and releasing now would let a
        // subsequent admit over-subscribe the shim mailbox.
        let guard = CancelGuard {
            pending: Arc::clone(&self.inner.pending),
            correlator,
        };

        let await_fut = async move {
            let result = rx.await;
            // Disarm — on success the entry was already taken by the
            // shim or observer; the explicit `Err` arm below handles
            // its own cleanup.
            std::mem::forget(guard);
            result
        };
        // `Instrument` keeps the span attached across yield points;
        // a bare `let _enter = span.enter()` would leak the span
        // into other tasks when the await yields.
        #[cfg(feature = "tracing")]
        let recv = await_fut.instrument(span.clone()).await;
        #[cfg(not(feature = "tracing"))]
        let recv = await_fut.await;

        let outcome = match recv {
            Ok(r) => r,
            Err(_) => {
                // `oneshot::Sender` was dropped without sending.
                // Means the shim isolate stopped before the reply
                // could be delivered; clean up and surface a typed
                // terminal error rather than hang.
                self.abort_admitted(correlator);
                return Err(BridgeError::ShimGone);
            }
        };

        #[cfg(feature = "tracing")]
        span.record("result_kind", tracing_kind(&outcome));

        let bytes = map_client_result(outcome)?;
        decoder(&bytes).map_err(BridgeError::Encoding)
    }

    /// Returns the underlying `Client` isolate's address.
    pub fn client_addr(&self) -> Address<ClientMsg> {
        self.inner.client_addr
    }

    /// Returns the bridge shim isolate's address.
    pub fn shim_addr(&self) -> Address<ClientResultMsg> {
        self.inner.shim_addr
    }

    /// Removes the pending entry **and** releases the admission slot.
    /// Used from bridge-side error paths (encoding failure, ingress
    /// failure, shim death) where no shim notification will ever
    /// arrive to release the slot for us.
    fn abort_admitted(&self, correlator: u64) {
        {
            let mut pending = self
                .inner
                .pending
                .lock()
                .expect("BridgeClient pending map mutex poisoned");
            pending.remove(&correlator);
        }
        self.inner.slots.add_permits(1);
    }
}

/// Marks the pending entry as cancelled (`Some(None)`) when the
/// awaiting future is dropped. The shim sees `Some(None)` when the
/// eventual reply lands and releases the admission slot without
/// trying a dead `oneshot::Sender`. The slot is **not** released
/// here — the reply is still in flight, and releasing now would let
/// a subsequent admit over-subscribe the shim mailbox.
struct CancelGuard {
    pending: Arc<Mutex<PendingMap>>,
    correlator: u64,
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.lock() {
            if let Some(slot) = pending.get_mut(&self.correlator) {
                *slot = None;
            }
        }
    }
}

#[cfg(feature = "tracing")]
fn tracing_kind(result: &ClientResult) -> &'static str {
    match result {
        ClientResult::Ok(_) => "ok",
        ClientResult::ServerError(_) => "server_error",
        ClientResult::Timeout => "timeout",
        ClientResult::ConnectionClosed => "connection_closed",
        ClientResult::Idle => "idle",
        ClientResult::Full => "full",
        ClientResult::LocalEncodeFailed => "local_encode_failed",
        ClientResult::IoError(_) => "io_error",
    }
}

// ---------------------------------------------------------------------------
// Retry wrapper
// ---------------------------------------------------------------------------

/// Per-call retry policy. **No retry by default** — the user
/// constructs a [`RetryPolicy`] explicitly and calls
/// [`call_with_retry`] when they want it.
///
/// First-form policy: bounded attempts plus a delay strategy. The
/// retry-on-which-outcomes set is also explicit; closed and IO
/// errors are *not* retried in any policy because they need a
/// fresh client.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Total number of attempts including the first. `attempts =
    /// 1` is "try once, do not retry"; `attempts = 3` is "try
    /// once and retry up to twice."
    pub attempts: u32,
    /// Delay strategy between retries.
    pub delay: RetryDelay,
    /// Whether to retry on [`BridgeError::Full`] (in-flight cap).
    pub on_full: bool,
    /// Whether to retry on [`BridgeError::Timeout`].
    pub on_timeout: bool,
}

impl Default for RetryPolicy {
    /// Three attempts, fixed 50 ms delay, retry only on `Full`.
    /// Timeout retries are opt-in because they can amplify load
    /// during the worst kind of overload.
    fn default() -> Self {
        Self {
            attempts: 3,
            delay: RetryDelay::Fixed(Duration::from_millis(50)),
            on_full: true,
            on_timeout: false,
        }
    }
}

impl RetryPolicy {
    /// Constructs a "no-retry" policy: exactly one attempt, no
    /// delays, no retryable outcomes.
    pub const fn none() -> Self {
        Self {
            attempts: 1,
            delay: RetryDelay::Fixed(Duration::ZERO),
            on_full: false,
            on_timeout: false,
        }
    }

    fn is_retryable(&self, err: &BridgeError) -> bool {
        (self.on_full && err.is_retryable_full()) || (self.on_timeout && err.is_retryable_timeout())
    }
}

/// Delay strategy for [`RetryPolicy`].
#[derive(Debug, Clone, Copy)]
pub enum RetryDelay {
    /// Wait `d` between every retry.
    Fixed(Duration),
    /// Exponential backoff with full jitter. Attempt N waits a
    /// random duration in `[0, min(max, base * 2^(N-1)))`.
    ExpJitter {
        /// Base unit for exponential growth.
        base: Duration,
        /// Cap on the per-attempt window.
        max: Duration,
    },
}

impl RetryDelay {
    fn duration_for_attempt(&self, attempt: u32) -> Duration {
        match self {
            Self::Fixed(d) => *d,
            Self::ExpJitter { base, max } => {
                // attempt is 1-indexed (attempt 1 = first retry).
                let shift = attempt.saturating_sub(1);
                let scaled = base
                    .checked_mul(1u32 << shift.min(31))
                    .unwrap_or(*max)
                    .min(*max);
                // Full jitter: random in [0, scaled). Use a
                // deterministic-but-cheap PRNG seeded by the
                // current time + attempt; for the bridge this is
                // good enough — production callers can swap their
                // own policy in.
                let nanos = scaled.as_nanos() as u64;
                if nanos == 0 {
                    return Duration::ZERO;
                }
                let mix = mix_seed(attempt);
                Duration::from_nanos(mix % nanos)
            }
        }
    }
}

/// Tiny deterministic-mix used for jitter when no seedable RNG is
/// configured. Good enough for back-off jitter; not for security.
fn mix_seed(attempt: u32) -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut x = now ^ (u64::from(attempt) << 32);
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    x.wrapping_mul(2685821657736338717)
}

/// Drives `op` under a [`RetryPolicy`]. Each attempt is a fresh
/// invocation of `op`, including a fresh `request_fn` /
/// correlator / oneshot.
///
/// Returns the first success, or the last error if every attempt
/// failed. Callers who want different error semantics (e.g.,
/// "first non-retryable wins, ignore retryables") wrap this
/// helper.
///
/// # Example
///
/// ```ignore
/// let policy = RetryPolicy {
///     attempts: 4,
///     delay: RetryDelay::ExpJitter { base: 50.ms(), max: 1.s() },
///     on_full: true,
///     ..RetryPolicy::default()
/// };
/// let receipt = call_with_retry(&policy, || async {
///     bridge.call(
///         |corr, rt| BillingClient::charge_request(amount, deadline, corr, rt, 1024),
///         |b| BillingClient::charge_decode_reply(b, 1024),
///     ).await
/// }).await?;
/// ```
pub async fn call_with_retry<F, Fut, T>(policy: &RetryPolicy, mut op: F) -> Result<T, BridgeError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, BridgeError>>,
{
    assert!(
        policy.attempts > 0,
        "RetryPolicy::attempts must be > 0; use RetryPolicy::none() for a single-attempt no-retry shape"
    );
    let mut last_err: Option<BridgeError> = None;
    for attempt in 1..=policy.attempts {
        match op().await {
            Ok(value) => {
                #[cfg(feature = "tracing")]
                debug!(attempt = attempt, "rpc bridge call succeeded");
                return Ok(value);
            }
            Err(err) => {
                let retryable = policy.is_retryable(&err);
                #[cfg(feature = "tracing")]
                debug!(
                    attempt = attempt,
                    retryable = retryable,
                    error = %err,
                    "rpc bridge call failed"
                );
                if !retryable || attempt == policy.attempts {
                    return Err(err);
                }
                last_err = Some(err);
                let delay = policy.delay.duration_for_attempt(attempt);
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
    Err(last_err.unwrap_or(BridgeError::ClientUnavailable))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_policy_default_is_three_attempts_full_only() {
        let p = RetryPolicy::default();
        assert_eq!(p.attempts, 3);
        assert!(p.on_full);
        assert!(!p.on_timeout);
    }

    #[test]
    fn retry_policy_none_is_one_attempt() {
        let p = RetryPolicy::none();
        assert_eq!(p.attempts, 1);
        assert!(!p.on_full);
        assert!(!p.on_timeout);
    }

    #[test]
    fn retry_policy_skips_non_retryable_outcomes() {
        let p = RetryPolicy::default();
        assert!(p.is_retryable(&BridgeError::Full));
        assert!(!p.is_retryable(&BridgeError::Timeout));
        assert!(!p.is_retryable(&BridgeError::ConnectionClosed));
        assert!(!p.is_retryable(&BridgeError::Idle));
        assert!(!p.is_retryable(&BridgeError::ClientUnavailable));
    }

    #[test]
    fn retry_policy_optionally_retries_timeout() {
        let p = RetryPolicy {
            on_timeout: true,
            ..RetryPolicy::default()
        };
        assert!(p.is_retryable(&BridgeError::Timeout));
    }

    #[test]
    fn fixed_delay_returns_same_duration() {
        let d = RetryDelay::Fixed(Duration::from_millis(40));
        assert_eq!(d.duration_for_attempt(1), Duration::from_millis(40));
        assert_eq!(d.duration_for_attempt(7), Duration::from_millis(40));
    }

    #[test]
    fn exp_jitter_respects_max() {
        let d = RetryDelay::ExpJitter {
            base: Duration::from_millis(10),
            max: Duration::from_millis(50),
        };
        for attempt in 1..=10 {
            let delay = d.duration_for_attempt(attempt);
            assert!(
                delay <= Duration::from_millis(50),
                "attempt {attempt} delay {delay:?} exceeded max"
            );
        }
    }

    #[tokio::test]
    async fn call_with_retry_returns_first_success() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let attempts = Arc::new(AtomicU32::new(0));
        let policy = RetryPolicy::default();
        let attempts2 = Arc::clone(&attempts);
        let result = call_with_retry(&policy, move || {
            let attempts = Arc::clone(&attempts2);
            async move {
                attempts.fetch_add(1, Ordering::Relaxed);
                Ok::<_, BridgeError>(42u32)
            }
        })
        .await
        .unwrap();
        assert_eq!(result, 42);
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn call_with_retry_retries_full_until_attempts_exhausted() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let attempts = Arc::new(AtomicU32::new(0));
        let policy = RetryPolicy {
            attempts: 3,
            delay: RetryDelay::Fixed(Duration::ZERO),
            on_full: true,
            on_timeout: false,
        };
        let attempts2 = Arc::clone(&attempts);
        let result = call_with_retry::<_, _, ()>(&policy, move || {
            let attempts = Arc::clone(&attempts2);
            async move {
                attempts.fetch_add(1, Ordering::Relaxed);
                Err(BridgeError::Full)
            }
        })
        .await;
        assert_eq!(result, Err(BridgeError::Full));
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            3,
            "policy has 3 attempts; full retried twice then surfaced"
        );
    }

    #[tokio::test]
    async fn call_with_retry_does_not_retry_non_retryable() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let attempts = Arc::new(AtomicU32::new(0));
        let policy = RetryPolicy {
            attempts: 5,
            delay: RetryDelay::Fixed(Duration::ZERO),
            on_full: true,
            on_timeout: false,
        };
        let attempts2 = Arc::clone(&attempts);
        let result = call_with_retry::<_, _, ()>(&policy, move || {
            let attempts = Arc::clone(&attempts2);
            async move {
                attempts.fetch_add(1, Ordering::Relaxed);
                Err(BridgeError::ConnectionClosed)
            }
        })
        .await;
        assert_eq!(result, Err(BridgeError::ConnectionClosed));
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            1,
            "ConnectionClosed is not retryable; one attempt"
        );
    }

    #[tokio::test]
    async fn call_with_retry_succeeds_after_transient_full() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let attempts = Arc::new(AtomicU32::new(0));
        let policy = RetryPolicy {
            attempts: 4,
            delay: RetryDelay::Fixed(Duration::ZERO),
            on_full: true,
            on_timeout: false,
        };
        let attempts2 = Arc::clone(&attempts);
        let result = call_with_retry::<_, _, u32>(&policy, move || {
            let attempts = Arc::clone(&attempts2);
            async move {
                let n = attempts.fetch_add(1, Ordering::Relaxed) + 1;
                if n < 3 { Err(BridgeError::Full) } else { Ok(n) }
            }
        })
        .await
        .unwrap();
        assert_eq!(result, 3);
        assert_eq!(attempts.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn map_client_result_round_trips_each_variant() {
        assert_eq!(
            map_client_result(ClientResult::Ok(b"x".to_vec())),
            Ok(b"x".to_vec())
        );
        assert_eq!(
            map_client_result(ClientResult::ServerError(FrameError::UnknownService)),
            Err(BridgeError::Server(FrameError::UnknownService))
        );
        assert_eq!(
            map_client_result(ClientResult::Timeout),
            Err(BridgeError::Timeout)
        );
        assert_eq!(
            map_client_result(ClientResult::ConnectionClosed),
            Err(BridgeError::ConnectionClosed)
        );
        assert_eq!(
            map_client_result(ClientResult::Idle),
            Err(BridgeError::Idle)
        );
        assert_eq!(
            map_client_result(ClientResult::Full),
            Err(BridgeError::Full)
        );
        assert_eq!(
            map_client_result(ClientResult::LocalEncodeFailed),
            Err(BridgeError::LocalEncodeFailed)
        );
        assert_eq!(
            map_client_result(ClientResult::IoError(tina_runtime::CallError::Io)),
            Err(BridgeError::IoError(tina_runtime::CallError::Io))
        );
    }
}
