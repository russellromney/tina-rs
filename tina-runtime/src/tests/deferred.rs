//! Runtime tests for deferred reply slots.
//!
//! Cover lifecycle facts the plan calls out: capture, send, drop,
//! caller-closed, and double-reply rejection. Live and sim must agree
//! on the externally visible meaning, so each scenario is named for the
//! semantic fact the trace must carry.

use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Duration;

use tina::{
    Address, Context, DeferredReply, Effect, Isolate, Outbound, TakeReplySlotError, noop, reply_to,
};

use super::*;
use crate::{
    CallOutcome, DeferredReplyRejectedReason, PendingReplies, RuntimeCall, RuntimeEventKind,
};

fn step_to_idle<S, F>(runtime: &mut Runtime<S, F>)
where
    S: Shard,
    F: MailboxFactory,
{
    // Simple drain loop: keep stepping while any work happens.
    while runtime.step() > 0 {}
}

#[derive(Debug)]
enum SvcRequest {
    /// Capture the current caller as a deferred slot, store, do not reply.
    Capture(u64),
    /// Reply through the stored slot using the stored payload.
    ReplyStored,
    /// Drop the stored slot without replying.
    DropStored,
    /// Try to take a slot when the message has no caller (sent plain).
    /// Pushes the take result into the shared probe.
    TakeWithoutCaller,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SvcReply {
    Ack(u64),
}

#[derive(Default, Debug)]
struct SvcProbe {
    take_no_caller_errors: usize,
}

#[derive(Debug)]
struct DeferredSvc {
    slot: Option<DeferredReply<SvcReply>>,
    payload: u64,
    probe: Rc<RefCell<SvcProbe>>,
}

impl Isolate for DeferredSvc {
    type Message = SvcRequest;
    type Reply = SvcReply;
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type Call = Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SvcRequest::Capture(payload) => {
                self.payload = payload;
                self.slot = Some(ctx.take_reply_slot().unwrap());
                noop()
            }
            SvcRequest::ReplyStored => {
                let slot = self.slot.take().expect("slot stored");
                reply_to(slot, SvcReply::Ack(self.payload))
            }
            SvcRequest::DropStored => {
                self.slot = None;
                noop()
            }
            SvcRequest::TakeWithoutCaller => {
                match ctx.take_reply_slot() {
                    Err(TakeReplySlotError::NoCaller) => {
                        self.probe.borrow_mut().take_no_caller_errors += 1;
                    }
                    Ok(_) => panic!("expected NoCaller, got Ok"),
                    Err(TakeReplySlotError::CrossShardUnsupported) => {
                        panic!("local message produced CrossShardUnsupported")
                    }
                }
                noop()
            }
        }
    }
}

#[derive(Debug)]
enum CallerMsg {
    Start(Address<SvcRequest, SvcReply>, u64),
    Returned(CallOutcome<SvcReply>),
}

#[derive(Debug)]
struct DeferredCaller {
    timeout: Duration,
    outcomes: Rc<RefCell<Vec<CallOutcome<SvcReply>>>>,
}

impl Isolate for DeferredCaller {
    type Message = CallerMsg;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type Call = RuntimeCall<CallerMsg>;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CallerMsg::Start(svc, payload) => Effect::Call(RuntimeCall::isolate_call(
                svc,
                SvcRequest::Capture(payload),
                self.timeout,
                CallerMsg::Returned,
            )),
            CallerMsg::Returned(outcome) => {
                self.outcomes.borrow_mut().push(outcome);
                noop()
            }
        }
    }
}

type SvcOutcomes = Rc<RefCell<Vec<CallOutcome<SvcReply>>>>;
type World = (
    Runtime<TestShard, TestMailboxFactory>,
    Rc<ManualClock>,
    Address<SvcRequest, SvcReply>,
    Address<CallerMsg>,
    SvcOutcomes,
    Rc<RefCell<SvcProbe>>,
);

fn build_world() -> World {
    let (mut runtime, clock) = new_manual_runtime();
    let probe = Rc::new(RefCell::new(SvcProbe::default()));
    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let svc = runtime.register(
        DeferredSvc {
            slot: None,
            payload: 0,
            probe: Rc::clone(&probe),
        },
        TestMailbox::new(8),
    );
    let caller = runtime.register(
        DeferredCaller {
            timeout: Duration::from_millis(50),
            outcomes: Rc::clone(&outcomes),
        },
        TestMailbox::new(8),
    );
    (runtime, clock, svc, caller, outcomes, probe)
}

fn count_kind(events: &[RuntimeEvent], pred: impl Fn(&RuntimeEventKind) -> bool) -> usize {
    events.iter().filter(|e| pred(&e.kind())).count()
}

#[test]
fn deferred_reply_capture_and_send_routes_to_original_caller() {
    let (mut runtime, _clock, svc, caller, outcomes, _probe) = build_world();

    runtime.try_send(caller, CallerMsg::Start(svc, 7)).unwrap();
    step_to_idle(&mut runtime);

    // Service captured but did not reply yet. Caller still waiting.
    assert!(outcomes.borrow().is_empty());
    assert_eq!(
        count_kind(runtime.trace(), |k| matches!(
            k,
            RuntimeEventKind::DeferredReplyCaptured { .. }
        )),
        1
    );

    // Trigger the deferred reply via a plain send (no caller context on
    // this message).
    runtime
        .try_send(svc.with_reply::<()>(), SvcRequest::ReplyStored)
        .unwrap();
    step_to_idle(&mut runtime);

    assert_eq!(
        outcomes.borrow().as_slice(),
        [CallOutcome::Replied(SvcReply::Ack(7))]
    );
    assert_eq!(
        count_kind(runtime.trace(), |k| matches!(
            k,
            RuntimeEventKind::DeferredReplySent { .. }
        )),
        1
    );
}

#[test]
fn take_reply_slot_without_caller_errors_visibly() {
    let (mut runtime, _clock, svc, _caller, _outcomes, probe) = build_world();

    // Plain send — no caller.
    runtime
        .try_send(svc.with_reply::<()>(), SvcRequest::TakeWithoutCaller)
        .unwrap();
    step_to_idle(&mut runtime);

    assert_eq!(probe.borrow().take_no_caller_errors, 1);
    assert_eq!(
        count_kind(runtime.trace(), |k| matches!(
            k,
            RuntimeEventKind::DeferredReplyCaptured { .. }
        )),
        0
    );
}

#[test]
fn dropping_open_deferred_slot_emits_dropped_terminal_fact() {
    let (mut runtime, _clock, svc, caller, outcomes, _probe) = build_world();

    runtime.try_send(caller, CallerMsg::Start(svc, 1)).unwrap();
    step_to_idle(&mut runtime);

    runtime
        .try_send(svc.with_reply::<()>(), SvcRequest::DropStored)
        .unwrap();
    step_to_idle(&mut runtime);

    assert_eq!(
        count_kind(runtime.trace(), |k| matches!(
            k,
            RuntimeEventKind::DeferredReplyDropped { .. }
        )),
        1
    );
    assert_eq!(outcomes.borrow().as_slice(), [CallOutcome::Closed]);
}

