//! Phase 110 — end-to-end tests for the workflow pending helpers.
//!
//! Unit tests cover the storage layer directly. These tests exercise
//! the real call path through `ThreadedRuntime` so the user-visible
//! behavior is proven: caller authority is consumed only on admission,
//! parked callers get the right reply, `Full` rejections do not strand
//! callers, and guarded leases drop exactly once.

use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Duration;

use tina::prelude::*;
use tina::{CallRejectedReason, reply_to};
use tina_runtime::{
    CallOutcome, CancelableWork, DefaultThreadedMailboxFactory, GuardedPendingReplies,
    GuardedParkCallError, ParkCallError, PendingReplies, ThreadedRuntime, WaitList, sleep,
};

// ---------------------------------------------------------------------------
// park_call: happy path — two callers admitted, both replied after timer.
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum ParkSvcMsg {
    Hold,
    Flush,
}

struct ParkSvc {
    pending: PendingReplies<u64, u32>,
    next_qid: u64,
    armed: bool,
}

#[tina_runtime::isolate(message = ParkSvcMsg, reply = u32)]
impl ParkSvc {
    fn handle(
        &mut self,
        msg: ParkSvcMsg,
        _ctx: &mut Context<'_, SingleShard, u32>,
    ) -> Effect<Self> {
        match msg {
            ParkSvcMsg::Hold => noop(),
            ParkSvcMsg::Flush => {
                self.armed = false;
                self.pending.drain_replies_into_effect(7u32)
            }
        }
    }

    fn handle_call(&mut self, msg: ParkSvcMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            ParkSvcMsg::Hold => {
                let qid = self.next_qid;
                let next_qid = qid + 1;
                match self.pending.park_call(qid, call) {
                    Ok(_ticket) => {
                        self.next_qid = next_qid;
                        if self.armed {
                            noop()
                        } else {
                            self.armed = true;
                            sleep(Duration::from_millis(10))
                                .then_event(|| ParkSvcMsg::Flush)
                        }
                    }
                    Err(ParkCallError::Full { call, .. }) => call.reply(99u32),
                    Err(ParkCallError::DuplicateKey { call, .. }) => {
                        call.reject(CallRejectedReason::UnsupportedMessage)
                    }
                    Err(ParkCallError::NoCaller { call, .. }) => {
                        call.reject(CallRejectedReason::UnsupportedMessage)
                    }
                    Err(ParkCallError::CrossShardUnsupported { call, .. }) => {
                        call.reject(CallRejectedReason::UnsupportedMessage)
                    }
                }
            }
            ParkSvcMsg::Flush => call.reject(CallRejectedReason::UnsupportedMessage),
        }
    }
}

#[test]
fn park_call_e2e_admits_two_callers_and_replies_after_timer() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let svc = runtime
        .register_service::<_, Infallible>(
            ParkSvc {
                pending: PendingReplies::with_capacity(2).named("park.e2e"),
                next_qid: 1,
                armed: false,
            },
            16,
        )
        .expect("register");

    let outcomes = Arc::new(Mutex::new(Vec::<u32>::new()));
    let gate = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let rt = Arc::clone(&runtime);
        let out = Arc::clone(&outcomes);
        let gate_h = Arc::clone(&gate);
        handles.push(thread::spawn(move || {
            gate_h.wait();
            let r = rt
                .call_blocking_typed(svc.call, ParkSvcMsg::Hold, Duration::from_secs(2))
                .expect("admission");
            if let CallOutcome::Replied(v) = r {
                out.lock().expect("out").push(v);
            } else {
                panic!("expected reply, got {r:?}");
            }
        }));
    }
    gate.wait();
    for h in handles {
        h.join().expect("worker");
    }
    let got = outcomes.lock().expect("out").clone();
    assert_eq!(got.len(), 2);
    assert!(got.iter().all(|v| *v == 7));
}

// ---------------------------------------------------------------------------
// park_call: Full path returns caller authority and the caller does NOT
// time out. Capacity = 1; first caller is parked indefinitely. Second
// caller must receive the typed Full reply (99) quickly.
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ParkSlowMsg;

struct ParkSlowSvc {
    pending: PendingReplies<u64, u32>,
    next_qid: u64,
}

