//! Hermetic webhook relay that exercises the bridge classifier path.
//!
//! A relay isolate receives events and publishes them to an outbound
//! port. The outbound port speaks the same shape as a Tina AWS bridge
//! (SNS publish / SQS send): a typed `CallOutcome<Result<_, _>>`. The
//! relay classifies each outcome using a bridge classifier and routes
//! the event to one of:
//!
//! - `delivered` — the SDK accepted the request;
//! - `retry` — transient classifier (caller retries; idempotency is
//!   the caller's story);
//! - `dead_letter` — fatal classifier (request will not succeed on
//!   retry without changing inputs or setup).
//!
//! The default tests use a fake outbound isolate that returns prepared
//! `OutboundOutcome` values, so no real AWS account is required. A
//! second `outbound_via_sqs` module shows the same relay wired to
//! `tina_aws_bridge::SqsAddress` for callers who want to point the
//! same shape at SQS through `tina-aws-bridge`.

use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::time::Duration;

use tina::CallRejectedReason;
use tina::prelude::*;
use tina_aws_bridge::{
    BridgeFatal, BridgeOutcomeClass, BridgeRetryable, BridgeUnavailable, SqsAddress, SqsConfig,
    SqsDrainReport, SqsError, SqsInstallError, SqsRequest, SqsResponse, SqsSendMessage, send_sqs,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, ReportedWorkloadError,
    RequestServiceHandle, RunToShutdownError, StartupError, ThreadedRuntimeError,
};

type RelaySystem = LocalSystem<SingleShard, DefaultThreadedMailboxFactory>;

const RELAY_MAILBOX_CAPACITY: usize = 64;
const FAKE_MAILBOX_CAPACITY: usize = 64;
const MAX_EVENTS: usize = 4_096;
const MAX_PROGRAM_ENTRIES: usize = 4_096;
const MAX_CALL_TIMEOUT_MS: u64 = 60_000;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Outbound port the relay calls. Mirrors the bridge two-layer shape.
pub type OutboundOutcome = CallOutcome<Result<OutboundReply, OutboundError>>;

/// Successful outbound reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundReply {
    /// Backend-assigned id (SNS message id, SQS message id, etc).
    pub backend_id: String,
}

/// Errors mapped from a Tina AWS bridge error shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboundError {
    /// Bridge admission saturated.
    Full,
    /// Bridge closed.
    Closed,
    /// Bridge per-operation timeout.
    Timeout,
    /// Service throttled the call.
    Throttled,
    /// SDK/service transient error.
    SdkTransient,
    /// Generic SDK error with no typed retry evidence.
    SdkUnknown,
    /// Resource not found (topic / queue gone).
    NotFound,
    /// Service rejected parameters.
    InvalidParameter,
    /// Caller has no permission.
    AccessDenied,
    /// Bridge rejected the request shape.
    InvalidRequest,
    /// Bridge or worker internal failure.
    Internal,
}

impl OutboundError {
    /// Classifier from the outbound layer's typed error.
    pub fn classify(&self) -> BridgeOutcomeClass {
        match self {
            Self::Full => BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeFull),
            Self::Closed => BridgeOutcomeClass::Unavailable(BridgeUnavailable::BridgeClosed),
            Self::Timeout => BridgeOutcomeClass::Retryable(BridgeRetryable::BridgeTimeout),
            Self::Throttled => BridgeOutcomeClass::Retryable(BridgeRetryable::ServiceThrottled),
            Self::SdkTransient => BridgeOutcomeClass::Retryable(BridgeRetryable::SdkRetryable),
            Self::SdkUnknown => BridgeOutcomeClass::Fatal(BridgeFatal::SdkUnknown),
            Self::NotFound => BridgeOutcomeClass::Fatal(BridgeFatal::NotFound),
            Self::InvalidParameter => BridgeOutcomeClass::Fatal(BridgeFatal::InvalidParameter),
            Self::AccessDenied => BridgeOutcomeClass::Fatal(BridgeFatal::AccessDenied),
            Self::InvalidRequest => BridgeOutcomeClass::Fatal(BridgeFatal::InvalidRequest),
            Self::Internal => BridgeOutcomeClass::Fatal(BridgeFatal::Internal),
        }
    }
}

/// One webhook event the relay attempts to deliver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// Caller-supplied event id (used for idempotency at the
    /// destination). The relay does not invent one.
    pub event_id: String,
    /// Event body.
    pub body: String,
}

/// Why the relay gave up on an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeadLetterReason {
    /// Fatal classifier from the outbound layer.
    Fatal(BridgeFatal),
    /// Unavailable classifier — the bridge or resource is closed and a
    /// new handle is required.
    Unavailable(BridgeUnavailable),
    /// The runtime rejected the outbound request without a domain reply.
    Rejected(CallRejectedReason),
}

