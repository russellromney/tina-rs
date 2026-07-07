//! Simulator parity for deferred reply slots.
//!
//! The simulator and live runtime must agree on the externally visible
//! meaning of capture, send, drop, caller-closed, and reply-path
//! failures. These tests cover the same lifecycle scenarios as
//! `tina-runtime`'s `tests/deferred.rs` against the simulator.

use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Duration;

use tina::{
    Address, Context, DeferredReply, Effect, Isolate, Outbound, Shard, ShardId, noop, reply_to,
};
use tina_runtime::{
    CallOutcome, DeferredReplyRejectedReason, RuntimeCall, RuntimeEvent, RuntimeEventKind,
    stable_trace_hash,
};
use tina_sim::{Simulator, SimulatorConfig};

#[derive(Debug, Default)]
struct DefShard;

impl Shard for DefShard {
    fn id(&self) -> ShardId {
        ShardId::new(7)
    }
}

#[derive(Debug)]
enum SvcMsg {
    Capture,
    ReplyStored,
    DropStored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SvcReply(u64);

#[derive(Debug)]
struct DeferredSvc {
    payload: u64,
    slot: Option<DeferredReply<SvcReply>>,
}

impl Isolate for DeferredSvc {
    type Message = SvcMsg;
    type Reply = SvcReply;
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Io = RuntimeCall<SvcMsg>;
    type Fact = ::std::convert::Infallible;
    type Shard = DefShard;

    fn handle_call(
        &mut self,
        msg: Self::Message,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        match msg {
            SvcMsg::Capture => {
                self.slot = Some(call.into_request_context().into_deferred());
                noop()
            }
            SvcMsg::ReplyStored | SvcMsg::DropStored => {
                call.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SvcMsg::Capture => noop(),
            SvcMsg::ReplyStored => {
                let slot = self.slot.take().expect("slot stored");
                reply_to(slot, SvcReply(self.payload))
            }
            SvcMsg::DropStored => {
                self.slot = None;
                noop()
            }
        }
    }
}

#[derive(Debug)]
enum CallerMsg {
    Start(Address<SvcMsg, SvcReply>),
    Returned(CallOutcome<SvcReply>),
}

#[derive(Debug)]
struct Caller {
    timeout: Duration,
    out: Rc<RefCell<Vec<CallOutcome<SvcReply>>>>,
}

impl Isolate for Caller {
    type Message = CallerMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Io = RuntimeCall<CallerMsg>;
    type Fact = ::std::convert::Infallible;
    type Shard = DefShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CallerMsg::Start(svc) => Effect::Io(RuntimeCall::isolate_call(
                svc,
                SvcMsg::Capture,
                self.timeout,
                CallerMsg::Returned,
            )),
            CallerMsg::Returned(outcome) => {
                self.out.borrow_mut().push(outcome);
                noop()
            }
        }
    }
}

fn count_kind(events: &[RuntimeEvent], pred: impl Fn(&RuntimeEventKind) -> bool) -> usize {
    events.iter().filter(|e| pred(&e.kind())).count()
}

fn step_to_idle<S: Shard + 'static>(sim: &mut Simulator<S>) {
    while sim.step() > 0 {}
}

#[test]
fn sim_capture_and_send_routes_to_original_caller() {
    let mut sim = Simulator::new(DefShard, SimulatorConfig::default());
    let svc = sim.register(DeferredSvc {
        payload: 17,
        slot: None,
    });
    let out = Rc::new(RefCell::new(Vec::new()));
    let caller = sim.register(Caller {
        timeout: Duration::from_millis(50),
        out: Rc::clone(&out),
    });
    sim.try_send(caller, CallerMsg::Start(svc)).unwrap();
    step_to_idle(&mut sim);

    assert!(out.borrow().is_empty(), "no reply yet");
    assert_eq!(
        count_kind(sim.trace(), |k| matches!(
            k,
            RuntimeEventKind::DeferredReplyCaptured { .. }
        )),
        1
    );

    sim.try_send(svc.with_reply::<()>(), SvcMsg::ReplyStored)
        .unwrap();
    step_to_idle(&mut sim);

    assert_eq!(
        out.borrow().as_slice(),
        [CallOutcome::Replied(SvcReply(17))]
    );
    assert_eq!(
        count_kind(sim.trace(), |k| matches!(
            k,
            RuntimeEventKind::DeferredReplySent { .. }
        )),
        1
    );
}

