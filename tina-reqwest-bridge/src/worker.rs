//! Reqwest worker isolate.

use std::collections::HashMap;
use std::convert::Infallible;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use http::{HeaderMap, HeaderName, HeaderValue, Method};
use reqwest::Client;
use tina::CallContext;
use tina::prelude::*;
use tina_runtime::{MailboxFactory, RuntimeCall, ThreadedRuntime, ThreadedRuntimeError, sleep};
use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tokio::task::AbortHandle;
#[cfg(feature = "tracing")]
use tracing::{Level, event};

use crate::metrics::{MetricsInner, ReqwestMetricsHandle};
use crate::types::{
    RedirectPolicy, ReqwestConfig, ReqwestConfigError, ReqwestError, ReqwestRequest,
    ReqwestResponse, RetryPolicy,
};

type ReqwestResult = Result<ReqwestResponse, ReqwestError>;

/// `tracing` target for per-call events.
#[cfg(feature = "tracing")]
const TRACE_TARGET_CALL: &str = "tina_reqwest.bridge.call";

/// `tracing` target for bridge lifecycle.
#[cfg(feature = "tracing")]
const TRACE_TARGET_BRIDGE: &str = "tina_reqwest.bridge";

#[cfg(feature = "tracing")]
fn reqwest_error_reason(err: &ReqwestError) -> &'static str {
    match err {
        ReqwestError::Full => "Full",
        ReqwestError::Closed => "Closed",
        ReqwestError::Timeout => "Timeout",
        ReqwestError::RequestTooLarge => "RequestTooLarge",
        ReqwestError::ResponseTooLarge => "ResponseTooLarge",
        ReqwestError::InvalidRequest(_) => "InvalidRequest",
        ReqwestError::Reqwest(_) => "Reqwest",
    }
}

/// Messages handled by [`ReqwestWorker`].
///
/// Only [`ReqwestMsg::Send`] is constructible by user code. To close
/// the worker use [`ReqwestCloser::close`].
#[derive(Debug)]
pub enum ReqwestMsg {
    /// Issue one outbound HTTP request and reply with the outcome.
    Send(ReqwestRequest),
    /// Internal sleep wakeup. User code must not construct.
    #[doc(hidden)]
    Poll(u64),
}

enum SlotKind {
    /// One reqwest task is running on the Tokio runtime.
    InFlight {
        attempt_started_at: Instant,
        receiver: oneshot::Receiver<Result<ReqwestResponse, ReqwestError>>,
        abort: AbortHandle,
    },
    /// Waiting for a retry delay before spawning the next attempt.
    PendingRetry {
        request: ReqwestRequest,
        attempt_due_at: Instant,
    },
}

struct Slot {
    kind: SlotKind,
    request_context: Option<RequestContext<ReqwestResult>>,
    per_attempt_timeout: Duration,
    /// Retry attempts remaining after the current one completes.
    attempts_remaining: u8,
    retry_delay: Duration,
    /// Saved request used to build retry attempts. None when retry is
    /// off or the retry budget has been exhausted.
    saved_request: Option<ReqwestRequest>,
    /// HTTP method as a stable lowercase-friendly string, captured at
    /// admission so every per-call tracing event (`admitted`, `retry`,
    /// `timeout`, `replied`) can carry the same `method` field.
    /// Always populated, regardless of whether the `tracing` feature
    /// is on, so the type does not branch on cfg.
    method: String,
}

struct Delivery {
    request_context: Option<RequestContext<ReqwestResult>>,
    per_attempt_timeout: Duration,
    attempts_remaining: u8,
    retry_delay: Duration,
    saved_request: Option<ReqwestRequest>,
    method: String,
}

/// Bounded outbound HTTP worker around reqwest.
///
/// Owns a [`reqwest::Client`] and a [`tokio::runtime::Handle`]. The
/// worker is registered as a Tina isolate. Users `call(...)` it with
/// [`ReqwestMsg::Send`] and receive a `Result<ReqwestResponse,
/// ReqwestError>` in their continuation message.
pub struct ReqwestWorker<S: Shard + 'static> {
    config: ReqwestConfig,
    client: Client,
    runtime: Handle,
    in_flight: HashMap<u64, Slot>,
    next_id: u64,
    closed: Arc<AtomicBool>,
    metrics: Arc<MetricsInner>,
    _owned_runtime: Option<OwnedRuntime>,
    _shard: PhantomData<S>,
}