/// Final relay outcome for one event (visible to callers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayReply {
    /// Outbound accepted the event.
    Delivered { backend_id: String },
    /// Retryable — caller should retry under their own idempotency
    /// story. The relay does **not** retry on its own.
    Retry { reason: BridgeRetryable },
    /// Will not succeed without input/setup change.
    DeadLetter { reason: DeadLetterReason },
    /// Reply to a `RelayRequest::Stats` call: typed counter snapshot.
    Stats(RelayStats),
}

/// Caller-visible counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RelayStats {
    /// Events the outbound accepted.
    pub delivered: u64,
    /// Events flagged for caller-driven retry.
    pub transient: u64,
    /// Events permanently dropped (fatal classifier).
    pub dead_letter: u64,
}

/// Internal completions the relay's outbound port reports back. Never
/// part of the caller-facing request surface.
#[derive(Debug)]
enum RelayEvent {
    /// Internal: the fake outbound replied.
    FakeFinished {
        /// Original caller request context.
        request: RequestContext<RelayReply>,
        /// Fake outbound's typed outcome (already shaped like
        /// `OutboundOutcome`).
        outcome: OutboundOutcome,
    },
    /// Internal: the SQS bridge replied.
    SqsFinished {
        /// Original caller request context.
        request: RequestContext<RelayReply>,
        /// Raw SQS bridge outcome — mapped to OutboundOutcome on
        /// handler entry.
        outcome: CallOutcome<Result<SqsResponse, SqsError>>,
    },
}

/// Caller-authority requests handled by the relay.
#[derive(Debug)]
enum RelayRequest {
    /// Caller asks the relay to deliver one event.
    Deliver(Event),
    /// Caller asks for relay-side counters.
    Stats,
}

/// Bridge to `tina-aws-bridge`'s SQS worker. Maps `SqsError` into
/// `OutboundError` and `SqsResponse::SentMessage` into
/// `OutboundReply`.
pub struct SqsOutbound {
    /// Bridge address.
    pub address: SqsAddress,
    /// Target SQS queue URL.
    pub queue_url: String,
    /// Per-call timeout.
    pub timeout: Duration,
}

/// Map an SQS bridge outcome into the relay's outbound shape.
pub fn map_sqs_outcome(outcome: CallOutcome<Result<SqsResponse, SqsError>>) -> OutboundOutcome {
    match outcome {
        CallOutcome::Full => CallOutcome::Full,
        CallOutcome::Closed => CallOutcome::Closed,
        CallOutcome::Timeout => CallOutcome::Timeout,
        CallOutcome::Rejected(r) => CallOutcome::Rejected(r),
        CallOutcome::Replied(Ok(SqsResponse::SentMessage(sent))) => match sent.message_id {
            Some(backend_id) => CallOutcome::Replied(Ok(OutboundReply { backend_id })),
            None => CallOutcome::Replied(Err(OutboundError::Internal)),
        },
        CallOutcome::Replied(Ok(_)) => CallOutcome::Replied(Err(OutboundError::InvalidRequest)),
        CallOutcome::Replied(Err(err)) => CallOutcome::Replied(Err(map_sqs_error(err))),
    }
}

fn map_sqs_error(err: SqsError) -> OutboundError {
    match err {
        SqsError::Full => OutboundError::Full,
        SqsError::Closed => OutboundError::Closed,
        SqsError::Timeout => OutboundError::Timeout,
        SqsError::MessageTooLarge | SqsError::ResponseTooLarge => OutboundError::InvalidRequest,
        SqsError::InvalidRequest(_) => OutboundError::InvalidRequest,
        SqsError::QueueDoesNotExist(_) => OutboundError::NotFound,
        SqsError::Throttled(_) => OutboundError::Throttled,
        SqsError::Sdk(_) => OutboundError::SdkUnknown,
        SqsError::Internal(_) => OutboundError::Internal,
    }
}

/// Outbound port behind the relay. Use `OutboundPort::fake` for
/// hermetic tests or `OutboundPort::sqs` to point the relay at a
/// real `tina-aws-bridge` SQS worker.
pub enum OutboundPort {
    /// In-process fake outbound (the program is a `VecDeque` of replies).
    Fake(RequestServiceHandle<FakeOutboundRequest, Result<OutboundReply, OutboundError>>),
    /// AWS SQS bridge address + queue URL + per-call timeout.
    Sqs(SqsOutbound),
}

/// Webhook relay isolate. Forwards events to its outbound port and
/// classifies each outcome.
struct Relay {
    outbound: OutboundPort,
    timeout: Duration,
    stats: RelayStats,
}

impl Relay {
    /// Build a relay around an outbound port. Use this constructor when
    /// you want to register the relay yourself; otherwise call
    /// [`run`] (fake path) or [`run_against_sqs`] (real SQS path).
    pub fn new(outbound: OutboundPort, timeout: Duration) -> Self {
        Self {
            outbound,
            timeout,
            stats: RelayStats::default(),
        }
    }
}