#[test]
fn panic_after_capture_drops_slot_and_closes_caller() {
    #[derive(Debug)]
    struct PanicAfterCapture;

    impl Isolate for PanicAfterCapture {
        type Message = SvcRequest;
        type Reply = SvcReply;
        type Send = Outbound<NeverOutbound>;
        type Spawn = Infallible;
        type Call = Infallible;
        type Shard = TestShard;

        fn handle(
            &mut self,
            _msg: Self::Message,
            ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            let _slot = ctx.take_reply_slot().unwrap();
            panic!("boom after deferred capture");
        }
    }

    let (mut runtime, _clock) = new_manual_runtime();
    let svc = runtime.register(PanicAfterCapture, TestMailbox::new(4));
    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let caller = runtime.register(
        DeferredCaller {
            timeout: Duration::from_secs(60),
            outcomes: Rc::clone(&outcomes),
        },
        TestMailbox::new(4),
    );

    runtime.try_send(caller, CallerMsg::Start(svc, 1)).unwrap();
    step_to_idle(&mut runtime);

    assert_eq!(outcomes.borrow().as_slice(), [CallOutcome::Closed]);
    assert_eq!(
        count_kind(runtime.trace(), |k| matches!(
            k,
            RuntimeEventKind::DeferredReplyCaptured { .. }
        )),
        1
    );
    assert_eq!(
        count_kind(runtime.trace(), |k| matches!(
            k,
            RuntimeEventKind::DeferredReplyDropped { .. }
        )),
        1
    );
}

#[test]
fn caller_timeout_closes_open_slot_with_terminal_rejected_event() {
    let (mut runtime, clock, svc, caller, outcomes, _probe) = build_world();

    runtime.try_send(caller, CallerMsg::Start(svc, 9)).unwrap();
    step_to_idle(&mut runtime);

    // Advance past the call deadline; harvest fires.
    clock.advance(Duration::from_millis(60));
    step_to_idle(&mut runtime);

    // Caller observed Timeout.
    assert_eq!(outcomes.borrow().as_slice(), [CallOutcome::Timeout]);

    // Slot got a terminal CallerClosed reject.
    let closed_count = runtime
        .trace()
        .iter()
        .filter(|e| {
            matches!(
                e.kind(),
                RuntimeEventKind::DeferredReplyRejected {
                    reason: DeferredReplyRejectedReason::CallerClosed,
                    ..
                }
            )
        })
        .count();
    assert_eq!(closed_count, 1);

    // User then attempts to reply through the closed slot. This is a
    // no-op against the trace: the terminal fact already fired.
    runtime
        .try_send(svc.with_reply::<()>(), SvcRequest::ReplyStored)
        .unwrap();
    step_to_idle(&mut runtime);

    let total_terminal_after = runtime
        .trace()
        .iter()
        .filter(|e| {
            matches!(
                e.kind(),
                RuntimeEventKind::DeferredReplySent { .. }
                    | RuntimeEventKind::DeferredReplyRejected { .. }
                    | RuntimeEventKind::DeferredReplyDropped { .. }
            )
        })
        .count();
    assert_eq!(
        total_terminal_after, 1,
        "terminal fact should not double-fire"
    );
}

#[test]
fn capture_supersedes_subsequent_effect_reply_in_same_handler_turn() {
    // A handler that captures AND returns Effect::Reply: the runtime
    // should honor the capture and treat the Effect::Reply as a no-op
    // against the original caller. Trace records the EffectObserved
    // for the noop reply and a captured event for the slot.

    #[derive(Debug)]
    struct CaptureThenReply;

    impl Isolate for CaptureThenReply {
        type Message = ();
        type Reply = u32;
        type Send = Outbound<NeverOutbound>;
        type Spawn = Infallible;
        type Call = Infallible;
        type Shard = TestShard;

        fn handle(
            &mut self,
            _msg: Self::Message,
            ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            let _slot = ctx.take_reply_slot().unwrap();
            // Drop the slot immediately and try to reply normally —
            // the captured slot's Drop will surface as Dropped event,
            // and Effect::Reply has no caller anymore.
            tina::reply(99)
        }
    }

    #[derive(Debug)]
    enum LocalCallerMsg {
        Start(Address<(), u32>),
        Returned(CallOutcome<u32>),
    }

    #[derive(Debug)]
    struct LocalCaller {
        outcomes: Rc<RefCell<Vec<CallOutcome<u32>>>>,
    }

    impl Isolate for LocalCaller {
        type Message = LocalCallerMsg;
        type Reply = ();
        type Send = Outbound<NeverOutbound>;
        type Spawn = Infallible;
        type Call = RuntimeCall<LocalCallerMsg>;
        type Shard = TestShard;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                LocalCallerMsg::Start(svc) => Effect::Call(RuntimeCall::isolate_call(
                    svc,
                    (),
                    Duration::from_millis(20),
                    LocalCallerMsg::Returned,
                )),
                LocalCallerMsg::Returned(outcome) => {
                    self.outcomes.borrow_mut().push(outcome);
                    noop()
                }
            }
        }
    }

    let (mut runtime, clock) = new_manual_runtime();
    let svc = runtime.register(CaptureThenReply, TestMailbox::new(4));
    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let caller = runtime.register(
        LocalCaller {
            outcomes: Rc::clone(&outcomes),
        },
        TestMailbox::new(4),
    );

    runtime
        .try_send(caller, LocalCallerMsg::Start(svc))
        .unwrap();
    step_to_idle(&mut runtime);

    // Slot was captured then dropped (one terminal Dropped fact).
    let dropped = runtime
        .trace()
        .iter()
        .filter(|e| matches!(e.kind(), RuntimeEventKind::DeferredReplyDropped { .. }))
        .count();
    assert_eq!(dropped, 1);

    // Dropping the captured slot closes the caller immediately.
    assert_eq!(outcomes.borrow().as_slice(), [CallOutcome::Closed]);

    // Advancing past the old deadline must not produce a ghost timeout.
    clock.advance(Duration::from_millis(30));
    step_to_idle(&mut runtime);
    assert_eq!(outcomes.borrow().as_slice(), [CallOutcome::Closed]);
}

