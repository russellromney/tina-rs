//! DynamoDB worker isolate around the AWS Rust SDK.

use std::collections::HashMap;
use std::convert::Infallible;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::config::retry::RetryConfig;
use aws_sdk_dynamodb::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_dynamodb::error::SdkError;
use aws_sdk_dynamodb::operation::delete_item::DeleteItemError;
use aws_sdk_dynamodb::operation::get_item::GetItemError;
use aws_sdk_dynamodb::operation::put_item::PutItemError;
use aws_sdk_dynamodb::operation::query::QueryError;
use aws_sdk_dynamodb::operation::update_item::UpdateItemError;
use aws_sdk_dynamodb::primitives::Blob;
use aws_sdk_dynamodb::types::{AttributeValue, ConsumedCapacity, ReturnConsumedCapacity, Select};
use tina::CallContext;
use tina::prelude::*;
use tina_runtime::{MailboxFactory, RuntimeCall, ThreadedRuntime, sleep};
use tokio::runtime::Handle;
use tokio::sync::oneshot;
#[cfg(feature = "tracing")]
use tracing::{Level, event};

use crate::dynamodb_metrics::{DynamoMetricsHandle, DynamoMetricsInner};
use crate::dynamodb_types::{
    DynamoConfig, DynamoConfigError, DynamoConsistency, DynamoConsumedCapacity, DynamoDeletedItem,
    DynamoError, DynamoGotItem, DynamoItem, DynamoPutItemOk, DynamoQueryResult, DynamoRequest,
    DynamoResponse, DynamoUpdatedItem, DynamoValue, ReturnCapacity,
};
use crate::types::SdkRetryPolicy;

#[cfg(feature = "tracing")]
const TRACE_TARGET_CALL: &str = "tina_aws.bridge.call";
#[cfg(feature = "tracing")]
const TRACE_TARGET_BRIDGE: &str = "tina_aws.bridge";

/// Worker reply type.
pub type DynamoResult = Result<DynamoResponse, DynamoError>;

/// Messages handled by [`DynamoWorker`].
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum DynamoMsg {
    /// Submit one DynamoDB operation.
    Send(DynamoRequest),
    /// Internal sleep wakeup.
    #[doc(hidden)]
    Poll(u64),
}

struct DynamoInFlight {
    started_at: Instant,
    receiver: oneshot::Receiver<DynamoResult>,
    abandoned: Arc<AtomicBool>,
    request_context: Option<RequestContext<DynamoResult>>,
    reply_plain: bool,
    request_kind: &'static str,
}

/// Result of [`DynamoWorker::install`].
pub struct InstalledDynamoBridge<S: Shard + 'static> {
    /// Tina address callers use with `call(...)`.
    pub address: Address<DynamoMsg, DynamoResult>,
    /// Closer for Tina-side admission.
    pub closer: DynamoCloser,
    /// Metrics handle.
    pub metrics: DynamoMetricsHandle,
    _shard: PhantomData<S>,
}

/// Cloneable closer. Stops new Tina-side admission; already admitted
/// SDK work runs to completion or bridge timeout.
#[derive(Debug, Clone)]
pub struct DynamoCloser {
    closed: Arc<AtomicBool>,
    metrics: Arc<DynamoMetricsInner>,
}

impl DynamoCloser {
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
    pub fn close_and_drain(&self, timeout: Duration) -> DynamoDrainReport {
        self.close();
        let result = crate::core::await_drain(
            &self.metrics.in_flight_current,
            || self.metrics.in_flight_kinds(),
            timeout,
        );
        DynamoDrainReport {
            closed: true,
            drained: result.drained,
            in_flight_remaining: result.in_flight_remaining,
            in_flight_kinds: result.in_flight_kinds,
        }
    }
}

