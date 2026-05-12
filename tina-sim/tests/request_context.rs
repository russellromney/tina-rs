//! Simulator parity for multi-turn request context.
//!
//! Mirrors the runtime `request_context.rs` tests to ensure the
//! abandoned-caller guard and RequestContext helpers behave the same
//! under virtual time.

use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Duration;

use tina::{
    Address, CallContext, Context, Effect, Isolate, Outbound, RequestContext, Shard, batch, noop,
    reply_to_request,
};
use tina_runtime::{CallOutcome, RuntimeCall, RuntimeEventKind};
use tina_sim::{MultiShardSimulator, MultiShardSimulatorConfig, Simulator, SimulatorConfig};

fn step_to_idle<S: Shard>(sim: &mut Simulator<S>) {
    while sim.step() > 0 {}
}

#[derive(Debug, Default)]
struct DefShard;

impl tina::Shard for DefShard {
    fn id(&self) -> tina::ShardId {
        tina::ShardId::new(7)
    }
}

// ---------------------------------------------------------------------------
// Multi-turn through RequestContext + reply_with_request
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProbeReply(u64);

#[derive(Debug)]
struct ProbeMsg;

#[derive(Debug)]
struct Probe;

impl Isolate for Probe {
    type Message = ProbeMsg;
    type Reply = ProbeReply;
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<ProbeMsg>;
    type Shard = DefShard;