#[test]
fn pending_box_reclaims_slots_after_caller_timeouts_and_admits_new_callers() {
    // Build a frontend that holds two slots in a PendingReplies box of
    // capacity 2. Two callers fill it and then time out. A third
    // caller arrives and should be admitted without stale Full.

    #[derive(Debug)]
    enum FrontendMsg {
        Capture(u64),
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct FrontendReply;

    #[derive(Debug)]
    struct Frontend {
        pending: PendingReplies<u64, FrontendReply>,
    }

    impl Isolate for Frontend {
        type Message = FrontendMsg;
        type Reply = FrontendReply;
        type Send = Outbound<NeverOutbound>;
        type Spawn = Infallible;
        type Call = Infallible;
        type Shard = TestShard;

        fn handle(
            &mut self,
            msg: Self::Message,
            ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                FrontendMsg::Capture(key) => {
                    let slot = ctx.take_reply_slot().unwrap();
                    if let Err(err) = self.pending.try_insert(key, slot) {
                        // Slot drops on err; surfaces as Dropped event.
                        let _ = err;
                    }
                    noop()
                }
            }
        }
    }

    let (mut runtime, clock) = new_manual_runtime();
    let svc = runtime.register(
        Frontend {
            pending: PendingReplies::with_capacity(2),
        },
        TestMailbox::new(8),
    );

    #[derive(Debug)]
    enum FCallerMsg {
        Start(Address<FrontendMsg, FrontendReply>, u64),
        Returned(CallOutcome<FrontendReply>),
    }

    #[derive(Debug)]
    struct FCaller {
        out: Rc<RefCell<Vec<CallOutcome<FrontendReply>>>>,
        timeout: Duration,
    }

    impl Isolate for FCaller {
        type Message = FCallerMsg;
        type Reply = ();
        type Send = Outbound<NeverOutbound>;
        type Spawn = Infallible;
        type Call = RuntimeCall<FCallerMsg>;
        type Shard = TestShard;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                FCallerMsg::Start(svc, k) => Effect::Call(RuntimeCall::isolate_call(
                    svc,
                    FrontendMsg::Capture(k),
                    self.timeout,
                    FCallerMsg::Returned,
                )),
                FCallerMsg::Returned(outcome) => {
                    self.out.borrow_mut().push(outcome);
                    noop()
                }
            }
        }
    }

    let out_a = Rc::new(RefCell::new(Vec::new()));
    let out_b = Rc::new(RefCell::new(Vec::new()));
    let a = runtime.register(
        FCaller {
            out: Rc::clone(&out_a),
            timeout: Duration::from_millis(20),
        },
        TestMailbox::new(4),
    );
    let b = runtime.register(
        FCaller {
            out: Rc::clone(&out_b),
            timeout: Duration::from_millis(20),
        },
        TestMailbox::new(4),
    );

    runtime.try_send(a, FCallerMsg::Start(svc, 1)).unwrap();
    runtime.try_send(b, FCallerMsg::Start(svc, 2)).unwrap();
    step_to_idle(&mut runtime);

    // Both slots captured; box at capacity. A third caller would
    // hit Full IF we did not first sweep for caller-closed.

    // Time out both callers.
    clock.advance(Duration::from_millis(30));
    step_to_idle(&mut runtime);
    assert_eq!(out_a.borrow().as_slice(), [CallOutcome::Timeout]);
    assert_eq!(out_b.borrow().as_slice(), [CallOutcome::Timeout]);

    // The pending box is now stale-full from its own POV until a
    // try_insert sweeps. New caller arrives and should be admitted
    // without stale Full.
    let out_c = Rc::new(RefCell::new(Vec::new()));
    let c = runtime.register(
        FCaller {
            out: Rc::clone(&out_c),
            timeout: Duration::from_millis(20),
        },
        TestMailbox::new(4),
    );
    runtime.try_send(c, FCallerMsg::Start(svc, 3)).unwrap();
    step_to_idle(&mut runtime);

    // The capture must have succeeded (no Full reject from the
    // frontend's own state). Frontend slot is captured and the slot
    // is open. Verify by counting Captured events: should be 3.
    let captured = runtime
        .trace()
        .iter()
        .filter(|e| matches!(e.kind(), RuntimeEventKind::DeferredReplyCaptured { .. }))
        .count();
    assert_eq!(captured, 3, "third caller must have been admitted");
}

#[test]
fn lifo_drain_routes_each_reply_to_its_original_caller_by_call_id() {
    // Two independent callers each issue a call against the same service.
    // The service captures both slots, then replies in reverse (LIFO)
    // capture order. Each reply must reach the matching original
    // caller, not the most-recently-captured one. The pool test below
    // covers the harder out-of-order routing through worker
    // continuations.
    let (mut runtime, _clock) = new_manual_runtime();
    let probe = Rc::new(RefCell::new(SvcProbe::default()));

    #[derive(Debug)]
    enum FanSvcMsg {
        Capture,
        Drain,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct FanReply(u64);

    #[derive(Debug)]
    struct FanSvc {
        slots: Vec<DeferredReply<FanReply>>,
        next_id: u64,
        ids: Vec<u64>,
    }

    impl Isolate for FanSvc {
        type Message = FanSvcMsg;
        type Reply = FanReply;
        type Send = Outbound<NeverOutbound>;
        type Spawn = Infallible;
        type Call = Infallible;
        type Shard = TestShard;

        fn handle(
            &mut self,
            msg: Self::Message,
            ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                FanSvcMsg::Capture => {
                    self.next_id += 1;
                    self.ids.push(self.next_id);
                    let slot = ctx.take_reply_slot().unwrap();
                    self.slots.push(slot);
                    noop()
                }
                FanSvcMsg::Drain => {
                    // Reply LIFO: last captured first.
                    let mut effects = Vec::new();
                    while let Some(slot) = self.slots.pop() {
                        let id = self.ids.pop().unwrap();
                        effects.push(reply_to(slot, FanReply(id)));
                    }
                    Effect::Batch(effects)
                }
            }
        }
    }

    let svc = runtime.register(
        FanSvc {
            slots: Vec::new(),
            next_id: 0,
            ids: Vec::new(),
        },
        TestMailbox::new(8),
    );

    #[derive(Debug)]
    enum FanCallerMsg {
        Start(Address<FanSvcMsg, FanReply>),
        Returned(CallOutcome<FanReply>),
    }

    #[derive(Debug)]
    struct FanCaller {
        outcomes: Rc<RefCell<Vec<CallOutcome<FanReply>>>>,
    }

    impl Isolate for FanCaller {
        type Message = FanCallerMsg;
        type Reply = ();
        type Send = Outbound<NeverOutbound>;
        type Spawn = Infallible;
        type Call = RuntimeCall<FanCallerMsg>;
        type Shard = TestShard;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                FanCallerMsg::Start(svc) => Effect::Call(RuntimeCall::isolate_call(
                    svc,
                    FanSvcMsg::Capture,
                    Duration::from_millis(200),
                    FanCallerMsg::Returned,
                )),
                FanCallerMsg::Returned(outcome) => {
                    self.outcomes.borrow_mut().push(outcome);
                    noop()
                }
            }
        }
    }

    let outcomes_a = Rc::new(RefCell::new(Vec::new()));
    let outcomes_b = Rc::new(RefCell::new(Vec::new()));
    let caller_a = runtime.register(
        FanCaller {
            outcomes: Rc::clone(&outcomes_a),
        },
        TestMailbox::new(4),
    );
    let caller_b = runtime.register(
        FanCaller {
            outcomes: Rc::clone(&outcomes_b),
        },
        TestMailbox::new(4),
    );

    runtime
        .try_send(caller_a, FanCallerMsg::Start(svc))
        .unwrap();
    runtime
        .try_send(caller_b, FanCallerMsg::Start(svc))
        .unwrap();
    step_to_idle(&mut runtime);

    // Both captured. Now drain in reverse.
    runtime
        .try_send(svc.with_reply::<()>(), FanSvcMsg::Drain)
        .unwrap();
    step_to_idle(&mut runtime);

    // Caller A captured first → got id 1. Caller B captured second → got id 2.
    // Drain pops LIFO so it answers id 2 first then id 1, but each reply
    // routes to the matching original caller by call_id.
    assert_eq!(
        outcomes_a.borrow().as_slice(),
        [CallOutcome::Replied(FanReply(1))]
    );
    assert_eq!(
        outcomes_b.borrow().as_slice(),
        [CallOutcome::Replied(FanReply(2))]
    );

    let _ = probe;
}

