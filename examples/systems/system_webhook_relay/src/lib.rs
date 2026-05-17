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

use tina::CallContext;
use tina::prelude::*;
use tina_aws_bridge::{
    BridgeOutcomeClass, FatalReason, SqsAddress, SqsError, SqsRequest, SqsResponse, SqsSendMessage,
    TransientReason, send_sqs,
};
use tina_runtime::{
    CallError, CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime,
};

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
            Self::Full => BridgeOutcomeClass::Transient(TransientReason::BridgeFull),
            Self::Closed => BridgeOutcomeClass::Transient(TransientReason::BridgeClosed),
            Self::Timeout => BridgeOutcomeClass::Transient(TransientReason::BridgeTimeout),
            Self::Throttled => BridgeOutcomeClass::Transient(TransientReason::ServiceThrottled),
            Self::SdkTransient => BridgeOutcomeClass::Transient(TransientReason::SdkError),
            Self::NotFound => BridgeOutcomeClass::Fatal(FatalReason::NotFound),
            Self::InvalidParameter => BridgeOutcomeClass::Fatal(FatalReason::InvalidParameter),
            Self::AccessDenied => BridgeOutcomeClass::Fatal(FatalReason::AccessDenied),
            Self::InvalidRequest => BridgeOutcomeClass::Fatal(FatalReason::InvalidRequest),
            Self::Internal => BridgeOutcomeClass::Fatal(FatalReason::Internal),
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
    Fatal(FatalReason),
    /// Tina-side `CallError`: target was unknown, full, or closed
    /// before the call left the relay.
    CallError(String),
}

/// Final relay outcome for one event (visible to callers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayReply {
    /// Outbound accepted the event.
    Delivered { backend_id: String },
    /// Transient — caller should retry under their own idempotency
    /// story. The relay does **not** retry on its own.
    Retry { reason: TransientReason },
    /// Will not succeed without input/setup change.
    DeadLetter { reason: DeadLetterReason },
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

/// Messages handled by the relay.
#[derive(Debug)]
pub enum RelayMsg {
    /// Caller asks the relay to deliver one event.
    Deliver(Event),
    /// Internal: the outbound port replied.
    #[doc(hidden)]
    Finished {
        /// Original caller request context.
        request: RequestContext<RelayReply>,
        /// Outbound port's typed outcome.
        outcome: OutboundOutcome,
    },
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

impl SqsOutbound {
    fn call_into_relay(
        &self,
        event: Event,
    ) -> tina_runtime::IsolateCall<tina_aws_bridge::SqsMsg, Result<SqsResponse, SqsError>> {
        send_sqs(
            self.address,
            SqsRequest::SendMessage(SqsSendMessage {
                queue_url: self.queue_url.clone(),
                body: event.body,
                message_group_id: Some(event.event_id),
                message_deduplication_id: None,
            }),
            self.timeout,
        )
    }
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

#[allow(dead_code)]
enum OutboundPort {
    Fake(Address<FakeOutboundMsg, Result<OutboundReply, OutboundError>>),
    Sqs(SqsOutbound),
}

/// Webhook relay isolate. Forwards events to its outbound port and
/// classifies each outcome.
pub struct Relay {
    outbound: OutboundPort,
    timeout: Duration,
    stats: RelayStats,
}

impl Relay {
    fn issue(&mut self, event: Event, call: CallContext<'_, Self>) -> Effect<Self> {
        match &self.outbound {
            OutboundPort::Fake(addr) => {
                let timeout = self.timeout;
                call.defer(tina_runtime::call(
                    *addr,
                    FakeOutboundMsg::Send(event),
                    timeout,
                ))
                .reply(|request, outcome| RelayMsg::Finished { request, outcome })
            }
            OutboundPort::Sqs(sqs) => {
                let issued = sqs.call_into_relay(event);
                call.defer(issued)
                    .reply(|request, outcome| RelayMsg::Finished {
                        request,
                        outcome: map_sqs_outcome(outcome),
                    })
            }
        }
    }
}

#[allow(dead_code)]
fn map_call_error(err: CallError) -> OutboundError {
    match err {
        CallError::TargetClosed => OutboundError::Closed,
        CallError::TargetFull => OutboundError::Full,
        CallError::Timeout => OutboundError::Timeout,
        CallError::Rejected(_) => OutboundError::InvalidRequest,
        _ => OutboundError::Internal,
    }
}

#[tina_runtime::isolate(message = RelayMsg, reply = RelayReply)]
impl Relay {
    fn handle(
        &mut self,
        msg: RelayMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            RelayMsg::Deliver(_) | RelayMsg::Stats => noop(),
            RelayMsg::Finished { request, outcome } => {
                let reply = self.classify_and_tally(outcome);
                reply_to_request(request, reply)
            }
        }
    }

