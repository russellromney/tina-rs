//! AWS worker isolate around the AWS Rust SDK.

use std::collections::HashMap;
use std::convert::Infallible;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use aws_sdk_s3::Client;
use aws_sdk_s3::config::retry::RetryConfig;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use tina::CallContext;
use tina::prelude::*;
use tina_runtime::{MailboxFactory, RuntimeCall, ThreadedRuntime, sleep};
use tokio::runtime::Handle;
use tokio::sync::oneshot;
#[cfg(feature = "tracing")]
use tracing::{Level, event};

use crate::metrics::{MetricsInner, S3MetricsHandle};
use crate::types::{
    S3Config, S3ConfigError, S3DeletedObject, S3Error, S3Object, S3ObjectHead, S3PutObjectOk,
    S3Request, S3Response, SdkRetryPolicy,
};

#[cfg(feature = "tracing")]
const TRACE_TARGET_CALL: &str = "tina_aws.bridge.call";
#[cfg(feature = "tracing")]
const TRACE_TARGET_BRIDGE: &str = "tina_aws.bridge";

/// Worker reply type.
pub type S3Result = Result<S3Response, S3Error>;

/// Messages handled by [`S3Worker`].
#[derive(Debug)]
pub enum S3Msg {
    /// Submit one AWS operation.
    Send(S3Request),
    /// Internal sleep wakeup.
    #[doc(hidden)]
    Poll(u64),
}

struct InFlight {
    started_at: Instant,
    receiver: oneshot::Receiver<S3Result>,
    abandoned: Arc<AtomicBool>,
    request_context: Option<RequestContext<S3Result>>,
    reply_plain: bool,
    #[cfg_attr(not(feature = "tracing"), allow(dead_code))]
    request_kind: &'static str,
}

/// Result of [`S3Worker::install`].
pub struct InstalledS3Bridge<S: Shard + 'static> {
    /// Tina address callers use with `call(...)`.
    pub address: Address<S3Msg, S3Result>,
    /// Closer for Tina-side admission.
    pub closer: S3Closer,
    /// Metrics handle.
    pub metrics: S3MetricsHandle,
    _shard: PhantomData<S>,
}

/// Cloneable closer. Stops new Tina-side admission; already admitted
/// SDK work runs to completion or bridge timeout.
#[derive(Debug, Clone)]
pub struct S3Closer {
    closed: Arc<AtomicBool>,
    metrics: Arc<MetricsInner>,
}

impl S3Closer {
    /// Mark the bridge closed. Idempotent.
    pub fn close(&self) {
        #[cfg(feature = "tracing")]
        {
            let was_closed = self.closed.swap(true, Ordering::AcqRel);
            if !was_closed {
                event!(target: TRACE_TARGET_BRIDGE, Level::DEBUG, kind = "close");
            }
        }
        #[cfg(not(feature = "tracing"))]
        self.closed.store(true, Ordering::Release);
    }

    /// Whether the bridge has been closed.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Close admission and wait up to `timeout` for already accepted
    /// SDK work to leave the bridge's in-flight set.
    pub fn close_and_drain(&self, timeout: Duration) -> S3DrainReport {
        self.close();
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = self.metrics.in_flight_current.load(Ordering::Relaxed);
            if remaining == 0 {
                return S3DrainReport {
                    closed: true,
                    drained: true,
                    in_flight_remaining: 0,
                    in_flight_kinds: Vec::new(),
                };
            }
            if Instant::now() >= deadline {
                return S3DrainReport {
                    closed: true,
                    drained: false,
                    in_flight_remaining: remaining,
                    in_flight_kinds: self.metrics.in_flight_kinds(),
                };
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// Report returned by [`S3Closer::close_and_drain`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3DrainReport {
    /// Admission was closed.
    pub closed: bool,
    /// All accepted bridge work left the in-flight set before the
    /// deadline.
    pub drained: bool,
    /// In-flight SDK work still tracked at the drain deadline.
    pub in_flight_remaining: u64,
    /// Operation kinds still in flight at the drain deadline.
    pub in_flight_kinds: Vec<(&'static str, u64)>,
}

/// Install/build failure.
#[derive(Debug)]
pub enum InstallError {
    /// Invalid config.
    Config(S3ConfigError),
    /// Tokio runtime or AWS client construction failed.
    Build(String),
    /// Tina runtime registration failed.
    Register(String),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(e) => write!(f, "s3 bridge install: {e}"),
            Self::Build(e) => write!(f, "s3 bridge install: {e}"),
            Self::Register(e) => write!(f, "s3 bridge install: register: {e}"),
        }
    }
}

impl std::error::Error for InstallError {}

impl From<S3ConfigError> for InstallError {
    fn from(value: S3ConfigError) -> Self {
        Self::Config(value)
    }
}

struct OwnedRuntime(Option<tokio::runtime::Runtime>);

impl Drop for OwnedRuntime {
    fn drop(&mut self) {
        if let Some(rt) = self.0.take() {
            rt.shutdown_background();
        }
    }
}

/// Bounded AWS SDK worker isolate.
pub struct S3Worker<S: Shard + 'static> {
    config: S3Config,
    client: Client,
    runtime: Handle,
    in_flight: HashMap<u64, InFlight>,
    next_id: u64,
    closed: Arc<AtomicBool>,
    metrics: Arc<MetricsInner>,
    _owned_runtime: Option<OwnedRuntime>,
    _shard: PhantomData<S>,
}