#[test]
fn sim_caller_timeout_emits_terminal_caller_closed_rejected() {
    let mut sim = Simulator::new(DefShard, SimulatorConfig::default());
    let svc = sim.register(DeferredSvc {
        payload: 0,
        slot: None,
    });
    let out = Rc::new(RefCell::new(Vec::new()));
    let caller = sim.register(Caller {
        timeout: Duration::from_millis(20),
        out: Rc::clone(&out),
    });
    sim.try_send(caller, CallerMsg::Start(svc)).unwrap();
    step_to_idle(&mut sim);

    // Advance virtual time past the deadline to fire the timeout.
    sim.advance_time(Duration::from_millis(30));
    step_to_idle(&mut sim);

    assert_eq!(out.borrow().as_slice(), [CallOutcome::Timeout]);
    assert_eq!(
        count_kind(sim.trace(), |k| matches!(
            k,
            RuntimeEventKind::DeferredReplyRejected {
                reason: DeferredReplyRejectedReason::CallerTimedOut,
                ..
            }
        )),
        1
    );
}

#[test]
fn sim_dropped_open_slot_emits_dropped_event() {
    let mut sim = Simulator::new(DefShard, SimulatorConfig::default());
    let svc = sim.register(DeferredSvc {
        payload: 0,
        slot: None,
    });
    let out = Rc::new(RefCell::new(Vec::new()));
    let caller = sim.register(Caller {
        timeout: Duration::from_secs(10),
        out: Rc::clone(&out),
    });
    sim.try_send(caller, CallerMsg::Start(svc)).unwrap();
    step_to_idle(&mut sim);

    sim.try_send(svc.with_reply::<()>(), SvcMsg::DropStored)
        .unwrap();
    step_to_idle(&mut sim);

    assert_eq!(
        count_kind(sim.trace(), |k| matches!(
            k,
            RuntimeEventKind::DeferredReplyDropped { .. }
        )),
        1
    );
    assert_eq!(out.borrow().as_slice(), [CallOutcome::Closed]);
}

#[test]
fn sim_panic_after_capture_drops_slot_and_closes_caller() {
    #[derive(Debug)]
    struct PanicAfterCapture;

    impl Isolate for PanicAfterCapture {
        type Message = SvcMsg;
        type Reply = SvcReply;
        type Send = Outbound<Infallible>;
        type Spawn = Infallible;
        type SpawnObserved = std::convert::Infallible;
        type Io = RuntimeCall<SvcMsg>;
        type Fact = ::std::convert::Infallible;
        type Shard = DefShard;

        fn handle_call(
            &mut self,
            _msg: Self::Message,
            call: tina::CallContext<'_, Self>,
        ) -> Effect<Self> {
            let _slot = call.into_request_context().into_deferred();
            panic!("boom after deferred capture");
        }

        fn handle(
            &mut self,
            _msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            noop()
        }
    }

    let mut sim = Simulator::new(DefShard, SimulatorConfig::default());
    let svc = sim.register(PanicAfterCapture);
    let out = Rc::new(RefCell::new(Vec::new()));
    let caller = sim.register(Caller {
        timeout: Duration::from_secs(60),
        out: Rc::clone(&out),
    });

    sim.try_send(caller, CallerMsg::Start(svc)).unwrap();
    step_to_idle(&mut sim);

    assert_eq!(out.borrow().as_slice(), [CallOutcome::Closed]);
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
            RuntimeEventKind::DeferredReplyDropped { .. }
        )),
        1
    );
}