/// Report returned by [`DynamoCloser::close_and_drain`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamoDrainReport {
    /// Admission was closed.
    pub closed: bool,
    /// All accepted bridge work left the in-flight set before the deadline.
    pub drained: bool,
    /// In-flight SDK work still tracked at the drain deadline.
    pub in_flight_remaining: u64,
    /// Operation kinds still in flight at the drain deadline.
    pub in_flight_kinds: Vec<(&'static str, u64)>,
}

/// DynamoDB install/build failure.
#[derive(Debug)]
pub enum DynamoInstallError {
    /// Invalid config.
    Config(DynamoConfigError),
    /// Tokio runtime or AWS client construction failed.
    Build(String),
    /// Tina runtime registration failed.
    Register(String),
}

impl std::fmt::Display for DynamoInstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(e) => write!(f, "dynamodb bridge install: {e}"),
            Self::Build(e) => write!(f, "dynamodb bridge install: {e}"),
            Self::Register(e) => write!(f, "dynamodb bridge install: register: {e}"),
        }
    }
}

impl std::error::Error for DynamoInstallError {}

impl From<DynamoConfigError> for DynamoInstallError {
    fn from(value: DynamoConfigError) -> Self {
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

/// Bounded DynamoDB worker isolate.
pub struct DynamoWorker<S: Shard + 'static> {
    config: DynamoConfig,
    client: Client,
    runtime: Handle,
    in_flight: HashMap<u64, DynamoInFlight>,
    next_id: u64,
    closed: Arc<AtomicBool>,
    metrics: Arc<DynamoMetricsInner>,
    _owned_runtime: Option<OwnedRuntime>,
    _shard: PhantomData<S>,
}

impl<S: Shard + 'static> DynamoWorker<S> {
    /// Build a worker that owns its own Tokio runtime and DynamoDB
    /// client.
    pub fn new(config: DynamoConfig) -> Result<(Self, DynamoMetricsHandle), DynamoError> {
        config
            .validate()
            .map_err(|e| DynamoError::InvalidRequest(format!("config: {e}")))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .thread_name("tina-aws-dynamodb-bridge")
            .build()
            .map_err(|e| DynamoError::Internal(format!("tokio runtime build: {e}")))?;
        let handle = runtime.handle().clone();
        let client = build_dynamodb_client(&config);
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

    /// Build a worker around a caller-supplied DynamoDB client and
    /// Tokio runtime handle.
    ///
    /// The supplied client owns credentials, region, endpoint, HTTP
    /// connector, TLS, and SDK retry configuration. The metrics field
    /// `sdk_max_attempts` is `0` on this path. The bridge still owns
    /// Tina-side `mailbox_capacity`, `max_in_flight`, item size cap,
    /// and per-operation timeout.
    pub fn with_supplied_client(
        config: DynamoConfig,
        client: Client,
        runtime: Handle,
    ) -> Result<(Self, DynamoMetricsHandle), DynamoConfigError> {
        config.validate_bridge_fields()?;
        Ok(Self::assemble(config, client, runtime, None, 0))
    }

    fn assemble(
        config: DynamoConfig,
        client: Client,
        runtime: Handle,
        owned_runtime: Option<OwnedRuntime>,
        sdk_max_attempts: u64,
    ) -> (Self, DynamoMetricsHandle) {
        let metrics = Arc::new(DynamoMetricsInner::default());
        metrics
            .sdk_max_attempts
            .store(sdk_max_attempts, Ordering::Relaxed);
        let handle = DynamoMetricsHandle {
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
    pub fn closer(&self) -> DynamoCloser {
        DynamoCloser {
            closed: Arc::clone(&self.closed),
            metrics: Arc::clone(&self.metrics),
        }
    }

    fn admit(
        &mut self,
        request: DynamoRequest,
        request_context: Option<RequestContext<DynamoResult>>,
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
            return Self::complete_request(request_context, Err(DynamoError::Closed));
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
            return Self::complete_request(request_context, Err(DynamoError::Full));
        }

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let reply_plain = request_context.is_none();
        let (tx, rx) = oneshot::channel();
        let client = self.client.clone();
        let return_capacity = self.config.return_capacity;
        let query_item_limit = self.config.query_item_limit;
        let abandoned = Arc::new(AtomicBool::new(false));
        let abandoned_for_task = Arc::clone(&abandoned);
        let metrics_for_task = Arc::clone(&self.metrics);
        self.runtime.spawn(async move {
            let result = run_request(client, request, return_capacity, query_item_limit).await;
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
            DynamoInFlight {
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
        sleep(self.config.poll_interval).then(move |_| DynamoMsg::Poll(id))
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
                        Self::complete_request(request_context, Err(DynamoError::Timeout)),
                        sleep(self.config.poll_interval).then(move |_| DynamoMsg::Poll(id)),
                    ]);
                }
                self.in_flight.insert(id, in_flight);
                sleep(self.config.poll_interval).then(move |_| DynamoMsg::Poll(id))
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
                    Err(DynamoError::Internal(
                        "sdk task ended without result".into(),
                    )),
                )
            }
        }
    }

    fn complete_request(
        request_context: Option<RequestContext<DynamoResult>>,
        result: DynamoResult,
    ) -> Effect<Self> {
        match request_context {
            Some(request) => reply_to(request, result),
            None => reply::<Self>(result),
        }
    }

    fn complete_terminal(
        request_context: Option<RequestContext<DynamoResult>>,
        reply_plain: bool,
        result: DynamoResult,
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

impl<S: Shard + Send + 'static> DynamoWorker<S> {
    /// Validate config, build the worker, register it, and return the
    /// address, closer, and metrics handle.
    pub fn install<F>(
        runtime: &ThreadedRuntime<S, F>,
        config: DynamoConfig,
    ) -> Result<InstalledDynamoBridge<S>, DynamoInstallError>
    where
        F: MailboxFactory + Send + 'static,
    {
        config.validate()?;
        let cap = config.mailbox_capacity;
        let (worker, metrics) =
            Self::new(config).map_err(|e| DynamoInstallError::Build(e.to_string()))?;
        let closer = worker.closer();
        let address = runtime
            .register_with_capacity::<_, Infallible>(worker, cap)
            .map_err(|e| DynamoInstallError::Register(format!("{e:?}")))?;
        Ok(InstalledDynamoBridge {
            address,
            closer,
            metrics,
            _shard: PhantomData,
        })
    }
}

