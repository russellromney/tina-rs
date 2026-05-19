//! Secrets Manager worker isolate around the AWS Rust SDK.

use std::collections::HashMap;
use std::convert::Infallible;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use aws_sdk_secretsmanager::Client;
use aws_sdk_secretsmanager::config::retry::RetryConfig;
use aws_sdk_secretsmanager::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_secretsmanager::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_secretsmanager::operation::get_secret_value::GetSecretValueError;
use tina::CallContext;
use tina::prelude::*;
use tina_runtime::{MailboxFactory, RuntimeCall, ThreadedRuntime, sleep};
use tokio::runtime::Handle;
use tokio::sync::oneshot;
#[cfg(feature = "tracing")]
use tracing::{Level, event};

use crate::secrets_metrics::{SecretsMetricsHandle, SecretsMetricsInner};
use crate::secrets_types::{
    SecretsConfig, SecretsConfigError, SecretsError, SecretsRequest, SecretsResponse, SecretsValue,
};
use crate::types::SdkRetryPolicy;

#[cfg(feature = "tracing")]
const TRACE_TARGET_CALL: &str = "tina_aws.bridge.call";
#[cfg(feature = "tracing")]
const TRACE_TARGET_BRIDGE: &str = "tina_aws.bridge";

/// Worker reply type.
pub type SecretsResult = Result<SecretsResponse, SecretsError>;

/// Messages handled by [`SecretsWorker`].
#[derive(Debug)]
pub enum SecretsMsg {
    /// Submit one Secrets Manager operation.
    Send(SecretsRequest),
    /// Internal sleep wakeup.
    #[doc(hidden)]
    Poll(u64),
}

struct SecretsInFlight {
    started_at: Instant,
    receiver: oneshot::Receiver<SecretsResult>,
    abandoned: Arc<AtomicBool>,
    request_context: Option<RequestContext<SecretsResult>>,
    reply_plain: bool,
    request_kind: &'static str,
}

/// Result of [`SecretsWorker::install`].
pub struct InstalledSecretsBridge<S: Shard + 'static> {
    /// Tina address callers use with `call(...)`.
    pub address: Address<SecretsMsg, SecretsResult>,
    /// Closer for Tina-side admission.
    pub closer: SecretsCloser,
    /// Metrics handle.
    pub metrics: SecretsMetricsHandle,
    _shard: PhantomData<S>,
}

/// Cloneable closer.
#[derive(Debug, Clone)]
pub struct SecretsCloser {
    closed: Arc<AtomicBool>,
    metrics: Arc<SecretsMetricsInner>,
}

impl SecretsCloser {
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
    pub fn close_and_drain(&self, timeout: Duration) -> SecretsDrainReport {
        self.close();
        let result = crate::core::await_drain(
            &self.metrics.in_flight_current,
            || self.metrics.in_flight_kinds(),
            timeout,
        );
        SecretsDrainReport {
            closed: true,
            drained: result.drained,
            in_flight_remaining: result.in_flight_remaining,
            in_flight_kinds: result.in_flight_kinds,
        }
    }
}

/// Report returned by [`SecretsCloser::close_and_drain`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretsDrainReport {
    /// Admission was closed.
    pub closed: bool,
    /// All accepted bridge work left the in-flight set before the deadline.
    pub drained: bool,
    /// In-flight SDK work still tracked at the drain deadline.
    pub in_flight_remaining: u64,
    /// Operation kinds still in flight at the drain deadline.
    pub in_flight_kinds: Vec<(&'static str, u64)>,
}

/// Secrets Manager install/build failure.
#[derive(Debug)]
pub enum SecretsInstallError {
    /// Invalid config.
    Config(SecretsConfigError),
    /// Tokio runtime or AWS client construction failed.
    Build(String),
    /// Tina runtime registration failed.
    Register(String),
}

impl std::fmt::Display for SecretsInstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(e) => write!(f, "secrets bridge install: {e}"),
            Self::Build(e) => write!(f, "secrets bridge install: {e}"),
            Self::Register(e) => write!(f, "secrets bridge install: register: {e}"),
        }
    }
}

impl std::error::Error for SecretsInstallError {}

