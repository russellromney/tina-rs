//! SNS worker isolate around the AWS Rust SDK.

use std::collections::HashMap;
use std::convert::Infallible;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use aws_sdk_sns::Client;
use aws_sdk_sns::config::retry::RetryConfig;
use aws_sdk_sns::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_sns::error::SdkError;
use aws_sdk_sns::operation::publish::PublishError;
use aws_sdk_sns::primitives::Blob;
use aws_sdk_sns::types::MessageAttributeValue;
use tina::CallContext;
use tina::prelude::*;
use tina_runtime::{MailboxFactory, RuntimeCall, ThreadedRuntime, sleep};
use tokio::runtime::Handle;
use tokio::sync::oneshot;
#[cfg(feature = "tracing")]
use tracing::{Level, event};

use crate::sns_metrics::{SnsMetricsHandle, SnsMetricsInner};
use crate::sns_types::{
    SnsAttributeValue, SnsConfig, SnsConfigError, SnsDestination, SnsError, SnsPublished,
    SnsRequest, SnsResponse,
};
use crate::types::SdkRetryPolicy;

#[cfg(feature = "tracing")]
const TRACE_TARGET_CALL: &str = "tina_aws.bridge.call";
#[cfg(feature = "tracing")]
const TRACE_TARGET_BRIDGE: &str = "tina_aws.bridge";

/// Worker reply type.
pub type SnsResult = Result<SnsResponse, SnsError>;

/// Messages handled by [`SnsWorker`].
#[derive(Debug)]
pub enum SnsMsg {
    /// Submit one SNS operation.
    Send(SnsRequest),
    /// Internal sleep wakeup.
    #[doc(hidden)]
    Poll(u64),
}

struct SnsInFlight {
    started_at: Instant,
    receiver: oneshot::Receiver<SnsResult>,
    abandoned: Arc<AtomicBool>,
    request_context: Option<RequestContext<SnsResult>>,
    reply_plain: bool,
    request_kind: &'static str,
}

/// Result of [`SnsWorker::install`].
pub struct InstalledSnsBridge<S: Shard + 'static> {
    /// Tina address callers use with `call(...)`.
    pub address: Address<SnsMsg, SnsResult>,
    /// Closer for Tina-side admission.
    pub closer: SnsCloser,
    /// Metrics handle.
    pub metrics: SnsMetricsHandle,
    _shard: PhantomData<S>,
}

/// Cloneable closer.
#[derive(Debug, Clone)]
pub struct SnsCloser {
    closed: Arc<AtomicBool>,
    metrics: Arc<SnsMetricsInner>,
}

impl SnsCloser {
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
    pub fn close_and_drain(&self, timeout: Duration) -> SnsDrainReport {
        self.close();
        let result = crate::core::await_drain(
            &self.metrics.in_flight_current,
            || self.metrics.in_flight_kinds(),
            timeout,
        );
        SnsDrainReport {
            closed: true,
            drained: result.drained,
            in_flight_remaining: result.in_flight_remaining,
            in_flight_kinds: result.in_flight_kinds,
        }
    }
}

/// Report returned by [`SnsCloser::close_and_drain`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnsDrainReport {
    /// Admission was closed.
    pub closed: bool,
    /// All accepted bridge work left the in-flight set before the deadline.
    pub drained: bool,
    /// In-flight SDK work still tracked at the drain deadline.
    pub in_flight_remaining: u64,
    /// Operation kinds still in flight at the drain deadline.
    pub in_flight_kinds: Vec<(&'static str, u64)>,
}

/// SNS install/build failure.
#[derive(Debug)]
pub enum SnsInstallError {
    /// Invalid config.
    Config(SnsConfigError),
    /// Tokio runtime or AWS client construction failed.
    Build(String),
    /// Tina runtime registration failed.
    Register(String),
}

impl std::fmt::Display for SnsInstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(e) => write!(f, "sns bridge install: {e}"),
            Self::Build(e) => write!(f, "sns bridge install: {e}"),
            Self::Register(e) => write!(f, "sns bridge install: register: {e}"),
        }
    }
}