/// Validate config, build the DynamoDB worker, register it, and return
/// the address, closer, and metrics handle.
pub fn install_dynamodb<S, F>(
    runtime: &ThreadedRuntime<S, F>,
    config: DynamoConfig,
) -> Result<InstalledDynamoBridge<S>, DynamoInstallError>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    DynamoWorker::<S>::install(runtime, config)
}

impl<S: Shard + 'static> Isolate for DynamoWorker<S> {
    tina::isolate_types! {
        message: DynamoMsg,
        reply: DynamoResult,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<DynamoMsg>,
        shard: S,
    }

    fn handle(&mut self, msg: DynamoMsg, _ctx: &mut Context<'_, S, Self::Reply>) -> Effect<Self> {
        match msg {
            DynamoMsg::Send(request) => self.admit(request, None),
            DynamoMsg::Poll(id) => self.poll(id),
        }
    }

    fn handle_call(&mut self, msg: DynamoMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            DynamoMsg::Send(request) => self.admit(request, Some(call.into_request_context())),
            DynamoMsg::Poll(_) => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

fn build_dynamodb_client(config: &DynamoConfig) -> Client {
    let mut builder = aws_sdk_dynamodb::Config::builder()
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

fn validate_request(request: &DynamoRequest, config: &DynamoConfig) -> Result<(), DynamoError> {
    match request {
        DynamoRequest::GetItem(get) => {
            validate_table(&get.table_name)?;
            validate_key(&get.key)?;
            if approximate_size(&get.key) > config.item_size_limit {
                return Err(DynamoError::ItemTooLarge);
            }
        }
        DynamoRequest::PutItem(put) => {
            validate_table(&put.table_name)?;
            if put.item.is_empty() {
                return Err(DynamoError::InvalidRequest("item must not be empty".into()));
            }
            let size = approximate_size(&put.item)
                .saturating_add(
                    put.condition_expression
                        .as_deref()
                        .map(str::len)
                        .unwrap_or(0),
                )
                .saturating_add(map_string_size(&put.expression_attribute_names))
                .saturating_add(approximate_size(&put.expression_attribute_values));
            if size > config.item_size_limit {
                return Err(DynamoError::ItemTooLarge);
            }
        }
        DynamoRequest::UpdateItem(update) => {
            validate_table(&update.table_name)?;
            validate_key(&update.key)?;
            if update.update_expression.trim().is_empty() {
                return Err(DynamoError::InvalidRequest(
                    "update_expression must not be empty".into(),
                ));
            }
            let size = approximate_size(&update.key)
                .saturating_add(update.update_expression.len())
                .saturating_add(
                    update
                        .condition_expression
                        .as_deref()
                        .map(str::len)
                        .unwrap_or(0),
                )
                .saturating_add(map_string_size(&update.expression_attribute_names))
                .saturating_add(approximate_size(&update.expression_attribute_values));
            if size > config.item_size_limit {
                return Err(DynamoError::ItemTooLarge);
            }
        }
        DynamoRequest::DeleteItem(delete) => {
            validate_table(&delete.table_name)?;
            validate_key(&delete.key)?;
            let size = approximate_size(&delete.key)
                .saturating_add(
                    delete
                        .condition_expression
                        .as_deref()
                        .map(str::len)
                        .unwrap_or(0),
                )
                .saturating_add(map_string_size(&delete.expression_attribute_names))
                .saturating_add(approximate_size(&delete.expression_attribute_values));
            if size > config.item_size_limit {
                return Err(DynamoError::ItemTooLarge);
            }
        }
        DynamoRequest::Query(query) => {
            validate_table(&query.table_name)?;
            if query.key_condition_expression.trim().is_empty() {
                return Err(DynamoError::InvalidRequest(
                    "key_condition_expression must not be empty".into(),
                ));
            }
            if let Some(limit) = query.limit {
                if limit <= 0 {
                    return Err(DynamoError::InvalidRequest("limit must be >= 1".into()));
                }
                if (limit as usize) > config.query_item_limit {
                    return Err(DynamoError::InvalidRequest(format!(
                        "limit {limit} exceeds query_item_limit {}",
                        config.query_item_limit
                    )));
                }
            }
            let size = query
                .key_condition_expression
                .len()
                .saturating_add(
                    query
                        .filter_expression
                        .as_deref()
                        .map(str::len)
                        .unwrap_or(0),
                )
                .saturating_add(map_string_size(&query.expression_attribute_names))
                .saturating_add(approximate_size(&query.expression_attribute_values))
                .saturating_add(
                    query
                        .exclusive_start_key
                        .as_ref()
                        .map(approximate_size)
                        .unwrap_or(0),
                );
            if size > config.item_size_limit {
                return Err(DynamoError::ItemTooLarge);
            }
        }
    }
    Ok(())
}

fn validate_table(table_name: &str) -> Result<(), DynamoError> {
    if table_name.trim().is_empty() {
        Err(DynamoError::InvalidRequest(
            "table_name must not be empty".into(),
        ))
    } else {
        Ok(())
    }
}

fn validate_key(key: &DynamoItem) -> Result<(), DynamoError> {
    if key.is_empty() {
        Err(DynamoError::InvalidRequest("key must not be empty".into()))
    } else {
        Ok(())
    }
}

/// Sum of key + value string lengths in an attribute-name map (used for
/// `expression_attribute_names` which is `HashMap<String, String>`).
fn map_string_size(map: &HashMap<String, String>) -> usize {
    map.iter()
        .map(|(k, v)| k.len().saturating_add(v.len()))
        .sum()
}

/// Cheap size estimate used by the bridge cap. Not byte-accurate to
/// DynamoDB's wire size; conservative enough to catch obvious overruns
/// before SDK rejection.
fn approximate_size(item: &DynamoItem) -> usize {
    let mut total = 0usize;
    for (k, v) in item {
        total = total.saturating_add(k.len());
        total = total.saturating_add(value_size(v));
    }
    total
}

fn value_size(value: &DynamoValue) -> usize {
    match value {
        DynamoValue::S(s) => s.len(),
        DynamoValue::N(n) => n.len(),
        DynamoValue::B(b) => b.len(),
        DynamoValue::Bool(_) => 1,
        DynamoValue::Null => 1,
        DynamoValue::L(list) => list.iter().map(value_size).sum::<usize>().saturating_add(1),
        DynamoValue::M(map) => map
            .iter()
            .map(|(k, v)| k.len().saturating_add(value_size(v)))
            .sum::<usize>()
            .saturating_add(1),
    }
}

fn to_attribute(value: DynamoValue) -> AttributeValue {
    match value {
        DynamoValue::S(s) => AttributeValue::S(s),
        DynamoValue::N(n) => AttributeValue::N(n),
        DynamoValue::B(b) => AttributeValue::B(Blob::new(b)),
        DynamoValue::Bool(b) => AttributeValue::Bool(b),
        DynamoValue::Null => AttributeValue::Null(true),
        DynamoValue::L(list) => AttributeValue::L(list.into_iter().map(to_attribute).collect()),
        DynamoValue::M(map) => {
            AttributeValue::M(map.into_iter().map(|(k, v)| (k, to_attribute(v))).collect())
        }
    }
}

fn from_attribute(value: AttributeValue) -> Result<DynamoValue, DynamoError> {
    match value {
        AttributeValue::S(s) => Ok(DynamoValue::S(s)),
        AttributeValue::N(n) => Ok(DynamoValue::N(n)),
        AttributeValue::B(b) => Ok(DynamoValue::B(b.into_inner())),
        AttributeValue::Bool(b) => Ok(DynamoValue::Bool(b)),
        AttributeValue::Null(_) => Ok(DynamoValue::Null),
        AttributeValue::L(list) => {
            let mut out = Vec::with_capacity(list.len());
            for v in list {
                out.push(from_attribute(v)?);
            }
            Ok(DynamoValue::L(out))
        }
        AttributeValue::M(map) => {
            let mut out = HashMap::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k, from_attribute(v)?);
            }
            Ok(DynamoValue::M(out))
        }
        other => Err(DynamoError::Sdk(format!(
            "unsupported AttributeValue variant: {other:?}"
        ))),
    }
}