#[tina_runtime::isolate(event = RelayEvent, request = RelayRequest, reply = RelayReply)]
impl Relay {
    fn handle_event(
        &mut self,
        event: RelayEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            RelayEvent::FakeFinished { request, outcome } => {
                let reply = self.classify_and_tally(outcome);
                reply_to(request, reply)
            }
            RelayEvent::SqsFinished { request, outcome } => {
                let reply = self.classify_and_tally(map_sqs_outcome(outcome));
                reply_to(request, reply)
            }
        }
    }

    fn handle_request(
        &mut self,
        request: RelayRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            RelayRequest::Deliver(event) => self.issue(event, call),
            RelayRequest::Stats => call.reply(RelayReply::Stats(self.stats)),
        }
    }
}

impl Relay {
    fn issue(&mut self, event: Event, call: RequestCall<'_, Self>) -> RequestEffect<Self> {
        let timeout = self.timeout;
        match &self.outbound {
            OutboundPort::Fake(addr) => {
                call.defer(tina_runtime::call_request(
                    *addr,
                    FakeOutboundRequest::Send(event),
                    timeout,
                ))
                .reply_service_event(|request, outcome| {
                    RelayEvent::FakeFinished { request, outcome }
                })
            }
            OutboundPort::Sqs(sqs) => {
                let address = sqs.address;
                let queue_url = sqs.queue_url.clone();
                let bridge_timeout = sqs.timeout;
                let issued = send_sqs(
                    address,
                    SqsRequest::SendMessage(SqsSendMessage {
                        queue_url,
                        body: event.body,
                        message_group_id: Some(event.event_id),
                        message_deduplication_id: None,
                    }),
                    bridge_timeout,
                );
                call.defer(issued)
                    .reply_service_event(|request, outcome| RelayEvent::SqsFinished {
                        request,
                        outcome,
                    })
            }
        }
    }
}

impl Relay {
    fn classify_and_tally(&mut self, outcome: OutboundOutcome) -> RelayReply {
        classify_outbound_outcome(&mut self.stats, outcome)
    }
}

fn classify_outbound_outcome(stats: &mut RelayStats, outcome: OutboundOutcome) -> RelayReply {
    match &outcome {
        CallOutcome::Replied(Ok(ok)) => {
            stats.delivered += 1;
            RelayReply::Delivered {
                backend_id: ok.backend_id.clone(),
            }
        }
        CallOutcome::Replied(Err(err)) => match err.classify() {
            BridgeOutcomeClass::Succeeded => {
                stats.dead_letter += 1;
                RelayReply::DeadLetter {
                    reason: DeadLetterReason::Fatal(BridgeFatal::Internal),
                }
            }
            BridgeOutcomeClass::Retryable(reason) => {
                stats.transient += 1;
                RelayReply::Retry { reason }
            }
            BridgeOutcomeClass::Unavailable(reason) => {
                stats.dead_letter += 1;
                RelayReply::DeadLetter {
                    reason: DeadLetterReason::Unavailable(reason),
                }
            }
            BridgeOutcomeClass::Fatal(reason) => {
                stats.dead_letter += 1;
                RelayReply::DeadLetter {
                    reason: DeadLetterReason::Fatal(reason),
                }
            }
        },
        CallOutcome::Full => {
            stats.transient += 1;
            RelayReply::Retry {
                reason: BridgeRetryable::BridgeFull,
            }
        }
        CallOutcome::Closed => {
            stats.dead_letter += 1;
            RelayReply::DeadLetter {
                reason: DeadLetterReason::Unavailable(BridgeUnavailable::BridgeClosed),
            }
        }
        CallOutcome::Timeout => {
            stats.transient += 1;
            RelayReply::Retry {
                reason: BridgeRetryable::CallerTimeout,
            }
        }
        CallOutcome::Rejected(reason) => {
            stats.dead_letter += 1;
            RelayReply::DeadLetter {
                reason: DeadLetterReason::Rejected(*reason),
            }
        }
    }
}

// ---------- Fake outbound for hermetic tests ----------

/// Outcome the fake outbound returns for the next call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeOutboundProgram {
    /// Reply success with the given backend id.
    Deliver(String),
    /// Reply an `OutboundError` (test the classifier branches).
    Fail(OutboundError),
    /// Reject the request without a domain reply.
    Reject(CallRejectedReason),
    /// Reply closed and stop the fake owner. A following request observes
    /// transport-level `CallOutcome::Closed`.
    Stop,
}

/// Requests handled by [`FakeOutbound`].
#[derive(Debug)]
pub enum FakeOutboundRequest {
    /// Caller asks the fake outbound to deliver one event.
    Send(Event),
}

/// In-process outbound used by hermetic tests. The script of replies
/// is supplied at construction; once exhausted, subsequent calls reply
/// with `OutboundError::Internal`.
pub struct FakeOutbound {
    program: std::collections::VecDeque<FakeOutboundProgram>,
}