#[tina_runtime::isolate(message = ParkSlowMsg, reply = u32)]
impl ParkSlowSvc {
    fn handle(
        &mut self,
        _msg: ParkSlowMsg,
        _ctx: &mut Context<'_, SingleShard, u32>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(&mut self, _msg: ParkSlowMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        let qid = self.next_qid;
        let next_qid = qid + 1;
        match self.pending.park_call(qid, call) {
            Ok(_ticket) => {
                self.next_qid = next_qid;
                noop()
            }
            Err(ParkCallError::Full { call, .. }) => call.reply(99),
            Err(ParkCallError::DuplicateKey { call, .. }) => {
                call.reject(CallRejectedReason::UnsupportedMessage)
            }
            Err(ParkCallError::NoCaller { call, .. }) => {
                call.reject(CallRejectedReason::UnsupportedMessage)
            }
            Err(ParkCallError::CrossShardUnsupported { call, .. }) => {
                call.reject(CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

#[test]
fn park_call_full_returns_typed_reply_immediately_without_timeout() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let svc = runtime
        .register_service::<_, Infallible>(
            ParkSlowSvc {
                pending: PendingReplies::with_capacity(1).named("park.full"),
                next_qid: 1,
            },
            8,
        )
        .expect("register");

    // First caller is parked forever (no flush message ever fires).
    let rt = Arc::clone(&runtime);
    let _bg = thread::spawn(move || {
        let _ = rt.call_blocking_typed(svc.call, ParkSlowMsg, Duration::from_millis(200));
    });

    // Let the first call land and park.
    thread::sleep(Duration::from_millis(30));

    // Second caller must receive Full reply (99) quickly, not time out.
    let t0 = std::time::Instant::now();
    let r = runtime
        .call_blocking_typed(svc.call, ParkSlowMsg, Duration::from_secs(2))
        .expect("admission");
    let elapsed = t0.elapsed();
    match r {
        CallOutcome::Replied(99) => {}
        other => panic!("expected typed Full reply (99), got {other:?}"),
    }
    assert!(
        elapsed < Duration::from_millis(500),
        "Full path should return quickly, took {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// GuardedPendingReplies: caller is replied AND the guard is dropped
// exactly once. We count guard drops via an atomic.
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct DropCounter {
    counter: Arc<AtomicUsize>,
}

impl Drop for DropCounter {
    fn drop(&mut self) {
        self.counter.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Debug)]
enum GuardSvcMsg {
    Hold,
    Flush,
}

struct GuardSvc {
    pending: GuardedPendingReplies<u64, u32, DropCounter>,
    counter: Arc<AtomicUsize>,
    next_qid: u64,
    armed: bool,
}

#[tina_runtime::isolate(message = GuardSvcMsg, reply = u32)]
impl GuardSvc {
    fn handle(
        &mut self,
        msg: GuardSvcMsg,
        _ctx: &mut Context<'_, SingleShard, u32>,
    ) -> Effect<Self> {
        match msg {
            GuardSvcMsg::Hold => noop(),
            GuardSvcMsg::Flush => {
                self.armed = false;
                let drained = self.pending.drain();
                let mut effects = Vec::with_capacity(drained.len());
                for (_key, slot, guard) in drained {
                    effects.push(reply_to::<Self>(slot, 7u32));
                    drop(guard);
                }
                if effects.is_empty() {
                    noop()
                } else {
                    Effect::Batch(effects)
                }
            }
        }
    }

    fn handle_call(&mut self, msg: GuardSvcMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            GuardSvcMsg::Hold => {
                let qid = self.next_qid;
                let next_qid = qid + 1;
                let guard = DropCounter {
                    counter: Arc::clone(&self.counter),
                };
                match self.pending.park_call_guarded(qid, call, guard) {
                    Ok(_ticket) => {
                        self.next_qid = next_qid;
                        if self.armed {
                            noop()
                        } else {
                            self.armed = true;
                            sleep(Duration::from_millis(10))
                                .then_event(|| GuardSvcMsg::Flush)
                        }
                    }
                    Err(GuardedParkCallError::Full { call, .. }) => call.reply(99),
                    Err(GuardedParkCallError::DuplicateKey { call, .. }) => {
                        call.reject(CallRejectedReason::UnsupportedMessage)
                    }
                    Err(GuardedParkCallError::NoCaller { call, .. }) => {
                        call.reject(CallRejectedReason::UnsupportedMessage)
                    }
                    Err(GuardedParkCallError::CrossShardUnsupported { call, .. }) => {
                        call.reject(CallRejectedReason::UnsupportedMessage)
                    }
                }
            }
            GuardSvcMsg::Flush => call.reject(CallRejectedReason::UnsupportedMessage),
        }
    }
}

#[test]
fn guarded_park_e2e_drops_guard_exactly_once_per_caller() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let counter = Arc::new(AtomicUsize::new(0));
    let svc = runtime
        .register_service::<_, Infallible>(
            GuardSvc {
                pending: GuardedPendingReplies::with_capacity(2).named("guard.e2e"),
                counter: Arc::clone(&counter),
                next_qid: 1,
                armed: false,
            },
            16,
        )
        .expect("register");

    let outcomes = Arc::new(Mutex::new(Vec::<u32>::new()));
    let gate = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let rt = Arc::clone(&runtime);
        let out = Arc::clone(&outcomes);
        let gate_h = Arc::clone(&gate);
        handles.push(thread::spawn(move || {
            gate_h.wait();
            let r = rt
                .call_blocking_typed(svc.call, GuardSvcMsg::Hold, Duration::from_secs(2))
                .expect("admission");
            if let CallOutcome::Replied(v) = r {
                out.lock().expect("out").push(v);
            }
        }));
    }
    gate.wait();
    for h in handles {
        h.join().expect("worker");
    }
    let got = outcomes.lock().expect("out").clone();
    assert_eq!(got.len(), 2);
    assert!(got.iter().all(|v| *v == 7));
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "each guard dropped exactly once on reply"
    );
}

#[test]
fn guarded_park_full_returns_guard_back_for_drop() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let counter = Arc::new(AtomicUsize::new(0));
    let svc = runtime
        .register_service::<_, Infallible>(
            GuardSvc {
                pending: GuardedPendingReplies::with_capacity(1).named("guard.full"),
                counter: Arc::clone(&counter),
                next_qid: 1,
                armed: false,
            },
            8,
        )
        .expect("register");

    let outcomes = Arc::new(Mutex::new(Vec::<u32>::new()));
    let gate = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let rt = Arc::clone(&runtime);
        let out = Arc::clone(&outcomes);
        let gate_h = Arc::clone(&gate);
        handles.push(thread::spawn(move || {
            gate_h.wait();
            let r = rt
                .call_blocking_typed(svc.call, GuardSvcMsg::Hold, Duration::from_secs(2))
                .expect("admission");
            if let CallOutcome::Replied(v) = r {
                out.lock().expect("out").push(v);
            }
        }));
    }
    gate.wait();
    for h in handles {
        h.join().expect("worker");
    }
    let got = outcomes.lock().expect("out").clone();
    assert_eq!(got.len(), 2, "both callers got some reply");
    // Either both got 7 (race won), or one got 7 and one got 99.
    // The invariant is: total guards dropped == 2 (one per caller).
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "every guard dropped exactly once (winner via reply, loser via Full)"
    );
}