fn to_attribute_map(map: HashMap<String, DynamoValue>) -> HashMap<String, AttributeValue> {
    map.into_iter().map(|(k, v)| (k, to_attribute(v))).collect()
}

fn from_attribute_map(map: HashMap<String, AttributeValue>) -> Result<DynamoItem, DynamoError> {
    let mut out = HashMap::with_capacity(map.len());
    for (k, v) in map {
        out.insert(k, from_attribute(v)?);
    }
    Ok(out)
}

fn consistency_strong(consistency: DynamoConsistency) -> bool {
    matches!(consistency, DynamoConsistency::Strong)
}

fn return_capacity_param(mode: ReturnCapacity) -> Option<ReturnConsumedCapacity> {
    match mode {
        ReturnCapacity::None => None,
        ReturnCapacity::Total => Some(ReturnConsumedCapacity::Total),
        ReturnCapacity::Indexes => Some(ReturnConsumedCapacity::Indexes),
    }
}

fn convert_consumed_capacity(
    capacity: Option<ConsumedCapacity>,
    mode: ReturnCapacity,
) -> Option<DynamoConsumedCapacity> {
    if matches!(mode, ReturnCapacity::None) {
        return None;
    }
    capacity.map(|c| DynamoConsumedCapacity {
        capacity_units: c.capacity_units,
        read_capacity_units: c.read_capacity_units,
        write_capacity_units: c.write_capacity_units,
    })
}