    fn handle_call(&mut self, msg: RelayMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            RelayMsg::Deliver(event) => self.issue(event, call),
            RelayMsg::Stats => call.reply(RelayReply::Delivered {
                backend_id: format!(
                    "stats(d={},t={},dl={})",
                    self.stats.delivered, self.stats.transient, self.stats.dead_letter
                ),
            }),
            RelayMsg::Finished { .. } => call.reject(tina::CallRejectedReason::UnsupportedMessage),
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
                BridgeOutcomeClass::Transient(reason) => {
                    self.stats.transient += 1;
                    RelayReply::Retry { reason }
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
                    reason: TransientReason::BridgeFull,
                }
            }
            CallOutcome::Closed => {
                self.stats.transient += 1;
                RelayReply::Retry {
                    reason: TransientReason::BridgeClosed,
                }
            }
            CallOutcome::Timeout => {
                self.stats.transient += 1;
                RelayReply::Retry {
                    reason: TransientReason::CallerTimeout,
                }
            }
            CallOutcome::Rejected(_) => {
                self.stats.dead_letter += 1;
                RelayReply::DeadLetter {
                    reason: DeadLetterReason::Fatal(FatalReason::InvalidRequest),
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

/// Driver outcome surface — keeps `CallOutcome` and runtime errors
/// visible to the host so failed test fixtures are debuggable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverReply {
    /// Relay returned a typed reply.
    Reply(RelayReply),
    /// `call_blocking` returned a runtime-level error.
    RuntimeError(String),
    /// `call_blocking` returned a non-replied outer outcome.
    CallOutcome(String),
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
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    let outbound_addr = runtime
        .register_with_capacity::<_, Infallible>(
            FakeOutbound {
                program: config.program.into(),
            },
            64,
        )
        .map_err(|e| anyhow::anyhow!("register fake outbound: {e:?}"))?;

    let relay_addr = runtime
        .register_with_capacity::<_, Infallible>(
            Relay {
                outbound: OutboundPort::Fake(outbound_addr),
                timeout: Duration::from_millis(config.call_timeout_ms),
                stats: RelayStats::default(),
            },
            64,
        )
        .map_err(|e| anyhow::anyhow!("register relay: {e:?}"))?;

    // Submit events sequentially so each event lines up with one
    // entry of the fake-outbound program. The classifier shape is
    // independent of concurrency; serializing keeps the test deterministic.
    let call_timeout = Duration::from_millis(config.call_timeout_ms);
    let mut replies = Vec::with_capacity(config.events);
    for n in 0..config.events {
        let outcome = runtime.call_blocking(
            relay_addr,
            RelayMsg::Deliver(Event {
                event_id: format!("evt-{n}"),
                body: format!("body-{n}"),
            }),
            call_timeout,
        );
        let reply = match outcome {
            Ok(CallOutcome::Replied(r)) => DriverReply::Reply(r),
            Ok(other) => DriverReply::CallOutcome(format!("{other:?}")),
            Err(e) => DriverReply::RuntimeError(format!("{e:?}")),
        };
        replies.push(reply);
    }

    let stats = match runtime
        .call_blocking(relay_addr, RelayMsg::Stats, Duration::from_secs(1))
        .map_err(|e| anyhow::anyhow!("stats call: {e:?}"))?
    {
        CallOutcome::Replied(RelayReply::Delivered { backend_id }) => {
            parse_stats_from_id(&backend_id)?
        }
        other => anyhow::bail!("stats call failed: {other:?}"),
    };

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }

    Ok(RunReport { replies, stats })
}

fn parse_stats_from_id(s: &str) -> anyhow::Result<RelayStats> {
    // Stats are encoded as "stats(d=N,t=N,dl=N)". This is purely a
    // smoke-test transport: the system_webhook_relay's stats RPC is
    // typed in real code; here we tunnel through the existing reply
    // shape to keep the driver tiny.
    let body = s
        .strip_prefix("stats(")
        .and_then(|s| s.strip_suffix(')'))
        .ok_or_else(|| anyhow::anyhow!("bad stats id: {s}"))?;
    let mut delivered = 0u64;
    let mut transient = 0u64;
    let mut dead_letter = 0u64;
    for piece in body.split(',') {
        let (k, v) = piece
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("bad piece: {piece}"))?;
        let n: u64 = v.parse()?;
        match k {
            "d" => delivered = n,
            "t" => transient = n,
            "dl" => dead_letter = n,
            _ => {}
        }
    }
    Ok(RelayStats {
        delivered,
        transient,
        dead_letter,
    })
}

// Wire `RuntimeCall<RelayMsg>` to the macro-generated isolate types.
const _: fn() = || {
    fn _check<T: tina::Isolate>() {}
    _check::<Relay>();
    let _ = std::marker::PhantomData::<RuntimeCall<RelayMsg>>;
};
