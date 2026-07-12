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
use std::sync::Arc;
use std::time::Duration;

use tina::prelude::*;
use tina_aws_bridge::{
    BridgeFatal, BridgeOutcomeClass, BridgeRetryable, BridgeUnavailable, SqsAddress, SqsError,
    SqsRequest, SqsResponse, SqsSendMessage, send_sqs,
};
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime};

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
            Self::Throttled => {
                BridgeOutcomeClass::Retryable(BridgeRetryable::ServiceThrottled)
            }
            Self::SdkTransient => {
                BridgeOutcomeClass::Retryable(BridgeRetryable::SdkRetryable)
            }
            Self::NotFound => BridgeOutcomeClass::Fatal(BridgeFatal::NotFound),
            Self::InvalidParameter => {
                BridgeOutcomeClass::Fatal(BridgeFatal::InvalidParameter)
            }
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
    /// Tina-side `CallError`: target was unknown, full, or closed
    /// before the call left the relay.
    CallError(String),
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
        CallOutcome::Replied(Ok(SqsResponse::SentMessage(sent))) => {
            CallOutcome::Replied(Ok(OutboundReply {
                backend_id: sent.message_id.unwrap_or_default(),
            }))
        }
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
        SqsError::Sdk(_) => OutboundError::SdkTransient,
        SqsError::Internal(_) => OutboundError::Internal,
    }
}

/// Outbound port behind the relay. Use `OutboundPort::fake` for
/// hermetic tests or `OutboundPort::sqs` to point the relay at a
/// real `tina-aws-bridge` SQS worker.
pub enum OutboundPort {
    /// In-process fake outbound (the program is a `VecDeque` of replies).
    Fake(Address<FakeOutboundMsg, Result<OutboundReply, OutboundError>>),
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
            OutboundPort::Fake(addr) => call
                .defer(tina_runtime::call(
                    *addr,
                    FakeOutboundMsg::Send(event),
                    timeout,
                ))
                .reply_service_event(|request, outcome| {
                    RelayEvent::FakeFinished { request, outcome }
                }),
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
                call.defer(issued).reply_service_event(|request, outcome| {
                    RelayEvent::SqsFinished { request, outcome }
                })
            }
        }
    }
}

impl Relay {
    fn classify_and_tally(&mut self, outcome: OutboundOutcome) -> RelayReply {
        match &outcome {
            CallOutcome::Replied(Ok(ok)) => {
                self.stats.delivered += 1;
                RelayReply::Delivered {
                    backend_id: ok.backend_id.clone(),
                }
            }
            CallOutcome::Replied(Err(err)) => match err.classify() {
                BridgeOutcomeClass::Succeeded => unreachable!(),
                BridgeOutcomeClass::Retryable(reason) => {
                    self.stats.transient += 1;
                    RelayReply::Retry { reason }
                }
                BridgeOutcomeClass::Unavailable(reason) => {
                    self.stats.dead_letter += 1;
                    RelayReply::DeadLetter {
                        reason: DeadLetterReason::Unavailable(reason),
                    }
                }
                BridgeOutcomeClass::Fatal(reason) => {
                    self.stats.dead_letter += 1;
                    RelayReply::DeadLetter {
                        reason: DeadLetterReason::Fatal(reason),
                    }
                }
            },
            CallOutcome::Full => {
                self.stats.transient += 1;
                RelayReply::Retry {
                    reason: BridgeRetryable::BridgeFull,
                }
            }
            CallOutcome::Closed => {
                self.stats.dead_letter += 1;
                RelayReply::DeadLetter {
                    reason: DeadLetterReason::Unavailable(BridgeUnavailable::BridgeClosed),
                }
            }
            CallOutcome::Timeout => {
                self.stats.transient += 1;
                RelayReply::Retry {
                    reason: BridgeRetryable::CallerTimeout,
                }
            }
            CallOutcome::Rejected(_) => {
                self.stats.dead_letter += 1;
                RelayReply::DeadLetter {
                    reason: DeadLetterReason::Fatal(BridgeFatal::InvalidRequest),
                }
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
}

/// Messages handled by [`FakeOutbound`].
#[derive(Debug)]
pub enum FakeOutboundMsg {
    /// Caller asks the fake outbound to deliver one event.
    Send(Event),
}

/// In-process outbound used by hermetic tests. The script of replies
/// is supplied at construction; once exhausted, subsequent calls reply
/// with `OutboundError::Internal`.
pub struct FakeOutbound {
    program: std::collections::VecDeque<FakeOutboundProgram>,
}

#[tina_runtime::isolate(message = FakeOutboundMsg, reply = Result<OutboundReply, OutboundError>)]
impl FakeOutbound {
    fn handle(
        &mut self,
        _msg: FakeOutboundMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(&mut self, msg: FakeOutboundMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            FakeOutboundMsg::Send(_event) => {
                let next = self
                    .program
                    .pop_front()
                    .unwrap_or(FakeOutboundProgram::Fail(OutboundError::Internal));
                let reply = match next {
                    FakeOutboundProgram::Deliver(id) => Ok(OutboundReply { backend_id: id }),
                    FakeOutboundProgram::Fail(err) => Err(err),
                };
                call.reply(reply)
            }
        }
    }
}

// ---------- Driver ----------

/// Configuration for [`run`].
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// Number of events to submit concurrently.
    pub events: usize,
    /// Per-call deadline.
    pub call_timeout_ms: u64,
    /// Fake outbound program (one entry per event).
    pub program: Vec<FakeOutboundProgram>,
}

/// Driver outcome surface — structured `CallOutcome` and runtime
/// error visibility so failed test fixtures are debuggable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverReply {
    /// Relay returned a typed reply.
    Reply(RelayReply),
    /// `call_blocking` returned a runtime-level error.
    RuntimeError(String),
    /// Caller-side `CallOutcome::Full` — relay's mailbox was saturated.
    OuterFull,
    /// Caller-side `CallOutcome::Closed` — relay was closed.
    OuterClosed,
    /// Caller-side `CallOutcome::Timeout` — relay did not reply in time.
    OuterTimeout,
    /// Caller-side `CallOutcome::Rejected` — relay rejected the call.
    OuterRejected(String),
}

/// Driver result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    /// Per-event relay replies in submit order.
    pub replies: Vec<DriverReply>,
    /// Final relay stats.
    pub stats: RelayStats,
}