async fn run_request(
    client: Client,
    request: DynamoRequest,
    return_capacity: ReturnCapacity,
    query_item_limit: usize,
) -> DynamoResult {
    let rc = return_capacity_param(return_capacity);
    match request {
        DynamoRequest::GetItem(get) => {
            let mut req = client
                .get_item()
                .table_name(get.table_name)
                .set_key(Some(to_attribute_map(get.key)))
                .consistent_read(consistency_strong(get.consistency));
            if let Some(rc) = rc {
                req = req.return_consumed_capacity(rc);
            }
            match req.send().await {
                Ok(out) => {
                    let item = match out.item {
                        Some(map) if !map.is_empty() => Some(from_attribute_map(map)?),
                        _ => None,
                    };
                    Ok(DynamoResponse::GetItem(DynamoGotItem {
                        item,
                        consumed_capacity: convert_consumed_capacity(
                            out.consumed_capacity,
                            return_capacity,
                        ),
                    }))
                }
                Err(e) => Err(classify_get_error(e)),
            }
        }
        DynamoRequest::PutItem(put) => {
            let mut req = client
                .put_item()
                .table_name(put.table_name)
                .set_item(Some(to_attribute_map(put.item)));
            if let Some(expr) = put.condition_expression {
                req = req.condition_expression(expr);
            }
            if !put.expression_attribute_names.is_empty() {
                req = req.set_expression_attribute_names(Some(put.expression_attribute_names));
            }
            if !put.expression_attribute_values.is_empty() {
                req = req.set_expression_attribute_values(Some(to_attribute_map(
                    put.expression_attribute_values,
                )));
            }
            if let Some(rc) = rc {
                req = req.return_consumed_capacity(rc);
            }
            match req.send().await {
                Ok(out) => Ok(DynamoResponse::PutItem(DynamoPutItemOk {
                    consumed_capacity: convert_consumed_capacity(
                        out.consumed_capacity,
                        return_capacity,
                    ),
                })),
                Err(e) => Err(classify_put_error(e)),
            }
        }
        DynamoRequest::UpdateItem(update) => {
            let mut req = client
                .update_item()
                .table_name(update.table_name)
                .set_key(Some(to_attribute_map(update.key)))
                .update_expression(update.update_expression);
            if let Some(expr) = update.condition_expression {
                req = req.condition_expression(expr);
            }
            if !update.expression_attribute_names.is_empty() {
                req = req.set_expression_attribute_names(Some(update.expression_attribute_names));
            }
            if !update.expression_attribute_values.is_empty() {
                req = req.set_expression_attribute_values(Some(to_attribute_map(
                    update.expression_attribute_values,
                )));
            }
            if let Some(rc) = rc {
                req = req.return_consumed_capacity(rc);
            }
            match req.send().await {
                Ok(out) => Ok(DynamoResponse::UpdateItem(DynamoUpdatedItem {
                    consumed_capacity: convert_consumed_capacity(
                        out.consumed_capacity,
                        return_capacity,
                    ),
                })),
                Err(e) => Err(classify_update_error(e)),
            }
        }
        DynamoRequest::DeleteItem(delete) => {
            let mut req = client
                .delete_item()
                .table_name(delete.table_name)
                .set_key(Some(to_attribute_map(delete.key)));
            if let Some(expr) = delete.condition_expression {
                req = req.condition_expression(expr);
            }
            if !delete.expression_attribute_names.is_empty() {
                req = req.set_expression_attribute_names(Some(delete.expression_attribute_names));
            }
            if !delete.expression_attribute_values.is_empty() {
                req = req.set_expression_attribute_values(Some(to_attribute_map(
                    delete.expression_attribute_values,
                )));
            }
            if let Some(rc) = rc {
                req = req.return_consumed_capacity(rc);
            }
            match req.send().await {
                Ok(out) => Ok(DynamoResponse::DeleteItem(DynamoDeletedItem {
                    consumed_capacity: convert_consumed_capacity(
                        out.consumed_capacity,
                        return_capacity,
                    ),
                })),
                Err(e) => Err(classify_delete_error(e)),
            }
        }
        DynamoRequest::Query(query) => {
            let effective_limit = query
                .limit
                .map(|l| (l as usize).min(query_item_limit))
                .unwrap_or(query_item_limit);
            let mut req = client
                .query()
                .table_name(query.table_name)
                .key_condition_expression(query.key_condition_expression)
                .consistent_read(consistency_strong(query.consistency))
                .scan_index_forward(query.scan_index_forward)
                .limit(effective_limit as i32)
                .select(Select::AllAttributes);
            if let Some(index) = query.index_name {
                req = req.index_name(index);
            }
            if let Some(expr) = query.filter_expression {
                req = req.filter_expression(expr);
            }
            if !query.expression_attribute_names.is_empty() {
                req = req.set_expression_attribute_names(Some(query.expression_attribute_names));
            }
            if !query.expression_attribute_values.is_empty() {
                req = req.set_expression_attribute_values(Some(to_attribute_map(
                    query.expression_attribute_values,
                )));
            }
            if let Some(start) = query.exclusive_start_key {
                req = req.set_exclusive_start_key(Some(to_attribute_map(start)));
            }
            if let Some(rc) = rc {
                req = req.return_consumed_capacity(rc);
            }
            match req.send().await {
                Ok(out) => {
                    let raw_items = out.items.unwrap_or_default();
                    let mut items = Vec::with_capacity(raw_items.len());
                    for raw in raw_items {
                        items.push(from_attribute_map(raw)?);
                    }
                    let last_evaluated_key = match out.last_evaluated_key {
                        Some(map) if !map.is_empty() => Some(from_attribute_map(map)?),
                        _ => None,
                    };
                    Ok(DynamoResponse::Query(DynamoQueryResult {
                        items,
                        last_evaluated_key,
                        count: out.count,
                        scanned_count: out.scanned_count,
                        consumed_capacity: convert_consumed_capacity(
                            out.consumed_capacity,
                            return_capacity,
                        ),
                    }))
                }
                Err(e) => Err(classify_query_error(e)),
            }
        }
    }
}