impl From<SecretsConfigError> for SecretsInstallError {
    fn from(value: SecretsConfigError) -> Self {
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

/// Bounded Secrets Manager worker isolate.
pub struct SecretsWorker<S: Shard + 'static> {
    config: SecretsConfig,
    client: Client,
    runtime: Handle,
    in_flight: HashMap<u64, SecretsInFlight>,
    next_id: u64,
    closed: Arc<AtomicBool>,
    metrics: Arc<SecretsMetricsInner>,
    _owned_runtime: Option<OwnedRuntime>,
    _shard: PhantomData<S>,
}

impl<S: Shard + 'static> SecretsWorker<S> {
    /// Build a worker that owns its own Tokio runtime and Secrets
    /// Manager client.
    pub fn new(config: SecretsConfig) -> Result<(Self, SecretsMetricsHandle), SecretsError> {
        config
            .validate()
            .map_err(|e| SecretsError::InvalidRequest(format!("config: {e}")))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .thread_name("tina-aws-secrets-bridge")
            .build()
            .map_err(|e| SecretsError::Internal(format!("tokio runtime build: {e}")))?;
        let handle = runtime.handle().clone();
        let client = build_secrets_client(&config);
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

    /// Build a worker around a caller-supplied client and Tokio runtime
    /// handle.
    pub fn with_supplied_client(
        config: SecretsConfig,
        client: Client,
        runtime: Handle,
    ) -> Result<(Self, SecretsMetricsHandle), SecretsConfigError> {
        config.validate_bridge_fields()?;
        Ok(Self::assemble(config, client, runtime, None, 0))
    }

    fn assemble(
        config: SecretsConfig,
        client: Client,
        runtime: Handle,
        owned_runtime: Option<OwnedRuntime>,
        sdk_max_attempts: u64,
    ) -> (Self, SecretsMetricsHandle) {
        let metrics = Arc::new(SecretsMetricsInner::default());
        metrics
            .sdk_max_attempts
            .store(sdk_max_attempts, Ordering::Relaxed);
        let handle = SecretsMetricsHandle {
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
    pub fn closer(&self) -> SecretsCloser {
        SecretsCloser {
            closed: Arc::clone(&self.closed),
            metrics: Arc::clone(&self.metrics),
        }
    }

    fn admit(
        &mut self,
        request: SecretsRequest,
        request_context: Option<RequestContext<SecretsResult>>,
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
            return Self::complete_request(request_context, Err(SecretsError::Closed));
        }
        if let Err(err) = validate_request(&request) {
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
            return Self::complete_request(request_context, Err(SecretsError::Full));
        }

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let reply_plain = request_context.is_none();
        let (tx, rx) = oneshot::channel();
        let client = self.client.clone();
        let secret_size_limit = self.config.secret_size_limit;
        let abandoned = Arc::new(AtomicBool::new(false));
        let abandoned_for_task = Arc::clone(&abandoned);
        let metrics_for_task = Arc::clone(&self.metrics);
        self.runtime.spawn(async move {
            let result = run_request(client, request, secret_size_limit).await;
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
            SecretsInFlight {
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
        sleep(self.config.poll_interval).then(move |_| SecretsMsg::Poll(id))
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
                        Self::complete_request(request_context, Err(SecretsError::Timeout)),
                        sleep(self.config.poll_interval).then(move |_| SecretsMsg::Poll(id)),
                    ]);
                }
                self.in_flight.insert(id, in_flight);
                sleep(self.config.poll_interval).then(move |_| SecretsMsg::Poll(id))
            }
            Err(oneshot::error::TryRecvError::Closed) => {
                self.note_terminal(in_flight.request_kind);
                Self::complete_terminal(
                    in_flight.request_context,
                    in_flight.reply_plain,
                    Err(SecretsError::Internal(
                        "sdk task ended without result".into(),
                    )),
                )
            }
        }
    }

    fn complete_request(
        request_context: Option<RequestContext<SecretsResult>>,
        result: SecretsResult,
    ) -> Effect<Self> {
        match request_context {
            Some(request) => reply_to_request(request, result),
            None => reply::<Self>(result),
        }
    }

    fn complete_terminal(
        request_context: Option<RequestContext<SecretsResult>>,
        reply_plain: bool,
        result: SecretsResult,
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

impl<S: Shard + Send + 'static> SecretsWorker<S> {
    /// Validate config, build the worker, register it, and return the
    /// address, closer, and metrics handle.
    pub fn install<F>(
        runtime: &ThreadedRuntime<S, F>,
        config: SecretsConfig,
    ) -> Result<InstalledSecretsBridge<S>, SecretsInstallError>
    where
        F: MailboxFactory + Send + 'static,
    {
        config.validate()?;
        let cap = config.mailbox_capacity;
        let (worker, metrics) =
            Self::new(config).map_err(|e| SecretsInstallError::Build(e.to_string()))?;
        let closer = worker.closer();
        let address = runtime
            .register_with_capacity::<_, Infallible>(worker, cap)
            .map_err(|e| SecretsInstallError::Register(format!("{e:?}")))?;
        Ok(InstalledSecretsBridge {
            address,
            closer,
            metrics,
            _shard: PhantomData,
        })
    }
}

/// Validate config, build the Secrets Manager worker, register it,
/// and return the address, closer, and metrics handle.
pub fn install_secrets<S, F>(
    runtime: &ThreadedRuntime<S, F>,
    config: SecretsConfig,
) -> Result<InstalledSecretsBridge<S>, SecretsInstallError>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    SecretsWorker::<S>::install(runtime, config)
}