impl std::error::Error for SnsInstallError {}

impl From<SnsConfigError> for SnsInstallError {
    fn from(value: SnsConfigError) -> Self {
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

/// Bounded SNS worker isolate.
pub struct SnsWorker<S: Shard + 'static> {
    config: SnsConfig,
    client: Client,
    runtime: Handle,
    in_flight: HashMap<u64, SnsInFlight>,
    next_id: u64,
    closed: Arc<AtomicBool>,
    metrics: Arc<SnsMetricsInner>,
    _owned_runtime: Option<OwnedRuntime>,
    _shard: PhantomData<S>,
}

impl<S: Shard + 'static> SnsWorker<S> {
    /// Build a worker that owns its own Tokio runtime and SNS client.
    pub fn new(config: SnsConfig) -> Result<(Self, SnsMetricsHandle), SnsError> {
        config
            .validate()
            .map_err(|e| SnsError::InvalidRequest(format!("config: {e}")))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .thread_name("tina-aws-sns-bridge")
            .build()
            .map_err(|e| SnsError::Internal(format!("tokio runtime build: {e}")))?;
        let handle = runtime.handle().clone();
        let client = build_sns_client(&config);
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

    /// Build a worker around a caller-supplied SNS client and Tokio
    /// runtime handle.
    pub fn with_supplied_client(
        config: SnsConfig,
        client: Client,
        runtime: Handle,
    ) -> Result<(Self, SnsMetricsHandle), SnsConfigError> {
        config.validate_bridge_fields()?;
        Ok(Self::assemble(config, client, runtime, None, 0))
    }

    fn assemble(
        config: SnsConfig,
        client: Client,
        runtime: Handle,
        owned_runtime: Option<OwnedRuntime>,
        sdk_max_attempts: u64,
    ) -> (Self, SnsMetricsHandle) {
        let metrics = Arc::new(SnsMetricsInner::default());
        metrics
            .sdk_max_attempts
            .store(sdk_max_attempts, Ordering::Relaxed);
        let handle = SnsMetricsHandle {
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
    pub fn closer(&self) -> SnsCloser {
        SnsCloser {
            closed: Arc::clone(&self.closed),
            metrics: Arc::clone(&self.metrics),
        }
    }

    fn admit(
        &mut self,
        request: SnsRequest,
        request_context: Option<RequestContext<SnsResult>>,
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
            return Self::complete_request(request_context, Err(SnsError::Closed));
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
            return Self::complete_request(request_context, Err(SnsError::Full));
        }

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let reply_plain = request_context.is_none();
        let (tx, rx) = oneshot::channel();
        let client = self.client.clone();
        let abandoned = Arc::new(AtomicBool::new(false));
        let abandoned_for_task = Arc::clone(&abandoned);
        let metrics_for_task = Arc::clone(&self.metrics);
        self.runtime.spawn(async move {
            let result = run_request(client, request).await;
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
            SnsInFlight {
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
        sleep(self.config.poll_interval).then(move |_| SnsMsg::Poll(id))
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
                        Self::complete_request(request_context, Err(SnsError::Timeout)),
                        sleep(self.config.poll_interval).then(move |_| SnsMsg::Poll(id)),
                    ]);
                }
                self.in_flight.insert(id, in_flight);
                sleep(self.config.poll_interval).then(move |_| SnsMsg::Poll(id))
            }
            Err(oneshot::error::TryRecvError::Closed) => {
                self.note_terminal(in_flight.request_kind);
                Self::complete_terminal(
                    in_flight.request_context,
                    in_flight.reply_plain,
                    Err(SnsError::Internal("sdk task ended without result".into())),
                )
            }
        }
    }

    fn complete_request(
        request_context: Option<RequestContext<SnsResult>>,
        result: SnsResult,
    ) -> Effect<Self> {
        match request_context {
            Some(request) => reply_to_request(request, result),
            None => reply::<Self>(result),
        }
    }

