//! Deterministic simulator parity for `tina::PendingCallSet`.
//!
//! The runtime-side tests in `tina-runtime/tests/pending_call_set.rs`
//! exercise the same invariants over a real shard, but they have to
//! poll a wall-clock trace because the runtime is multi-threaded and
//! timer-driven. That introduces "fast box passes, slow box flakes"
//! risk: the CI-found bug where a naive `CallCompleted` count picked
//! up `CancelCall` completions only surfaced because the wall-clock
//! pacing of the runtime test happened to lose the race a different
//! way.
//!
//! These sim tests are the prevention. The simulator is single-
//! threaded and message-FIFO; `drain(&mut sim)` runs until the
//! mailbox is empty and every timer has fired. There is no race, no
//! "did the host's `try_send(Finish)` win" question. If a future
//! change reintroduces the same bug, this file fails reproducibly
//! with a concrete trace mismatch.
//!
//! Coverage:
//! - fill -> cancel -> refill reclaims capacity;
//! - fill -> timeout -> refill reclaims capacity (timeout cleanup
//!   path, distinct from cancel);
//! - duplicate key under a still-pending prior entry is rejected
//!   (the loud-error contract that defends against the silent ABA
//!   bug auto-sweep would have introduced).

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallKind, CallOutcome, RuntimeEventKind, SleepReply, call_cancelable, cancel_call, sleep,
};
use tina_sim::{Simulator, SimulatorConfig};

#[derive(Debug)]
struct SimShard;

impl Shard for SimShard {
    fn id(&self) -> ShardId {
        ShardId::new(0)
    }
}

/// Drains all currently-deliverable messages **without** advancing
/// virtual time. Stops as soon as no more messages can be delivered
/// without a timer firing — same shape as the helper in
/// `cancel_call.rs`. Tests advance time explicitly with
/// `sim.advance_time(...)` when they want sleep/timeout machinery
/// to fire; otherwise `drain()` keeps the test in lockstep with
/// the test author's intent.
///
/// Using `run_until_quiescent` here would silently auto-advance
/// past every pending sleep, completing in-flight calls before
/// `CancelAll` (or `AttemptDuplicateInsert`) had a chance to
/// observe them as still-pending — exactly the kind of "fast box
/// passes, slow box flakes" hazard the sim is supposed to avoid.
fn drain(sim: &mut Simulator<SimShard>) {
    while sim.step() > 0 {}
}

const CAPACITY: usize = 4;
const WORK_BUDGET_MS: u64 = 200;

