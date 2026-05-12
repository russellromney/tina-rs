//! Multi-turn request context tests.
//!
//! Covers RequestContext capture, reply_with_request helpers, and
//! abandoned-caller guard behavior. Runtime and sim must agree.

use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Duration;

use tina::{
    Address, Context, Effect, Isolate, Outbound, RequestContext, noop, reply, reply_to_request,
};

use super::*;
use crate::{CallOutcome, RuntimeCall, RuntimeEventKind, SendOutcome, call, call_with_handle};

fn step_to_idle<S, F>(runtime: &mut Runtime<S, F>)
where
    S: Shard,
    F: MailboxFactory,
{
    while runtime.step() > 0 {}
}

// ---------------------------------------------------------------------------
// Reply-through-probe multi-turn workflow
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
    type Call = Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        _msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        reply(ProbeReply(42))
    }
}

// ---------------------------------------------------------------------------
// Multi-turn through RequestContext + reply_with_request on IsolateCall
// ---------------------------------------------------------------------------

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
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SvcMsg::Start => {
                let req = ctx.take_request_context().unwrap();
                call(self.probe, ProbeMsg, Duration::from_millis(50))
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
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<ClientMsg>;
    type Shard = TestShard;

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

#[test]
fn request_context_multi_turn_call_replies_to_original_caller() {
    let (mut runtime, _clock) = new_manual_runtime();
    let probe = runtime.register(Probe, TestMailbox::new(4));
    let svc = runtime.register(Svc { probe }, TestMailbox::new(8));
    let out = Rc::new(RefCell::new(Vec::new()));
    let caller = runtime.register(
        Client {
            out: Rc::clone(&out),
        },
        TestMailbox::new(4),
    );

    runtime.try_send(caller, ClientMsg::Start(svc)).unwrap();
    step_to_idle(&mut runtime);

    assert_eq!(
        out.borrow().as_slice(),
        [CallOutcome::Replied(SvcReply::Ready)]
    );

    let captured = runtime
        .trace()
        .iter()
        .filter(|e| matches!(e.kind(), RuntimeEventKind::DeferredReplyCaptured { .. }))
        .count();
    assert_eq!(captured, 1);

    let sent = runtime
        .trace()
        .iter()
        .filter(|e| matches!(e.kind(), RuntimeEventKind::DeferredReplySent { .. }))
        .count();
    assert_eq!(sent, 1);
}

// ---------------------------------------------------------------------------
// reply_with_request on IsolateCallWithHandle
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct HandleSvc {
    probe: Address<ProbeMsg, ProbeReply>,
}

#[derive(Debug)]
enum HandleSvcMsg {
    Start,
    ProbeResult(RequestContext<SvcReply>, CallOutcome<ProbeReply>),
}

impl Isolate for HandleSvc {
    type Message = HandleSvcMsg;
    type Reply = SvcReply;
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<HandleSvcMsg>;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            HandleSvcMsg::Start => {
                let req = ctx.take_request_context().unwrap();
                let (effect, _handle) =
                    call_with_handle(self.probe, ProbeMsg, Duration::from_millis(50))
                        .reply_with_request(req, HandleSvcMsg::ProbeResult);
                effect
            }
            HandleSvcMsg::ProbeResult(req, outcome) => match outcome {
                CallOutcome::Replied(ProbeReply(val)) if val >= 10 => {
                    reply_to_request(req, SvcReply::Ready)
                }
                _ => reply_to_request(req, SvcReply::NotReady),
            },
        }
    }
}

