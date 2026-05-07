//! Reqwest worker isolate.

use std::collections::HashMap;
use std::convert::Infallible;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use http::{HeaderMap, HeaderName, HeaderValue, Method};
use reqwest::Client;
use tina::prelude::*;
use tina_runtime::{RuntimeCall, sleep};
use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tokio::task::AbortHandle;

use crate::metrics::{MetricsInner, ReqwestMetricsHandle};
use crate::types::{
    RedirectPolicy, ReqwestConfig, ReqwestError, ReqwestRequest, ReqwestResponse, RetryPolicy,
};

/// Messages handled by [`ReqwestWorker`].
#[derive(Debug)]
pub enum ReqwestMsg {
    /// Issue one outbound HTTP request and reply with the outcome.
    Send(ReqwestRequest),
    /// Internal sleep wakeup. User code must not construct.
    #[doc(hidden)]
    Poll(u64),
    /// Stop accepting new sends. Abort in-flight tasks. Idempotent.
    Close,
}

enum SlotKind {
    InFlight {
        receiver: oneshot::Receiver<Result<ReqwestResponse, ReqwestError>>,
        abort: AbortHandle,
    },
    PendingRetry {
        request: ReqwestRequest,
        scheduled_at: Instant,
        delay: Duration,
    },
}

struct Slot {
    kind: SlotKind,
    started_at: Instant,
    timeout: Duration,
    /// Retry attempts left after this one. 0 = no more retry.
    attempts_remaining: u8,
    retry_delay: Duration,
    /// Saved for retry. None when retry is off.
    saved_request: Option<ReqwestRequest>,
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
    _owned_runtime: Option<Arc<tokio::runtime::Runtime>>,
    _shard: PhantomData<S>,
}

