//! Multi-turn request context tests.
//!
//! Covers RequestContext capture, CallContext::defer helpers, and
//! abandoned-caller guard behavior. Runtime and sim must agree.

use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Duration;

use tina::{
    Address, CallContext, CancelOutcome, Context, Effect, Isolate, Outbound, RequestContext, batch,
    noop, reply_to_request,
};

use super::*;
use crate::{
    CallOutcome, PendingCancelableCall, RuntimeCall, RuntimeEventKind, SendOutcome, call,
    call_cancelable, sleep,
};

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
// Multi-turn through CallContext::defer on IsolateCall
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum SvcReply {
    Ready,
    NotReady,
}

#[derive(Debug)]
enum SvcMsg {
    Start,
    Unsupported,
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
            SvcMsg::Unsupported => noop(),
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
            SvcMsg::Start => call_ctx
                .defer(call(self.probe, ProbeMsg, Duration::from_millis(50)))
                .reply(SvcMsg::ProbeResult),
            SvcMsg::Unsupported | SvcMsg::ProbeResult(_, _) => {
                call_ctx.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

#[derive(Debug)]
enum ClientMsg {
    Start(Address<SvcMsg, SvcReply>),
    StartUnsupported(Address<SvcMsg, SvcReply>),
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
            ClientMsg::StartUnsupported(svc) => Effect::Call(RuntimeCall::isolate_call(
                svc,
                SvcMsg::Unsupported,
                Duration::from_secs(60),
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

#[test]
fn current_request_defer_preserves_caller_on_child_call_full() {
    let (mut runtime, _clock) = new_manual_runtime();
    let probe = runtime.register(Probe, TestMailbox::new(0));
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
        [CallOutcome::Replied(SvcReply::NotReady)]
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
            .filter(|e| matches!(e.kind(), RuntimeEventKind::DeferredReplySent { .. }))
            .count(),
        1
    );
}

#[derive(Debug)]
struct HoldingProbeMsg;

#[derive(Debug)]
struct HoldingProbe {
    held: Rc<RefCell<Vec<RequestContext<ProbeReply>>>>,
}

#[derive(Debug)]
enum ClosedProbeMsg {
    Request,
    Stop,
}

#[derive(Debug)]
struct ClosedProbe;

impl Isolate for ClosedProbe {
    type Message = ClosedProbeMsg;
    type Reply = ProbeReply;
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
        match msg {
            ClosedProbeMsg::Request => noop(),
            ClosedProbeMsg::Stop => Effect::Stop,
        }
    }

    fn handle_call(&mut self, msg: Self::Message, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            ClosedProbeMsg::Request => call.reply(ProbeReply(42)),
            ClosedProbeMsg::Stop => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

#[derive(Debug)]
enum ClosedSvcMsg {
    Start,
    ProbeResult(RequestContext<SvcReply>, CallOutcome<ProbeReply>),
}

#[derive(Debug)]
struct ClosedSvc {
    probe: Address<ClosedProbeMsg, ProbeReply>,
}

impl Isolate for ClosedSvc {
    type Message = ClosedSvcMsg;
    type Reply = SvcReply;
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<ClosedSvcMsg>;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ClosedSvcMsg::Start => noop(),
            ClosedSvcMsg::ProbeResult(req, CallOutcome::Closed) => {
                reply_to_request(req, SvcReply::NotReady)
            }
            ClosedSvcMsg::ProbeResult(req, _) => reply_to_request(req, SvcReply::Ready),
        }
    }

    fn handle_call(&mut self, msg: Self::Message, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            ClosedSvcMsg::Start => call_ctx
                .defer(call(
                    self.probe,
                    ClosedProbeMsg::Request,
                    Duration::from_millis(10),
                ))
                .reply(ClosedSvcMsg::ProbeResult),
            ClosedSvcMsg::ProbeResult(_, _) => {
                call_ctx.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

#[test]
fn current_request_defer_preserves_caller_on_child_call_closed() {
    let (mut runtime, _clock) = new_manual_runtime();
    let probe = runtime.register(ClosedProbe, TestMailbox::new(4));
    runtime.try_send(probe, ClosedProbeMsg::Stop).unwrap();
    step_to_idle(&mut runtime);

    let svc = runtime.register(ClosedSvc { probe }, TestMailbox::new(8));
    let out = Rc::new(RefCell::new(Vec::new()));

    #[derive(Debug)]
    enum ClosedClientMsg {
        Start(Address<ClosedSvcMsg, SvcReply>),
        Returned(CallOutcome<SvcReply>),
    }

    #[derive(Debug)]
    struct ClosedClient {
        out: Rc<RefCell<Vec<CallOutcome<SvcReply>>>>,
    }

    impl Isolate for ClosedClient {
        type Message = ClosedClientMsg;
        type Reply = ();
        type Send = Outbound<Infallible>;
        type Spawn = Infallible;
        type SpawnObserved = std::convert::Infallible;
        type Call = RuntimeCall<ClosedClientMsg>;
        type Shard = TestShard;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                ClosedClientMsg::Start(svc) => Effect::Call(RuntimeCall::isolate_call(
                    svc,
                    ClosedSvcMsg::Start,
                    Duration::from_millis(100),
                    ClosedClientMsg::Returned,
                )),
                ClosedClientMsg::Returned(outcome) => {
                    self.out.borrow_mut().push(outcome);
                    noop()
                }
            }
        }
    }

    let caller = runtime.register(
        ClosedClient {
            out: Rc::clone(&out),
        },
        TestMailbox::new(4),
    );

    runtime
        .try_send(caller, ClosedClientMsg::Start(svc))
        .unwrap();
    step_to_idle(&mut runtime);

    assert_eq!(
        out.borrow().as_slice(),
        [CallOutcome::Replied(SvcReply::NotReady)]
    );
    assert_eq!(
        runtime
            .trace()
            .iter()
            .filter(|e| matches!(e.kind(), RuntimeEventKind::CallReplyAbandoned { .. }))
            .count(),
        0
    );
}

impl Isolate for HoldingProbe {
    type Message = HoldingProbeMsg;
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
        self.held.borrow_mut().push(call.into_request_context());
        noop()
    }
}

#[derive(Debug)]
enum TimeoutSvcMsg {
    Start,
    ProbeResult(RequestContext<SvcReply>, CallOutcome<ProbeReply>),
}

#[derive(Debug)]
struct TimeoutSvc {
    probe: Address<HoldingProbeMsg, ProbeReply>,
}

impl Isolate for TimeoutSvc {
    type Message = TimeoutSvcMsg;
    type Reply = SvcReply;
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<TimeoutSvcMsg>;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            TimeoutSvcMsg::Start => noop(),
            TimeoutSvcMsg::ProbeResult(req, CallOutcome::Timeout) => {
                reply_to_request(req, SvcReply::NotReady)
            }
            TimeoutSvcMsg::ProbeResult(req, _) => reply_to_request(req, SvcReply::Ready),
        }
    }

    fn handle_call(&mut self, msg: Self::Message, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            TimeoutSvcMsg::Start => call_ctx
                .defer(call(self.probe, HoldingProbeMsg, Duration::from_millis(10)))
                .reply(TimeoutSvcMsg::ProbeResult),
            TimeoutSvcMsg::ProbeResult(_, _) => {
                call_ctx.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

#[test]
fn current_request_defer_preserves_caller_on_child_call_timeout() {
    let (mut runtime, clock) = new_manual_runtime();
    let held = Rc::new(RefCell::new(Vec::new()));
    let probe = runtime.register(
        HoldingProbe {
            held: Rc::clone(&held),
        },
        TestMailbox::new(4),
    );
    let svc = runtime.register(TimeoutSvc { probe }, TestMailbox::new(8));
    let out = Rc::new(RefCell::new(Vec::new()));

    #[derive(Debug)]
    enum TimeoutClientMsg {
        Start(Address<TimeoutSvcMsg, SvcReply>),
        Returned(CallOutcome<SvcReply>),
    }

    #[derive(Debug)]
    struct TimeoutClient {
        out: Rc<RefCell<Vec<CallOutcome<SvcReply>>>>,
    }

    impl Isolate for TimeoutClient {
        type Message = TimeoutClientMsg;
        type Reply = ();
        type Send = Outbound<Infallible>;
        type Spawn = Infallible;
        type SpawnObserved = std::convert::Infallible;
        type Call = RuntimeCall<TimeoutClientMsg>;
        type Shard = TestShard;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                TimeoutClientMsg::Start(svc) => Effect::Call(RuntimeCall::isolate_call(
                    svc,
                    TimeoutSvcMsg::Start,
                    Duration::from_millis(100),
                    TimeoutClientMsg::Returned,
                )),
                TimeoutClientMsg::Returned(outcome) => {
                    self.out.borrow_mut().push(outcome);
                    noop()
                }
            }
        }
    }

    let caller = runtime.register(
        TimeoutClient {
            out: Rc::clone(&out),
        },
        TestMailbox::new(4),
    );

    runtime
        .try_send(caller, TimeoutClientMsg::Start(svc))
        .unwrap();
    step_to_idle(&mut runtime);
    assert_eq!(held.borrow().len(), 1);
    assert!(out.borrow().is_empty());

    clock.advance(Duration::from_millis(10));
    step_to_idle(&mut runtime);

    assert_eq!(
        out.borrow().as_slice(),
        [CallOutcome::Replied(SvcReply::NotReady)]
    );
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
fn current_request_defer_respects_original_caller_timeout() {
    let (mut runtime, clock) = new_manual_runtime();
    let held = Rc::new(RefCell::new(Vec::new()));
    let probe = runtime.register(
        HoldingProbe {
            held: Rc::clone(&held),
        },
        TestMailbox::new(4),
    );
    let svc = runtime.register(TimeoutSvc { probe }, TestMailbox::new(8));
    let out = Rc::new(RefCell::new(Vec::new()));

    #[derive(Debug)]
    enum OuterTimeoutClientMsg {
        Start(Address<TimeoutSvcMsg, SvcReply>),
        Returned(CallOutcome<SvcReply>),
    }

    #[derive(Debug)]
    struct OuterTimeoutClient {
        out: Rc<RefCell<Vec<CallOutcome<SvcReply>>>>,
    }

    impl Isolate for OuterTimeoutClient {
        type Message = OuterTimeoutClientMsg;
        type Reply = ();
        type Send = Outbound<Infallible>;
        type Spawn = Infallible;
        type SpawnObserved = std::convert::Infallible;
        type Call = RuntimeCall<OuterTimeoutClientMsg>;
        type Shard = TestShard;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                OuterTimeoutClientMsg::Start(svc) => Effect::Call(RuntimeCall::isolate_call(
                    svc,
                    TimeoutSvcMsg::Start,
                    Duration::from_millis(5),
                    OuterTimeoutClientMsg::Returned,
                )),
                OuterTimeoutClientMsg::Returned(outcome) => {
                    self.out.borrow_mut().push(outcome);
                    noop()
                }
            }
        }
    }

    let caller = runtime.register(
        OuterTimeoutClient {
            out: Rc::clone(&out),
        },
        TestMailbox::new(4),
    );

    runtime
        .try_send(caller, OuterTimeoutClientMsg::Start(svc))
        .unwrap();
    step_to_idle(&mut runtime);
    assert_eq!(held.borrow().len(), 1);
    assert!(out.borrow().is_empty());

    clock.advance(Duration::from_millis(5));
    step_to_idle(&mut runtime);
    assert_eq!(out.borrow().as_slice(), [CallOutcome::Timeout]);
    assert_eq!(
        runtime
            .trace()
            .iter()
            .filter(|e| {
                matches!(
                    e.kind(),
                    RuntimeEventKind::DeferredReplyRejected {
                        reason: DeferredReplyRejectedReason::CallerTimedOut,
                        ..
                    }
                )
            })
            .count(),
        1
    );

    clock.advance(Duration::from_millis(5));
    step_to_idle(&mut runtime);

    assert_eq!(out.borrow().as_slice(), [CallOutcome::Timeout]);
    assert_eq!(
        runtime
            .trace()
            .iter()
            .filter(|e| {
                matches!(
                    e.kind(),
                    RuntimeEventKind::DeferredReplyRejected {
                        reason: DeferredReplyRejectedReason::CallerTimedOut,
                        ..
                    }
                )
            })
            .count(),
        2
    );
    assert_eq!(
        runtime
            .trace()
            .iter()
            .filter(|e| matches!(e.kind(), RuntimeEventKind::DeferredReplySent { .. }))
            .count(),
        0
    );
}

#[test]
fn unsupported_call_path_rejects_without_waiting_for_timeout() {
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

    runtime
        .try_send(caller, ClientMsg::StartUnsupported(svc))
        .unwrap();
    step_to_idle(&mut runtime);

    assert_eq!(
        out.borrow().as_slice(),
        [CallOutcome::Rejected(
            tina::CallRejectedReason::UnsupportedMessage
        )]
    );
}

// ---------------------------------------------------------------------------
// Plain continuations do not inherit caller authority
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum PlainSvcMsg {
    Start,
    ProbeResult(CallOutcome<ProbeReply>),
}

#[derive(Debug)]
struct PlainSvc {
    probe: Address<ProbeMsg, ProbeReply>,
    seen: Rc<RefCell<Vec<&'static str>>>,
}

impl Isolate for PlainSvc {
    type Message = PlainSvcMsg;
    type Reply = SvcReply;
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<PlainSvcMsg>;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            PlainSvcMsg::Start => noop(),
            PlainSvcMsg::ProbeResult(CallOutcome::Replied(_)) => {
                self.seen.borrow_mut().push("probe-replied");
                noop()
            }
            PlainSvcMsg::ProbeResult(_) => {
                self.seen.borrow_mut().push("probe-failed");
                noop()
            }
        }
    }

    fn handle_call(&mut self, msg: Self::Message, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            PlainSvcMsg::Start => {
                call(self.probe, ProbeMsg, Duration::from_millis(50)).then(PlainSvcMsg::ProbeResult)
            }
            PlainSvcMsg::ProbeResult(_) => {
                call_ctx.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

#[derive(Debug)]
enum PlainClientMsg {
    Start(Address<PlainSvcMsg, SvcReply>),
    Returned(CallOutcome<SvcReply>),
}

#[derive(Debug)]
struct PlainClient {
    out: Rc<RefCell<Vec<CallOutcome<SvcReply>>>>,
}

impl Isolate for PlainClient {
    type Message = PlainClientMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<PlainClientMsg>;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            PlainClientMsg::Start(svc) => Effect::Call(RuntimeCall::isolate_call(
                svc,
                PlainSvcMsg::Start,
                Duration::from_secs(60),
                PlainClientMsg::Returned,
            )),
            PlainClientMsg::Returned(outcome) => {
                self.out.borrow_mut().push(outcome);
                noop()
            }
        }
    }
}