    fn complete_terminal(
        request_context: Option<RequestContext<SnsResult>>,
        reply_plain: bool,
        result: SnsResult,
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

impl<S: Shard + Send + 'static> SnsWorker<S> {
    /// Validate config, build the worker, register it, and return the
    /// address, closer, and metrics handle.
    pub fn install<F>(
        runtime: &ThreadedRuntime<S, F>,
        config: SnsConfig,
    ) -> Result<InstalledSnsBridge<S>, SnsInstallError>
    where
        F: MailboxFactory + Send + 'static,
    {
        config.validate()?;
        let cap = config.mailbox_capacity;
        let (worker, metrics) =
            Self::new(config).map_err(|e| SnsInstallError::Build(e.to_string()))?;
        let closer = worker.closer();
        let address = runtime
            .register_with_capacity::<_, Infallible>(worker, cap)
            .map_err(|e| SnsInstallError::Register(format!("{e:?}")))?;
        Ok(InstalledSnsBridge {
            address,
            closer,
            metrics,
            _shard: PhantomData,
        })
    }
}

/// Validate config, build the SNS worker, register it, and return the
/// address, closer, and metrics handle.
pub fn install_sns<S, F>(
    runtime: &ThreadedRuntime<S, F>,
    config: SnsConfig,
) -> Result<InstalledSnsBridge<S>, SnsInstallError>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    SnsWorker::<S>::install(runtime, config)
}