#[tina_runtime::isolate(
    request = FakeOutboundRequest,
    reply = Result<OutboundReply, OutboundError>
)]
impl FakeOutbound {
    fn handle_request(
        &mut self,
        request: FakeOutboundRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            FakeOutboundRequest::Send(_event) => {
                let next = self
                    .program
                    .pop_front()
                    .unwrap_or(FakeOutboundProgram::Fail(OutboundError::Internal));
                match next {
                    FakeOutboundProgram::Deliver(id) => {
                        call.reply(Ok(OutboundReply { backend_id: id }))
                    }
                    FakeOutboundProgram::Fail(err) => call.reply(Err(err)),
                    FakeOutboundProgram::Reject(reason) => call.reject(reason),
                    FakeOutboundProgram::Stop => {
                        call.reply_and(Err(OutboundError::Closed), vec![stop()])
                    }
                }
            }
        }
    }
}

// ---------- Driver ----------

/// Configuration for [`run`].
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// Number of events to submit sequentially.
    pub events: usize,
    /// Per-call deadline. Zero is allowed as an explicit immediate timeout.
    pub call_timeout_ms: u64,
    /// Fake outbound program (one entry per event).
    pub program: Vec<FakeOutboundProgram>,
}

impl RunConfig {
    /// Validate all allocation- and wait-sized inputs before startup.
    pub fn validate(self) -> Result<Self, RunConfigError> {
        validate_event_count(self.events)?;
        if self.program.len() > MAX_PROGRAM_ENTRIES {
            return Err(RunConfigError::TooManyProgramEntries {
                requested: self.program.len(),
                max: MAX_PROGRAM_ENTRIES,
            });
        }
        if self.program.len() != self.events {
            return Err(RunConfigError::ProgramLengthMismatch {
                events: self.events,
                entries: self.program.len(),
            });
        }
        validate_timeout_ms("call timeout", self.call_timeout_ms)?;
        Ok(self)
    }
}

/// Invalid bounded-driver configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunConfigError {
    /// The event count exceeds the public driver cap.
    TooManyEvents { requested: usize, max: usize },
    /// The fake script exceeds its cap.
    TooManyProgramEntries { requested: usize, max: usize },
    /// The fake script must account for each submitted event exactly.
    ProgramLengthMismatch { events: usize, entries: usize },
    /// A millisecond duration exceeds the public cap.
    DurationTooLarge {
        field: &'static str,
        requested_ms: u128,
        max_ms: u64,
    },
    /// The real SQS path requires a concrete queue URL.
    EmptyQueueUrl,
}

impl fmt::Display for RunConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyEvents { requested, max } => {
                write!(f, "event count {requested} exceeds maximum {max}")
            }
            Self::TooManyProgramEntries { requested, max } => {
                write!(f, "fake program length {requested} exceeds maximum {max}")
            }
            Self::ProgramLengthMismatch { events, entries } => write!(
                f,
                "fake program has {entries} entries for {events} requested events"
            ),
            Self::DurationTooLarge {
                field,
                requested_ms,
                max_ms,
            } => write!(f, "{field} {requested_ms}ms exceeds maximum {max_ms}ms"),
            Self::EmptyQueueUrl => f.write_str("SQS queue URL must not be empty"),
        }
    }
}

impl Error for RunConfigError {}

fn validate_event_count(events: usize) -> Result<(), RunConfigError> {
    if events > MAX_EVENTS {
        return Err(RunConfigError::TooManyEvents {
            requested: events,
            max: MAX_EVENTS,
        });
    }
    Ok(())
}

fn validate_timeout_ms(field: &'static str, timeout_ms: u64) -> Result<(), RunConfigError> {
    if timeout_ms > MAX_CALL_TIMEOUT_MS {
        return Err(RunConfigError::DurationTooLarge {
            field,
            requested_ms: u128::from(timeout_ms),
            max_ms: MAX_CALL_TIMEOUT_MS,
        });
    }
    Ok(())
}

/// Driver outcome surface — structured `CallOutcome` and runtime
/// error visibility so failed test fixtures are debuggable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverReply {
    /// Relay returned a typed reply.
    Reply(RelayReply),
    /// `call_blocking` returned a runtime-level error.
    RuntimeError(ThreadedRuntimeError),
    /// Caller-side `CallOutcome::Full` — relay's mailbox was saturated.
    OuterFull,
    /// Caller-side `CallOutcome::Closed` — relay was closed.
    OuterClosed,
    /// Caller-side `CallOutcome::Timeout` — relay did not reply in time.
    OuterTimeout,
    /// Caller-side `CallOutcome::Rejected` — relay rejected the call.
    OuterRejected(CallRejectedReason),
}

/// Driver result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    /// Per-event relay replies in submit order.
    pub replies: Vec<DriverReply>,
    /// Final relay stats.
    pub stats: RelayStats,
}