    fn handle(
        &mut self,
        _msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(&mut self, _msg: Self::Message, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(ProbeReply(42))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SvcReply {
    Ready,
    NotReady,
}

#[derive(Debug)]
enum SvcMsg {
    Start,
    ProbeResult(RequestContext<SvcReply>, CallOutcome<ProbeReply>),
}

#[derive(Debug)]
struct Svc {
    probe: Address<ProbeMsg, ProbeReply>,
}

impl Isolate for Svc {
    type Message = SvcMsg;
    type Reply = SvcReply;
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<SvcMsg>;
    type Shard = DefShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SvcMsg::Start => noop(),
            SvcMsg::ProbeResult(req, outcome) => match outcome {
                CallOutcome::Replied(ProbeReply(val)) if val >= 10 => {
                    reply_to_request(req, SvcReply::Ready)
                }
                _ => reply_to_request(req, SvcReply::NotReady),
            },
        }
    }

    fn handle_call(&mut self, msg: Self::Message, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            SvcMsg::Start => {
                let req = call_ctx.into_request_context();
                tina_runtime::call(self.probe, ProbeMsg, Duration::from_millis(50))
                    .reply_with_request(req, SvcMsg::ProbeResult)
            }
            SvcMsg::ProbeResult(_, _) => {
                call_ctx.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

#[derive(Debug)]
enum ClientMsg {
    Start(Address<SvcMsg, SvcReply>),
    Returned(CallOutcome<SvcReply>),
}

#[derive(Debug)]
struct Client {
    out: Rc<RefCell<Vec<CallOutcome<SvcReply>>>>,
}

impl Isolate for Client {
    type Message = ClientMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<ClientMsg>;
    type Shard = DefShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ClientMsg::Start(svc) => Effect::Call(RuntimeCall::isolate_call(
                svc,
                SvcMsg::Start,
                Duration::from_millis(100),
                ClientMsg::Returned,
            )),
            ClientMsg::Returned(outcome) => {
                self.out.borrow_mut().push(outcome);
                noop()
            }
        }
    }
}

fn count_kind(
    events: &[tina_runtime::RuntimeEvent],
    pred: impl Fn(&RuntimeEventKind) -> bool,
) -> usize {
    events.iter().filter(|e| pred(&e.kind())).count()
}

#[test]
fn sim_request_context_multi_turn_call_replies_to_original_caller() {
    let mut sim = Simulator::new(DefShard, SimulatorConfig::default());
    let probe = sim.register(Probe);
    let svc = sim.register(Svc { probe });
    let out = Rc::new(RefCell::new(Vec::new()));
    let caller = sim.register(Client {
        out: Rc::clone(&out),
    });

    sim.try_send(caller, ClientMsg::Start(svc)).unwrap();
    step_to_idle(&mut sim);

    assert_eq!(
        out.borrow().as_slice(),
        [CallOutcome::Replied(SvcReply::Ready)]
    );

    assert_eq!(
        count_kind(sim.trace(), |k| matches!(
            k,
            RuntimeEventKind::DeferredReplyCaptured { .. }
        )),
        1
    );
    assert_eq!(
        count_kind(sim.trace(), |k| matches!(
            k,
            RuntimeEventKind::DeferredReplySent { .. }
        )),
        1
    );
}

// ---------------------------------------------------------------------------
// Abandoned caller in simulation
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum AbandonMsg {
    Noop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AbandonReply;

#[derive(Debug)]
struct AbandonSvc;

impl Isolate for AbandonSvc {
    type Message = AbandonMsg;
    type Reply = AbandonReply;
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<AbandonMsg>;
    type Shard = DefShard;

    fn handle(
        &mut self,
        _msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(&mut self, _msg: Self::Message, _call: CallContext<'_, Self>) -> Effect<Self> {
        noop()
    }
}

#[derive(Debug)]
enum AbandonClientMsg {
    Start(Address<AbandonMsg, AbandonReply>),
    Returned(CallOutcome<AbandonReply>),
}

#[derive(Debug)]
struct AbandonClient {
    out: Rc<RefCell<Vec<CallOutcome<AbandonReply>>>>,
}

impl Isolate for AbandonClient {
    type Message = AbandonClientMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<AbandonClientMsg>;
    type Shard = DefShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            AbandonClientMsg::Start(svc) => Effect::Call(RuntimeCall::isolate_call(
                svc,
                AbandonMsg::Noop,
                Duration::from_secs(60),
                AbandonClientMsg::Returned,
            )),
            AbandonClientMsg::Returned(outcome) => {
                self.out.borrow_mut().push(outcome);
                noop()
            }
        }
    }
}

#[test]
fn sim_abandoned_caller_rejects_immediately() {
    let mut sim = Simulator::new(DefShard, SimulatorConfig::default());
    let svc = sim.register(AbandonSvc);
    let out = Rc::new(RefCell::new(Vec::new()));
    let caller = sim.register(AbandonClient {
        out: Rc::clone(&out),
    });

    sim.try_send(caller, AbandonClientMsg::Start(svc)).unwrap();
    step_to_idle(&mut sim);

    assert_eq!(
        count_kind(sim.trace(), |k| matches!(
            k,
            RuntimeEventKind::CallReplyAbandoned { .. }
        )),
        1
    );

    assert_eq!(
        out.borrow().as_slice(),
        [CallOutcome::Rejected(
            tina::CallRejectedReason::ReplyAbandoned
        )]
    );
}

// ---------------------------------------------------------------------------
// Immediate reply is not falsely flagged abandoned in sim
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum ImmMsg {
    Ping,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ImmReply {
    Pong,
}

#[derive(Debug)]
struct ImmSvc;

impl Isolate for ImmSvc {
    type Message = ImmMsg;
    type Reply = ImmReply;
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<ImmMsg>;
    type Shard = DefShard;

    fn handle(
        &mut self,
        _msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(&mut self, _msg: Self::Message, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(ImmReply::Pong)
    }
}

#[derive(Debug)]
enum ImmClientMsg {
    Start(Address<ImmMsg, ImmReply>),
    Returned(CallOutcome<ImmReply>),
}

#[derive(Debug)]
struct ImmClient {
    out: Rc<RefCell<Vec<CallOutcome<ImmReply>>>>,
}

impl Isolate for ImmClient {
    type Message = ImmClientMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<ImmClientMsg>;
    type Shard = DefShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ImmClientMsg::Start(svc) => Effect::Call(RuntimeCall::isolate_call(
                svc,
                ImmMsg::Ping,
                Duration::from_secs(60),
                ImmClientMsg::Returned,
            )),
            ImmClientMsg::Returned(outcome) => {
                self.out.borrow_mut().push(outcome);
                noop()
            }
        }
    }
}

#[test]
fn sim_immediate_reply_is_not_falsely_flagged_abandoned() {
    let mut sim = Simulator::new(DefShard, SimulatorConfig::default());
    let svc = sim.register(ImmSvc);
    let out = Rc::new(RefCell::new(Vec::new()));
    let caller = sim.register(ImmClient {
        out: Rc::clone(&out),
    });

    sim.try_send(caller, ImmClientMsg::Start(svc)).unwrap();
    step_to_idle(&mut sim);

    assert_eq!(
        out.borrow().as_slice(),
        [CallOutcome::Replied(ImmReply::Pong)]
    );

    assert_eq!(
        count_kind(sim.trace(), |k| matches!(
            k,
            RuntimeEventKind::CallReplyAbandoned { .. }
        )),
        0
    );
}

// ---------------------------------------------------------------------------
// Batched reply and abandoned-context returned effects in sim
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct AuditMsg(&'static str);

#[derive(Debug)]
struct Audit {
    seen: Rc<RefCell<Vec<&'static str>>>,
}

impl Isolate for Audit {
    type Message = AuditMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<AuditMsg>;
    type Shard = DefShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        self.seen.borrow_mut().push(msg.0);
        noop()
    }
}

#[derive(Debug)]
enum BatchMsg {
    BatchedReply,
    AbandonButSend,
    ExplicitReject,
    ReplyThenReject,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BatchReply {
    Ok,
}

#[derive(Debug)]
struct BatchSvc {
    audit: Address<AuditMsg>,
}

impl Isolate for BatchSvc {
    type Message = BatchMsg;
    type Reply = BatchReply;
    type Send = Outbound<AuditMsg>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<BatchMsg>;
    type Shard = DefShard;

    fn handle(
        &mut self,
        _msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(&mut self, msg: Self::Message, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            BatchMsg::BatchedReply => batch([
                call.reply(BatchReply::Ok),
                tina::send(self.audit, AuditMsg("batched")),
            ]),
            BatchMsg::AbandonButSend => tina::send(self.audit, AuditMsg("abandoned")),
            BatchMsg::ExplicitReject => call.reject(tina::CallRejectedReason::UnsupportedMessage),
            BatchMsg::ReplyThenReject => batch([
                call.reply(BatchReply::Ok),
                tina::reject(tina::CallRejectedReason::UnsupportedMessage),
            ]),
        }
    }
}

#[derive(Debug)]
enum BatchClientMsg {
    Start(Address<BatchMsg, BatchReply>, BatchMsg),
    Returned(CallOutcome<BatchReply>),
}

#[derive(Debug)]
struct BatchClient {
    out: Rc<RefCell<Vec<CallOutcome<BatchReply>>>>,
}

impl Isolate for BatchClient {
    type Message = BatchClientMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<BatchClientMsg>;
    type Shard = DefShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            BatchClientMsg::Start(svc, request) => Effect::Call(RuntimeCall::isolate_call(
                svc,
                request,
                Duration::from_secs(60),
                BatchClientMsg::Returned,
            )),
            BatchClientMsg::Returned(outcome) => {
                self.out.borrow_mut().push(outcome);
                noop()
            }
        }
    }
}

#[test]
fn sim_batched_reply_consumes_call_authority_and_runs_later_effects() {
    let mut sim = Simulator::new(DefShard, SimulatorConfig::default());
    let audit_seen = Rc::new(RefCell::new(Vec::new()));
    let audit = sim.register(Audit {
        seen: Rc::clone(&audit_seen),
    });
    let svc = sim.register(BatchSvc { audit });
    let out = Rc::new(RefCell::new(Vec::new()));
    let caller = sim.register(BatchClient {
        out: Rc::clone(&out),
    });

    sim.try_send(caller, BatchClientMsg::Start(svc, BatchMsg::BatchedReply))
        .unwrap();
    step_to_idle(&mut sim);

    assert_eq!(
        out.borrow().as_slice(),
        [CallOutcome::Replied(BatchReply::Ok)]
    );
    assert_eq!(audit_seen.borrow().as_slice(), ["batched"]);
    assert_eq!(
        count_kind(sim.trace(), |k| matches!(
            k,
            RuntimeEventKind::CallReplyAbandoned { .. }
        )),
        0
    );
}

#[test]
fn sim_unused_call_authority_rejects_but_returned_effect_still_runs() {
    let mut sim = Simulator::new(DefShard, SimulatorConfig::default());
    let audit_seen = Rc::new(RefCell::new(Vec::new()));
    let audit = sim.register(Audit {
        seen: Rc::clone(&audit_seen),
    });
    let svc = sim.register(BatchSvc { audit });
    let out = Rc::new(RefCell::new(Vec::new()));
    let caller = sim.register(BatchClient {
        out: Rc::clone(&out),
    });

    sim.try_send(caller, BatchClientMsg::Start(svc, BatchMsg::AbandonButSend))
        .unwrap();
    step_to_idle(&mut sim);

    assert_eq!(
        out.borrow().as_slice(),
        [CallOutcome::Rejected(
            tina::CallRejectedReason::ReplyAbandoned
        )]
    );
    assert_eq!(audit_seen.borrow().as_slice(), ["abandoned"]);
}

#[test]
fn sim_explicit_reject_uses_rejected_trace_vocabulary() {
    let mut sim = Simulator::new(DefShard, SimulatorConfig::default());
    let audit = sim.register(Audit {
        seen: Rc::new(RefCell::new(Vec::new())),
    });
    let svc = sim.register(BatchSvc { audit });
    let out = Rc::new(RefCell::new(Vec::new()));
    let caller = sim.register(BatchClient {
        out: Rc::clone(&out),
    });

    sim.try_send(caller, BatchClientMsg::Start(svc, BatchMsg::ExplicitReject))
        .unwrap();
    step_to_idle(&mut sim);

    assert_eq!(
        out.borrow().as_slice(),
        [CallOutcome::Rejected(
            tina::CallRejectedReason::UnsupportedMessage
        )]
    );
    assert_eq!(
        count_kind(sim.trace(), |k| matches!(
            k,
            RuntimeEventKind::CallReplyAbandoned { .. }
        )),
        0
    );
    assert_eq!(
        count_kind(sim.trace(), |k| matches!(
            k,
            RuntimeEventKind::CallRejected {
                reason: tina::CallRejectedReason::UnsupportedMessage,
                ..
            }
        )),
        1
    );
}

#[test]
fn sim_batch_reply_consumes_authority_before_later_reject() {
    let mut sim = Simulator::new(DefShard, SimulatorConfig::default());
    let audit = sim.register(Audit {
        seen: Rc::new(RefCell::new(Vec::new())),
    });
    let svc = sim.register(BatchSvc { audit });
    let out = Rc::new(RefCell::new(Vec::new()));
    let caller = sim.register(BatchClient {
        out: Rc::clone(&out),
    });

    sim.try_send(
        caller,
        BatchClientMsg::Start(svc, BatchMsg::ReplyThenReject),
    )
    .unwrap();
    step_to_idle(&mut sim);

    assert_eq!(
        out.borrow().as_slice(),
        [CallOutcome::Replied(BatchReply::Ok)]
    );
    assert_eq!(
        count_kind(sim.trace(), |k| matches!(
            k,
            RuntimeEventKind::CallReplyRejected { .. }
                | RuntimeEventKind::CallRejected { .. }
                | RuntimeEventKind::CallReplyAbandoned { .. }
        )),
        0
    );
}

// ---------------------------------------------------------------------------
// Cross-shard request context promotion
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct NumberedShard(u32);

impl tina::Shard for NumberedShard {
    fn id(&self) -> tina::ShardId {
        tina::ShardId::new(self.0)
    }
}

#[derive(Debug)]
struct CrossProbeMsg;

#[derive(Debug, Clone, PartialEq, Eq)]
struct CrossProbeReply(u64);

#[derive(Debug)]
struct CrossProbe;

impl Isolate for CrossProbe {
    type Message = CrossProbeMsg;
    type Reply = CrossProbeReply;
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<CrossProbeMsg>;
    type Shard = NumberedShard;