// ---------------------------------------------------------------------------
// WaitList: many callers wait for one key, single resolution unblocks all.
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum WaitSvcMsg {
    Get,
    Resolve,
}

struct WaitSvc {
    waiters: WaitList<u32, u64>,
    armed: bool,
}

#[tina_runtime::isolate(message = WaitSvcMsg, reply = u64)]
impl WaitSvc {
    fn handle(
        &mut self,
        msg: WaitSvcMsg,
        _ctx: &mut Context<'_, SingleShard, u64>,
    ) -> Effect<Self> {
        match msg {
            WaitSvcMsg::Get => noop(),
            WaitSvcMsg::Resolve => {
                self.armed = false;
                let effects: Vec<Effect<Self>> = self.waiters.reply_all_clone(&1, 42u64);
                if effects.is_empty() {
                    noop()
                } else {
                    Effect::Batch(effects)
                }
            }
        }
    }

    fn handle_call(&mut self, msg: WaitSvcMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            WaitSvcMsg::Get => match self.waiters.park_call(1u32, call) {
                Ok(_ticket) => {
                    if self.armed {
                        noop()
                    } else {
                        self.armed = true;
                        sleep(Duration::from_millis(20))
                            .then_event(|| WaitSvcMsg::Resolve)
                    }
                }
                Err(tina_runtime::wait_list::WaitCallError::Full { call, .. }) => call.reply(0),
                Err(tina_runtime::wait_list::WaitCallError::KeyFull { call, .. }) => {
                    call.reply(0)
                }
                Err(tina_runtime::wait_list::WaitCallError::NoCaller { call, .. }) => {
                    call.reject(CallRejectedReason::UnsupportedMessage)
                }
                Err(tina_runtime::wait_list::WaitCallError::CrossShardUnsupported {
                    call,
                    ..
                }) => call.reject(CallRejectedReason::UnsupportedMessage),
            },
            WaitSvcMsg::Resolve => call.reject(CallRejectedReason::UnsupportedMessage),
        }
    }
}