#[test]
fn isolate_call_with_handle_reply_with_request_replies_to_original_caller() {
    let (mut runtime, _clock) = new_manual_runtime();
    let probe = runtime.register(Probe, TestMailbox::new(4));
    let svc = runtime.register(HandleSvc { probe }, TestMailbox::new(8));
    let out = Rc::new(RefCell::new(Vec::new()));

    #[derive(Debug)]
    enum HClientMsg {
        Start(Address<HandleSvcMsg, SvcReply>),
        Returned(CallOutcome<SvcReply>),
    }

    #[derive(Debug)]
    struct HClient {
        out: Rc<RefCell<Vec<CallOutcome<SvcReply>>>>,
    }

    impl Isolate for HClient {
        type Message = HClientMsg;
        type Reply = ();
        type Send = Outbound<Infallible>;
        type Spawn = Infallible;
        type SpawnObserved = std::convert::Infallible;
        type Call = RuntimeCall<HClientMsg>;
        type Shard = TestShard;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                HClientMsg::Start(svc) => Effect::Call(RuntimeCall::isolate_call(
                    svc,
                    HandleSvcMsg::Start,
                    Duration::from_millis(100),
                    HClientMsg::Returned,
                )),
                HClientMsg::Returned(outcome) => {
                    self.out.borrow_mut().push(outcome);
                    noop()
                }
            }
        }
    }

    let caller = runtime.register(
        HClient {
            out: Rc::clone(&out),
        },
        TestMailbox::new(4),
    );

    runtime.try_send(caller, HClientMsg::Start(svc)).unwrap();
    step_to_idle(&mut runtime);

    assert_eq!(
        out.borrow().as_slice(),
        [CallOutcome::Replied(SvcReply::Ready)]
    );
}

// ---------------------------------------------------------------------------
// Abandoned caller settles with Closed + CallReplyAbandoned trace
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
    type Call = Infallible;
    type Shard = TestShard;

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
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<AbandonClientMsg>;
    type Shard = TestShard;

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
fn abandoned_caller_traces_warning_but_times_out() {
    let (mut runtime, clock) = new_manual_runtime();
    let svc = runtime.register(AbandonSvc, TestMailbox::new(4));
    let out = Rc::new(RefCell::new(Vec::new()));
    let caller = runtime.register(
        AbandonClient {
            out: Rc::clone(&out),
        },
        TestMailbox::new(4),
    );

    runtime
        .try_send(caller, AbandonClientMsg::Start(svc))
        .unwrap();
    step_to_idle(&mut runtime);

    // Guard emitted trace warning but did not close the call.
    assert_eq!(
        runtime
            .trace()
            .iter()
            .filter(|e| matches!(e.kind(), RuntimeEventKind::CallReplyAbandoned { .. }))
            .count(),
        1
    );

    // Advance clock past timeout to trigger the timeout.
    clock.advance(Duration::from_secs(61));
    step_to_idle(&mut runtime);

    assert_eq!(out.borrow().as_slice(), [CallOutcome::Timeout]);

    let abandoned = runtime
        .trace()
        .iter()
        .filter(|e| matches!(e.kind(), RuntimeEventKind::CallReplyAbandoned { .. }))
        .count();
    assert_eq!(abandoned, 1);
}

// ---------------------------------------------------------------------------
// Immediate reply is not falsely flagged abandoned
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum ImmediateMsg {
    Ping,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ImmediateReply {
    Pong,
}

#[derive(Debug)]
struct ImmediateSvc;

impl Isolate for ImmediateSvc {
    type Message = ImmediateMsg;
    type Reply = ImmediateReply;
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        _msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        reply(ImmediateReply::Pong)
    }
}

#[derive(Debug)]
enum ImmClientMsg {
    Start(Address<ImmediateMsg, ImmediateReply>),
    Returned(CallOutcome<ImmediateReply>),
}

#[derive(Debug)]
struct ImmClient {
    out: Rc<RefCell<Vec<CallOutcome<ImmediateReply>>>>,
}