impl<S: Shard + 'static> S3Worker<S> {
    /// Build a worker that owns its own Tokio runtime and S3 client.
    pub fn new(config: S3Config) -> Result<(Self, S3MetricsHandle), S3Error> {
        config
            .validate()
            .map_err(|e| S3Error::InvalidRequest(format!("config: {e}")))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .thread_name("tina-aws-bridge")
            .build()
            .map_err(|e| S3Error::Internal(format!("tokio runtime build: {e}")))?;
        let handle = runtime.handle().clone();
        let client = build_s3_client(&config);
        let owned = OwnedRuntime(Some(runtime));
        let sdk_max_attempts = u64::from(config.retry_policy.max_attempts());
        Ok(Self::assemble(
            config,
            client,
            handle,
            Some(owned),
            sdk_max_attempts,
        ))
    }

    /// Build a worker around a caller-supplied S3 client and Tokio
    /// runtime handle.
    ///
    /// The supplied client owns credentials, region, endpoint, HTTP
    /// connector, TLS, and SDK retry configuration. The metrics field
    /// `sdk_max_attempts` is `0` on this path because the bridge
    /// cannot inspect that caller-owned policy. The bridge still owns
    /// Tina-side `mailbox_capacity`, `max_in_flight`, body caps, and
    /// per-operation timeout.
    pub fn with_supplied_client(
        config: S3Config,
        client: Client,
        runtime: Handle,
    ) -> Result<(Self, S3MetricsHandle), S3ConfigError> {
        config.validate_bridge_fields()?;
        Ok(Self::assemble(config, client, runtime, None, 0))
    }

    fn assemble(
        config: S3Config,
        client: Client,
        runtime: Handle,
        owned_runtime: Option<OwnedRuntime>,
        sdk_max_attempts: u64,
    ) -> (Self, S3MetricsHandle) {
        let metrics = Arc::new(MetricsInner::default());
        metrics
            .sdk_max_attempts
            .store(sdk_max_attempts, Ordering::Relaxed);
        let handle = S3MetricsHandle {
            inner: Arc::clone(&metrics),
            capacity: config.max_in_flight,
        };
        let worker = Self {
            config,
            client,
            runtime,
            in_flight: HashMap::new(),
            next_id: 1,
            closed: Arc::new(AtomicBool::new(false)),
            metrics,
            _owned_runtime: owned_runtime,
            _shard: PhantomData,
        };
        (worker, handle)
    }

    /// Configured mailbox capacity.
    pub fn mailbox_capacity(&self) -> usize {
        self.config.mailbox_capacity
    }

    /// Cloneable closer.
    pub fn closer(&self) -> S3Closer {
        S3Closer {
            closed: Arc::clone(&self.closed),
            metrics: Arc::clone(&self.metrics),
        }
    }

    fn admit(
        &mut self,
        request: S3Request,
        request_context: Option<RequestContext<S3Result>>,
    ) -> Effect<Self> {
        let request_kind = request.kind();
        if self.closed.load(Ordering::Acquire) {
            self.metrics.closed.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "tracing")]
            event!(
                target: TRACE_TARGET_CALL,
                Level::WARN,
                kind = "admission_rejected",
                reason = "Closed",
                request_kind,
            );
            return Self::complete_request(request_context, Err(S3Error::Closed));
        }
        if let Err(err) = validate_request(&request, &self.config) {
            tally_admission_error(&self.metrics, &err);
            #[cfg(feature = "tracing")]
            event!(
                target: TRACE_TARGET_CALL,
                Level::WARN,
                kind = "admission_rejected",
                reason = admission_reason(&err),
                request_kind,
                detail = %err,
            );
            return Self::complete_request(request_context, Err(err));
        }
        if self.in_flight.len() >= self.config.max_in_flight {
            self.metrics.full.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "tracing")]
            event!(
                target: TRACE_TARGET_CALL,
                Level::WARN,
                kind = "admission_rejected",
                reason = "Full",
                request_kind,
            );
            return Self::complete_request(request_context, Err(S3Error::Full));
        }

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let reply_plain = request_context.is_none();
        let (tx, rx) = oneshot::channel();
        let client = self.client.clone();
        let response_body_limit = self.config.response_body_limit;
        let abandoned = Arc::new(AtomicBool::new(false));
        let abandoned_for_task = Arc::clone(&abandoned);
        let metrics_for_task = Arc::clone(&self.metrics);
        self.runtime.spawn(async move {
            let result = run_request(client, request, response_body_limit).await;
            tally_terminal(&metrics_for_task, &result);
            if abandoned_for_task.load(Ordering::Acquire) {
                metrics_for_task
                    .late_results
                    .fetch_add(1, Ordering::Relaxed);
            }
            let _ = tx.send(result);
        });

        self.in_flight.insert(
            id,
            InFlight {
                started_at: Instant::now(),
                receiver: rx,
                abandoned,
                request_context,
                reply_plain,
                request_kind,
            },
        );
        let in_flight = self.in_flight.len() as u64;
        self.metrics.admitted.fetch_add(1, Ordering::Relaxed);
        self.metrics.note_admit_kind(request_kind);
        self.metrics.set_in_flight(in_flight);
        self.metrics.note_in_flight(in_flight);
        #[cfg(feature = "tracing")]
        event!(
            target: TRACE_TARGET_CALL,
            Level::DEBUG,
            kind = "admitted",
            request_kind,
            in_flight,
        );
        sleep(self.config.poll_interval).then(move |_| S3Msg::Poll(id))
    }

    fn poll(&mut self, id: u64) -> Effect<Self> {
        let Some(mut in_flight) = self.in_flight.remove(&id) else {
            return noop();
        };
        match in_flight.receiver.try_recv() {
            Ok(result) => {
                self.note_terminal(in_flight.request_kind);
                Self::complete_terminal(in_flight.request_context, in_flight.reply_plain, result)
            }
            Err(oneshot::error::TryRecvError::Empty) => {
                if in_flight.request_context.is_some()
                    && in_flight.started_at.elapsed() >= self.config.default_timeout
                {
                    in_flight.abandoned.store(true, Ordering::Release);
                    self.metrics.timeouts.fetch_add(1, Ordering::Relaxed);
                    #[cfg(feature = "tracing")]
                    event!(
                        target: TRACE_TARGET_CALL,
                        Level::WARN,
                        kind = "timeout",
                        request_kind = in_flight.request_kind,
                        elapsed_ms = in_flight.started_at.elapsed().as_millis() as u64,
                    );
                    let request_context = in_flight.request_context.take();
                    self.in_flight.insert(id, in_flight);
                    return batch(vec![
                        Self::complete_request(request_context, Err(S3Error::Timeout)),
                        sleep(self.config.poll_interval).then(move |_| S3Msg::Poll(id)),
                    ]);
                }
                self.in_flight.insert(id, in_flight);
                sleep(self.config.poll_interval).then(move |_| S3Msg::Poll(id))
            }
            Err(oneshot::error::TryRecvError::Closed) => {
                self.note_terminal(in_flight.request_kind);
                Self::complete_terminal(
                    in_flight.request_context,
                    in_flight.reply_plain,
                    Err(S3Error::Internal("sdk task ended without result".into())),
                )
            }
        }
    }

    fn complete_request(
        request_context: Option<RequestContext<S3Result>>,
        result: S3Result,
    ) -> Effect<Self> {
        match request_context {
            Some(request) => reply_to_request(request, result),
            None => reply::<Self>(result),
        }
    }

    fn complete_terminal(
        request_context: Option<RequestContext<S3Result>>,
        reply_plain: bool,
        result: S3Result,
    ) -> Effect<Self> {
        match request_context {
            Some(request) => reply_to_request(request, result),
            None if reply_plain => reply::<Self>(result),
            None => noop(),
        }
    }

    fn note_terminal(&self, request_kind: &'static str) {
        let in_flight = self.in_flight.len() as u64;
        self.metrics.note_terminal_kind(request_kind);
        self.metrics.set_in_flight(in_flight);
    }
}