/// Wraps an owned `tokio::runtime::Runtime` so that drop on the Tina
/// shard thread returns immediately instead of blocking on pending
/// tasks.
struct OwnedRuntime(Option<tokio::runtime::Runtime>);

impl Drop for OwnedRuntime {
    fn drop(&mut self) {
        if let Some(rt) = self.0.take() {
            rt.shutdown_background();
        }
    }
}

/// Result of [`ReqwestWorker::install`].
pub struct InstalledReqwestBridge<S: Shard + 'static> {
    /// Tina address callers use with `call(...)`.
    pub address: Address<ReqwestMsg, Result<ReqwestResponse, ReqwestError>>,
    /// Closer for graceful drain.
    pub closer: ReqwestCloser,
    /// Metrics handle.
    pub metrics: ReqwestMetricsHandle,
    _shard: PhantomData<S>,
}

/// Reasons [`ReqwestWorker::install`] cannot register the worker.
#[derive(Debug)]
pub enum InstallError {
    /// Config rejected by [`ReqwestConfig::validate`].
    Config(ReqwestConfigError),
    /// Reqwest client or Tokio runtime construction failed.
    Build(ReqwestError),
    /// Tina runtime registration failed.
    Register(ThreadedRuntimeError),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(e) => write!(f, "reqwest bridge install: {e}"),
            Self::Build(e) => write!(f, "reqwest bridge install: {e}"),
            Self::Register(e) => write!(f, "reqwest bridge install: register: {e:?}"),
        }
    }
}

impl std::error::Error for InstallError {}

impl From<ReqwestConfigError> for InstallError {
    fn from(e: ReqwestConfigError) -> Self {
        Self::Config(e)
    }
}

impl From<ReqwestError> for InstallError {
    fn from(e: ReqwestError) -> Self {
        Self::Build(e)
    }
}

impl From<ThreadedRuntimeError> for InstallError {
    fn from(e: ThreadedRuntimeError) -> Self {
        Self::Register(e)
    }
}