/// Successful real-SQS run with mandatory relay and bridge-drain truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqsRunReport {
    /// Relay workload outcomes.
    pub workload: RunReport,
    /// Successful bridge close-and-drain result captured before facade shutdown.
    pub drain: SqsDrainReport,
}

/// A non-reply terminal outcome from the final stats request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatsTerminalOutcome {
    /// Relay mailbox was full.
    Full,
    /// Relay owner was closed.
    Closed,
    /// Stats deadline elapsed.
    Timeout,
    /// Runtime rejected the request.
    Rejected(CallRejectedReason),
}

/// Typed workload failure retained by [`run_to_shutdown_reported`](LocalSystem::run_to_shutdown_reported).
#[derive(Debug)]
pub enum RelayWorkloadError {
    /// The SQS bridge config, client, runtime, or Tina registration failed.
    BridgeInstall(SqsInstallError),
    /// A root service could not be registered.
    Registration {
        service: &'static str,
        source: ThreadedRuntimeError,
    },
    /// The host control plane failed while collecting stats.
    StatsHost(ThreadedRuntimeError),
    /// The stats request ended without an application reply.
    StatsTerminal(StatsTerminalOutcome),
    /// The relay answered the stats request with the wrong reply variant.
    UnexpectedStatsReply(Box<RelayReply>),
    /// Application work succeeded but accepted SQS work did not drain.
    BridgeDrain(SqsDrainReport),
    /// Application work and SQS drain both failed; neither is discarded.
    WorkloadAndBridgeDrain {
        workload: Box<RelayWorkloadError>,
        drain: SqsDrainReport,
    },
}

impl fmt::Display for RelayWorkloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BridgeInstall(source) => write!(f, "install SQS bridge: {source}"),
            Self::Registration { service, source } => {
                write!(f, "register {service}: {source}")
            }
            Self::StatsHost(source) => write!(f, "stats host call failed: {source}"),
            Self::StatsTerminal(outcome) => {
                write!(f, "stats call ended without a reply: {outcome:?}")
            }
            Self::UnexpectedStatsReply(reply) => {
                write!(f, "stats call returned unexpected reply: {reply:?}")
            }
            Self::BridgeDrain(report) => write!(
                f,
                "SQS bridge did not drain: remaining={} kinds={:?}",
                report.in_flight_remaining, report.in_flight_kinds
            ),
            Self::WorkloadAndBridgeDrain { workload, drain } => write!(
                f,
                "relay workload failed ({workload}) and SQS bridge did not drain: \
                 remaining={} kinds={:?}",
                drain.in_flight_remaining, drain.in_flight_kinds
            ),
        }
    }
}

impl Error for RelayWorkloadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::BridgeInstall(source) => Some(source),
            Self::Registration { source, .. } | Self::StatsHost(source) => Some(source),
            Self::WorkloadAndBridgeDrain { workload, .. } => Some(workload.as_ref()),
            Self::StatsTerminal(_) | Self::UnexpectedStatsReply(_) | Self::BridgeDrain(_) => None,
        }
    }
}

impl AsRef<dyn Error + Send + Sync + 'static> for RelayWorkloadError {
    fn as_ref(&self) -> &(dyn Error + Send + Sync + 'static) {
        self
    }
}

/// Terminal runner error preserving workload and shutdown truth.
pub type RelayTerminalError = RunToShutdownError<ReportedWorkloadError<RelayWorkloadError>>;

/// Complete public run error surface.
#[derive(Debug)]
pub enum RunError {
    /// Inputs were rejected before runtime construction.
    InvalidConfig(RunConfigError),
    /// The local runtime could not start.
    Startup(StartupError),
    /// Workload failure, shutdown failure, or both.
    Terminal(Box<RelayTerminalError>),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(error) => write!(f, "invalid relay configuration: {error}"),
            Self::Startup(error) => write!(f, "relay startup failed: {error}"),
            Self::Terminal(error) => write!(f, "relay run failed: {error}"),
        }
    }
}

impl Error for RunError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidConfig(error) => Some(error),
            Self::Startup(error) => Some(error),
            Self::Terminal(error) => Some(error.as_ref()),
        }
    }
}

/// Run the hermetic relay with the supplied fake-outbound script.
pub fn run(config: RunConfig) -> Result<RunReport, RunError> {
    let config = config.validate().map_err(RunError::InvalidConfig)?;
    let runtime = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
        .try_build()
        .map_err(RunError::Startup)?;
    runtime
        .run_to_shutdown_reported(SHUTDOWN_TIMEOUT, |runtime| {
            let outbound_addr = runtime
                .register_request_service::<FakeOutbound, FakeOutboundRequest, Infallible>(
                    FakeOutbound {
                        program: config.program.into(),
                    },
                    FAKE_MAILBOX_CAPACITY,
                )
                .map_err(|source| RelayWorkloadError::Registration {
                    service: "fake outbound",
                    source,
                })?;

            let relay = runtime
                .register_split_service::<Relay, RelayEvent, RelayRequest, Infallible>(
                    Relay {
                        outbound: OutboundPort::Fake(outbound_addr),
                        timeout: Duration::from_millis(config.call_timeout_ms),
                        stats: RelayStats::default(),
                    },
                    RELAY_MAILBOX_CAPACITY,
                )
                .map_err(|source| RelayWorkloadError::Registration {
                    service: "relay",
                    source,
                })?;

            drive_relay(
                runtime,
                relay.requests,
                config.events,
                config.call_timeout_ms,
            )
        })
        .map_err(|error| RunError::Terminal(Box::new(error)))
}