fn classify_get_error(error: SdkError<GetItemError>) -> DynamoError {
    if let Some(service) = error.as_service_error() {
        if service.is_provisioned_throughput_exceeded_exception() {
            return DynamoError::ProvisionedThroughputExceeded(error_detail(&error));
        }
        if service.is_resource_not_found_exception() {
            return DynamoError::ResourceNotFound(error_detail(&error));
        }
        if service.is_request_limit_exceeded() {
            return DynamoError::Throttled(error_detail(&error));
        }
    }
    DynamoError::Sdk(error_detail(&error))
}

fn classify_put_error(error: SdkError<PutItemError>) -> DynamoError {
    if let Some(service) = error.as_service_error() {
        if service.is_conditional_check_failed_exception() {
            return DynamoError::ConditionalCheckFailed(error_detail(&error));
        }
        if service.is_provisioned_throughput_exceeded_exception() {
            return DynamoError::ProvisionedThroughputExceeded(error_detail(&error));
        }
        if service.is_resource_not_found_exception() {
            return DynamoError::ResourceNotFound(error_detail(&error));
        }
        if service.is_request_limit_exceeded() {
            return DynamoError::Throttled(error_detail(&error));
        }
    }
    DynamoError::Sdk(error_detail(&error))
}

fn classify_update_error(error: SdkError<UpdateItemError>) -> DynamoError {
    if let Some(service) = error.as_service_error() {
        if service.is_conditional_check_failed_exception() {
            return DynamoError::ConditionalCheckFailed(error_detail(&error));
        }
        if service.is_provisioned_throughput_exceeded_exception() {
            return DynamoError::ProvisionedThroughputExceeded(error_detail(&error));
        }
        if service.is_resource_not_found_exception() {
            return DynamoError::ResourceNotFound(error_detail(&error));
        }
        if service.is_transaction_conflict_exception() {
            return DynamoError::TransactionConflict(error_detail(&error));
        }
        if service.is_request_limit_exceeded() {
            return DynamoError::Throttled(error_detail(&error));
        }
    }
    DynamoError::Sdk(error_detail(&error))
}