#[test]
fn waitlist_e2e_three_callers_get_same_reply_on_resolution() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let svc = runtime
        .register_service::<_, Infallible>(
            WaitSvc {
                waiters: WaitList::with_capacity(8).named("wait.e2e"),
                armed: false,
            },
            16,
        )
        .expect("register");

    let outcomes = Arc::new(Mutex::new(Vec::<u64>::new()));
    let gate = Arc::new(Barrier::new(4));
    let mut handles = Vec::new();
    for _ in 0..3 {
        let rt = Arc::clone(&runtime);
        let out = Arc::clone(&outcomes);
        let gate_h = Arc::clone(&gate);
        handles.push(thread::spawn(move || {
            gate_h.wait();
            let r = rt
                .call_blocking_typed(svc.call, WaitSvcMsg::Get, Duration::from_secs(2))
                .expect("admission");
            if let CallOutcome::Replied(v) = r {
                out.lock().expect("out").push(v);
            }
        }));
    }
    gate.wait();
    for h in handles {
        h.join().expect("worker");
    }
    let got = outcomes.lock().expect("out").clone();
    assert_eq!(got.len(), 3);
    assert!(got.iter().all(|v| *v == 42));
}

// ---------------------------------------------------------------------------
// WaitList: per-key cap full returns typed KeyFull reply.
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum WaitFullMsg {
    GetOne,
}

struct WaitFullSvc {
    waiters: WaitList<u32, u64>,
}

#[tina_runtime::isolate(message = WaitFullMsg, reply = u64)]
impl WaitFullSvc {
    fn handle(
        &mut self,
        _msg: WaitFullMsg,
        _ctx: &mut Context<'_, SingleShard, u64>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(&mut self, _msg: WaitFullMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match self.waiters.park_call(1u32, call) {
            Ok(_ticket) => noop(),
            Err(tina_runtime::wait_list::WaitCallError::KeyFull { call, .. }) => call.reply(11),
            Err(tina_runtime::wait_list::WaitCallError::Full { call, .. }) => call.reply(22),
            Err(tina_runtime::wait_list::WaitCallError::NoCaller { call, .. }) => {
                call.reject(CallRejectedReason::UnsupportedMessage)
            }
            Err(tina_runtime::wait_list::WaitCallError::CrossShardUnsupported {
                call, ..
            }) => call.reject(CallRejectedReason::UnsupportedMessage),
        }
    }
}

#[test]
fn waitlist_per_key_full_returns_typed_keyfull_reply() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let svc = runtime
        .register_service::<_, Infallible>(
            WaitFullSvc {
                waiters: WaitList::with_key_limit(8, 2).named("wait.keyfull"),
            },
            16,
        )
        .expect("register");

    // Two callers park (never replied). Third must get KeyFull (11).
    let outcomes = Arc::new(Mutex::new(Vec::<u64>::new()));
    let gate = Arc::new(Barrier::new(4));
    let mut handles = Vec::new();
    for _ in 0..3 {
        let rt = Arc::clone(&runtime);
        let out = Arc::clone(&outcomes);
        let gate_h = Arc::clone(&gate);
        handles.push(thread::spawn(move || {
            gate_h.wait();
            // Short timeout; the two parked callers will time out, the
            // rejected caller returns quickly with 11.
            let r = rt.call_blocking_typed(svc.call, WaitFullMsg::GetOne, Duration::from_millis(300));
            if let Ok(CallOutcome::Replied(v)) = r {
                out.lock().expect("out").push(v);
            }
        }));
    }
    gate.wait();
    for h in handles {
        h.join().expect("worker");
    }
    let got = outcomes.lock().expect("out").clone();
    assert!(
        got.contains(&11),
        "expected at least one KeyFull reply (11), got {got:?}"
    );
}