/// Run the relay against an SQS worker installed into the same `LocalSystem`.
/// The caller supplies bridge configuration, the queue URL, and the per-call
/// timeout. The relay forwards `events` events
/// sequentially through the bridge and returns each typed outcome
/// plus the final stats.
///
/// Hermetic tests against a fake SQS HTTP server work the same way as
/// `tina-aws-bridge`'s own integration tests; see
/// `tina-aws-bridge/tests/sqs_bridge.rs` for the `FakeSqs` shape.
pub fn run_against_sqs(
    events: usize,
    call_timeout_ms: u64,
    sqs_config: SqsConfig,
    queue_url: String,
    bridge_timeout: Duration,
) -> Result<SqsRunReport, RunError> {
    validate_event_count(events).map_err(RunError::InvalidConfig)?;
    validate_timeout_ms("call timeout", call_timeout_ms).map_err(RunError::InvalidConfig)?;
    if queue_url.trim().is_empty() {
        return Err(RunError::InvalidConfig(RunConfigError::EmptyQueueUrl));
    }
    if bridge_timeout > Duration::from_millis(MAX_CALL_TIMEOUT_MS) {
        return Err(RunError::InvalidConfig(RunConfigError::DurationTooLarge {
            field: "bridge timeout",
            requested_ms: duration_millis_ceil(bridge_timeout),
            max_ms: MAX_CALL_TIMEOUT_MS,
        }));
    }

    let runtime = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
        .try_build()
        .map_err(RunError::Startup)?;
    runtime
        .run_to_shutdown_reported(SHUTDOWN_TIMEOUT, |runtime| {
            let bridge = tina_aws_bridge::install_sqs_local(runtime, sqs_config)
                .map_err(RelayWorkloadError::BridgeInstall)?;
            let workload = runtime
                .register_split_service::<Relay, RelayEvent, RelayRequest, Infallible>(
                    Relay::new(
                        OutboundPort::Sqs(SqsOutbound {
                            address: bridge.address,
                            queue_url,
                            timeout: bridge_timeout,
                        }),
                        Duration::from_millis(call_timeout_ms),
                    ),
                    RELAY_MAILBOX_CAPACITY,
                )
                .map_err(|source| RelayWorkloadError::Registration {
                    service: "relay",
                    source,
                })
                .and_then(|relay| drive_relay(runtime, relay.requests, events, call_timeout_ms));
            let drain = bridge.closer.close_and_drain(SHUTDOWN_TIMEOUT);
            finish_sqs_workload(workload, drain)
        })
        .map_err(|error| RunError::Terminal(Box::new(error)))
}

fn duration_millis_ceil(duration: Duration) -> u128 {
    duration.as_nanos().div_ceil(1_000_000)
}

fn finish_sqs_workload(
    workload: Result<RunReport, RelayWorkloadError>,
    drain: SqsDrainReport,
) -> Result<SqsRunReport, RelayWorkloadError> {
    match (workload, drain.drained) {
        (Ok(workload), true) => Ok(SqsRunReport { workload, drain }),
        (Err(error), true) => Err(error),
        (Ok(_), false) => Err(RelayWorkloadError::BridgeDrain(drain)),
        (Err(workload), false) => Err(RelayWorkloadError::WorkloadAndBridgeDrain {
            workload: Box::new(workload),
            drain,
        }),
    }
}