impl<S: Shard + 'static> ReqwestWorker<S> {
    /// Builds a worker that owns its own Tokio runtime and reqwest
    /// client honoring `config`. The bridge config's redirect, timeout,
    /// and retry knobs all apply to the constructed client.
    pub fn new(config: ReqwestConfig) -> Result<(Self, ReqwestMetricsHandle), ReqwestError> {
        config
            .validate()
            .map_err(|e| ReqwestError::InvalidRequest(format!("config: {e}")))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .thread_name("tina-reqwest-bridge")
            .build()
            .map_err(|e| ReqwestError::Reqwest(format!("tokio runtime build: {e}")))?;
        let handle = runtime.handle().clone();
        let client = build_client(&config)?;
        let owned = OwnedRuntime(Some(runtime));
        let (worker, metrics) = Self::assemble(config, client, handle, Some(owned));
        Ok((worker, metrics))
    }

    /// Builds a worker around a caller-supplied reqwest client and
    /// Tokio runtime handle.
    ///
    /// **Ownership split.** The supplied reqwest client owns its own
    /// *client-level* settings — redirect policy, the reqwest
    /// `Client::timeout`, connection-reuse, TLS config, proxy. The
    /// bridge does not re-apply [`ReqwestConfig::redirect`] to it.
    ///
    /// The bridge still owns the *Tina-side per-attempt deadline*: every
    /// request runs under `tokio::time::timeout(per_attempt, ...)` where
    /// `per_attempt = request.timeout.unwrap_or(config.default_timeout)`,
    /// regardless of who built the client. So
    /// [`ReqwestConfig::default_timeout`] still matters even on this
    /// path: it bounds how long the bridge will wait on one attempt
    /// before surfacing [`ReqwestError::Timeout`]. Set the supplied
    /// client's own `Client::timeout` to a value at least as large.
    ///
    /// The caller must keep the underlying `tokio::runtime::Runtime`
    /// alive for the worker's lifetime.
    pub fn with_supplied_client(
        config: ReqwestConfig,
        client: Client,
        runtime: Handle,
    ) -> Result<(Self, ReqwestMetricsHandle), ReqwestConfigError> {
        config.validate()?;
        Ok(Self::assemble(config, client, runtime, None))
    }

    fn assemble(
        config: ReqwestConfig,
        client: Client,
        runtime: Handle,
        owned_runtime: Option<OwnedRuntime>,
    ) -> (Self, ReqwestMetricsHandle) {
        let metrics_inner = Arc::new(MetricsInner::default());
        let metrics = ReqwestMetricsHandle {
            inner: Arc::clone(&metrics_inner),
        };
        let worker = Self {
            config,
            client,
            runtime,
            in_flight: HashMap::new(),
            next_id: 1,
            closed: Arc::new(AtomicBool::new(false)),
            metrics: metrics_inner,
            _owned_runtime: owned_runtime,
            _shard: PhantomData,
        };
        (worker, metrics)
    }

    /// Configured mailbox capacity. Use when registering manually.
    pub fn mailbox_capacity(&self) -> usize {
        self.config.mailbox_capacity
    }

    /// Cloneable closer for graceful drain.
    pub fn closer(&self) -> ReqwestCloser {
        ReqwestCloser {
            closed: Arc::clone(&self.closed),
        }
    }

    fn admit_initial(
        &mut self,
        request: ReqwestRequest,
        request_context: Option<RequestContext<ReqwestResult>>,
    ) -> Effect<Self> {
        #[cfg(feature = "tracing")]
        let method = request.method.as_str();

        if self.closed.load(Ordering::Acquire) {
            self.metrics.closed.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "tracing")]
            event!(
                target: TRACE_TARGET_CALL,
                Level::WARN,
                kind = "admission_rejected",
                reason = "Closed",
                method,
            );
            return Self::complete_request(request_context, Err(ReqwestError::Closed));
        }
        if request.body.len() > self.config.request_body_limit {
            self.metrics
                .request_too_large
                .fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "tracing")]
            event!(
                target: TRACE_TARGET_CALL,
                Level::WARN,
                kind = "admission_rejected",
                reason = "RequestTooLarge",
                method,
            );
            return Self::complete_request(request_context, Err(ReqwestError::RequestTooLarge));
        }
        if self.in_flight.len() >= self.config.max_in_flight {
            self.metrics.full.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "tracing")]
            event!(
                target: TRACE_TARGET_CALL,
                Level::WARN,
                kind = "admission_rejected",
                reason = "Full",
                method,
            );
            return Self::complete_request(request_context, Err(ReqwestError::Full));
        }

        let (attempts_remaining, retry_delay) = match self.config.retry {
            RetryPolicy::None => (0u8, Duration::ZERO),
            RetryPolicy::Bounded {
                max_attempts,
                delay,
                ..
            } => (max_attempts.saturating_sub(1), delay),
        };
        let per_attempt_timeout = request.timeout.unwrap_or(self.config.default_timeout);
        let saved_request = if attempts_remaining > 0 {
            Some(request.clone())
        } else {
            None
        };
        self.spawn_attempt(
            request,
            request_context,
            attempts_remaining,
            retry_delay,
            per_attempt_timeout,
            saved_request,
        )
    }

    fn spawn_attempt(
        &mut self,
        request: ReqwestRequest,
        request_context: Option<RequestContext<ReqwestResult>>,
        attempts_remaining: u8,
        retry_delay: Duration,
        per_attempt_timeout: Duration,
        saved_request: Option<ReqwestRequest>,
    ) -> Effect<Self> {
        let method = request.method.as_str().to_string();
        let request_for_reqwest = match build_reqwest_request(&self.client, &request) {
            Ok(req) => req,
            Err(err) => {
                #[cfg(feature = "tracing")]
                event!(
                    target: TRACE_TARGET_CALL,
                    Level::WARN,
                    kind = "admission_rejected",
                    reason = reqwest_error_reason(&err),
                    method = method.as_str(),
                    detail = %err,
                );
                self.tally_error(&err);
                return Self::complete_request(request_context, Err(err));
            }
        };

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let response_limit = self.config.response_body_limit;
        let (tx, rx) = oneshot::channel();
        let client = self.client.clone();
        let task_handle = self.runtime.spawn(async move {
            let result = execute(
                client,
                request_for_reqwest,
                response_limit,
                per_attempt_timeout,
            )
            .await;
            let _ = tx.send(result);
        });
        let abort = task_handle.abort_handle();
        let attempt_started_at = Instant::now();

        self.in_flight.insert(
            id,
            Slot {
                kind: SlotKind::InFlight {
                    attempt_started_at,
                    receiver: rx,
                    abort,
                },
                request_context,
                per_attempt_timeout,
                attempts_remaining,
                retry_delay,
                saved_request,
                method: method.clone(),
            },
        );
        let in_flight = self.in_flight.len() as u64;
        self.metrics.admitted.fetch_add(1, Ordering::Relaxed);
        self.metrics.set_in_flight(in_flight);
        #[cfg(feature = "tracing")]
        event!(
            target: TRACE_TARGET_CALL,
            Level::DEBUG,
            kind = "admitted",
            method = method.as_str(),
            in_flight,
        );
        #[cfg(not(feature = "tracing"))]
        let _ = method;
        self.metrics.note_in_flight(in_flight);

        sleep(self.config.poll_interval).reply(move |_| ReqwestMsg::Poll(id))
    }

    fn schedule_retry(
        &mut self,
        request: ReqwestRequest,
        request_context: Option<RequestContext<ReqwestResult>>,
        attempts_remaining: u8,
        retry_delay: Duration,
        per_attempt_timeout: Duration,
    ) -> Effect<Self> {
        if retry_delay.is_zero() {
            let saved = if attempts_remaining > 0 {
                Some(request.clone())
            } else {
                None
            };
            return self.spawn_attempt(
                request,
                request_context,
                attempts_remaining,
                retry_delay,
                per_attempt_timeout,
                saved,
            );
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let attempt_due_at = Instant::now() + retry_delay;
        let method = request.method.as_str().to_string();
        self.in_flight.insert(
            id,
            Slot {
                kind: SlotKind::PendingRetry {
                    request,
                    attempt_due_at,
                },
                request_context,
                per_attempt_timeout,
                attempts_remaining,
                retry_delay,
                saved_request: None,
                method,
            },
        );
        let in_flight = self.in_flight.len() as u64;
        self.metrics.set_in_flight(in_flight);
        self.metrics.note_in_flight(in_flight);
        sleep(self.config.poll_interval).reply(move |_| ReqwestMsg::Poll(id))
    }

    fn poll(&mut self, id: u64) -> Effect<Self> {
        let slot = match self.in_flight.remove(&id) {
            Some(slot) => slot,
            None => return noop(),
        };

        let Slot {
            kind,
            request_context,
            per_attempt_timeout,
            attempts_remaining,
            retry_delay,
            saved_request,
            method,
        } = slot;

        match kind {
            SlotKind::InFlight {
                attempt_started_at,
                mut receiver,
                abort,
            } => match receiver.try_recv() {
                Ok(result) => self.deliver(
                    Delivery {
                        request_context,
                        per_attempt_timeout,
                        attempts_remaining,
                        retry_delay,
                        saved_request,
                        method,
                    },
                    result,
                ),
                Err(oneshot::error::TryRecvError::Empty) => {
                    if attempt_started_at.elapsed() >= per_attempt_timeout {
                        abort.abort();
                        if let (Some(saved), true) = (
                            saved_request,
                            attempts_remaining > 0 && retry_on_timeout(&self.config.retry),
                        ) {
                            self.metrics.retries.fetch_add(1, Ordering::Relaxed);
                            #[cfg(feature = "tracing")]
                            event!(
                                target: TRACE_TARGET_CALL,
                                Level::DEBUG,
                                kind = "retry",
                                reason = "Timeout",
                                method = method.as_str(),
                            );
                            return self.schedule_retry(
                                saved,
                                request_context,
                                attempts_remaining - 1,
                                retry_delay,
                                per_attempt_timeout,
                            );
                        }
                        self.metrics.timeout.fetch_add(1, Ordering::Relaxed);
                        self.note_terminal();
                        #[cfg(feature = "tracing")]
                        event!(
                            target: TRACE_TARGET_CALL,
                            Level::WARN,
                            kind = "timeout",
                            reason = "Timeout",
                            method = method.as_str(),
                            elapsed_ms = attempt_started_at.elapsed().as_millis() as u64,
                        );
                        return Self::complete_request(request_context, Err(ReqwestError::Timeout));
                    }
                    self.in_flight.insert(
                        id,
                        Slot {
                            kind: SlotKind::InFlight {
                                attempt_started_at,
                                receiver,
                                abort,
                            },
                            request_context,
                            per_attempt_timeout,
                            attempts_remaining,
                            retry_delay,
                            saved_request,
                            method,
                        },
                    );
                    sleep(self.config.poll_interval).reply(move |_| ReqwestMsg::Poll(id))
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    let err = ReqwestError::Reqwest("reqwest task ended without result".into());
                    if let (Some(saved), true) = (
                        saved_request,
                        attempts_remaining > 0 && retry_on_reqwest_io(&self.config.retry),
                    ) {
                        self.metrics.retries.fetch_add(1, Ordering::Relaxed);
                        #[cfg(feature = "tracing")]
                        event!(
                            target: TRACE_TARGET_CALL,
                            Level::DEBUG,
                            kind = "retry",
                            reason = "Reqwest",
                            method = method.as_str(),
                        );
                        return self.schedule_retry(
                            saved,
                            request_context,
                            attempts_remaining - 1,
                            retry_delay,
                            per_attempt_timeout,
                        );
                    }
                    self.tally_error(&err);
                    self.note_terminal();
                    #[cfg(feature = "tracing")]
                    event!(
                        target: TRACE_TARGET_CALL,
                        Level::WARN,
                        kind = "replied",
                        reason = "Reqwest",
                        method = method.as_str(),
                        detail = %err,
                    );
                    Self::complete_request(request_context, Err(err))
                }
            },
            SlotKind::PendingRetry {
                request,
                attempt_due_at,
            } => {
                if Instant::now() >= attempt_due_at {
                    let saved = if attempts_remaining > 0 {
                        Some(request.clone())
                    } else {
                        None
                    };
                    self.spawn_attempt(
                        request,
                        request_context,
                        attempts_remaining,
                        retry_delay,
                        per_attempt_timeout,
                        saved,
                    )
                } else {
                    self.in_flight.insert(
                        id,
                        Slot {
                            kind: SlotKind::PendingRetry {
                                request,
                                attempt_due_at,
                            },
                            request_context,
                            per_attempt_timeout,
                            attempts_remaining,
                            retry_delay,
                            saved_request,
                            method,
                        },
                    );
                    sleep(self.config.poll_interval).reply(move |_| ReqwestMsg::Poll(id))
                }
            }
        }
    }

    fn deliver(
        &mut self,
        delivery: Delivery,
        result: Result<ReqwestResponse, ReqwestError>,
    ) -> Effect<Self> {
        let Delivery {
            request_context,
            per_attempt_timeout,
            attempts_remaining,
            retry_delay,
            saved_request,
            method,
        } = delivery;

        if let Err(err) = &result {
            if attempts_remaining > 0 {
                if let (Some(saved), true) = (
                    saved_request.as_ref(),
                    is_retryable(err, &self.config.retry),
                ) {
                    self.metrics.retries.fetch_add(1, Ordering::Relaxed);
                    #[cfg(feature = "tracing")]
                    event!(
                        target: TRACE_TARGET_CALL,
                        Level::DEBUG,
                        kind = "retry",
                        reason = reqwest_error_reason(err),
                        method = method.as_str(),
                    );
                    let request = saved.clone();
                    return self.schedule_retry(
                        request,
                        request_context,
                        attempts_remaining - 1,
                        retry_delay,
                        per_attempt_timeout,
                    );
                }
            }
        }

        match &result {
            Ok(response) => {
                self.metrics.responses.fetch_add(1, Ordering::Relaxed);
                #[cfg(feature = "tracing")]
                event!(
                    target: TRACE_TARGET_CALL,
                    Level::DEBUG,
                    kind = "replied",
                    outcome = "response",
                    method = method.as_str(),
                    status = response.status.as_u16(),
                );
                #[cfg(not(feature = "tracing"))]
                let _ = response;
            }
            Err(err) => {
                self.tally_error(err);
                #[cfg(feature = "tracing")]
                event!(
                    target: TRACE_TARGET_CALL,
                    Level::WARN,
                    kind = "replied",
                    reason = reqwest_error_reason(err),
                    method = method.as_str(),
                    detail = %err,
                );
            }
        }
        #[cfg(not(feature = "tracing"))]
        let _ = method;
        self.note_terminal();
        Self::complete_request(request_context, result)
    }

    fn complete_request(
        request_context: Option<RequestContext<ReqwestResult>>,
        result: ReqwestResult,
    ) -> Effect<Self> {
        match request_context {
            Some(request) => reply_to_request(request, result),
            None => reply::<Self>(result),
        }
    }

    fn note_terminal(&self) {
        let in_flight = self.in_flight.len() as u64;
        self.metrics.set_in_flight(in_flight);
    }

    fn tally_error(&self, err: &ReqwestError) {
        let counter = match err {
            ReqwestError::Timeout => &self.metrics.timeout,
            ReqwestError::ResponseTooLarge => &self.metrics.response_too_large,
            ReqwestError::Reqwest(_) => &self.metrics.reqwest_error,
            ReqwestError::RequestTooLarge => &self.metrics.request_too_large,
            ReqwestError::InvalidRequest(_) => &self.metrics.invalid,
            ReqwestError::Full => &self.metrics.full,
            ReqwestError::Closed => &self.metrics.closed,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

fn is_retryable(err: &ReqwestError, policy: &RetryPolicy) -> bool {
    match err {
        ReqwestError::Timeout => retry_on_timeout(policy),
        ReqwestError::Reqwest(_) => retry_on_reqwest_io(policy),
        _ => false,
    }
}

fn retry_on_timeout(policy: &RetryPolicy) -> bool {
    matches!(
        policy,
        RetryPolicy::Bounded {
            on_timeout: true,
            ..
        }
    )
}

fn retry_on_reqwest_io(policy: &RetryPolicy) -> bool {
    matches!(
        policy,
        RetryPolicy::Bounded {
            on_reqwest_io: true,
            ..
        }
    )
}

impl<S: Shard + Send + 'static> ReqwestWorker<S> {
    /// One-call helper: validate config, build the worker, register it
    /// on `runtime`, and return the address, closer, and metrics handle.
    pub fn install<F>(
        runtime: &ThreadedRuntime<S, F>,
        config: ReqwestConfig,
    ) -> Result<InstalledReqwestBridge<S>, InstallError>
    where
        F: MailboxFactory + Send + 'static,
    {
        config.validate()?;
        let cap = config.mailbox_capacity;
        let (worker, metrics) = Self::new(config)?;
        let closer = worker.closer();
        let address = runtime.register_with_capacity::<_, Infallible>(worker, cap)?;
        Ok(InstalledReqwestBridge {
            address,
            closer,
            metrics,
            _shard: PhantomData,
        })
    }
}

impl<S: Shard + 'static> Isolate for ReqwestWorker<S> {
    tina::isolate_types! {
        message: ReqwestMsg,
        reply: Result<ReqwestResponse, ReqwestError>,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<ReqwestMsg>,
        shard: S,
    }

    fn handle(&mut self, msg: ReqwestMsg, _ctx: &mut Context<'_, S, Self::Reply>) -> Effect<Self> {
        match msg {
            ReqwestMsg::Send(request) => self.admit_initial(request, None),
            ReqwestMsg::Poll(id) => self.poll(id),
        }
    }

    fn handle_call(&mut self, msg: ReqwestMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            ReqwestMsg::Send(request) => {
                let request_context = call.into_request_context();
                self.admit_initial(request, Some(request_context))
            }
            ReqwestMsg::Poll(_) => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

/// Cloneable handle that flips the worker into the closed state.
///
/// Closing is a graceful drain: new sends reply
/// [`ReqwestError::Closed`], in-flight requests run to natural
/// completion (or per-attempt timeout). To force-cancel in-flight
/// work, drop the hosting Tina runtime.
#[derive(Debug, Clone)]
pub struct ReqwestCloser {
    closed: Arc<AtomicBool>,
}

impl ReqwestCloser {
    /// Mark the worker closed. Idempotent.
    pub fn close(&self) {
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

    /// Whether the worker has been closed.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

fn build_client(config: &ReqwestConfig) -> Result<Client, ReqwestError> {
    let mut builder = Client::builder().timeout(config.default_timeout);
    builder = match config.redirect {
        RedirectPolicy::None => builder.redirect(reqwest::redirect::Policy::none()),
        RedirectPolicy::Limited(n) => {
            if n == 0 {
                builder.redirect(reqwest::redirect::Policy::none())
            } else {
                builder.redirect(reqwest::redirect::Policy::limited(n as usize))
            }
        }
    };
    builder
        .build()
        .map_err(|e| ReqwestError::Reqwest(format!("client build: {e}")))
}

fn build_reqwest_request(
    client: &Client,
    request: &ReqwestRequest,
) -> Result<reqwest::Request, ReqwestError> {
    let url = reqwest::Url::parse(&request.url)
        .map_err(|e| ReqwestError::InvalidRequest(format!("url parse: {e}")))?;
    let method = match Method::from_bytes(request.method.as_str().as_bytes()) {
        Ok(m) => m,
        Err(e) => return Err(ReqwestError::InvalidRequest(format!("method: {e}"))),
    };
    let mut builder = client.request(method, url);
    builder = builder.headers(clone_headers(&request.headers)?);
    if !request.body.is_empty() {
        builder = builder.body(request.body.clone());
    }
    if let Some(timeout) = request.timeout {
        builder = builder.timeout(timeout);
    }
    builder
        .build()
        .map_err(|e| ReqwestError::Reqwest(format!("build request: {e}")))
}

fn clone_headers(src: &HeaderMap) -> Result<HeaderMap, ReqwestError> {
    let mut out = HeaderMap::with_capacity(src.len());
    for (name, value) in src.iter() {
        let n = HeaderName::from_bytes(name.as_str().as_bytes()).map_err(|e| {
            ReqwestError::InvalidRequest(format!("header name '{}': {e}", name.as_str()))
        })?;
        let v = HeaderValue::from_bytes(value.as_bytes()).map_err(|e| {
            ReqwestError::InvalidRequest(format!("header value for '{}': {e}", name.as_str()))
        })?;
        out.append(n, v);
    }
    Ok(out)
}

async fn execute(
    client: Client,
    request: reqwest::Request,
    response_limit: usize,
    overall_timeout: Duration,
) -> Result<ReqwestResponse, ReqwestError> {
    let send_future = async move {
        let url_for_diag = request.url().clone();
        let response = match client.execute(request).await {
            Ok(r) => r,
            Err(e) => return Err(reqwest_to_error(&url_for_diag, e)),
        };
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = read_body_capped(response, response_limit).await?;
        Ok(ReqwestResponse {
            status,
            headers,
            body: bytes,
        })
    };
    match tokio::time::timeout(overall_timeout, send_future).await {
        Ok(out) => out,
        Err(_) => Err(ReqwestError::Timeout),
    }
}

async fn read_body_capped(
    response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, ReqwestError> {
    let mut bytes = Vec::new();
    let mut response = response;
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if bytes.len().saturating_add(chunk.len()) > limit {
                    return Err(ReqwestError::ResponseTooLarge);
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(bytes),
            Err(e) => return Err(ReqwestError::Reqwest(format!("read body: {e}"))),
        }
    }
}

fn reqwest_to_error(url: &reqwest::Url, e: reqwest::Error) -> ReqwestError {
    if e.is_timeout() {
        ReqwestError::Timeout
    } else {
        ReqwestError::Reqwest(format!("{} {e}", url.as_str()))
    }
}