    fn handle(
        &mut self,
        _msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(&mut self, _msg: Self::Message, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(CrossProbeReply(42))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CrossSvcReply {
    Ready,
}

#[derive(Debug)]
enum CrossSvcMsg {
    Start,
    RejectNow,
    ProbeResult(RequestContext<CrossSvcReply>, CallOutcome<CrossProbeReply>),
}

#[derive(Debug)]
struct CrossSvc {
    probe: Address<CrossProbeMsg, CrossProbeReply>,
}

impl Isolate for CrossSvc {
    type Message = CrossSvcMsg;
    type Reply = CrossSvcReply;
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<CrossSvcMsg>;
    type Shard = NumberedShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CrossSvcMsg::Start => noop(),
            CrossSvcMsg::RejectNow => noop(),
            CrossSvcMsg::ProbeResult(req, CallOutcome::Replied(CrossProbeReply(42))) => {
                reply_to_request(req, CrossSvcReply::Ready)
            }
            CrossSvcMsg::ProbeResult(req, _) => reply_to_request(req, CrossSvcReply::Ready),
        }
    }

    fn handle_call(&mut self, msg: Self::Message, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            CrossSvcMsg::Start => {
                let req = call.into_request_context();
                tina_runtime::call(self.probe, CrossProbeMsg, Duration::from_millis(50))
                    .reply_with_request(req, CrossSvcMsg::ProbeResult)
            }
            CrossSvcMsg::RejectNow => call.reject(tina::CallRejectedReason::UnsupportedMessage),
            CrossSvcMsg::ProbeResult(_, _) => {
                call.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

#[derive(Debug)]
enum CrossClientMsg {
    Start(Address<CrossSvcMsg, CrossSvcReply>),
    StartWith(Address<CrossSvcMsg, CrossSvcReply>, CrossSvcMsg),
    Returned(CallOutcome<CrossSvcReply>),
}

#[derive(Debug)]
struct CrossClient {
    out: Rc<RefCell<Vec<CallOutcome<CrossSvcReply>>>>,
}

impl Isolate for CrossClient {
    type Message = CrossClientMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<CrossClientMsg>;
    type Shard = NumberedShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CrossClientMsg::Start(svc) => Effect::Call(RuntimeCall::isolate_call(
                svc,
                CrossSvcMsg::Start,
                Duration::from_millis(100),
                CrossClientMsg::Returned,
            )),
            CrossClientMsg::StartWith(svc, request) => Effect::Call(RuntimeCall::isolate_call(
                svc,
                request,
                Duration::from_millis(100),
                CrossClientMsg::Returned,
            )),
            CrossClientMsg::Returned(outcome) => {
                self.out.borrow_mut().push(outcome);
                noop()
            }
        }
    }
}

#[test]
fn sim_cross_shard_call_can_promote_into_request_context_and_reply_later() {
    let mut sim = MultiShardSimulator::with_config(
        [NumberedShard(71), NumberedShard(72)],
        SimulatorConfig::default(),
        MultiShardSimulatorConfig::default(),
    );
    let probe = sim
        .register_with_capacity_on::<CrossProbe, CrossProbeMsg, Infallible>(
            tina::ShardId::new(72),
            CrossProbe,
            4,
        )
        .with_reply::<CrossProbeReply>();
    let svc = sim.register_with_capacity_on::<CrossSvc, CrossSvcMsg, Infallible>(
        tina::ShardId::new(72),
        CrossSvc { probe },
        8,
    );
    let out = Rc::new(RefCell::new(Vec::new()));
    let caller = sim.register_with_capacity_on::<CrossClient, CrossClientMsg, Infallible>(
        tina::ShardId::new(71),
        CrossClient {
            out: Rc::clone(&out),
        },
        8,
    );

    sim.try_send(caller, CrossClientMsg::Start(svc)).unwrap();
    sim.run_until_quiescent();

    assert_eq!(
        out.borrow().as_slice(),
        [CallOutcome::Replied(CrossSvcReply::Ready)]
    );
    let trace = sim.trace();
    assert_eq!(
        count_kind(&trace, |k| matches!(
            k,
            RuntimeEventKind::CallReplyAbandoned { .. } | RuntimeEventKind::CallRejected { .. }
        )),
        0
    );
    assert_eq!(
        count_kind(&trace, |k| matches!(
            k,
            RuntimeEventKind::DeferredReplyCaptured { .. }
        )),
        1
    );
}

#[test]
fn sim_cross_shard_rejected_reason_is_preserved() {
    let mut sim = MultiShardSimulator::with_config(
        [NumberedShard(81), NumberedShard(82)],
        SimulatorConfig::default(),
        MultiShardSimulatorConfig::default(),
    );
    let probe = sim
        .register_with_capacity_on::<CrossProbe, CrossProbeMsg, Infallible>(
            tina::ShardId::new(82),
            CrossProbe,
            4,
        )
        .with_reply::<CrossProbeReply>();
    let svc = sim.register_with_capacity_on::<CrossSvc, CrossSvcMsg, Infallible>(
        tina::ShardId::new(82),
        CrossSvc { probe },
        8,
    );
    let out = Rc::new(RefCell::new(Vec::new()));
    let caller = sim.register_with_capacity_on::<CrossClient, CrossClientMsg, Infallible>(
        tina::ShardId::new(81),
        CrossClient {
            out: Rc::clone(&out),
        },
        8,
    );

    sim.try_send(
        caller,
        CrossClientMsg::StartWith(svc, CrossSvcMsg::RejectNow),
    )
    .unwrap();
    sim.run_until_quiescent();

    assert_eq!(
        out.borrow().as_slice(),
        [CallOutcome::Rejected(
            tina::CallRejectedReason::UnsupportedMessage
        )]
    );
    let trace = sim.trace();
    assert_eq!(
        count_kind(&trace, |k| matches!(
            k,
            RuntimeEventKind::CallRejected {
                reason: tina::CallRejectedReason::UnsupportedMessage,
                ..
            }
        )),
        1
    );
    assert_eq!(
        count_kind(&trace, |k| matches!(
            k,
            RuntimeEventKind::CallReplyAbandoned { .. }
        )),
        0
    );
}