fn drive_relay(
    runtime: &RelaySystem,
    relay_requests: tina::ServiceRequestAddress<RelayEvent, RelayRequest, RelayReply>,
    events: usize,
    call_timeout_ms: u64,
) -> Result<RunReport, RelayWorkloadError> {
    // Submit events sequentially so each event lines up with one
    // outbound reply. The classifier shape is independent of
    // concurrency; serializing keeps the test deterministic.
    let call_timeout = Duration::from_millis(call_timeout_ms);
    let mut replies = Vec::with_capacity(events);
    for n in 0..events {
        let outcome = runtime.call_blocking_request(
            relay_requests,
            RelayRequest::Deliver(Event {
                event_id: format!("evt-{n}"),
                body: format!("body-{n}"),
            }),
            call_timeout,
        );
        let reply = classify_driver_outcome(outcome);
        replies.push(reply);
    }

    let stats = match runtime.call_blocking_request(
        relay_requests,
        RelayRequest::Stats,
        Duration::from_secs(1),
    ) {
        Ok(CallOutcome::Replied(RelayReply::Stats(stats))) => stats,
        Ok(CallOutcome::Replied(reply)) => {
            return Err(RelayWorkloadError::UnexpectedStatsReply(Box::new(reply)));
        }
        Ok(CallOutcome::Full) => {
            return Err(RelayWorkloadError::StatsTerminal(
                StatsTerminalOutcome::Full,
            ));
        }
        Ok(CallOutcome::Closed) => {
            return Err(RelayWorkloadError::StatsTerminal(
                StatsTerminalOutcome::Closed,
            ));
        }
        Ok(CallOutcome::Timeout) => {
            return Err(RelayWorkloadError::StatsTerminal(
                StatsTerminalOutcome::Timeout,
            ));
        }
        Ok(CallOutcome::Rejected(reason)) => {
            return Err(RelayWorkloadError::StatsTerminal(
                StatsTerminalOutcome::Rejected(reason),
            ));
        }
        Err(error) => return Err(RelayWorkloadError::StatsHost(error)),
    };

    Ok(RunReport { replies, stats })
}