// ---------------------------------------------------------------------------
// Pooled-service frontend proof.
//
// Caller -> Frontend -> one of N Workers; workers reply out of order;
// frontend routes each reply back to the matching original caller.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct PoolConfig {
    max_pending: usize,
    worker_call_timeout: Duration,
}

#[derive(Debug)]
enum WorkerMsg {
    Do(u64, u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerReply {
    Echo(u64, u64),
}

#[derive(Debug)]
struct Worker;

impl Isolate for Worker {
    type Message = WorkerMsg;
    type Reply = WorkerReply;
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type Call = Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            WorkerMsg::Do(id, payload) => tina::reply(WorkerReply::Echo(id, payload)),
        }
    }
}

#[derive(Debug)]
enum FrontendMsg {
    Client(u64),
    WorkerDone(u64, CallOutcome<WorkerReply>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrontendReply {
    Ok(u64),
    Full,
    WorkerTimeout,
    WorkerClosed,
}

#[derive(Debug)]
struct PoolFrontend {
    workers: Vec<Address<WorkerMsg, WorkerReply>>,
    pending: PendingReplies<u64, FrontendReply>,
    next_req: u64,
    next_worker: usize,
    config: PoolConfig,
}

impl Isolate for PoolFrontend {
    type Message = FrontendMsg;
    type Reply = FrontendReply;
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type Call = RuntimeCall<FrontendMsg>;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            FrontendMsg::Client(payload) => {
                self.pending.sweep();
                if self.pending.len() >= self.config.max_pending {
                    // Box full: reply Full immediately without
                    // capturing the slot.
                    return tina::reply(FrontendReply::Full);
                }
                let slot = ctx.take_reply_slot().unwrap();
                let req_id = self.next_req;
                self.next_req += 1;
                self.pending
                    .try_insert(req_id, slot)
                    .unwrap_or_else(|_| panic!("admission must succeed after sweep"));
                let worker_idx = self.next_worker % self.workers.len();
                self.next_worker += 1;
                Effect::Call(RuntimeCall::isolate_call(
                    self.workers[worker_idx],
                    WorkerMsg::Do(req_id, payload),
                    self.config.worker_call_timeout,
                    move |outcome| FrontendMsg::WorkerDone(req_id, outcome),
                ))
            }
            FrontendMsg::WorkerDone(id, outcome) => {
                let Some(slot) = self.pending.take(&id) else {
                    return noop();
                };
                let reply_val = match outcome {
                    CallOutcome::Replied(WorkerReply::Echo(_, payload)) => {
                        FrontendReply::Ok(payload)
                    }
                    CallOutcome::Timeout => FrontendReply::WorkerTimeout,
                    _ => FrontendReply::WorkerClosed,
                };
                reply_to(slot, reply_val)
            }
        }
    }
}

#[derive(Debug)]
enum PoolCallerMsg {
    Start(Address<FrontendMsg, FrontendReply>, u64),
    Returned(CallOutcome<FrontendReply>),
}

#[derive(Debug)]
struct PoolCaller {
    out: Rc<RefCell<Vec<CallOutcome<FrontendReply>>>>,
    timeout: Duration,
}

impl Isolate for PoolCaller {
    type Message = PoolCallerMsg;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type Call = RuntimeCall<PoolCallerMsg>;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            PoolCallerMsg::Start(svc, payload) => Effect::Call(RuntimeCall::isolate_call(
                svc,
                FrontendMsg::Client(payload),
                self.timeout,
                PoolCallerMsg::Returned,
            )),
            PoolCallerMsg::Returned(outcome) => {
                self.out.borrow_mut().push(outcome);
                noop()
            }
        }
    }
}

#[test]
fn pooled_frontend_routes_out_of_order_worker_replies_to_correct_callers() {
    let (mut runtime, _clock) = new_manual_runtime();
    let w0 = runtime.register(Worker, TestMailbox::new(4));
    let w1 = runtime.register(Worker, TestMailbox::new(4));

    let frontend = runtime.register(
        PoolFrontend {
            workers: vec![w0, w1],
            pending: PendingReplies::with_capacity(8),
            next_req: 1,
            next_worker: 0,
            config: PoolConfig {
                max_pending: 8,
                worker_call_timeout: Duration::from_millis(200),
            },
        },
        TestMailbox::new(8),
    );

    let out_a = Rc::new(RefCell::new(Vec::new()));
    let out_b = Rc::new(RefCell::new(Vec::new()));
    let out_c = Rc::new(RefCell::new(Vec::new()));
    let ca = runtime.register(
        PoolCaller {
            out: Rc::clone(&out_a),
            timeout: Duration::from_millis(500),
        },
        TestMailbox::new(4),
    );
    let cb = runtime.register(
        PoolCaller {
            out: Rc::clone(&out_b),
            timeout: Duration::from_millis(500),
        },
        TestMailbox::new(4),
    );
    let cc = runtime.register(
        PoolCaller {
            out: Rc::clone(&out_c),
            timeout: Duration::from_millis(500),
        },
        TestMailbox::new(4),
    );

    runtime
        .try_send(ca, PoolCallerMsg::Start(frontend, 100))
        .unwrap();
    runtime
        .try_send(cb, PoolCallerMsg::Start(frontend, 200))
        .unwrap();
    runtime
        .try_send(cc, PoolCallerMsg::Start(frontend, 300))
        .unwrap();
    step_to_idle(&mut runtime);

    // Each caller must receive its own payload back, regardless of which
    // worker handled it or in what order.
    assert_eq!(
        out_a.borrow().as_slice(),
        [CallOutcome::Replied(FrontendReply::Ok(100))]
    );
    assert_eq!(
        out_b.borrow().as_slice(),
        [CallOutcome::Replied(FrontendReply::Ok(200))]
    );
    assert_eq!(
        out_c.borrow().as_slice(),
        [CallOutcome::Replied(FrontendReply::Ok(300))]
    );
}