#[test]
fn plain_then_still_does_not_preserve_caller_context() {
    let (mut runtime, _clock) = new_manual_runtime();
    let probe = runtime.register(Probe, TestMailbox::new(4));
    let seen = Rc::new(RefCell::new(Vec::new()));
    let svc = runtime.register(
        PlainSvc {
            probe,
            seen: Rc::clone(&seen),
        },
        TestMailbox::new(8),
    );
    let out = Rc::new(RefCell::new(Vec::new()));
    let caller = runtime.register(
        PlainClient {
            out: Rc::clone(&out),
        },
        TestMailbox::new(4),
    );

    runtime
        .try_send(caller, PlainClientMsg::Start(svc))
        .unwrap();
    step_to_idle(&mut runtime);

    assert_eq!(
        out.borrow().as_slice(),
        [CallOutcome::Rejected(
            tina::CallRejectedReason::ReplyAbandoned
        )]
    );
    assert_eq!(
        tina::CallRejectedReason::ReplyAbandoned.diagnostic_hint(),
        Some(
            "call handler returned without consuming CallContext; use \
             call_ctx.reply(...), call_ctx.reject(...), or \
             call_ctx.defer(work).reply(...)"
        )
    );
    assert_eq!(seen.borrow().as_slice(), ["probe-replied"]);
    assert_eq!(
        runtime
            .trace()
            .iter()
            .filter(|e| matches!(e.kind(), RuntimeEventKind::DeferredReplyCaptured { .. }))
            .count(),
        0
    );
    assert_eq!(
        runtime
            .trace()
            .iter()
            .filter(|e| matches!(e.kind(), RuntimeEventKind::CallReplyAbandoned { .. }))
            .count(),
        1
    );
}

