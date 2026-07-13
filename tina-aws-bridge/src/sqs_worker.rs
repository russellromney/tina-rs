//! SQS worker isolate around the AWS Rust SDK.

use std::collections::HashMap;
use std::convert::Infallible;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use aws_sdk_sqs::Client;
use aws_sdk_sqs::config::retry::RetryConfig;
use aws_sdk_sqs::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_sqs::error::SdkError;
use aws_sdk_sqs::operation::delete_message::DeleteMessageError;
use aws_sdk_sqs::operation::receive_message::ReceiveMessageError;
use aws_sdk_sqs::operation::send_message::SendMessageError;
use tina::CallContext;
use tina::prelude::*;
use tina_runtime::{
    LocalSystem, MailboxFactory, RuntimeCall, ThreadedRuntime, ThreadedRuntimeError, sleep,
};
use tokio::runtime::Handle;
use tokio::sync::oneshot;
#[cfg(feature = "tracing")]
use tracing::{Level, event};

use crate::sqs_metrics::{SqsMetricsHandle, SqsMetricsInner};
use crate::sqs_types::{
    SqsConfig, SqsConfigError, SqsDeletedMessage, SqsError, SqsMessage, SqsReceivedMessages,
    SqsRequest, SqsResponse, SqsSentMessage,
};
use crate::types::SdkRetryPolicy;

#[cfg(feature = "tracing")]
const TRACE_TARGET_CALL: &str = "tina_aws.bridge.call";
#[cfg(feature = "tracing")]
const TRACE_TARGET_BRIDGE: &str = "tina_aws.bridge";

/// Worker reply type.
pub type SqsResult = Result<SqsResponse, SqsError>;

/// Messages handled by [`SqsWorker`].
#[derive(Debug)]
pub enum SqsMsg {
    /// Submit one SQS operation.
    Send(SqsRequest),
    /// Internal sleep wakeup.
    #[doc(hidden)]
    Poll(u64),
}

struct SqsInFlight {
    started_at: Instant,
    receiver: oneshot::Receiver<SqsResult>,
    abandoned: Arc<AtomicBool>,
    request_context: Option<RequestContext<SqsResult>>,
    reply_plain: bool,
    request_kind: &'static str,
}

/// Result of [`SqsWorker::install`] or [`SqsWorker::install_local`].
pub struct InstalledSqsBridge<S: Shard + 'static> {
    /// Tina address callers use with `call(...)`.
    pub address: Address<SqsMsg, SqsResult>,
    /// Closer for Tina-side admission.
    pub closer: SqsCloser,
    /// Metrics handle.
    pub metrics: SqsMetricsHandle,
    _shard: PhantomData<S>,
}

/// Cloneable closer. Stops new Tina-side admission; already admitted
/// SDK work runs to completion or bridge timeout.
#[derive(Debug, Clone)]
pub struct SqsCloser {
    closed: Arc<AtomicBool>,
    metrics: Arc<SqsMetricsInner>,
}

impl SqsCloser {
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

    /// Close admission and block this host thread up to `timeout` for already
    /// accepted SDK work to leave the bridge's in-flight set.
    ///
    /// Do not call this from a Tina shard handler for this bridge; the shard
    /// must keep running to poll completions and release in-flight slots.
    pub fn close_and_drain(&self, timeout: Duration) -> SqsDrainReport {
        self.close();
        let result = crate::core::await_drain(
            &self.metrics.in_flight_current,
            || self.metrics.in_flight_kinds(),
            timeout,
        );
        SqsDrainReport {
            closed: true,
            drained: result.drained,
            in_flight_remaining: result.in_flight_remaining,
            in_flight_kinds: result.in_flight_kinds,
        }
    }
}

/// Report returned by [`SqsCloser::close_and_drain`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqsDrainReport {
    /// Admission was closed.
    pub closed: bool,
    /// All accepted bridge work left the in-flight set before the deadline.
    pub drained: bool,
    /// In-flight SDK work still tracked at the drain deadline.
    pub in_flight_remaining: u64,
    /// Operation kinds still in flight at the drain deadline.
    pub in_flight_kinds: Vec<(&'static str, u64)>,
}

/// SQS install/build failure.
#[derive(Debug)]
pub enum SqsInstallError {
    /// Invalid config.
    Config(SqsConfigError),
    /// Tokio runtime or AWS client construction failed.
    Build(SqsError),
    /// Tina runtime registration failed.
    Register(ThreadedRuntimeError),
}