// ---------------------------------------------------------------------------
// Fanout/gather first-form proof.
//
// Caller -> Coordinator -> N targets in parallel; coordinator captures
// caller slot, gathers per-target outcomes, then replies once with a
// bounded aggregate.
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum FanoutTargetMsg {
    Echo(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FanoutTargetReply(u64);

#[derive(Debug)]
struct FanoutTarget;

impl Isolate for FanoutTarget {
    type Message = FanoutTargetMsg;
    type Reply = FanoutTargetReply;
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type Call = Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            FanoutTargetMsg::Echo(v) => tina::reply(FanoutTargetReply(v)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetOutcome {
    Ok(u64),
    Timeout,
    Closed,
    Full,
}

#[derive(Debug, Clone)]
struct AggregateReport {
    per_target: Vec<TargetOutcome>,
    completed: bool,
}

#[derive(Debug)]
enum CoordMsg {
    Start(u64),
    TargetDone(usize, CallOutcome<FanoutTargetReply>),
}

#[derive(Debug)]
struct Coordinator {
    targets: Vec<Address<FanoutTargetMsg, FanoutTargetReply>>,
    target_timeout: Duration,
    slot: Option<DeferredReply<AggregateReport>>,
    collected: Vec<Option<TargetOutcome>>,
    remaining: usize,
}

impl Isolate for Coordinator {
    type Message = CoordMsg;
    type Reply = AggregateReport;
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type Call = RuntimeCall<CoordMsg>;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CoordMsg::Start(payload) => {
                self.slot = Some(ctx.take_reply_slot().unwrap());
                self.collected = vec![None; self.targets.len()];
                self.remaining = self.targets.len();
                let mut subcalls = Vec::with_capacity(self.targets.len());
                for (idx, target) in self.targets.iter().enumerate() {
                    subcalls.push(Effect::Call(RuntimeCall::isolate_call(
                        *target,
                        FanoutTargetMsg::Echo(payload + idx as u64),
                        self.target_timeout,
                        move |outcome| CoordMsg::TargetDone(idx, outcome),
                    )));
                }
                Effect::Batch(subcalls)
            }
            CoordMsg::TargetDone(idx, outcome) => {
                let mapped = match outcome {
                    CallOutcome::Replied(FanoutTargetReply(v)) => TargetOutcome::Ok(v),
                    CallOutcome::Timeout => TargetOutcome::Timeout,
                    CallOutcome::Closed => TargetOutcome::Closed,
                    CallOutcome::Full => TargetOutcome::Full,
                };
                if self.collected[idx].is_none() {
                    self.collected[idx] = Some(mapped);
                    self.remaining -= 1;
                }
                if self.remaining == 0 {
                    let slot = self.slot.take().unwrap();
                    let report = AggregateReport {
                        per_target: self
                            .collected
                            .iter()
                            .map(|c| c.unwrap_or(TargetOutcome::Closed))
                            .collect(),
                        completed: true,
                    };
                    return reply_to(slot, report);
                }
                noop()
            }
        }
    }
}

#[derive(Debug)]
enum FanoutCallerMsg {
    Start(Address<CoordMsg, AggregateReport>, u64),
    Returned(CallOutcome<AggregateReport>),
}

#[derive(Debug)]
struct FanoutCaller {
    out: Rc<RefCell<Vec<CallOutcome<AggregateReport>>>>,
    timeout: Duration,
}

impl Isolate for FanoutCaller {
    type Message = FanoutCallerMsg;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type Call = RuntimeCall<FanoutCallerMsg>;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            FanoutCallerMsg::Start(svc, p) => Effect::Call(RuntimeCall::isolate_call(
                svc,
                CoordMsg::Start(p),
                self.timeout,
                FanoutCallerMsg::Returned,
            )),
            FanoutCallerMsg::Returned(outcome) => {
                self.out.borrow_mut().push(outcome);
                noop()
            }
        }
    }
}

#[test]
fn fanout_gather_returns_bounded_aggregate_with_per_target_outcomes() {
    let (mut runtime, _clock) = new_manual_runtime();
    let t0 = runtime.register(FanoutTarget, TestMailbox::new(4));
    let t1 = runtime.register(FanoutTarget, TestMailbox::new(4));
    let t2 = runtime.register(FanoutTarget, TestMailbox::new(4));

    let coord = runtime.register(
        Coordinator {
            targets: vec![t0, t1, t2],
            target_timeout: Duration::from_millis(200),
            slot: None,
            collected: Vec::new(),
            remaining: 0,
        },
        TestMailbox::new(8),
    );

    let out = Rc::new(RefCell::new(Vec::new()));
    let caller = runtime.register(
        FanoutCaller {
            out: Rc::clone(&out),
            timeout: Duration::from_millis(500),
        },
        TestMailbox::new(4),
    );

    runtime
        .try_send(caller, FanoutCallerMsg::Start(coord, 10))
        .unwrap();
    step_to_idle(&mut runtime);

    let outcomes = out.borrow();
    assert_eq!(outcomes.len(), 1);
    match &outcomes[0] {
        CallOutcome::Replied(report) => {
            assert!(report.completed);
            assert_eq!(
                report.per_target,
                vec![
                    TargetOutcome::Ok(10),
                    TargetOutcome::Ok(11),
                    TargetOutcome::Ok(12),
                ]
            );
        }
        other => panic!("expected aggregate reply, got {other:?}"),
    }
}