// ---------------------------------------------------------------------------
// CallContext::defer_cancelable on CancelableCall
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct HandleSvc {
    probe: Address<ProbeMsg, ProbeReply>,
    pending: Option<PendingCancelableCall<(), SvcReply, ProbeReply>>,
}

#[derive(Debug)]
enum HandleSvcMsg {
    Start,
    Cancel,
    ProbeResult((), CallOutcome<ProbeReply>),
    Cancelled((), RequestContext<SvcReply>, CancelOutcome),
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
            HandleSvcMsg::Cancel => match self.pending.take() {
                Some(pending) => pending.cancel(HandleSvcMsg::Cancelled),
                None => noop(),
            },
            HandleSvcMsg::ProbeResult((), outcome) => match self.pending.take() {
                Some(pending) => {
                    let req = pending.into_request_context();
                    match outcome {
                        CallOutcome::Replied(ProbeReply(val)) if val >= 10 => {
                            reply_to_request(req, SvcReply::Ready)
                        }
                        _ => reply_to_request(req, SvcReply::NotReady),
                    }
                }
                None => noop(),
            },
            HandleSvcMsg::Cancelled((), req, _outcome) => reply_to_request(req, SvcReply::NotReady),
        }
    }

    fn handle_call(&mut self, msg: Self::Message, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            HandleSvcMsg::Start => {
                let (pending, effect) = call_ctx
                    .defer_cancelable(call_cancelable(
                        self.probe,
                        ProbeMsg,
                        Duration::from_millis(50),
                    ))
                    .reply((), HandleSvcMsg::ProbeResult);
                self.pending = Some(pending);
                effect
            }
            HandleSvcMsg::Cancel
            | HandleSvcMsg::ProbeResult(_, _)
            | HandleSvcMsg::Cancelled(_, _, _) => {
                call_ctx.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

#[test]
fn isolate_call_cancelable_defer_replies_to_original_caller() {
    let (mut runtime, _clock) = new_manual_runtime();
    let probe = runtime.register(Probe, TestMailbox::new(4));
    let svc = runtime.register(
        HandleSvc {
            probe,
            pending: None,
        },
        TestMailbox::new(8),
    );
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

#[derive(Debug, Default)]
struct CancelHoldingProbe {
    pending: Option<RequestContext<ProbeReply>>,
}

impl Isolate for CancelHoldingProbe {
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
        self.pending = Some(call.into_request_context());
        noop()
    }
}

#[test]
fn isolate_call_cancelable_defer_cancel_path_answers_original_caller() {
    let (mut runtime, _clock) = new_manual_runtime();
    let probe = runtime.register(CancelHoldingProbe::default(), TestMailbox::new(4));
    let svc = runtime.register(
        HandleSvc {
            probe,
            pending: None,
        },
        TestMailbox::new(8),
    );
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
    assert!(out.borrow().is_empty());

    runtime.try_send(svc, HandleSvcMsg::Cancel).unwrap();
    step_to_idle(&mut runtime);

    assert_eq!(
        out.borrow().as_slice(),
        [CallOutcome::Replied(SvcReply::NotReady)]
    );
    assert!(
        runtime
            .trace()
            .iter()
            .any(|e| matches!(e.kind(), RuntimeEventKind::CallCancelled { .. })),
        "cancel path must close the child wait explicitly"
    );
    assert_eq!(
        runtime
            .trace()
            .iter()
            .filter(|e| matches!(e.kind(), RuntimeEventKind::CallReplyAbandoned { .. }))
            .count(),
        0
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
// ObservedSend CallContext::defer helper
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
            ObsSvcMsg::Start => call
                .defer(crate::send_observed(self.sink, SinkMsg))
                .reply(ObsSvcMsg::SendResult),
            ObsSvcMsg::SendResult(_, _) => {
                call.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

#[test]
fn observed_send_defer_carries_request_context() {
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

#[test]
fn observed_send_defer_carries_request_context_on_full() {
    let (mut runtime, _clock) = new_manual_runtime();
    let sink = runtime.register(Sink, TestMailbox::new(0));
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
        [CallOutcome::Replied(SvcReply::NotReady)]
    );
    assert_eq!(
        runtime
            .trace()
            .iter()
            .filter(|e| matches!(e.kind(), RuntimeEventKind::CallReplyAbandoned { .. }))
            .count(),
        0
    );
}

// ---------------------------------------------------------------------------
// Typed runtime call CallContext::defer helper
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum TimerSvcMsg {
    Start,
    Fired(RequestContext<SvcReply>, crate::SleepReply),
}

#[derive(Debug)]
struct TimerSvc;

impl Isolate for TimerSvc {
    type Message = TimerSvcMsg;
    type Reply = SvcReply;
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<TimerSvcMsg>;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            TimerSvcMsg::Start => noop(),
            TimerSvcMsg::Fired(req, Ok(())) => reply_to_request(req, SvcReply::Ready),
            TimerSvcMsg::Fired(req, Err(_)) => reply_to_request(req, SvcReply::NotReady),
        }
    }

    fn handle_call(&mut self, msg: Self::Message, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            TimerSvcMsg::Start => call
                .defer(sleep(Duration::from_millis(5)))
                .reply(TimerSvcMsg::Fired),
            TimerSvcMsg::Fired(_, _) => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

#[test]
fn typed_runtime_call_defer_carries_request_context() {
    let (mut runtime, clock) = new_manual_runtime();
    let svc = runtime.register(TimerSvc, TestMailbox::new(8));
    let out = Rc::new(RefCell::new(Vec::new()));

    #[derive(Debug)]
    enum TClientMsg {
        Start(Address<TimerSvcMsg, SvcReply>),
        Returned(CallOutcome<SvcReply>),
    }

    #[derive(Debug)]
    struct TClient {
        out: Rc<RefCell<Vec<CallOutcome<SvcReply>>>>,
    }

    impl Isolate for TClient {
        type Message = TClientMsg;
        type Reply = ();
        type Send = Outbound<Infallible>;
        type Spawn = Infallible;
        type SpawnObserved = std::convert::Infallible;
        type Call = RuntimeCall<TClientMsg>;
        type Shard = TestShard;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                TClientMsg::Start(svc) => Effect::Call(RuntimeCall::isolate_call(
                    svc,
                    TimerSvcMsg::Start,
                    Duration::from_millis(100),
                    TClientMsg::Returned,
                )),
                TClientMsg::Returned(outcome) => {
                    self.out.borrow_mut().push(outcome);
                    noop()
                }
            }
        }
    }

    let caller = runtime.register(
        TClient {
            out: Rc::clone(&out),
        },
        TestMailbox::new(4),
    );

    runtime.try_send(caller, TClientMsg::Start(svc)).unwrap();
    step_to_idle(&mut runtime);
    assert!(out.borrow().is_empty());

    clock.advance(Duration::from_millis(5));
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