impl Isolate for ImmClient {
    type Message = ImmClientMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<ImmClientMsg>;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ImmClientMsg::Start(svc) => Effect::Call(RuntimeCall::isolate_call(
                svc,
                ImmediateMsg::Ping,
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
fn immediate_reply_is_not_falsely_flagged_abandoned() {
    let (mut runtime, _clock) = new_manual_runtime();
    let svc = runtime.register(ImmediateSvc, TestMailbox::new(4));
    let out = Rc::new(RefCell::new(Vec::new()));
    let caller = runtime.register(
        ImmClient {
            out: Rc::clone(&out),
        },
        TestMailbox::new(4),
    );

    runtime.try_send(caller, ImmClientMsg::Start(svc)).unwrap();
    step_to_idle(&mut runtime);

    assert_eq!(
        out.borrow().as_slice(),
        [CallOutcome::Replied(ImmediateReply::Pong)]
    );

    let abandoned = runtime
        .trace()
        .iter()
        .filter(|e| matches!(e.kind(), RuntimeEventKind::CallReplyAbandoned { .. }))
        .count();
    assert_eq!(abandoned, 0);
}

// ---------------------------------------------------------------------------
// ObservedSend reply_with_request helper
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct SinkMsg;

#[derive(Debug)]
struct Sink;

impl Isolate for Sink {
    type Message = SinkMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        _msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }
}

#[derive(Debug)]
enum ObsSvcMsg {
    Start,
    SendResult(RequestContext<SvcReply>, SendOutcome),
}

#[derive(Debug)]
struct ObsSvc {
    sink: Address<SinkMsg, ()>,
}

impl Isolate for ObsSvc {
    type Message = ObsSvcMsg;
    type Reply = SvcReply;
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<ObsSvcMsg>;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ObsSvcMsg::Start => {
                let req = ctx.take_request_context().unwrap();
                crate::send_observed(self.sink, SinkMsg)
                    .reply_with_request(req, ObsSvcMsg::SendResult)
            }
            ObsSvcMsg::SendResult(req, outcome) => match outcome {
                SendOutcome::Accepted => reply_to_request(req, SvcReply::Ready),
                _ => reply_to_request(req, SvcReply::NotReady),
            },
        }
    }
}

#[test]
fn observed_send_reply_with_request_carries_request_context() {
    let (mut runtime, _clock) = new_manual_runtime();
    let sink = runtime.register(Sink, TestMailbox::new(4));
    let svc = runtime.register(ObsSvc { sink }, TestMailbox::new(8));
    let out = Rc::new(RefCell::new(Vec::new()));

    #[derive(Debug)]
    enum OClientMsg {
        Start(Address<ObsSvcMsg, SvcReply>),
        Returned(CallOutcome<SvcReply>),
    }

    #[derive(Debug)]
    struct OClient {
        out: Rc<RefCell<Vec<CallOutcome<SvcReply>>>>,
    }

    impl Isolate for OClient {
        type Message = OClientMsg;
        type Reply = ();
        type Send = Outbound<Infallible>;
        type Spawn = Infallible;
        type SpawnObserved = std::convert::Infallible;
        type Call = RuntimeCall<OClientMsg>;
        type Shard = TestShard;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                OClientMsg::Start(svc) => Effect::Call(RuntimeCall::isolate_call(
                    svc,
                    ObsSvcMsg::Start,
                    Duration::from_millis(100),
                    OClientMsg::Returned,
                )),
                OClientMsg::Returned(outcome) => {
                    self.out.borrow_mut().push(outcome);
                    noop()
                }
            }
        }
    }

    let caller = runtime.register(
        OClient {
            out: Rc::clone(&out),
        },
        TestMailbox::new(4),
    );

    runtime.try_send(caller, OClientMsg::Start(svc)).unwrap();
    step_to_idle(&mut runtime);

    assert_eq!(
        out.borrow().as_slice(),
        [CallOutcome::Replied(SvcReply::Ready)]
    );
}

// ---------------------------------------------------------------------------
// RequestContext is move-only compile-fail proof
// ---------------------------------------------------------------------------

/// ```compile_fail
/// use tina::{RequestContext, Effect, Isolate, Outbound};
/// struct S;
/// impl Isolate for S {
///     type Message = (); type Reply = u32;
///     type Send = Outbound<std::convert::Infallible>;
///     type Spawn = std::convert::Infallible;
///     type SpawnObserved = std::convert::Infallible;
///     type Call = std::convert::Infallible;
///     type Shard = tina::SingleShard;
///     fn handle(&mut self, _: (), _: &mut tina::Context<'_, Self::Shard, Self::Reply>) -> Effect<Self> {
///         tina::noop()
///     }
/// }
/// fn _double_reply(req: RequestContext<u32>) -> (Effect<S>, Effect<S>) {
///     (tina::reply_to_request(req, 1), tina::reply_to_request(req, 2))
/// }
/// ```
#[allow(dead_code)]
const REQUEST_CONTEXT_MOVE_ONLY: () = ();
