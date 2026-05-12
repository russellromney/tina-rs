//! Multi-turn request context tests.
//!
//! Covers RequestContext capture, reply_with_request helpers, and
//! abandoned-caller guard behavior. Runtime and sim must agree.

use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Duration;

use tina::{
    Address, CallContext, Context, Effect, Isolate, Outbound, RequestContext, batch, noop,
    reply_to_request,
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
        noop()
    }

    fn handle_call(&mut self, _msg: Self::Message, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(ProbeReply(42))
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
                call(self.probe, ProbeMsg, Duration::from_millis(50))
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
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            HandleSvcMsg::Start => noop(),
            HandleSvcMsg::ProbeResult(req, outcome) => match outcome {
                CallOutcome::Replied(ProbeReply(val)) if val >= 10 => {
                    reply_to_request(req, SvcReply::Ready)
                }
                _ => reply_to_request(req, SvcReply::NotReady),
            },
        }
    }

    fn handle_call(&mut self, msg: Self::Message, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            HandleSvcMsg::Start => {
                let req = call_ctx.into_request_context();
                let (effect, _handle) =
                    call_with_handle(self.probe, ProbeMsg, Duration::from_millis(50))
                        .reply_with_request(req, HandleSvcMsg::ProbeResult);
                effect
            }
            HandleSvcMsg::ProbeResult(_, _) => {
                call_ctx.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
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
// Abandoned caller settles with ReplyAbandoned + CallReplyAbandoned trace
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
fn abandoned_caller_rejects_immediately() {
    let (mut runtime, _clock) = new_manual_runtime();
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

    assert_eq!(
        runtime
            .trace()
            .iter()
            .filter(|e| matches!(e.kind(), RuntimeEventKind::CallReplyAbandoned { .. }))
            .count(),
        1
    );

    assert_eq!(
        out.borrow().as_slice(),
        [CallOutcome::Rejected(
            tina::CallRejectedReason::ReplyAbandoned
        )]
    );

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
        noop()
    }

    fn handle_call(&mut self, _msg: Self::Message, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(ImmediateReply::Pong)
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
// Batched reply and abandoned-context returned effects
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
    type Call = Infallible;
    type Shard = TestShard;

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
    type Call = Infallible;
    type Shard = TestShard;

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
    type Shard = TestShard;

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
fn batched_reply_consumes_call_authority_and_runs_later_effects() {
    let (mut runtime, _clock) = new_manual_runtime();
    let audit_seen = Rc::new(RefCell::new(Vec::new()));
    let audit = runtime.register(
        Audit {
            seen: Rc::clone(&audit_seen),
        },
        TestMailbox::new(4),
    );
    let svc = runtime.register(BatchSvc { audit }, TestMailbox::new(4));
    let out = Rc::new(RefCell::new(Vec::new()));
    let caller = runtime.register(
        BatchClient {
            out: Rc::clone(&out),
        },
        TestMailbox::new(4),
    );

    runtime
        .try_send(caller, BatchClientMsg::Start(svc, BatchMsg::BatchedReply))
        .unwrap();
    step_to_idle(&mut runtime);

    assert_eq!(
        out.borrow().as_slice(),
        [CallOutcome::Replied(BatchReply::Ok)]
    );
    assert_eq!(audit_seen.borrow().as_slice(), ["batched"]);
    assert_eq!(
        runtime
            .trace()
            .iter()
            .filter(|e| matches!(e.kind(), RuntimeEventKind::CallReplyAbandoned { .. }))
            .count(),
        0
    );
}

#[test]
fn unused_call_authority_rejects_but_returned_effect_still_runs() {
    let (mut runtime, _clock) = new_manual_runtime();
    let audit_seen = Rc::new(RefCell::new(Vec::new()));
    let audit = runtime.register(
        Audit {
            seen: Rc::clone(&audit_seen),
        },
        TestMailbox::new(4),
    );
    let svc = runtime.register(BatchSvc { audit }, TestMailbox::new(4));
    let out = Rc::new(RefCell::new(Vec::new()));
    let caller = runtime.register(
        BatchClient {
            out: Rc::clone(&out),
        },
        TestMailbox::new(4),
    );

    runtime
        .try_send(caller, BatchClientMsg::Start(svc, BatchMsg::AbandonButSend))
        .unwrap();
    step_to_idle(&mut runtime);

    assert_eq!(
        out.borrow().as_slice(),
        [CallOutcome::Rejected(
            tina::CallRejectedReason::ReplyAbandoned
        )]
    );
    assert_eq!(audit_seen.borrow().as_slice(), ["abandoned"]);
}

#[test]
fn explicit_reject_uses_rejected_trace_vocabulary() {
    let (mut runtime, _clock) = new_manual_runtime();
    let audit = runtime.register(
        Audit {
            seen: Rc::new(RefCell::new(Vec::new())),
        },
        TestMailbox::new(4),
    );
    let svc = runtime.register(BatchSvc { audit }, TestMailbox::new(4));
    let out = Rc::new(RefCell::new(Vec::new()));
    let caller = runtime.register(
        BatchClient {
            out: Rc::clone(&out),
        },
        TestMailbox::new(4),
    );

    runtime
        .try_send(caller, BatchClientMsg::Start(svc, BatchMsg::ExplicitReject))
        .unwrap();
    step_to_idle(&mut runtime);

    assert_eq!(
        out.borrow().as_slice(),
        [CallOutcome::Rejected(
            tina::CallRejectedReason::UnsupportedMessage
        )]
    );
    assert_eq!(
        runtime
            .trace()
            .iter()
            .filter(|e| matches!(e.kind(), RuntimeEventKind::CallReplyAbandoned { .. }))
            .count(),
        0
    );
    assert_eq!(
        runtime
            .trace()
            .iter()
            .filter(|e| matches!(
                e.kind(),
                RuntimeEventKind::CallRejected {
                    reason: tina::CallRejectedReason::UnsupportedMessage,
                    ..
                }
            ))
            .count(),
        1
    );
}

#[test]
fn batch_reply_consumes_authority_before_later_reject() {
    let (mut runtime, _clock) = new_manual_runtime();
    let audit = runtime.register(
        Audit {
            seen: Rc::new(RefCell::new(Vec::new())),
        },
        TestMailbox::new(4),
    );
    let svc = runtime.register(BatchSvc { audit }, TestMailbox::new(4));
    let out = Rc::new(RefCell::new(Vec::new()));
    let caller = runtime.register(
        BatchClient {
            out: Rc::clone(&out),
        },
        TestMailbox::new(4),
    );

    runtime
        .try_send(
            caller,
            BatchClientMsg::Start(svc, BatchMsg::ReplyThenReject),
        )
        .unwrap();
    step_to_idle(&mut runtime);

    assert_eq!(
        out.borrow().as_slice(),
        [CallOutcome::Replied(BatchReply::Ok)]
    );
    assert_eq!(
        runtime
            .trace()
            .iter()
            .filter(|e| matches!(
                e.kind(),
                RuntimeEventKind::CallReplyRejected { .. }
                    | RuntimeEventKind::CallRejected { .. }
                    | RuntimeEventKind::CallReplyAbandoned { .. }
            ))
            .count(),
        0
    );
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
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ObsSvcMsg::Start => noop(),
            ObsSvcMsg::SendResult(req, outcome) => match outcome {
                SendOutcome::Accepted => reply_to_request(req, SvcReply::Ready),
                _ => reply_to_request(req, SvcReply::NotReady),
            },
        }
    }

    fn handle_call(&mut self, msg: Self::Message, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            ObsSvcMsg::Start => {
                let req = call.into_request_context();
                crate::send_observed(self.sink, SinkMsg)
                    .reply_with_request(req, ObsSvcMsg::SendResult)
            }
            ObsSvcMsg::SendResult(_, _) => {
                call.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
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