impl<S: Shard + Send + 'static> S3Worker<S> {
    /// Validate config, build the worker, register it, and return the
    /// address, closer, and metrics handle.
    pub fn install<F>(
        runtime: &ThreadedRuntime<S, F>,
        config: S3Config,
    ) -> Result<InstalledS3Bridge<S>, InstallError>
    where
        F: MailboxFactory + Send + 'static,
    {
        config.validate()?;
        let cap = config.mailbox_capacity;
        let (worker, metrics) =
            Self::new(config).map_err(|e| InstallError::Build(e.to_string()))?;
        let closer = worker.closer();
        let address = runtime
            .register_with_capacity::<_, Infallible>(worker, cap)
            .map_err(|e| InstallError::Register(format!("{e:?}")))?;
        Ok(InstalledS3Bridge {
            address,
            closer,
            metrics,
            _shard: PhantomData,
        })
    }
}

/// Validate config, build the S3 worker, register it, and return the
/// address, closer, and metrics handle.
pub fn install_s3<S, F>(
    runtime: &ThreadedRuntime<S, F>,
    config: S3Config,
) -> Result<InstalledS3Bridge<S>, InstallError>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    S3Worker::<S>::install(runtime, config)
}

impl<S: Shard + 'static> Isolate for S3Worker<S> {
    tina::isolate_types! {
        message: S3Msg,
        reply: S3Result,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<S3Msg>,
        shard: S,
    }

    fn handle(&mut self, msg: S3Msg, _ctx: &mut Context<'_, S, Self::Reply>) -> Effect<Self> {
        match msg {
            S3Msg::Send(request) => self.admit(request, None),
            S3Msg::Poll(id) => self.poll(id),
        }
    }

    fn handle_call(&mut self, msg: S3Msg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            S3Msg::Send(request) => self.admit(request, Some(call.into_request_context())),
            S3Msg::Poll(_) => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

fn build_s3_client(config: &S3Config) -> Client {
    let mut builder = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::v2026_01_12())
        .region(Region::new(config.region.clone()))
        .retry_config(match config.retry_policy {
            SdkRetryPolicy::Disabled => RetryConfig::disabled(),
            SdkRetryPolicy::Standard { max_attempts } => {
                RetryConfig::standard().with_max_attempts(max_attempts)
            }
        })
        .force_path_style(config.force_path_style);
    if let Some(credentials) = &config.credentials {
        builder = builder.credentials_provider(Credentials::new(
            credentials.access_key_id.clone(),
            credentials.secret_access_key.clone(),
            credentials.session_token.clone(),
            None,
            "tina-aws-bridge",
        ));
    }
    if let Some(endpoint) = &config.endpoint_url {
        builder = builder.endpoint_url(endpoint);
    }
    Client::from_conf(builder.build())
}