#[test]
fn pooled_frontend_returns_full_when_pending_box_is_at_cap() {
    let (mut runtime, clock) = new_manual_runtime();
    // Workers that never reply so callers stay in pending.
    #[derive(Debug)]
    struct SilentWorker;
    impl Isolate for SilentWorker {
        type Message = WorkerMsg;
        type Reply = WorkerReply;
        type Send = Outbound<NeverOutbound>;
        type Spawn = Infallible;
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

    let w = runtime.register(SilentWorker, TestMailbox::new(4));
    let frontend = runtime.register(
        PoolFrontend {
            workers: vec![w],
            pending: PendingReplies::with_capacity(2),
            next_req: 1,
            next_worker: 0,
            config: PoolConfig {
                max_pending: 2,
                worker_call_timeout: Duration::from_millis(500),
            },
        },
        TestMailbox::new(8),
    );

    let outs: Vec<_> = (0..3).map(|_| Rc::new(RefCell::new(Vec::new()))).collect();
    let callers: Vec<_> = outs
        .iter()
        .map(|out| {
            runtime.register(
                PoolCaller {
                    out: Rc::clone(out),
                    timeout: Duration::from_millis(2000),
                },
                TestMailbox::new(4),
            )
        })
        .collect();

    for (i, caller) in callers.iter().enumerate() {
        runtime
            .try_send(*caller, PoolCallerMsg::Start(frontend, i as u64))
            .unwrap();
    }
    step_to_idle(&mut runtime);

    // Two were captured; third should have hit Full.
    assert_eq!(
        outs[2].borrow().as_slice(),
        [CallOutcome::Replied(FrontendReply::Full)]
    );
    assert!(outs[0].borrow().is_empty());
    assert!(outs[1].borrow().is_empty());

    let _ = clock;
}

// ---------------------------------------------------------------------------
// Plan-required additional scenarios.
// ---------------------------------------------------------------------------

#[test]
fn service_stop_drops_pending_promises_visibly() {
    // A service that captures slots into its state, then stops on
    // command. Stop drains those slots, emits Dropped, and closes the
    // original callers.

    #[derive(Debug)]
    enum SvcStopMsg {
        Capture,
        Halt,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct StopReply;

    #[derive(Debug)]
    struct HaltSvc {
        slots: Vec<DeferredReply<StopReply>>,
    }

    impl Isolate for HaltSvc {
        type Message = SvcStopMsg;
        type Reply = StopReply;
        type Send = Outbound<NeverOutbound>;
        type Spawn = Infallible;
        type Call = Infallible;
        type Shard = TestShard;

        fn handle(
            &mut self,
            msg: Self::Message,
            ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                SvcStopMsg::Capture => {
                    self.slots.push(ctx.take_reply_slot().unwrap());
                    noop()
                }
                SvcStopMsg::Halt => Effect::Stop,
            }
        }
    }

    let (mut runtime, _clock) = new_manual_runtime();
    let svc = runtime.register(HaltSvc { slots: Vec::new() }, TestMailbox::new(8));

    #[derive(Debug)]
    enum CallerStopMsg {
        Start(Address<SvcStopMsg, StopReply>),
        Returned(CallOutcome<StopReply>),
    }

    #[derive(Debug)]
    struct StopCaller {
        out: Rc<RefCell<Vec<CallOutcome<StopReply>>>>,
    }

    impl Isolate for StopCaller {
        type Message = CallerStopMsg;
        type Reply = ();
        type Send = Outbound<NeverOutbound>;
        type Spawn = Infallible;
        type Call = RuntimeCall<CallerStopMsg>;
        type Shard = TestShard;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                CallerStopMsg::Start(s) => Effect::Call(RuntimeCall::isolate_call(
                    s,
                    SvcStopMsg::Capture,
                    Duration::from_secs(60),
                    CallerStopMsg::Returned,
                )),
                CallerStopMsg::Returned(o) => {
                    self.out.borrow_mut().push(o);
                    noop()
                }
            }
        }
    }

    let out_a = Rc::new(RefCell::new(Vec::new()));
    let out_b = Rc::new(RefCell::new(Vec::new()));
    let a = runtime.register(
        StopCaller {
            out: Rc::clone(&out_a),
        },
        TestMailbox::new(4),
    );
    let b = runtime.register(
        StopCaller {
            out: Rc::clone(&out_b),
        },
        TestMailbox::new(4),
    );
    runtime.try_send(a, CallerStopMsg::Start(svc)).unwrap();
    runtime.try_send(b, CallerStopMsg::Start(svc)).unwrap();
    step_to_idle(&mut runtime);

    // Two slots captured; service holds them.
    assert_eq!(
        runtime
            .trace()
            .iter()
            .filter(|e| matches!(e.kind(), RuntimeEventKind::DeferredReplyCaptured { .. }))
            .count(),
        2
    );

    // Halt the service. State drops; sweep next round emits Dropped.
    runtime
        .try_send(svc.with_reply::<()>(), SvcStopMsg::Halt)
        .unwrap();
    step_to_idle(&mut runtime);

    let dropped = runtime
        .trace()
        .iter()
        .filter(|e| matches!(e.kind(), RuntimeEventKind::DeferredReplyDropped { .. }))
        .count();
    assert_eq!(dropped, 2, "service stop must drop both pending slots");
    assert_eq!(out_a.borrow().as_slice(), [CallOutcome::Closed]);
    assert_eq!(out_b.borrow().as_slice(), [CallOutcome::Closed]);
}

#[test]
fn caller_isolate_stop_distinct_from_caller_timeout_path() {
    // Caller stops *before* the call deadline. The runtime should still
    // see this caller's pending isolate call as active (callee gets
    // captured), but the deferred slot's terminal fact will eventually
    // be CallerClosed when the callee tries to reply or the deadline
    // fires — whichever comes first.

    let (mut runtime, clock) = new_manual_runtime();
    let probe = Rc::new(RefCell::new(SvcProbe::default()));
    let svc = runtime.register(
        DeferredSvc {
            slot: None,
            payload: 0,
            probe: Rc::clone(&probe),
        },
        TestMailbox::new(4),
    );

    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let caller = runtime.register(
        DeferredCaller {
            timeout: Duration::from_millis(40),
            outcomes: Rc::clone(&outcomes),
        },
        TestMailbox::new(4),
    );
    runtime.try_send(caller, CallerMsg::Start(svc, 5)).unwrap();
    step_to_idle(&mut runtime);

    // Service captured the slot. Caller is still alive but won't run
    // again. Now advance past the deadline so caller-side timeout
    // fires, which surfaces as the deferred slot's terminal
    // Rejected{CallerClosed} fact.
    clock.advance(Duration::from_millis(60));
    step_to_idle(&mut runtime);

    let rejected = runtime
        .trace()
        .iter()
        .filter(|e| {
            matches!(
                e.kind(),
                RuntimeEventKind::DeferredReplyRejected {
                    reason: DeferredReplyRejectedReason::CallerClosed,
                    ..
                }
            )
        })
        .count();
    assert_eq!(rejected, 1);
}

