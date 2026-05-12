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
    Address, Context, Effect, Isolate, Outbound, RequestContext, Shard, noop, reply,
    reply_to_request,
};
use tina_runtime::{CallOutcome, RuntimeCall, RuntimeEventKind};
use tina_sim::{Simulator, SimulatorConfig};

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
    type Call = RuntimeCall<ProbeMsg>;
    type Shard = DefShard;

    fn handle(
        &mut self,
        _msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        reply(ProbeReply(42))
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
    type Call = RuntimeCall<SvcMsg>;
    type Shard = DefShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SvcMsg::Start => {
                let req = ctx.take_request_context().unwrap();
                tina_runtime::call(self.probe, ProbeMsg, Duration::from_millis(50))
                    .reply_with_request(req, SvcMsg::ProbeResult)
            }
            SvcMsg::ProbeResult(req, outcome) => match outcome {
                CallOutcome::Replied(ProbeReply(val)) if val >= 10 => {
                    reply_to_request(req, SvcReply::Ready)
                }
                _ => reply_to_request(req, SvcReply::NotReady),
            },
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
    type Call = RuntimeCall<AbandonMsg>;
    type Shard = DefShard;

    fn handle(
        &mut self,
        _msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
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
fn sim_abandoned_caller_traces_warning_but_times_out() {
    let mut sim = Simulator::new(DefShard, SimulatorConfig::default());
    let svc = sim.register(AbandonSvc);
    let out = Rc::new(RefCell::new(Vec::new()));
    let caller = sim.register(AbandonClient {
        out: Rc::clone(&out),
    });

    sim.try_send(caller, AbandonClientMsg::Start(svc)).unwrap();
    step_to_idle(&mut sim);

    // Guard emitted trace warning but did not close the call.
    assert_eq!(
        count_kind(sim.trace(), |k| matches!(
            k,
            RuntimeEventKind::CallReplyAbandoned { .. }
        )),
        1
    );

    // Advance virtual time past timeout to trigger the timeout.
    sim.advance_time(Duration::from_secs(61));
    step_to_idle(&mut sim);

    assert_eq!(out.borrow().as_slice(), [CallOutcome::Timeout]);
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
    type Call = RuntimeCall<ImmMsg>;
    type Shard = DefShard;

    fn handle(
        &mut self,
        _msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        reply(ImmReply::Pong)
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