impl std::fmt::Display for SqsInstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(e) => write!(f, "sqs bridge install: {e}"),
            Self::Build(e) => write!(f, "sqs bridge install: {e}"),
            Self::Register(e) => write!(f, "sqs bridge install: register: {e}"),
        }
    }
}

impl std::error::Error for SqsInstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(source) => Some(source),
            Self::Build(source) => Some(source),
            Self::Register(source) => Some(source),
        }
    }
}

impl From<SqsConfigError> for SqsInstallError {
    fn from(value: SqsConfigError) -> Self {
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

/// Bounded AWS SDK SQS worker isolate.
pub struct SqsWorker<S: Shard + 'static> {
    config: SqsConfig,
    client: Client,
    runtime: Handle,
    in_flight: HashMap<u64, SqsInFlight>,
    next_id: u64,
    closed: Arc<AtomicBool>,
    metrics: Arc<SqsMetricsInner>,
    _owned_runtime: Option<OwnedRuntime>,
    _shard: PhantomData<S>,
}

impl<S: Shard + 'static> Drop for SqsWorker<S> {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
    }
}

impl<S: Shard + 'static> SqsWorker<S> {
    /// Build a worker that owns its own Tokio runtime and SQS client.
    pub fn new(config: SqsConfig) -> Result<(Self, SqsMetricsHandle), SqsError> {
        config
            .validate()
            .map_err(|e| SqsError::InvalidRequest(format!("config: {e}")))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .thread_name("tina-aws-sqs-bridge")
            .build()
            .map_err(|e| SqsError::Internal(format!("tokio runtime build: {e}")))?;
        let handle = runtime.handle().clone();
        let client = build_sqs_client(&config);
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

    /// Build a worker around a caller-supplied SQS client and Tokio
    /// runtime handle.
    pub fn with_supplied_client(
        config: SqsConfig,
        client: Client,
        runtime: Handle,
    ) -> Result<(Self, SqsMetricsHandle), SqsConfigError> {
        config.validate_bridge_fields()?;
        Ok(Self::assemble(config, client, runtime, None, 0))
    }

    fn assemble(
        config: SqsConfig,
        client: Client,
        runtime: Handle,
        owned_runtime: Option<OwnedRuntime>,
        sdk_max_attempts: u64,
    ) -> (Self, SqsMetricsHandle) {
        let metrics = Arc::new(SqsMetricsInner::default());
        metrics
            .sdk_max_attempts
            .store(sdk_max_attempts, Ordering::Relaxed);
        let handle = SqsMetricsHandle {
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
    pub fn closer(&self) -> SqsCloser {
        SqsCloser {
            closed: Arc::clone(&self.closed),
            metrics: Arc::clone(&self.metrics),
        }
    }

    fn admit(
        &mut self,
        request: SqsRequest,
        request_context: Option<RequestContext<SqsResult>>,
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
            return Self::complete_request(request_context, Err(SqsError::Closed));
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
            return Self::complete_request(request_context, Err(SqsError::Full));
        }

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let reply_plain = request_context.is_none();
        let (tx, rx) = oneshot::channel();
        let client = self.client.clone();
        let message_body_limit = self.config.message_body_limit;
        let max_receive_messages = self.config.max_receive_messages;
        let abandoned = Arc::new(AtomicBool::new(false));
        let abandoned_for_task = Arc::clone(&abandoned);
        let metrics_for_task = Arc::clone(&self.metrics);
        self.runtime.spawn(async move {
            let result =
                run_request(client, request, message_body_limit, max_receive_messages).await;
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
            SqsInFlight {
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
        if !reply_plain {
            self.metrics.note_caller_waiting_admitted();
        }
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
        sleep(self.config.poll_interval).then(move |_| SqsMsg::Poll(id))
    }

    fn poll(&mut self, id: u64) -> Effect<Self> {
        let Some(mut in_flight) = self.in_flight.remove(&id) else {
            return noop();
        };
        match in_flight.receiver.try_recv() {
            Ok(result) => {
                if in_flight.abandoned.load(Ordering::Acquire) {
                    self.metrics.note_late_external_terminal();
                } else if in_flight.request_context.is_some() {
                    self.metrics.note_caller_waiting_terminal();
                }
                self.note_terminal(in_flight.request_kind);
                Self::complete_terminal(in_flight.request_context, in_flight.reply_plain, result)
            }
            Err(oneshot::error::TryRecvError::Empty) => {
                if in_flight.request_context.is_some()
                    && in_flight.started_at.elapsed() >= self.config.default_timeout
                {
                    in_flight.abandoned.store(true, Ordering::Release);
                    self.metrics.timeouts.fetch_add(1, Ordering::Relaxed);
                    self.metrics.note_caller_timed_out_but_external_running();
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
                        Self::complete_request(request_context, Err(SqsError::Timeout)),
                        sleep(self.config.poll_interval).then(move |_| SqsMsg::Poll(id)),
                    ]);
                }
                self.in_flight.insert(id, in_flight);
                sleep(self.config.poll_interval).then(move |_| SqsMsg::Poll(id))
            }
            Err(oneshot::error::TryRecvError::Closed) => {
                if in_flight.abandoned.load(Ordering::Acquire) {
                    self.metrics.note_late_external_terminal();
                } else if in_flight.request_context.is_some() {
                    self.metrics.note_caller_waiting_terminal();
                }
                self.note_terminal(in_flight.request_kind);
                Self::complete_terminal(
                    in_flight.request_context,
                    in_flight.reply_plain,
                    Err(SqsError::Internal("sdk task ended without result".into())),
                )
            }
        }
    }

    fn complete_request(
        request_context: Option<RequestContext<SqsResult>>,
        result: SqsResult,
    ) -> Effect<Self> {
        match request_context {
            Some(request) => reply_to(request, result),
            None => reply::<Self>(result),
        }
    }

    fn complete_terminal(
        request_context: Option<RequestContext<SqsResult>>,
        reply_plain: bool,
        result: SqsResult,
    ) -> Effect<Self> {
        match request_context {
            Some(request) => reply_to(request, result),
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

impl<S: Shard + Send + 'static> SqsWorker<S> {
    fn install_via(
        config: SqsConfig,
        register: impl FnOnce(Self, usize) -> Result<Address<SqsMsg, SqsResult>, ThreadedRuntimeError>,
    ) -> Result<InstalledSqsBridge<S>, SqsInstallError> {
        config.validate()?;
        let cap = config.mailbox_capacity;
        let (worker, metrics) = Self::new(config).map_err(SqsInstallError::Build)?;
        let closer = worker.closer();
        let address = register(worker, cap).map_err(SqsInstallError::Register)?;
        Ok(InstalledSqsBridge {
            address,
            closer,
            metrics,
            _shard: PhantomData,
        })
    }

    /// Validate config, build the worker, register it, and return the
    /// address, closer, and metrics handle.
    pub fn install<F>(
        runtime: &ThreadedRuntime<S, F>,
        config: SqsConfig,
    ) -> Result<InstalledSqsBridge<S>, SqsInstallError>
    where
        F: MailboxFactory + Send + 'static,
    {
        Self::install_via(config, |worker, cap| {
            runtime.register_with_capacity::<_, Infallible>(worker, cap)
        })
    }

    /// Local-system first form of [`Self::install`].
    pub fn install_local<F>(
        system: &LocalSystem<S, F>,
        config: SqsConfig,
    ) -> Result<InstalledSqsBridge<S>, SqsInstallError>
    where
        F: MailboxFactory + Send + 'static,
    {
        Self::install_via(config, |worker, cap| {
            system.register_root::<_, Infallible>(worker, cap)
        })
    }
}

/// Validate config, build the SQS worker, register it, and return the
/// address, closer, and metrics handle.
pub fn install_sqs<S, F>(
    runtime: &ThreadedRuntime<S, F>,
    config: SqsConfig,
) -> Result<InstalledSqsBridge<S>, SqsInstallError>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    SqsWorker::<S>::install(runtime, config)
}

/// Local-system first form of [`install_sqs`].
pub fn install_sqs_local<S, F>(
    system: &LocalSystem<S, F>,
    config: SqsConfig,
) -> Result<InstalledSqsBridge<S>, SqsInstallError>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    SqsWorker::<S>::install_local(system, config)
}

impl<S: Shard + 'static> Isolate for SqsWorker<S> {
    tina::isolate_types! {
        message: SqsMsg,
        reply: SqsResult,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        io: RuntimeCall<SqsMsg>,
        shard: S,
    }

    fn handle(&mut self, msg: SqsMsg, _ctx: &mut Context<'_, S, Self::Reply>) -> Effect<Self> {
        match msg {
            SqsMsg::Send(request) => self.admit(request, None),
            SqsMsg::Poll(id) => self.poll(id),
        }
    }

    fn handle_call(&mut self, msg: SqsMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            SqsMsg::Send(request) => self.admit(request, Some(call.into_request_context())),
            SqsMsg::Poll(_) => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

fn build_sqs_client(config: &SqsConfig) -> Client {
    let mut builder = aws_sdk_sqs::Config::builder()
        .behavior_version(BehaviorVersion::v2026_01_12())
        .region(Region::new(config.region.clone()))
        .retry_config(match config.retry_policy {
            SdkRetryPolicy::Disabled => RetryConfig::disabled(),
            SdkRetryPolicy::Standard { max_attempts } => {
                RetryConfig::standard().with_max_attempts(max_attempts)
            }
        });
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

fn validate_request(request: &SqsRequest, config: &SqsConfig) -> Result<(), SqsError> {
    match request {
        SqsRequest::SendMessage(send) => {
            validate_queue_url(&send.queue_url)?;
            if send.body.len() > config.message_body_limit {
                return Err(SqsError::MessageTooLarge);
            }
            Ok(())
        }
        SqsRequest::ReceiveMessage(receive) => {
            validate_queue_url(&receive.queue_url)?;
            if receive.max_messages == 0 || receive.max_messages > config.max_receive_messages {
                return Err(SqsError::InvalidRequest(format!(
                    "max_messages {} must be in 1..={}",
                    receive.max_messages, config.max_receive_messages
                )));
            }
            if !(0..=43_200).contains(&receive.visibility_timeout_seconds) {
                return Err(SqsError::InvalidRequest(
                    "visibility_timeout_seconds must be in 0..=43200".into(),
                ));
            }
            if !(0..=20).contains(&receive.wait_time_seconds) {
                return Err(SqsError::InvalidRequest(
                    "wait_time_seconds must be in 0..=20".into(),
                ));
            }
            Ok(())
        }
        SqsRequest::DeleteMessage(delete) => {
            validate_queue_url(&delete.queue_url)?;
            if delete.receipt_handle.is_empty() {
                return Err(SqsError::InvalidRequest(
                    "receipt_handle must not be empty".into(),
                ));
            }
            Ok(())
        }
    }
}

fn validate_queue_url(queue_url: &str) -> Result<(), SqsError> {
    if queue_url.trim().is_empty() {
        Err(SqsError::InvalidRequest(
            "queue_url must not be empty".into(),
        ))
    } else {
        Ok(())
    }
}

async fn run_request(
    client: Client,
    request: SqsRequest,
    body_limit: usize,
    max_receive_messages: usize,
) -> SqsResult {
    match request {
        SqsRequest::SendMessage(send) => {
            let mut req = client
                .send_message()
                .queue_url(send.queue_url)
                .message_body(send.body);
            if let Some(group_id) = send.message_group_id {
                req = req.message_group_id(group_id);
            }
            if let Some(dedup_id) = send.message_deduplication_id {
                req = req.message_deduplication_id(dedup_id);
            }
            req.send()
                .await
                .map(|out| {
                    SqsResponse::SentMessage(SqsSentMessage {
                        message_id: out.message_id,
                        md5_of_body: out.md5_of_message_body,
                        sequence_number: out.sequence_number,
                    })
                })
                .map_err(classify_send_error)
        }
        SqsRequest::ReceiveMessage(receive) => {
            let receive_cap = receive.max_messages.min(max_receive_messages);
            let out = client
                .receive_message()
                .queue_url(receive.queue_url)
                .max_number_of_messages(receive.max_messages as i32)
                .visibility_timeout(receive.visibility_timeout_seconds)
                .wait_time_seconds(receive.wait_time_seconds)
                .send()
                .await
                .map_err(classify_receive_error)?;
            let mut messages = Vec::new();
            let raw_messages = out.messages.unwrap_or_default();
            if raw_messages.len() > receive_cap {
                return Err(SqsError::ResponseTooLarge);
            }
            for msg in raw_messages {
                let body = msg.body.unwrap_or_default();
                if body.len() > body_limit {
                    return Err(SqsError::ResponseTooLarge);
                }
                let Some(receipt_handle) = msg.receipt_handle else {
                    return Err(SqsError::Sdk(
                        "received message without receipt handle".into(),
                    ));
                };
                messages.push(SqsMessage {
                    message_id: msg.message_id,
                    receipt_handle,
                    body,
                });
            }
            Ok(SqsResponse::ReceivedMessages(SqsReceivedMessages {
                messages,
            }))
        }
        SqsRequest::DeleteMessage(delete) => client
            .delete_message()
            .queue_url(delete.queue_url)
            .receipt_handle(delete.receipt_handle)
            .send()
            .await
            .map(|_| SqsResponse::DeletedMessage(SqsDeletedMessage))
            .map_err(classify_delete_error),
    }
}

fn classify_send_error(error: SdkError<SendMessageError>) -> SqsError {
    if let Some(service) = error.as_service_error() {
        if service.is_queue_does_not_exist() {
            return SqsError::QueueDoesNotExist(error_detail(&error));
        }
        if service.is_request_throttled() || service.is_kms_throttled() {
            return SqsError::Throttled(error_detail(&error));
        }
    }
    SqsError::Sdk(error_detail(&error))
}

fn classify_receive_error(error: SdkError<ReceiveMessageError>) -> SqsError {
    if let Some(service) = error.as_service_error() {
        if service.is_queue_does_not_exist() {
            return SqsError::QueueDoesNotExist(error_detail(&error));
        }
        if service.is_request_throttled() || service.is_kms_throttled() || service.is_over_limit() {
            return SqsError::Throttled(error_detail(&error));
        }
    }
    SqsError::Sdk(error_detail(&error))
}

fn classify_delete_error(error: SdkError<DeleteMessageError>) -> SqsError {
    if let Some(service) = error.as_service_error() {
        if service.is_queue_does_not_exist() {
            return SqsError::QueueDoesNotExist(error_detail(&error));
        }
        if service.is_request_throttled() {
            return SqsError::Throttled(error_detail(&error));
        }
    }
    SqsError::Sdk(error_detail(&error))
}

fn error_detail<E, R>(error: &SdkError<E, R>) -> String
where
    E: std::fmt::Debug + std::fmt::Display,
    R: std::fmt::Debug,
{
    if let Some(service) = error.as_service_error() {
        format!("{service}; {service:?}")
    } else {
        format!("{error}; {error:?}")
    }
}

fn tally_admission_error(metrics: &SqsMetricsInner, err: &SqsError) {
    // `validate_request` only produces `MessageTooLarge` or
    // `InvalidRequest`. Future validator additions land in `invalid`
    // until they earn a typed counter.
    match err {
        SqsError::MessageTooLarge => {
            metrics.message_too_large.fetch_add(1, Ordering::Relaxed);
        }
        _ => {
            metrics.invalid.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Maps an admission-class [`SqsError`] to the wire-stable tracing
/// `reason` label. Only the two variants `validate_request` can
/// produce are listed; other variants are not reachable on the
/// admission tracing path.
#[cfg(feature = "tracing")]
fn admission_reason(err: &SqsError) -> &'static str {
    match err {
        SqsError::MessageTooLarge => "MessageTooLarge",
        SqsError::InvalidRequest(_) => "InvalidRequest",
        // Unreachable: validate_request only emits MessageTooLarge or
        // InvalidRequest. Match arm is required for exhaustiveness;
        // the label keeps the tracing schema stable if the error
        // shape grows.
        _ => "Invalid",
    }
}

fn tally_terminal(metrics: &SqsMetricsInner, result: &SqsResult) {
    match result {
        Ok(_) => {
            metrics.responses.fetch_add(1, Ordering::Relaxed);
        }
        Err(SqsError::ResponseTooLarge) => {
            metrics.response_too_large.fetch_add(1, Ordering::Relaxed);
        }
        Err(SqsError::Sdk(_) | SqsError::QueueDoesNotExist(_) | SqsError::Throttled(_)) => {
            metrics.sdk_errors.fetch_add(1, Ordering::Relaxed);
        }
        Err(_) => {}
    }
}

#[cfg(test)]
mod install_tests {
    use super::*;
    use tina::SingleShard;

    #[test]
    fn rejected_install_drops_the_fully_built_worker() {
        for expected in [
            ThreadedRuntimeError::CommandFull,
            ThreadedRuntimeError::WorkerStopped,
        ] {
            let retained = Arc::new(std::sync::Mutex::new(None));
            let retained_for_register = Arc::clone(&retained);
            let result =
                SqsWorker::<SingleShard>::install_via(SqsConfig::default(), move |worker, _| {
                    *retained_for_register.lock().expect("retained closer") = Some(worker.closer());
                    Err(expected)
                });
            assert!(matches!(result, Err(SqsInstallError::Register(error)) if error == expected));
            let closer = retained
                .lock()
                .expect("retained closer")
                .take()
                .expect("closer captured before registration");
            assert!(closer.is_closed());
            assert_eq!(Arc::strong_count(&closer.closed), 1);
            assert_eq!(Arc::strong_count(&closer.metrics), 1);
        }
    }
}