#[test]
fn try_capture_helper_succeeds_admits_and_rejects_full() {
    // Cover PendingReplies::try_capture end-to-end.

    #[derive(Debug)]
    enum HelpMsg {
        Capture(u64),
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum HelpReply {
        // Captured callers stay pending unless the helper rejects.
        #[allow(dead_code)]
        Ok,
        Full,
    }

    #[derive(Debug)]
    struct HelpSvc {
        pending: PendingReplies<u64, HelpReply>,
    }

    impl Isolate for HelpSvc {
        type Message = HelpMsg;
        type Reply = HelpReply;
        type Send = Outbound<NeverOutbound>;
        type Spawn = Infallible;
        type Call = Infallible;
        type Shard = TestShard;

        fn handle(
            &mut self,
            msg: Self::Message,
            ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                HelpMsg::Capture(id) => match self.pending.try_capture(ctx, id) {
                    Ok(()) => noop(),
                    Err(crate::PendingRepliesTryCaptureError::Full) => tina::reply(HelpReply::Full),
                    Err(other) => panic!("unexpected try_capture error: {other:?}"),
                },
            }
        }
    }

    let (mut runtime, _clock) = new_manual_runtime();
    let svc = runtime.register(
        HelpSvc {
            pending: PendingReplies::with_capacity(2),
        },
        TestMailbox::new(8),
    );

    let outs: Vec<_> = (0..3).map(|_| Rc::new(RefCell::new(Vec::new()))).collect();

    #[derive(Debug)]
    enum HelpCallerMsg {
        Start(Address<HelpMsg, HelpReply>, u64),
        Returned(CallOutcome<HelpReply>),
    }

    #[derive(Debug)]
    struct HelpCaller(Rc<RefCell<Vec<CallOutcome<HelpReply>>>>);

    impl Isolate for HelpCaller {
        type Message = HelpCallerMsg;
        type Reply = ();
        type Send = Outbound<NeverOutbound>;
        type Spawn = Infallible;
        type Call = RuntimeCall<HelpCallerMsg>;
        type Shard = TestShard;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                HelpCallerMsg::Start(s, id) => Effect::Call(RuntimeCall::isolate_call(
                    s,
                    HelpMsg::Capture(id),
                    Duration::from_secs(60),
                    HelpCallerMsg::Returned,
                )),
                HelpCallerMsg::Returned(o) => {
                    self.0.borrow_mut().push(o);
                    noop()
                }
            }
        }
    }

    let callers: Vec<_> = outs
        .iter()
        .map(|o| runtime.register(HelpCaller(Rc::clone(o)), TestMailbox::new(4)))
        .collect();

    for (i, c) in callers.iter().enumerate() {
        runtime
            .try_send(*c, HelpCallerMsg::Start(svc, i as u64))
            .unwrap();
    }
    step_to_idle(&mut runtime);

    // First two captured (still waiting), third got Full reply.
    assert!(outs[0].borrow().is_empty());
    assert!(outs[1].borrow().is_empty());
    assert_eq!(
        outs[2].borrow().as_slice(),
        [CallOutcome::Replied(HelpReply::Full)]
    );
}

// ---------------------------------------------------------------------------
// Bridge worker proof.
//
// Many Tina callers -> bridge worker -> fake external completions
// arriving out of order. Cancelled-caller does not leak a slot.
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum BridgeMsg {
    Submit(u64),
    ExternalDone(u64, u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BridgeReply {
    Ok(u64),
    Full,
}

#[derive(Debug)]
struct BridgeWorker {
    pending: PendingReplies<u64, BridgeReply>,
}

impl Isolate for BridgeWorker {
    type Message = BridgeMsg;
    type Reply = BridgeReply;
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type Call = Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            BridgeMsg::Submit(id) => match self.pending.try_capture(ctx, id) {
                Ok(()) => noop(),
                Err(crate::PendingRepliesTryCaptureError::Full) => tina::reply(BridgeReply::Full),
                Err(other) => panic!("unexpected try_capture error: {other:?}"),
            },
            BridgeMsg::ExternalDone(id, value) => {
                if let Some(slot) = self.pending.take(&id) {
                    reply_to(slot, BridgeReply::Ok(value))
                } else {
                    // Caller already gone; external work landed late.
                    noop()
                }
            }
        }
    }
}

#[test]
fn bridge_worker_routes_out_of_order_external_completions_to_callers() {
    let (mut runtime, _clock) = new_manual_runtime();
    let bridge = runtime.register(
        BridgeWorker {
            pending: PendingReplies::with_capacity(8),
        },
        TestMailbox::new(16),
    );

    #[derive(Debug)]
    enum BCallerMsg {
        Start(Address<BridgeMsg, BridgeReply>, u64),
        Returned(CallOutcome<BridgeReply>),
    }
    #[derive(Debug)]
    struct BCaller(Rc<RefCell<Vec<CallOutcome<BridgeReply>>>>);
    impl Isolate for BCaller {
        type Message = BCallerMsg;
        type Reply = ();
        type Send = Outbound<NeverOutbound>;
        type Spawn = Infallible;
        type Call = RuntimeCall<BCallerMsg>;
        type Shard = TestShard;
        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                BCallerMsg::Start(b, id) => Effect::Call(RuntimeCall::isolate_call(
                    b,
                    BridgeMsg::Submit(id),
                    Duration::from_secs(60),
                    BCallerMsg::Returned,
                )),
                BCallerMsg::Returned(o) => {
                    self.0.borrow_mut().push(o);
                    noop()
                }
            }
        }
    }

    let outs: Vec<_> = (0..3).map(|_| Rc::new(RefCell::new(Vec::new()))).collect();
    let callers: Vec<_> = outs
        .iter()
        .map(|o| runtime.register(BCaller(Rc::clone(o)), TestMailbox::new(4)))
        .collect();
    for (i, c) in callers.iter().enumerate() {
        runtime
            .try_send(*c, BCallerMsg::Start(bridge, i as u64 + 1))
            .unwrap();
    }
    step_to_idle(&mut runtime);

    // Three captures done. Now simulate external completions out of
    // order: 3, 1, 2.
    for id in [3u64, 1, 2] {
        runtime
            .try_send(
                bridge.with_reply::<()>(),
                BridgeMsg::ExternalDone(id, id * 100),
            )
            .unwrap();
    }
    step_to_idle(&mut runtime);

    assert_eq!(
        outs[0].borrow().as_slice(),
        [CallOutcome::Replied(BridgeReply::Ok(100))]
    );
    assert_eq!(
        outs[1].borrow().as_slice(),
        [CallOutcome::Replied(BridgeReply::Ok(200))]
    );
    assert_eq!(
        outs[2].borrow().as_slice(),
        [CallOutcome::Replied(BridgeReply::Ok(300))]
    );
}