#[test]
fn sim_frontend_stop_drops_pending_promises_visibly() {
    #[derive(Debug)]
    enum HaltMsg {
        Capture,
        Halt,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct HaltReply;

    #[derive(Debug)]
    struct HaltSvc {
        slots: Vec<DeferredReply<HaltReply>>,
    }

    impl Isolate for HaltSvc {
        type Message = HaltMsg;
        type Reply = HaltReply;
        type Send = Outbound<Infallible>;
        type Spawn = Infallible;
        type SpawnObserved = std::convert::Infallible;
        type Io = RuntimeCall<HaltMsg>;
        type Fact = ::std::convert::Infallible;
        type Shard = DefShard;

        fn handle_call(
            &mut self,
            msg: Self::Message,
            call: tina::CallContext<'_, Self>,
        ) -> Effect<Self> {
            match msg {
                HaltMsg::Capture => {
                    self.slots.push(call.into_request_context().into_deferred());
                    noop()
                }
                HaltMsg::Halt => call.reject(tina::CallRejectedReason::UnsupportedMessage),
            }
        }

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                HaltMsg::Capture => noop(),
                HaltMsg::Halt => Effect::Stop,
            }
        }
    }

    let mut sim = Simulator::new(DefShard, SimulatorConfig::default());
    let svc = sim.register(HaltSvc { slots: Vec::new() });

    #[derive(Debug)]
    enum HCallerMsg {
        Start(Address<HaltMsg, HaltReply>),
        Returned(CallOutcome<HaltReply>),
    }
    #[derive(Debug)]
    struct HCaller(std::rc::Rc<std::cell::RefCell<Vec<CallOutcome<HaltReply>>>>);
    impl Isolate for HCaller {
        type Message = HCallerMsg;
        type Reply = ();
        type Send = Outbound<Infallible>;
        type Spawn = Infallible;
        type SpawnObserved = std::convert::Infallible;
        type Io = RuntimeCall<HCallerMsg>;
        type Fact = ::std::convert::Infallible;
        type Shard = DefShard;
        fn handle(
            &mut self,
            msg: Self::Message,
            _: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                HCallerMsg::Start(s) => Effect::Io(RuntimeCall::isolate_call(
                    s,
                    HaltMsg::Capture,
                    Duration::from_secs(60),
                    HCallerMsg::Returned,
                )),
                HCallerMsg::Returned(o) => {
                    self.0.borrow_mut().push(o);
                    noop()
                }
            }
        }
    }

    let outs: Vec<_> = (0..2)
        .map(|_| std::rc::Rc::new(std::cell::RefCell::new(Vec::new())))
        .collect();
    let callers: Vec<_> = outs
        .iter()
        .map(|o| sim.register(HCaller(std::rc::Rc::clone(o))))
        .collect();
    for c in &callers {
        sim.try_send(*c, HCallerMsg::Start(svc)).unwrap();
    }
    step_to_idle(&mut sim);

    sim.try_send(svc.with_reply::<()>(), HaltMsg::Halt).unwrap();
    step_to_idle(&mut sim);

    let dropped = count_kind(sim.trace(), |k| {
        matches!(k, RuntimeEventKind::DeferredReplyDropped { .. })
    });
    assert_eq!(dropped, 2);
    assert_eq!(outs[0].borrow().as_slice(), [CallOutcome::Closed]);
    assert_eq!(outs[1].borrow().as_slice(), [CallOutcome::Closed]);
}

#[test]
fn sim_same_seed_replays_to_same_trace_fingerprint() {
    fn run_once() -> u64 {
        let mut sim = Simulator::new(DefShard, SimulatorConfig::default());
        let svc = sim.register(DeferredSvc {
            payload: 42,
            slot: None,
        });
        let out = Rc::new(RefCell::new(Vec::new()));
        let caller = sim.register(Caller {
            timeout: Duration::from_millis(50),
            out,
        });
        sim.try_send(caller, CallerMsg::Start(svc)).unwrap();
        let mut sim = sim;
        step_to_idle(&mut sim);
        sim.try_send(svc.with_reply::<()>(), SvcMsg::ReplyStored)
            .unwrap();
        step_to_idle(&mut sim);
        stable_trace_hash(sim.trace())
    }
    assert_eq!(run_once(), run_once(), "deferred trace must be replayable");
}