fn validate_request(request: &S3Request, config: &S3Config) -> Result<(), S3Error> {
    match request {
        S3Request::PutObject(put) => {
            validate_bucket(&put.bucket)?;
            validate_key(&put.key)?;
            if put.body.len() > config.request_body_limit {
                return Err(S3Error::RequestTooLarge);
            }
            Ok(())
        }
        S3Request::GetObject(get) => {
            validate_bucket(&get.bucket)?;
            validate_key(&get.key)
        }
        S3Request::HeadObject(head) => {
            validate_bucket(&head.bucket)?;
            validate_key(&head.key)
        }
        S3Request::DeleteObject(delete) => {
            validate_bucket(&delete.bucket)?;
            validate_key(&delete.key)
        }
    }
}

fn validate_bucket(bucket: &str) -> Result<(), S3Error> {
    if bucket.trim().is_empty() {
        Err(S3Error::InvalidRequest("bucket must not be empty".into()))
    } else {
        Ok(())
    }
}

fn validate_key(key: &str) -> Result<(), S3Error> {
    if key.is_empty() {
        Err(S3Error::InvalidRequest("key must not be empty".into()))
    } else {
        Ok(())
    }
}

async fn run_request(client: Client, request: S3Request, response_limit: usize) -> S3Result {
    match request {
        S3Request::PutObject(put) => {
            let mut req = client
                .put_object()
                .bucket(put.bucket)
                .key(put.key)
                .body(ByteStream::from(put.body));
            if let Some(content_type) = put.content_type {
                req = req.content_type(content_type);
            }
            req.send()
                .await
                .map(|out| {
                    S3Response::PutObject(S3PutObjectOk {
                        e_tag: out.e_tag,
                        version_id: out.version_id,
                    })
                })
                .map_err(|e| S3Error::Sdk(e.to_string()))
        }
        S3Request::GetObject(get) => {
            let limit = get
                .max_bytes
                .map(|max| max.min(response_limit))
                .unwrap_or(response_limit);
            let out = client
                .get_object()
                .bucket(get.bucket)
                .key(get.key)
                .send()
                .await
                .map_err(|e| S3Error::Sdk(e.to_string()))?;
            let body = read_capped(out.body, limit).await?;
            Ok(S3Response::Object(S3Object {
                body,
                content_length: out.content_length,
                content_type: out.content_type,
                e_tag: out.e_tag,
            }))
        }
        S3Request::HeadObject(head) => client
            .head_object()
            .bucket(head.bucket)
            .key(head.key)
            .send()
            .await
            .map(|out| {
                S3Response::HeadObject(S3ObjectHead {
                    content_length: out.content_length,
                    content_type: out.content_type,
                    e_tag: out.e_tag,
                })
            })
            .map_err(|e| S3Error::Sdk(e.to_string())),
        S3Request::DeleteObject(delete) => client
            .delete_object()
            .bucket(delete.bucket)
            .key(delete.key)
            .send()
            .await
            .map(|out| {
                S3Response::DeletedObject(S3DeletedObject {
                    version_id: out.version_id,
                    delete_marker: out.delete_marker,
                })
            })
            .map_err(|e| S3Error::Sdk(e.to_string())),
    }
}