impl<S: Shard + 'static> Isolate for SnsWorker<S> {
    tina::isolate_types! {
        message: SnsMsg,
        reply: SnsResult,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<SnsMsg>,
        shard: S,
    }

    fn handle(&mut self, msg: SnsMsg, _ctx: &mut Context<'_, S, Self::Reply>) -> Effect<Self> {
        match msg {
            SnsMsg::Send(request) => self.admit(request, None),
            SnsMsg::Poll(id) => self.poll(id),
        }
    }

    fn handle_call(&mut self, msg: SnsMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            SnsMsg::Send(request) => self.admit(request, Some(call.into_request_context())),
            SnsMsg::Poll(_) => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

fn build_sns_client(config: &SnsConfig) -> Client {
    let mut builder = aws_sdk_sns::Config::builder()
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

fn validate_request(request: &SnsRequest, config: &SnsConfig) -> Result<(), SnsError> {
    match request {
        SnsRequest::Publish(pub_) => {
            match &pub_.destination {
                SnsDestination::TopicArn(s)
                | SnsDestination::TargetArn(s)
                | SnsDestination::PhoneNumber(s) => {
                    if s.trim().is_empty() {
                        return Err(SnsError::InvalidRequest(
                            "destination must not be empty".into(),
                        ));
                    }
                }
            }
            if pub_.message.is_empty() {
                return Err(SnsError::InvalidRequest("message must not be empty".into()));
            }
            if pub_.message.len() > config.message_body_limit {
                return Err(SnsError::MessageTooLarge);
            }
            let attr_size: usize = pub_
                .attributes
                .iter()
                .map(|(k, v)| {
                    k.len()
                        + match v {
                            SnsAttributeValue::String(s) => s.len(),
                            SnsAttributeValue::Binary(b) => b.len(),
                        }
                })
                .sum();
            if attr_size > config.attribute_body_limit {
                return Err(SnsError::AttributesTooLarge);
            }
            Ok(())
        }
    }
}

fn convert_attributes(
    attrs: HashMap<String, SnsAttributeValue>,
) -> HashMap<String, MessageAttributeValue> {
    let mut out = HashMap::with_capacity(attrs.len());
    for (k, v) in attrs {
        let mav = match v {
            SnsAttributeValue::String(s) => MessageAttributeValue::builder()
                .data_type("String")
                .string_value(s)
                .build(),
            SnsAttributeValue::Binary(b) => MessageAttributeValue::builder()
                .data_type("Binary")
                .binary_value(Blob::new(b))
                .build(),
        };
        if let Ok(mav) = mav {
            out.insert(k, mav);
        }
    }
    out
}

async fn run_request(client: Client, request: SnsRequest) -> SnsResult {
    match request {
        SnsRequest::Publish(pub_) => {
            let mut req = client.publish().message(pub_.message);
            req = match pub_.destination {
                SnsDestination::TopicArn(s) => req.topic_arn(s),
                SnsDestination::TargetArn(s) => req.target_arn(s),
                SnsDestination::PhoneNumber(s) => req.phone_number(s),
            };
            if let Some(subject) = pub_.subject {
                req = req.subject(subject);
            }
            if let Some(group) = pub_.message_group_id {
                req = req.message_group_id(group);
            }
            if let Some(dedup) = pub_.message_deduplication_id {
                req = req.message_deduplication_id(dedup);
            }
            if !pub_.attributes.is_empty() {
                req = req.set_message_attributes(Some(convert_attributes(pub_.attributes)));
            }
            match req.send().await {
                Ok(out) => Ok(SnsResponse::Published(SnsPublished {
                    message_id: out.message_id,
                    sequence_number: out.sequence_number,
                })),
                Err(e) => Err(classify_publish_error(e)),
            }
        }
    }
}

fn classify_publish_error(error: SdkError<PublishError>) -> SnsError {
    if let Some(service) = error.as_service_error() {
        if service.is_not_found_exception() || service.is_kms_not_found_exception() {
            return SnsError::NotFound(error_detail(&error));
        }
        if service.is_authorization_error_exception() || service.is_kms_access_denied_exception() {
            return SnsError::AccessDenied(error_detail(&error));
        }
        if service.is_kms_throttling_exception() {
            return SnsError::Throttled(error_detail(&error));
        }
        if service.is_invalid_parameter_exception()
            || service.is_invalid_parameter_value_exception()
            || service.is_endpoint_disabled_exception()
            || service.is_invalid_security_exception()
            || service.is_validation_exception()
            || service.is_platform_application_disabled_exception()
            || service.is_kms_disabled_exception()
            || service.is_kms_invalid_state_exception()
            || service.is_kms_opt_in_required()
        {
            return SnsError::InvalidParameter(error_detail(&error));
        }
        if service.is_internal_error_exception() {
            return SnsError::Sdk(error_detail(&error));
        }
    }
    SnsError::Sdk(error_detail(&error))
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

fn tally_admission_error(metrics: &SnsMetricsInner, err: &SnsError) {
    // `validate_request` only produces `MessageTooLarge`,
    // `AttributesTooLarge`, or `InvalidRequest`. Future validator
    // additions land in `invalid` until they earn a typed counter.
    match err {
        SnsError::MessageTooLarge => {
            metrics.message_too_large.fetch_add(1, Ordering::Relaxed);
        }
        SnsError::AttributesTooLarge => {
            metrics.attributes_too_large.fetch_add(1, Ordering::Relaxed);
        }
        _ => {
            metrics.invalid.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(feature = "tracing")]
fn admission_reason(err: &SnsError) -> &'static str {
    match err {
        SnsError::MessageTooLarge => "MessageTooLarge",
        SnsError::AttributesTooLarge => "AttributesTooLarge",
        SnsError::InvalidRequest(_) => "InvalidRequest",
        _ => "Invalid",
    }
}

fn tally_terminal(metrics: &SnsMetricsInner, result: &SnsResult) {
    match result {
        Ok(_) => {
            metrics.responses.fetch_add(1, Ordering::Relaxed);
        }
        Err(SnsError::InvalidParameter(_)) => {
            metrics.invalid_parameter.fetch_add(1, Ordering::Relaxed);
        }
        Err(SnsError::NotFound(_)) => {
            metrics.not_found.fetch_add(1, Ordering::Relaxed);
        }
        Err(SnsError::AccessDenied(_)) => {
            metrics.access_denied.fetch_add(1, Ordering::Relaxed);
        }
        Err(SnsError::Throttled(_)) => {
            metrics.throttled.fetch_add(1, Ordering::Relaxed);
        }
        Err(SnsError::Sdk(_)) => {
            metrics.sdk_errors.fetch_add(1, Ordering::Relaxed);
        }
        Err(_) => {}
    }
}