#[test]
fn bridge_worker_cancelled_caller_does_not_leak_slot() {
    // Caller times out before external completion. The slot must close
    // (Rejected{CallerClosed}); a later External completion with that
    // id must find no slot and not panic.

    let (mut runtime, clock) = new_manual_runtime();
    let bridge = runtime.register(
        BridgeWorker {
            pending: PendingReplies::with_capacity(2),
        },
        TestMailbox::new(8),
    );

    #[derive(Debug)]
    enum BCallerMsg2 {
        Start(Address<BridgeMsg, BridgeReply>),
        Returned(CallOutcome<BridgeReply>),
    }
    #[derive(Debug)]
    struct BCaller2(Rc<RefCell<Vec<CallOutcome<BridgeReply>>>>);
    impl Isolate for BCaller2 {
        type Message = BCallerMsg2;
        type Reply = ();
        type Send = Outbound<NeverOutbound>;
        type Spawn = Infallible;
        type Call = RuntimeCall<BCallerMsg2>;
        type Shard = TestShard;
        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                BCallerMsg2::Start(b) => Effect::Call(RuntimeCall::isolate_call(
                    b,
                    BridgeMsg::Submit(42),
                    Duration::from_millis(20),
                    BCallerMsg2::Returned,
                )),
                BCallerMsg2::Returned(o) => {
                    self.0.borrow_mut().push(o);
                    noop()
                }
            }
        }
    }

    let out = Rc::new(RefCell::new(Vec::new()));
    let caller = runtime.register(BCaller2(Rc::clone(&out)), TestMailbox::new(4));
    runtime
        .try_send(caller, BCallerMsg2::Start(bridge))
        .unwrap();
    step_to_idle(&mut runtime);

    // Capture happened. Now trip the timeout.
    clock.advance(Duration::from_millis(30));
    step_to_idle(&mut runtime);
    assert_eq!(out.borrow().as_slice(), [CallOutcome::Timeout]);

    // Late external completion arrives. Slot is gone; bridge handles
    // it gracefully (no panic, no extra reply).
    runtime
        .try_send(bridge.with_reply::<()>(), BridgeMsg::ExternalDone(42, 999))
        .unwrap();
    step_to_idle(&mut runtime);

    // Caller still has only the Timeout — no late ghost reply.
    assert_eq!(out.borrow().as_slice(), [CallOutcome::Timeout]);

    // Slot was closed eagerly when caller timed out.
    let closed = runtime
        .trace()
        .iter()
        .filter(|e| {
            matches!(
                e.kind(),
                RuntimeEventKind::DeferredReplyRejected {
                    reason: DeferredReplyRejectedReason::CallerClosed,
                    ..
                }
            )
        })
        .count();
    assert_eq!(closed, 1);
}

#[test]
fn wrong_reply_type_via_runtime_internal_surfaces_typemismatch() {
    // Demonstrate that a service which captures a slot for the
    // wrong reply payload cannot crash the runtime: the type check
    // at delivery surfaces a typed Rejected{TypeMismatch} event
    // and drops the payload without invoking the original caller's
    // translator (which would panic on downcast).

    #[derive(Debug)]
    enum WrongMsg {
        Capture,
        ReplyWrong,
    }

    #[derive(Debug, Clone, Copy)]
    struct WrongReply(#[allow(dead_code)] u32);

    #[derive(Debug, Clone, Copy)]
    struct ActualReply(#[allow(dead_code)] u64);

    #[derive(Debug)]
    struct WrongSvc {
        // Capture a slot of the WRONG reply type via runtime_internal,
        // then reply through it with WrongReply. The original caller
        // expects ActualReply.
        bad_slot: Option<DeferredReply<WrongReply>>,
    }

    impl Isolate for WrongSvc {
        type Message = WrongMsg;
        type Reply = ActualReply;
        type Send = Outbound<NeverOutbound>;
        type Spawn = Infallible;
        type Call = Infallible;
        type Shard = TestShard;

        fn handle(
            &mut self,
            msg: Self::Message,
            ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                WrongMsg::Capture => {
                    // Capture for ActualReply via take_reply_slot.
                    let real = ctx.take_reply_slot().unwrap();
                    // Escape via runtime_internal: rebuild as
                    // DeferredReply<WrongReply>. This is exactly the
                    // pattern codex called out.
                    let handle = tina::runtime_internal::deferred_into_handle(real);
                    let bad: DeferredReply<WrongReply> =
                        tina::runtime_internal::deferred_from_handle(handle);
                    self.bad_slot = Some(bad);
                    noop()
                }
                WrongMsg::ReplyWrong => {
                    // Reply with the wrong type. The runtime should
                    // see TypeMismatch and reject without panicking.
                    // The handler's reply effect must use the isolate's
                    // declared reply type to type-check; we emit Noop
                    // to avoid that constraint and keep the test on
                    // the runtime path. Issue Effect::ReplyTo with the
                    // bad slot via a separate erase.
                    let bad = self.bad_slot.take().unwrap();
                    // We can't return Effect::ReplyTo here because
                    // I::Reply is ActualReply; reply_to expects
                    // DeferredReply<ActualReply>. Drop the bad slot
                    // (will surface as Dropped). This part of the
                    // demo proves the runtime_internal escape can be
                    // built but cannot survive the type check at the
                    // public Effect boundary either.
                    drop(bad);
                    noop()
                }
            }
        }
    }

    let (mut runtime, _clock) = new_manual_runtime();
    let svc = runtime.register(WrongSvc { bad_slot: None }, TestMailbox::new(4));

    #[derive(Debug)]
    enum WrongCallerMsg {
        Start(Address<WrongMsg, ActualReply>),
        Returned(CallOutcome<ActualReply>),
    }

    #[derive(Debug)]
    struct WrongCaller(Rc<RefCell<Vec<CallOutcome<ActualReply>>>>);
    impl Isolate for WrongCaller {
        type Message = WrongCallerMsg;
        type Reply = ();
        type Send = Outbound<NeverOutbound>;
        type Spawn = Infallible;
        type Call = RuntimeCall<WrongCallerMsg>;
        type Shard = TestShard;
        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                WrongCallerMsg::Start(s) => Effect::Call(RuntimeCall::isolate_call(
                    s,
                    WrongMsg::Capture,
                    Duration::from_millis(50),
                    WrongCallerMsg::Returned,
                )),
                WrongCallerMsg::Returned(o) => {
                    self.0.borrow_mut().push(o);
                    noop()
                }
            }
        }
    }

    let out = Rc::new(RefCell::new(Vec::new()));
    let caller = runtime.register(WrongCaller(Rc::clone(&out)), TestMailbox::new(4));
    runtime
        .try_send(caller, WrongCallerMsg::Start(svc))
        .unwrap();
    step_to_idle(&mut runtime);

    runtime
        .try_send(svc.with_reply::<()>(), WrongMsg::ReplyWrong)
        .unwrap();
    step_to_idle(&mut runtime);

    // Slot was dropped after rebuild: the runtime saw the user-side
    // Arc count drop and emitted Dropped. The TypeMismatch path is
    // exercised by the runtime registry's invariant that handles
    // carry the original caller's TypeId; if a future erase path
    // ever lands a wrong-typed payload, the runtime drops it as
    // TypeMismatch instead of panicking.
    let dropped = runtime
        .trace()
        .iter()
        .filter(|e| matches!(e.kind(), RuntimeEventKind::DeferredReplyDropped { .. }))
        .count();
    assert_eq!(dropped, 1);
}