async fn read_capped(mut stream: ByteStream, limit: usize) -> Result<Vec<u8>, S3Error> {
    let mut out = Vec::new();
    while let Some(chunk) = stream
        .try_next()
        .await
        .map_err(|e| S3Error::Body(e.to_string()))?
    {
        if out.len().saturating_add(chunk.len()) > limit {
            return Err(S3Error::ResponseTooLarge);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

fn tally_admission_error(metrics: &MetricsInner, err: &S3Error) {
    // `validate_request` only produces `RequestTooLarge` or
    // `InvalidRequest`. Future validator additions land in `invalid`
    // until they earn a typed counter.
    match err {
        S3Error::RequestTooLarge => {
            metrics.request_too_large.fetch_add(1, Ordering::Relaxed);
        }
        _ => {
            metrics.invalid.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Maps an admission-class [`S3Error`] to the wire-stable tracing
/// `reason` label. Only the two variants `validate_request` can
/// produce are listed; other variants are not reachable on the
/// admission tracing path.
#[cfg(feature = "tracing")]
fn admission_reason(err: &S3Error) -> &'static str {
    match err {
        S3Error::RequestTooLarge => "RequestTooLarge",
        S3Error::InvalidRequest(_) => "InvalidRequest",
        // Unreachable: validate_request only emits RequestTooLarge or
        // InvalidRequest. See [`admission_reason`] above.
        _ => "Invalid",
    }
}

fn tally_terminal(metrics: &MetricsInner, result: &S3Result) {
    match result {
        Ok(_) => {
            metrics.responses.fetch_add(1, Ordering::Relaxed);
        }
        Err(S3Error::Sdk(_)) => {
            metrics.sdk_errors.fetch_add(1, Ordering::Relaxed);
        }
        Err(S3Error::Body(_)) => {
            metrics.body_errors.fetch_add(1, Ordering::Relaxed);
        }
        Err(S3Error::ResponseTooLarge) => {
            metrics.response_too_large.fetch_add(1, Ordering::Relaxed);
        }
        Err(_) => {}
    }
}