// CancelableWork e2e (real worker round-trip + cancel continuation) is
// covered by the unit tests in `tina-runtime/src/call.rs` and the
// `system_job_queue` migration. Here we only re-assert the cross-helper
// capacity-report and zero-capacity invariants.

// ---------------------------------------------------------------------------
// Stale-key ABA: a parked caller times out (caller-gone), the helper's
// next admission sweeps the slot and admits a new caller into the same
// slot index. The new caller's reply must reach the new caller, not the
// long-departed previous one. We can't observe the dead caller, so the
// invariant we pin is: after timeout + readmit, the second caller still
// gets the intended reply on the next flush.
// ---------------------------------------------------------------------------

#[test]
fn park_call_timed_out_caller_does_not_block_a_fresh_admission() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let svc = runtime
        .register_service::<_, Infallible>(
            ParkSvc {
                pending: PendingReplies::with_capacity(1).named("park.stale"),
                next_qid: 1,
                armed: false,
            },
            16,
        )
        .expect("register");

    // First caller: short timeout, never gets replied because the flush
    // fires after the timeout window.
    let rt = Arc::clone(&runtime);
    let svc_call = svc.call;
    let first = thread::spawn(move || {
        // ParkSvc's flush fires 10ms after admission. We deliberately
        // park here and let the runtime hold the slot; the goal is that
        // even a long-held slot does not strand a *future* caller.
        let _ =
            rt.call_blocking_typed(svc_call, ParkSvcMsg::Hold, Duration::from_millis(5));
    });
    first.join().expect("first");

    // Give the runtime time to observe the caller-gone signal.
    thread::sleep(Duration::from_millis(50));

    // Second caller: should be admitted (sweep on next park reclaims the
    // first slot) and replied by the next flush.
    let r = runtime
        .call_blocking_typed(svc.call, ParkSvcMsg::Hold, Duration::from_secs(2))
        .expect("admission");
    match r {
        CallOutcome::Replied(7) => {}
        other => panic!("expected reply 7 from second admission, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Capacity reports — every new helper exposes a named line.
// ---------------------------------------------------------------------------

#[test]
fn capacity_reports_have_named_helper_lines() {
    let p = PendingReplies::<u32, u32>::with_capacity(4).named("svc.parked");
    assert_eq!(p.capacity_report().name, "svc.parked");

    let g = GuardedPendingReplies::<u32, u32, DropCounter>::with_capacity(4).named("svc.guarded");
    assert_eq!(g.capacity_report().name, "svc.guarded");

    let w = WaitList::<u32, u32>::with_capacity(4).named("svc.waiters");
    assert_eq!(w.capacity_report().name, "svc.waiters");

    let cw =
        CancelableWork::<u32, &'static str, u32>::with_capacity(4).named("svc.cancelable.work");
    assert_eq!(cw.capacity_report().name, "svc.cancelable.work");
}

// ---------------------------------------------------------------------------
// Zero-capacity panics — every new helper rejects capacity == 0.
// ---------------------------------------------------------------------------

#[test]
#[should_panic(expected = "GuardedPendingReplies capacity must be positive")]
fn guarded_pending_replies_zero_capacity_panics() {
    let _: GuardedPendingReplies<u32, u32, DropCounter> = GuardedPendingReplies::with_capacity(0);
}

#[test]
#[should_panic(expected = "WaitList capacity must be positive")]
fn waitlist_zero_capacity_panics() {
    let _: WaitList<u32, u32> = WaitList::with_capacity(0);
}

#[test]
#[should_panic(expected = "WaitList per-key limit must be positive")]
fn waitlist_zero_per_key_limit_panics() {
    let _: WaitList<u32, u32> = WaitList::with_key_limit(8, 0);
}

#[test]
#[should_panic(expected = "CancelableWork capacity must be positive")]
fn cancelable_work_zero_capacity_panics() {
    let _: CancelableWork<u32, &'static str, u32> = CancelableWork::with_capacity(0);
}

#[test]
#[should_panic(expected = "CancelableWork per-key limit must be positive")]
fn cancelable_work_zero_per_key_limit_panics() {
    let _: CancelableWork<u32, &'static str, u32> = CancelableWork::with_key_limit(8, 0);
}