/// Run the hermetic relay with the supplied fake-outbound script.
pub fn run(config: RunConfig) -> anyhow::Result<RunReport> {
    let runtime = Arc::new(ThreadedRuntime::try_new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    )?);

    let outbound_addr = runtime
        .register_with_capacity::<_, Infallible>(
            FakeOutbound {
                program: config.program.into(),
            },
            64,
        )
        .map_err(|e| anyhow::anyhow!("register fake outbound: {e:?}"))?;

    let relay = runtime
        .register_split_service::<Relay, RelayEvent, RelayRequest, Infallible>(
            Relay {
                outbound: OutboundPort::Fake(outbound_addr),
                timeout: Duration::from_millis(config.call_timeout_ms),
                stats: RelayStats::default(),
            },
            64,
        )
        .map_err(|e| anyhow::anyhow!("register relay: {e:?}"))?;

    drive_relay(&runtime, relay.requests, config.events, config.call_timeout_ms)
}

/// Run the relay against a `tina-aws-bridge` SQS worker. The caller
/// supplies an already-installed bridge address, the queue URL, and
/// the per-call timeout. The relay forwards `events` events
/// sequentially through the bridge and returns each typed outcome
/// plus the final stats.
///
/// Hermetic tests against a fake SQS HTTP server work the same way as
/// `tina-aws-bridge`'s own integration tests; see
/// `tina-aws-bridge/tests/sqs_bridge.rs` for the `FakeSqs` shape.
pub fn run_against_sqs(
    events: usize,
    call_timeout_ms: u64,
    sqs_address: tina_aws_bridge::SqsAddress,
    queue_url: String,
    bridge_timeout: Duration,
) -> anyhow::Result<RunReport> {
    let runtime = Arc::new(ThreadedRuntime::try_new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    )?);

    let relay = runtime
        .register_split_service::<Relay, RelayEvent, RelayRequest, Infallible>(
            Relay::new(
                OutboundPort::Sqs(SqsOutbound {
                    address: sqs_address,
                    queue_url,
                    timeout: bridge_timeout,
                }),
                Duration::from_millis(call_timeout_ms),
            ),
            64,
        )
        .map_err(|e| anyhow::anyhow!("register relay: {e:?}"))?;

    drive_relay(&runtime, relay.requests, events, call_timeout_ms)
}

fn drive_relay(
    runtime: &Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>,
    relay_requests: tina::ServiceRequestAddress<RelayEvent, RelayRequest, RelayReply>,
    events: usize,
    call_timeout_ms: u64,
) -> anyhow::Result<RunReport> {
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
        let reply = match outcome {
            Ok(CallOutcome::Replied(r)) => DriverReply::Reply(r),
            Ok(CallOutcome::Full) => DriverReply::OuterFull,
            Ok(CallOutcome::Closed) => DriverReply::OuterClosed,
            Ok(CallOutcome::Timeout) => DriverReply::OuterTimeout,
            Ok(CallOutcome::Rejected(r)) => DriverReply::OuterRejected(format!("{r:?}")),
            Err(e) => DriverReply::RuntimeError(format!("{e:?}")),
        };
        replies.push(reply);
    }

    let stats = match runtime
        .call_blocking_request(relay_requests, RelayRequest::Stats, Duration::from_secs(1))
        .map_err(|e| anyhow::anyhow!("stats call: {e:?}"))?
    {
        CallOutcome::Replied(RelayReply::Stats(stats)) => stats,
        other => anyhow::bail!("stats call returned unexpected reply: {other:?}"),
    };

    Ok(RunReport { replies, stats })
}