#[derive(Debug)]
enum WorkerMsg {
    Do,
    Done(SleepReply),
    DoneForCall(tina::RequestContext<WorkerReply>, SleepReply),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkerReply;

struct Worker {
    sleep_ms: u64,
}

#[tina_runtime::isolate(message = WorkerMsg, reply = WorkerReply, shard = SimShard)]
impl Worker {
    fn handle_call(&mut self, msg: WorkerMsg, call: tina::CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            WorkerMsg::Do => {
                let req = call.into_request_context();
                sleep(Duration::from_millis(self.sleep_ms))
                    .then_with_request(req, WorkerMsg::DoneForCall)
            }
            WorkerMsg::Done(_) | WorkerMsg::DoneForCall(_, _) => {
                call.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }

    fn handle(
        &mut self,
        msg: WorkerMsg,
        _ctx: &mut Context<'_, SimShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            WorkerMsg::Do => sleep(Duration::from_millis(self.sleep_ms)).then(WorkerMsg::Done),
            WorkerMsg::Done(Ok(())) => reply(WorkerReply),
            WorkerMsg::Done(Err(_)) => stop(),
            WorkerMsg::DoneForCall(req, Ok(())) => tina::reply_to(req, WorkerReply),
            WorkerMsg::DoneForCall(_, Err(_)) => stop(),
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct Observations {
    cancelled: u32,
    timed_out: u32,
    replied_after_refill: u32,
    duplicate_key_observed: bool,
}

#[derive(Debug)]
enum DriverMsg {
    FillBatch1,
    CancelAll,
    Cancelled(CancelOutcome),
    FillBatch2,
    /// Try to reinsert a key that is already present (and still
    /// pending). The set must reject loudly with `DuplicateKey`.
    AttemptDuplicateInsert,
    Returned {
        batch: u8,
        worker: u32,
        outcome: CallOutcome<WorkerReply>,
    },
    /// Translator for the second-attempted call B that the duplicate-
    /// insert path produces. B is never inserted, so this never fires
    /// on the success path.
    BReturned(#[allow(dead_code)] CallOutcome<WorkerReply>),
}

struct Driver {
    workers: Vec<Address<WorkerMsg, WorkerReply>>,
    pending: PendingCallSet<u32, WorkerReply>,
    /// Per-call timeout for the call-with-handle dispatches.
    call_timeout: Duration,
    observations: Arc<Mutex<Observations>>,
}

#[tina_runtime::isolate(message = DriverMsg, shard = SimShard)]
impl Driver {
    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, SimShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::FillBatch1 => self.fill(1),
            DriverMsg::CancelAll => {
                let mut effects = Vec::with_capacity(self.pending.len());
                for (_, handle) in self.pending.drain() {
                    effects.push(cancel_call(handle).then(DriverMsg::Cancelled));
                }
                Effect::Batch(effects)
            }
            DriverMsg::Cancelled(outcome) => {
                if outcome.is_cancelled() {
                    self.observations.lock().expect("obs").cancelled += 1;
                }
                noop()
            }
            DriverMsg::FillBatch2 => self.fill(2),
            DriverMsg::AttemptDuplicateInsert => {
                let target = self.workers[0];
                let (effect_b, handle_b): (Effect<Self>, _) =
                    call_cancelable(target, WorkerMsg::Do, self.call_timeout)
                        .then(DriverMsg::BReturned);
                match self.pending.insert(0, handle_b) {
                    Err(tina::PendingCallSetInsertError::DuplicateKey { key, handle: _ }) => {
                        assert_eq!(key, 0);
                        self.observations
                            .lock()
                            .expect("obs")
                            .duplicate_key_observed = true;
                        // Drop the unused effect — never dispatched.
                        let _ = effect_b;
                        noop()
                    }
                    other => panic!(
                        "expected DuplicateKey for ABA case, got {:?}",
                        other.is_ok()
                    ),
                }
            }
            DriverMsg::Returned {
                batch,
                worker,
                outcome,
            } => {
                self.pending.remove(&worker);
                let mut obs = self.observations.lock().expect("obs");
                match (batch, &outcome) {
                    (1, CallOutcome::Timeout) => obs.timed_out += 1,
                    (2, CallOutcome::Replied(_)) => obs.replied_after_refill += 1,
                    _ => {}
                }
                noop()
            }
            DriverMsg::BReturned(_) => noop(),
        }
    }
}

impl Driver {
    fn fill(&mut self, batch: u8) -> Effect<Self> {
        let mut effects = Vec::with_capacity(CAPACITY);
        for (idx, worker) in self.workers.iter().enumerate() {
            let key = idx as u32;
            let (effect, handle) =
                call_cancelable(*worker, WorkerMsg::Do, self.call_timeout).then(move |outcome| {
                    DriverMsg::Returned {
                        batch,
                        worker: key,
                        outcome,
                    }
                });
            self.pending
                .insert(key, handle)
                .expect("set sized to CAPACITY refills cleanly between batches");
            effects.push(effect);
        }
        Effect::Batch(effects)
    }
}

#[allow(clippy::type_complexity)]
fn build(
    worker_sleep_ms: u64,
    call_timeout: Duration,
) -> (
    Simulator<SimShard>,
    Address<DriverMsg>,
    Arc<Mutex<Observations>>,
) {
    let mut sim = Simulator::new(SimShard, SimulatorConfig::default());
    let mut workers = Vec::with_capacity(CAPACITY);
    for _ in 0..CAPACITY {
        let addr = sim.register::<_, _, Infallible>(Worker {
            sleep_ms: worker_sleep_ms,
        });
        workers.push(addr);
    }
    let observations = Arc::new(Mutex::new(Observations::default()));
    let driver = sim.register::<_, _, Infallible>(Driver {
        workers,
        pending: PendingCallSet::with_capacity(CAPACITY),
        call_timeout,
        observations: Arc::clone(&observations),
    });
    (sim, driver, observations)
}

/// Sim-side proof that fill -> cancel -> refill reclaims capacity.
/// Mirrors `tina-runtime/tests/pending_call_set.rs::
/// fill_cancel_refill_reclaims_capacity` deterministically.
#[test]
fn fill_cancel_refill_reclaims_capacity() {
    let (mut sim, driver, observations) = build(WORK_BUDGET_MS, Duration::from_secs(5));

    sim.try_send(driver, DriverMsg::FillBatch1).expect("fill 1");
    // Drain without advancing time: every batch-1 call dispatches
    // and parks on its sleep, but no sleep timer fires yet.
    drain(&mut sim);

    // Cancel happens *before* any sleep timer fires — so the
    // runtime sees pending entries and reports Cancelled.
    sim.try_send(driver, DriverMsg::CancelAll).expect("cancel");
    drain(&mut sim);

    sim.try_send(driver, DriverMsg::FillBatch2).expect("fill 2");
    drain(&mut sim);
    // Now advance past the worker sleep so batch 2's calls reply.
    sim.advance_time(Duration::from_millis(WORK_BUDGET_MS + 50));
    drain(&mut sim);

    let obs = *observations.lock().expect("obs");
    assert_eq!(
        obs.cancelled as usize, CAPACITY,
        "every batch-1 call should report Cancelled",
    );
    assert_eq!(
        obs.replied_after_refill as usize, CAPACITY,
        "every batch-2 call should run to completion, proving slot reuse",
    );

    // Trace-level cross-check: counting `IsolateCall` dispatches
    // should equal 2 * CAPACITY (one per call across both batches),
    // and CancelCall completions should not bleed into the IsolateCall
    // count. This is the regression guard for the bug that surfaced
    // in the runtime test: the naive trace count must distinguish
    // call kinds.
    let trace = sim.trace();
    let isolate_dispatches = trace
        .iter()
        .filter(|e| {
            matches!(
                e.kind(),
                RuntimeEventKind::CallDispatchAttempted {
                    call_kind: CallKind::IsolateCall,
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        isolate_dispatches,
        2 * CAPACITY,
        "every IsolateCall in both batches should appear in the trace",
    );
    let cancel_completions = trace
        .iter()
        .filter(|e| {
            matches!(
                e.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::CancelCall,
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        cancel_completions, CAPACITY,
        "every batch-1 cancel should produce exactly one CancelCall completion",
    );
    let isolate_completions = trace
        .iter()
        .filter(|e| {
            matches!(
                e.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::IsolateCall,
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        isolate_completions,
        CAPACITY,
        "only batch-2 IsolateCalls should report CallCompleted; \
         batch-1 was cancelled. A naive `CallCompleted` count without \
         the `IsolateCall` filter would equal {} here, which is the \
         specific regression this test pins.",
        cancel_completions + isolate_completions,
    );
}

/// Sim-side proof that fill -> timeout -> refill reclaims capacity.
///
/// The plan rule under test: every stored handle has a completion,
/// cancel, or *timeout* continuation that removes its key. This test
/// exercises the timeout path by giving every batch-1 call a tight
/// 50 ms call timeout against a 200 ms worker sleep, advancing virtual
/// time past the timeout, and confirming the next `insert` cycle
/// admits the same keys without `Full` / `DuplicateKey`.
#[test]
fn fill_timeout_refill_reclaims_capacity() {
    const TIMEOUT_BUDGET_MS: u64 = 50;
    let (mut sim, driver, observations) =
        build(WORK_BUDGET_MS, Duration::from_millis(TIMEOUT_BUDGET_MS));

    sim.try_send(driver, DriverMsg::FillBatch1)
        .expect("fill timeouts");
    // Drain first so calls dispatch at virtual_now=0 with their 50 ms
    // deadlines stamped. Advancing time before this would set
    // deadlines relative to the post-advance now, and the next drain
    // would fire nothing.
    drain(&mut sim);

    // Advance past the call timeout (but not past the worker sleep).
    // Each call times out, the runtime stamps the handle Settled,
    // delivers `Returned { Timeout }` to the driver, and the driver
    // calls `pending.remove(&key)`. Set should be empty after this.
    sim.advance_time(Duration::from_millis(TIMEOUT_BUDGET_MS + 5));
    drain(&mut sim);

    let obs = *observations.lock().expect("obs");
    assert_eq!(
        obs.timed_out as usize, CAPACITY,
        "every batch-1 IsolateCall should report Timeout",
    );

    // The plan's load-bearing claim: the next insert must succeed
    // under the same keys without stale `Full` / `DuplicateKey`.
    // Refill batch 2 (worker sleeps will time out again — that is
    // not what we are testing here; we are testing that the slots
    // were reclaimed). The fill itself succeeding without panic IS
    // the proof.
    sim.try_send(driver, DriverMsg::FillBatch2)
        .expect("fill batch 2");
    drain(&mut sim);

    // Trace-level guard: every batch-1 IsolateCall must surface as
    // `CallFailed` with `IsolateCall` kind. Filtering by kind is the
    // regression guard for the bug that surfaced in the runtime test
    // — without it, completions from other call kinds could
    // accidentally satisfy the count.
    let trace = sim.trace();
    let isolate_failures = trace
        .iter()
        .filter(|e| {
            matches!(
                e.kind(),
                RuntimeEventKind::CallFailed {
                    call_kind: CallKind::IsolateCall,
                    ..
                }
            )
        })
        .count();
    assert!(
        isolate_failures >= CAPACITY,
        "every batch-1 IsolateCall should fail with timeout (saw {isolate_failures}, expected at least {CAPACITY})",
    );
}

/// Sim-side proof for the codex P2 ABA defense: a duplicate-key
/// insert must be rejected even when the prior call is still pending.
/// (The settled-but-not-yet-observed case is unit-tested directly in
/// `tina/src/pending_call_set.rs`, where handle state can be set
/// synchronously.) Here the sim's deterministic ordering guarantees
/// that `AttemptDuplicateInsert` is processed while batch-1's call
/// for key=0 is still `Pending`; the set must reject the reinsert.
#[test]
fn duplicate_key_is_rejected_for_pending_prior_entry() {
    let (mut sim, driver, observations) = build(WORK_BUDGET_MS, Duration::from_secs(5));

    sim.try_send(driver, DriverMsg::FillBatch1).expect("fill");
    drain(&mut sim);

    // Send the duplicate attempt while batch-1's calls are still
    // pending (no time advancement, so neither sleeps nor timeouts
    // have fired). The set must reject loudly.
    sim.try_send(driver, DriverMsg::AttemptDuplicateInsert)
        .expect("attempt duplicate");
    drain(&mut sim);

    let obs = *observations.lock().expect("obs");
    assert!(
        obs.duplicate_key_observed,
        "duplicate-key insert under a still-pending prior entry must be rejected loudly",
    );
}