impl<S: Shard + 'static> Isolate for SecretsWorker<S> {
    tina::isolate_types! {
        message: SecretsMsg,
        reply: SecretsResult,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<SecretsMsg>,
        shard: S,
    }

    fn handle(&mut self, msg: SecretsMsg, _ctx: &mut Context<'_, S, Self::Reply>) -> Effect<Self> {
        match msg {
            SecretsMsg::Send(request) => self.admit(request, None),
            SecretsMsg::Poll(id) => self.poll(id),
        }
    }

    fn handle_call(&mut self, msg: SecretsMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            SecretsMsg::Send(request) => self.admit(request, Some(call.into_request_context())),
            SecretsMsg::Poll(_) => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

fn build_secrets_client(config: &SecretsConfig) -> Client {
    let mut builder = aws_sdk_secretsmanager::Config::builder()
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

fn validate_request(request: &SecretsRequest) -> Result<(), SecretsError> {
    match request {
        SecretsRequest::GetSecretValue(get) => {
            if get.secret_id.trim().is_empty() {
                return Err(SecretsError::InvalidRequest(
                    "secret_id must not be empty".into(),
                ));
            }
            if get.version_id.is_some() && get.version_stage.is_some() {
                return Err(SecretsError::InvalidRequest(
                    "version_id and version_stage are mutually exclusive".into(),
                ));
            }
            Ok(())
        }
    }
}

async fn run_request(
    client: Client,
    request: SecretsRequest,
    secret_size_limit: usize,
) -> SecretsResult {
    match request {
        SecretsRequest::GetSecretValue(get) => {
            let mut req = client.get_secret_value().secret_id(get.secret_id);
            if let Some(v) = get.version_id {
                req = req.version_id(v);
            }
            if let Some(s) = get.version_stage {
                req = req.version_stage(s);
            }
            match req.send().await {
                Ok(out) => {
                    let secret_string = out.secret_string;
                    let secret_binary = out.secret_binary.map(|b| b.into_inner());

                    let size = secret_string.as_ref().map(|s| s.len()).unwrap_or(0)
                        + secret_binary.as_ref().map(|b| b.len()).unwrap_or(0);
                    if size > secret_size_limit {
                        return Err(SecretsError::SecretTooLarge);
                    }

                    Ok(SecretsResponse::SecretValue(SecretsValue {
                        arn: out.arn,
                        name: out.name,
                        version_id: out.version_id,
                        version_stages: out.version_stages.unwrap_or_default(),
                        secret_string,
                        secret_binary,
                    }))
                }
                Err(e) => Err(classify_get_error(e)),
            }
        }
    }
}

fn classify_get_error(error: SdkError<GetSecretValueError>) -> SecretsError {
    if let Some(service) = error.as_service_error() {
        if service.is_resource_not_found_exception() {
            return SecretsError::NotFound(error_detail(&error));
        }
        if service.is_invalid_request_exception() || service.is_invalid_parameter_exception() {
            return SecretsError::InvalidParameter(error_detail(&error));
        }
        if service.is_decryption_failure() {
            return SecretsError::DecryptionFailed(error_detail(&error));
        }
        // Secrets Manager folds access-denied and throttling into the
        // unmodeled `GetSecretValueError::Unhandled` variant; their
        // typed error code rides on the `ProvideErrorMetadata::code()`
        // string ("AccessDeniedException", "ThrottlingException", or
        // "TooManyRequestsException"). AWS sends throttling as HTTP
        // 400 with that error code, so HTTP-status sniffing alone
        // would miss it.
        if let Some(code) = service.code() {
            match code {
                "AccessDeniedException" => {
                    return SecretsError::AccessDenied(error_detail(&error));
                }
                "ThrottlingException" | "TooManyRequestsException" => {
                    return SecretsError::Throttled(error_detail(&error));
                }
                _ => {}
            }
        }
    }
    SecretsError::Sdk(error_detail(&error))
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

fn tally_admission_error(metrics: &SecretsMetricsInner, _err: &SecretsError) {
    // `validate_request` for the Secrets bridge only produces
    // `InvalidRequest` today. Future validator additions land in this
    // bucket until they earn a typed counter.
    metrics.invalid.fetch_add(1, Ordering::Relaxed);
}

#[cfg(feature = "tracing")]
fn admission_reason(err: &SecretsError) -> &'static str {
    match err {
        SecretsError::InvalidRequest(_) => "InvalidRequest",
        _ => "Invalid",
    }
}

fn tally_terminal(metrics: &SecretsMetricsInner, result: &SecretsResult) {
    match result {
        Ok(_) => {
            metrics.responses.fetch_add(1, Ordering::Relaxed);
        }
        Err(SecretsError::SecretTooLarge) => {
            metrics.secret_too_large.fetch_add(1, Ordering::Relaxed);
        }
        Err(SecretsError::NotFound(_)) => {
            metrics.not_found.fetch_add(1, Ordering::Relaxed);
        }
        Err(SecretsError::AccessDenied(_)) => {
            metrics.access_denied.fetch_add(1, Ordering::Relaxed);
        }
        Err(SecretsError::InvalidParameter(_)) => {
            metrics.invalid_parameter.fetch_add(1, Ordering::Relaxed);
        }
        Err(SecretsError::Throttled(_)) => {
            metrics.throttled.fetch_add(1, Ordering::Relaxed);
        }
        Err(SecretsError::DecryptionFailed(_)) => {
            metrics.decryption_failed.fetch_add(1, Ordering::Relaxed);
        }
        Err(SecretsError::Sdk(_)) => {
            metrics.sdk_errors.fetch_add(1, Ordering::Relaxed);
        }
        Err(_) => {}
    }
}