fn classify_delete_error(error: SdkError<DeleteItemError>) -> DynamoError {
    if let Some(service) = error.as_service_error() {
        if service.is_conditional_check_failed_exception() {
            return DynamoError::ConditionalCheckFailed(error_detail(&error));
        }
        if service.is_provisioned_throughput_exceeded_exception() {
            return DynamoError::ProvisionedThroughputExceeded(error_detail(&error));
        }
        if service.is_resource_not_found_exception() {
            return DynamoError::ResourceNotFound(error_detail(&error));
        }
        if service.is_request_limit_exceeded() {
            return DynamoError::Throttled(error_detail(&error));
        }
    }
    DynamoError::Sdk(error_detail(&error))
}

fn classify_query_error(error: SdkError<QueryError>) -> DynamoError {
    if let Some(service) = error.as_service_error() {
        if service.is_provisioned_throughput_exceeded_exception() {
            return DynamoError::ProvisionedThroughputExceeded(error_detail(&error));
        }
        if service.is_resource_not_found_exception() {
            return DynamoError::ResourceNotFound(error_detail(&error));
        }
        if service.is_request_limit_exceeded() {
            return DynamoError::Throttled(error_detail(&error));
        }
    }
    DynamoError::Sdk(error_detail(&error))
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

fn tally_admission_error(metrics: &DynamoMetricsInner, err: &DynamoError) {
    // `validate_request` only produces `ItemTooLarge` or
    // `InvalidRequest`. Future validator additions land in `invalid`
    // until they earn a typed counter.
    match err {
        DynamoError::ItemTooLarge => {
            metrics.item_too_large.fetch_add(1, Ordering::Relaxed);
        }
        _ => {
            metrics.invalid.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(feature = "tracing")]
fn admission_reason(err: &DynamoError) -> &'static str {
    match err {
        DynamoError::ItemTooLarge => "ItemTooLarge",
        DynamoError::InvalidRequest(_) => "InvalidRequest",
        _ => "Invalid",
    }
}

fn tally_terminal(metrics: &DynamoMetricsInner, result: &DynamoResult) {
    match result {
        Ok(_) => {
            metrics.responses.fetch_add(1, Ordering::Relaxed);
        }
        Err(DynamoError::ConditionalCheckFailed(_)) => {
            metrics
                .conditional_check_failed
                .fetch_add(1, Ordering::Relaxed);
        }
        Err(DynamoError::ProvisionedThroughputExceeded(_)) => {
            metrics
                .provisioned_throughput_exceeded
                .fetch_add(1, Ordering::Relaxed);
        }
        Err(DynamoError::ResourceNotFound(_)) => {
            metrics.resource_not_found.fetch_add(1, Ordering::Relaxed);
        }
        Err(DynamoError::Throttled(_)) => {
            metrics.throttled.fetch_add(1, Ordering::Relaxed);
        }
        Err(DynamoError::TransactionConflict(_)) => {
            metrics.transaction_conflict.fetch_add(1, Ordering::Relaxed);
        }
        Err(DynamoError::Sdk(_)) => {
            metrics.sdk_errors.fetch_add(1, Ordering::Relaxed);
        }
        Err(_) => {}
    }
}