fn classify_driver_outcome(
    outcome: Result<CallOutcome<RelayReply>, ThreadedRuntimeError>,
) -> DriverReply {
    match outcome {
        Ok(CallOutcome::Replied(r)) => DriverReply::Reply(r),
        Ok(CallOutcome::Full) => DriverReply::OuterFull,
        Ok(CallOutcome::Closed) => DriverReply::OuterClosed,
        Ok(CallOutcome::Timeout) => DriverReply::OuterTimeout,
        Ok(CallOutcome::Rejected(reason)) => DriverReply::OuterRejected(reason),
        Err(error) => DriverReply::RuntimeError(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tina_aws_bridge::{SqsReceivedMessages, SqsSentMessage};

    fn classify(stats: &mut RelayStats, outcome: OutboundOutcome) -> RelayReply {
        classify_outbound_outcome(stats, outcome)
    }

    #[test]
    fn every_outbound_transport_and_worker_class_is_preserved() {
        let mut stats = RelayStats::default();

        assert!(matches!(
            classify(
                &mut stats,
                CallOutcome::Replied(Ok(OutboundReply {
                    backend_id: "backend".into(),
                })),
            ),
            RelayReply::Delivered { backend_id } if backend_id == "backend"
        ));

        let retryable = [
            (OutboundError::Full, BridgeRetryable::BridgeFull),
            (OutboundError::Timeout, BridgeRetryable::BridgeTimeout),
            (OutboundError::Throttled, BridgeRetryable::ServiceThrottled),
            (OutboundError::SdkTransient, BridgeRetryable::SdkRetryable),
        ];
        for (error, expected) in retryable {
            assert_eq!(
                classify(&mut stats, CallOutcome::Replied(Err(error))),
                RelayReply::Retry { reason: expected }
            );
        }

        assert_eq!(
            classify(&mut stats, CallOutcome::Replied(Err(OutboundError::Closed))),
            RelayReply::DeadLetter {
                reason: DeadLetterReason::Unavailable(BridgeUnavailable::BridgeClosed),
            }
        );

        let fatal = [
            (OutboundError::NotFound, BridgeFatal::NotFound),
            (
                OutboundError::InvalidParameter,
                BridgeFatal::InvalidParameter,
            ),
            (OutboundError::AccessDenied, BridgeFatal::AccessDenied),
            (OutboundError::InvalidRequest, BridgeFatal::InvalidRequest),
            (OutboundError::Internal, BridgeFatal::Internal),
            (OutboundError::SdkUnknown, BridgeFatal::SdkUnknown),
        ];
        for (error, expected) in fatal {
            assert_eq!(
                classify(&mut stats, CallOutcome::Replied(Err(error))),
                RelayReply::DeadLetter {
                    reason: DeadLetterReason::Fatal(expected),
                }
            );
        }

        assert_eq!(
            classify(&mut stats, CallOutcome::Full),
            RelayReply::Retry {
                reason: BridgeRetryable::BridgeFull,
            }
        );
        assert_eq!(
            classify(&mut stats, CallOutcome::Closed),
            RelayReply::DeadLetter {
                reason: DeadLetterReason::Unavailable(BridgeUnavailable::BridgeClosed),
            }
        );
        assert_eq!(
            classify(&mut stats, CallOutcome::Timeout),
            RelayReply::Retry {
                reason: BridgeRetryable::CallerTimeout,
            }
        );
        assert_eq!(
            classify(
                &mut stats,
                CallOutcome::Rejected(CallRejectedReason::HandlerPanicked),
            ),
            RelayReply::DeadLetter {
                reason: DeadLetterReason::Rejected(CallRejectedReason::HandlerPanicked),
            }
        );

        assert_eq!(
            stats,
            RelayStats {
                delivered: 1,
                transient: 6,
                dead_letter: 9,
            }
        );
    }

    #[test]
    fn driver_mapping_retains_each_outer_terminal_value() {
        let reply = RelayReply::Retry {
            reason: BridgeRetryable::CallerTimeout,
        };
        assert_eq!(
            classify_driver_outcome(Ok(CallOutcome::Replied(reply.clone()))),
            DriverReply::Reply(reply)
        );
        assert_eq!(
            classify_driver_outcome(Ok(CallOutcome::Full)),
            DriverReply::OuterFull
        );
        assert_eq!(
            classify_driver_outcome(Ok(CallOutcome::Closed)),
            DriverReply::OuterClosed
        );
        assert_eq!(
            classify_driver_outcome(Ok(CallOutcome::Timeout)),
            DriverReply::OuterTimeout
        );
        assert_eq!(
            classify_driver_outcome(Ok(CallOutcome::Rejected(
                CallRejectedReason::UnsupportedMessage,
            ))),
            DriverReply::OuterRejected(CallRejectedReason::UnsupportedMessage)
        );
        assert_eq!(
            classify_driver_outcome(Err(ThreadedRuntimeError::HostWaitTimeout)),
            DriverReply::RuntimeError(ThreadedRuntimeError::HostWaitTimeout)
        );
    }

    #[test]
    fn sqs_lifecycle_retains_workload_and_drain_failures_together() {
        let drain = SqsDrainReport {
            closed: true,
            drained: false,
            in_flight_remaining: 1,
            in_flight_kinds: vec![("send_message", 1)],
        };
        let error = finish_sqs_workload(
            Err(RelayWorkloadError::StatsTerminal(
                StatsTerminalOutcome::Timeout,
            )),
            drain,
        )
        .expect_err("both failures must remain visible");
        assert!(matches!(
            error,
            RelayWorkloadError::WorkloadAndBridgeDrain {
                workload,
                drain: SqsDrainReport {
                    drained: false,
                    in_flight_remaining: 1,
                    ..
                },
            } if matches!(
                workload.as_ref(),
                RelayWorkloadError::StatsTerminal(StatsTerminalOutcome::Timeout)
            )
        ));
    }

    #[test]
    fn missing_sqs_message_id_is_typed_internal_failure() {
        let mapped = map_sqs_outcome(CallOutcome::Replied(Ok(SqsResponse::SentMessage(
            SqsSentMessage {
                message_id: None,
                md5_of_body: Some("md5".into()),
                sequence_number: None,
            },
        ))));
        assert_eq!(mapped, CallOutcome::Replied(Err(OutboundError::Internal)));
    }

    #[test]
    fn wrong_sqs_success_shape_is_typed_invalid_request() {
        let mapped = map_sqs_outcome(CallOutcome::Replied(Ok(SqsResponse::ReceivedMessages(
            SqsReceivedMessages { messages: vec![] },
        ))));
        assert_eq!(
            mapped,
            CallOutcome::Replied(Err(OutboundError::InvalidRequest))
        );
    }

    #[test]
    fn config_rejects_every_unbounded_input_before_startup() {
        assert!(matches!(
            RunConfig {
                events: MAX_EVENTS + 1,
                call_timeout_ms: 1,
                program: vec![],
            }
            .validate(),
            Err(RunConfigError::TooManyEvents { .. })
        ));
        assert!(matches!(
            RunConfig {
                events: 0,
                call_timeout_ms: 1,
                program: vec![
                    FakeOutboundProgram::Fail(OutboundError::Internal);
                    MAX_PROGRAM_ENTRIES + 1
                ],
            }
            .validate(),
            Err(RunConfigError::TooManyProgramEntries { .. })
        ));
        assert!(matches!(
            RunConfig {
                events: 1,
                call_timeout_ms: 1,
                program: vec![],
            }
            .validate(),
            Err(RunConfigError::ProgramLengthMismatch { .. })
        ));
        assert!(matches!(
            RunConfig {
                events: 0,
                call_timeout_ms: MAX_CALL_TIMEOUT_MS + 1,
                program: vec![],
            }
            .validate(),
            Err(RunConfigError::DurationTooLarge { .. })
        ));
    }

    #[test]
    fn reported_runner_shuts_down_after_early_workload_error() {
        let runtime = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
            .try_build()
            .expect("runtime starts");
        let result = runtime.run_to_shutdown_reported(SHUTDOWN_TIMEOUT, |_runtime| {
            Err::<(), _>(RelayWorkloadError::StatsTerminal(
                StatsTerminalOutcome::Closed,
            ))
        });

        let Err(RunToShutdownError::Workload(error)) = result else {
            panic!("expected workload-only failure after clean shutdown");
        };
        assert!(matches!(
            error.get_ref(),
            RelayWorkloadError::StatsTerminal(StatsTerminalOutcome::Closed)
        ));
    }
}