impl<S: Shard + 'static> ReqwestWorker<S> {
    /// Builds a worker that owns its own Tokio runtime and reqwest
    /// client.
    pub fn new(config: ReqwestConfig) -> Result<(Self, ReqwestMetricsHandle), ReqwestError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .thread_name("tina-reqwest-bridge")
            .build()
            .map_err(|e| ReqwestError::Reqwest(format!("tokio runtime build: {e}")))?;
        let handle = runtime.handle().clone();
        let client = build_client(&config)?;
        let runtime = Arc::new(runtime);
        let (worker, metrics) =
            Self::with_parts_inner(config, client, handle, Some(Arc::clone(&runtime)));
        Ok((worker, metrics))
    }

    /// Builds a worker around a caller-supplied reqwest client and
    /// Tokio runtime handle. Caller keeps the runtime alive.
    pub fn with_parts(
        config: ReqwestConfig,
        client: Client,
        runtime: Handle,
    ) -> (Self, ReqwestMetricsHandle) {
        Self::with_parts_inner(config, client, runtime, None)
    }

    fn with_parts_inner(
        config: ReqwestConfig,
        client: Client,
        runtime: Handle,
        owned_runtime: Option<Arc<tokio::runtime::Runtime>>,
    ) -> (Self, ReqwestMetricsHandle) {
        let config = config.validated();
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

    /// Configured mailbox capacity, for use at registration.
    pub fn mailbox_capacity(&self) -> usize {
        self.config.mailbox_capacity
    }

    /// Cloneable closer that flips the worker into the closed state
    /// from outside the hosting runtime.
    pub fn closer(&self) -> ReqwestCloser {
        ReqwestCloser {
            closed: Arc::clone(&self.closed),
        }
    }

    fn admit_initial(&mut self, request: ReqwestRequest) -> Effect<Self> {
        if self.closed.load(Ordering::Acquire) {
            self.metrics.closed.fetch_add(1, Ordering::Relaxed);
            return reply::<Self>(Err(ReqwestError::Closed));
        }
        if request.body.len() > self.config.request_body_limit {
            self.metrics
                .request_too_large
                .fetch_add(1, Ordering::Relaxed);
            return reply::<Self>(Err(ReqwestError::RequestTooLarge));
        }
        if self.in_flight.len() >= self.config.max_in_flight {
            self.metrics.full.fetch_add(1, Ordering::Relaxed);
            return reply::<Self>(Err(ReqwestError::Full));
        }

        let (attempts_remaining, retry_delay, save_for_retry) = match self.config.retry {
            RetryPolicy::None => (0u8, Duration::ZERO, false),
            RetryPolicy::Bounded {
                attempts, delay, ..
            } => {
                let remaining = attempts.saturating_sub(1);
                (remaining, delay, remaining > 0)
            }
        };
        let timeout = request.timeout.unwrap_or(self.config.default_timeout);
        let started_at = Instant::now();
        let saved = if save_for_retry {
            Some(request.clone())
        } else {
            None
        };
        self.spawn_attempt(
            request,
            attempts_remaining,
            retry_delay,
            timeout,
            started_at,
            saved,
        )
    }

    fn spawn_attempt(
        &mut self,
        request: ReqwestRequest,
        attempts_remaining: u8,
        retry_delay: Duration,
        timeout: Duration,
        started_at: Instant,
        saved_request: Option<ReqwestRequest>,
    ) -> Effect<Self> {
        let request_for_reqwest = match build_reqwest_request(&self.client, &request) {
            Ok(req) => req,
            Err(err) => {
                self.metrics.invalid.fetch_add(1, Ordering::Relaxed);
                return reply::<Self>(Err(err));
            }
        };

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let response_limit = self.config.response_body_limit;
        let (tx, rx) = oneshot::channel();
        let client = self.client.clone();
        let task_handle = self.runtime.spawn(async move {
            let result = execute(client, request_for_reqwest, response_limit, timeout).await;
            let _ = tx.send(result);
        });
        let abort = task_handle.abort_handle();

        self.in_flight.insert(
            id,
            Slot {
                kind: SlotKind::InFlight {
                    receiver: rx,
                    abort,
                },
                started_at,
                timeout,
                attempts_remaining,
                retry_delay,
                saved_request,
            },
        );
        let in_flight = self.in_flight.len() as u64;
        self.metrics.admitted.fetch_add(1, Ordering::Relaxed);
        self.metrics.note_in_flight(in_flight);

        sleep(self.config.poll_interval).reply(move |_| ReqwestMsg::Poll(id))
    }

    fn schedule_retry(
        &mut self,
        request: ReqwestRequest,
        attempts_remaining: u8,
        retry_delay: Duration,
        timeout: Duration,
        started_at: Instant,
    ) -> Effect<Self> {
        if retry_delay.is_zero() {
            let saved = if attempts_remaining > 0 {
                Some(request.clone())
            } else {
                None
            };
            return self.spawn_attempt(
                request,
                attempts_remaining,
                retry_delay,
                timeout,
                started_at,
                saved,
            );
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.in_flight.insert(
            id,
            Slot {
                kind: SlotKind::PendingRetry {
                    request,
                    scheduled_at: Instant::now(),
                    delay: retry_delay,
                },
                started_at,
                timeout,
                attempts_remaining,
                retry_delay,
                saved_request: None,
            },
        );
        sleep(self.config.poll_interval).reply(move |_| ReqwestMsg::Poll(id))
    }

    fn poll(&mut self, id: u64) -> Effect<Self> {
        let slot = match self.in_flight.remove(&id) {
            Some(slot) => slot,
            None => return noop(),
        };

        if self.closed.load(Ordering::Acquire) {
            if let SlotKind::InFlight { abort, .. } = &slot.kind {
                abort.abort();
            }
            self.metrics.closed.fetch_add(1, Ordering::Relaxed);
            return reply::<Self>(Err(ReqwestError::Closed));
        }

        let Slot {
            kind,
            started_at,
            timeout,
            attempts_remaining,
            retry_delay,
            mut saved_request,
        } = slot;

        match kind {
            SlotKind::InFlight {
                mut receiver,
                abort,
            } => match receiver.try_recv() {
                Ok(result) => self.deliver(
                    started_at,
                    timeout,
                    attempts_remaining,
                    retry_delay,
                    saved_request,
                    result,
                ),
                Err(oneshot::error::TryRecvError::Empty) => {
                    if started_at.elapsed() >= timeout {
                        abort.abort();
                        self.metrics.timeout.fetch_add(1, Ordering::Relaxed);
                        if let Some(saved) = saved_request.take() {
                            if let RetryPolicy::Bounded { on_timeout, .. } = self.config.retry {
                                if on_timeout && attempts_remaining > 0 {
                                    self.metrics.retries.fetch_add(1, Ordering::Relaxed);
                                    return self.schedule_retry(
                                        saved,
                                        attempts_remaining - 1,
                                        retry_delay,
                                        timeout,
                                        Instant::now(),
                                    );
                                }
                            }
                        }
                        return reply::<Self>(Err(ReqwestError::Timeout));
                    }
                    self.in_flight.insert(
                        id,
                        Slot {
                            kind: SlotKind::InFlight { receiver, abort },
                            started_at,
                            timeout,
                            attempts_remaining,
                            retry_delay,
                            saved_request,
                        },
                    );
                    sleep(self.config.poll_interval).reply(move |_| ReqwestMsg::Poll(id))
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    self.metrics
                        .reqwest_error
                        .fetch_add(1, Ordering::Relaxed);
                    reply::<Self>(Err(ReqwestError::Reqwest(
                        "reqwest task ended without result".into(),
                    )))
                }
            },
            SlotKind::PendingRetry {
                request,
                scheduled_at,
                delay,
            } => {
                if scheduled_at.elapsed() >= delay {
                    let saved = if attempts_remaining > 0 {
                        Some(request.clone())
                    } else {
                        None
                    };
                    self.spawn_attempt(
                        request,
                        attempts_remaining,
                        retry_delay,
                        timeout,
                        started_at,
                        saved,
                    )
                } else {
                    self.in_flight.insert(
                        id,
                        Slot {
                            kind: SlotKind::PendingRetry {
                                request,
                                scheduled_at,
                                delay,
                            },
                            started_at,
                            timeout,
                            attempts_remaining,
                            retry_delay,
                            saved_request,
                        },
                    );
                    sleep(self.config.poll_interval).reply(move |_| ReqwestMsg::Poll(id))
                }
            }
        }
    }

    fn deliver(
        &mut self,
        started_at: Instant,
        timeout: Duration,
        attempts_remaining: u8,
        retry_delay: Duration,
        saved_request: Option<ReqwestRequest>,
        result: Result<ReqwestResponse, ReqwestError>,
    ) -> Effect<Self> {
        if started_at.elapsed() > timeout {
            self.metrics.late_results.fetch_add(1, Ordering::Relaxed);
        }

        if let Err(err) = &result {
            if attempts_remaining > 0 {
                if let (Some(saved), RetryPolicy::Bounded { on_full, on_timeout, on_reqwest_io, .. }) =
                    (saved_request.as_ref(), &self.config.retry)
                {
                    if is_retryable(err, *on_full, *on_timeout, *on_reqwest_io) {
                        self.metrics.retries.fetch_add(1, Ordering::Relaxed);
                        let request = saved.clone();
                        return self.schedule_retry(
                            request,
                            attempts_remaining - 1,
                            retry_delay,
                            timeout,
                            started_at,
                        );
                    }
                }
            }
        }

        match &result {
            Ok(_) => {
                self.metrics.responses.fetch_add(1, Ordering::Relaxed);
            }
            Err(err) => self.tally_error(err),
        }
        reply::<Self>(result)
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

    fn close(&mut self) {
        self.closed.store(true, Ordering::Release);
        for slot in self.in_flight.values() {
            if let SlotKind::InFlight { abort, .. } = &slot.kind {
                abort.abort();
            }
        }
    }
}

fn is_retryable(
    err: &ReqwestError,
    on_full: bool,
    on_timeout: bool,
    on_reqwest_io: bool,
) -> bool {
    match err {
        ReqwestError::Full => on_full,
        ReqwestError::Timeout => on_timeout,
        ReqwestError::Reqwest(_) => on_reqwest_io,
        _ => false,
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

    fn handle(&mut self, msg: ReqwestMsg, _ctx: &mut Context<'_, S>) -> Effect<Self> {
        match msg {
            ReqwestMsg::Send(request) => self.admit_initial(request),
            ReqwestMsg::Poll(id) => self.poll(id),
            ReqwestMsg::Close => {
                self.close();
                noop()
            }
        }
    }
}

/// Cloneable handle that flips the worker into the closed state.
#[derive(Debug, Clone)]
pub struct ReqwestCloser {
    closed: Arc<AtomicBool>,
}

impl ReqwestCloser {
    /// Mark the worker closed. Idempotent.
    pub fn close(&self) {
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
    builder = builder.headers(clone_headers(&request.headers));
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

fn clone_headers(src: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(src.len());
    for (name, value) in src.iter() {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            out.append(n, v);
        }
    }
    out
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
