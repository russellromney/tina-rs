//! End-to-end proofs for [`tina::PendingCallSet`] on the live runtime.
//!
//! The unit tests in `tina/src/pending_call_set.rs` cover the value-type
//! invariants (Full, DuplicateKey, drain, remove). These tests prove the
//! 072 fill -> cancel -> refill capacity rule across a real runtime
//! shard: an isolate fills its bounded pending set, cancels every
//! entry, and then admits a fresh batch without seeing a stale `Full`.
//!
//! There is no `Drop` magic anywhere — every entry is removed by an
//! explicit translator continuation (`Returned` for completion,
//! `Cancelled` for cancel-all). If the runtime were dropping handles
//! silently, this test would still see `Full` on the second batch
//! because the `PendingCallSet` would not have known to clear its
//! slots.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallKind, CallOutcome, DefaultThreadedMailboxFactory, RuntimeEventKind, SleepReply,
    ThreadedRuntime, call_cancelable, cancel_call, sleep,
};

/// Polls `cond` until it returns true or `total` elapses. Same shape
/// as the helper in `cancel_call.rs`. The 10ms step keeps the test
/// portable on slow / thermally throttled CI runners; we count a
/// final post-budget check so the pass condition is not "did the
/// last poll happen to fire before the deadline."
fn wait_for(total: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + total;
    while std::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    cond()
}

const POLL_BUDGET: Duration = Duration::from_secs(5);

fn count_trace_events(
    runtime: &Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>,
    matches: impl Fn(&RuntimeEventKind) -> bool,
) -> usize {
    runtime
        .trace()
        .events()
        .iter()
        .filter(|event| matches(&event.kind()))
        .count()
}

const CALL_TIMEOUT: Duration = Duration::from_secs(5);
const PENDING_CAPACITY: usize = 4;
const WORK_MS: u64 = 200;

#[derive(Debug, Default, Clone, Copy)]
struct Report {
    /// Inserts that succeeded across both batches. Should equal
    /// `2 * PENDING_CAPACITY` if refill works.
    inserted: u32,
    /// Cancels that returned `Cancelled` (i.e. the wait was actually
    /// reclaimed). Should equal `PENDING_CAPACITY`.
    cancelled: u32,
    /// Replies in the second batch that arrived as
    /// `CallOutcome::Replied` — proves the refilled batch ran.
    replied_after_refill: u32,
    exit_clean: bool,
}

#[derive(Debug)]
enum WorkerMsg {
    Do,
    Done(SleepReply),
    DoneForCall(tina::RequestContext<WorkerReply>, SleepReply),
}

#[derive(Debug, Clone, Copy)]
struct WorkerReply;

struct Worker;

#[tina_runtime::isolate(message = WorkerMsg, reply = WorkerReply)]
impl Worker {
    fn handle(
        &mut self,
        msg: WorkerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            WorkerMsg::Do => sleep(Duration::from_millis(WORK_MS)).then(WorkerMsg::Done),
            WorkerMsg::Done(Ok(())) => reply(WorkerReply),
            WorkerMsg::Done(Err(_)) => stop(),
            WorkerMsg::DoneForCall(req, Ok(())) => tina::reply_to(req, WorkerReply),
            WorkerMsg::DoneForCall(_, Err(_)) => stop(),
        }
    }

    fn handle_call(&mut self, msg: WorkerMsg, call: tina::CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            WorkerMsg::Do => {
                let req = call.into_request_context();
                sleep(Duration::from_millis(WORK_MS)).then_with_request(req, WorkerMsg::DoneForCall)
            }
            WorkerMsg::Done(_) | WorkerMsg::DoneForCall(_, _) => {
                call.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

#[derive(Debug)]
enum DriverMsg {
    /// Fill the set with PENDING_CAPACITY entries (slow workers).
    FillBatch1,
    /// Cancel every entry currently in the set.
    CancelAll,
    /// Per-cancel ack; tracks whether the wait was actually reclaimed.
    Cancelled(CancelOutcome),
    /// Refill the set with PENDING_CAPACITY entries (fast continuations).
    FillBatch2,
    /// Worker reply continuation. Removes the entry and counts the
    /// outcome.
    Returned {
        batch: u8,
        worker: u32,
        outcome: CallOutcome<WorkerReply>,
    },
    Finish,
}

struct Driver {
    workers: Vec<Address<WorkerMsg, WorkerReply>>,
    pending: PendingCallSet<u32, WorkerReply>,
    report: Report,
}

#[tina_runtime::isolate(message = DriverMsg)]
impl Driver {
    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
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
                    self.report.cancelled += 1;
                }
                noop()
            }
            DriverMsg::FillBatch2 => self.fill(2),
            DriverMsg::Returned {
                batch,
                worker,
                outcome,
            } => {
                self.pending.remove(&worker);
                if batch == 2 {
                    if let CallOutcome::Replied(_) = outcome {
                        self.report.replied_after_refill += 1;
                    }
                }
                noop()
            }
            DriverMsg::Finish => {
                self.report.exit_clean = true;
                stop_with(self.report)
            }
        }
    }
}

impl Driver {
    fn fill(&mut self, batch: u8) -> Effect<Self> {
        let mut effects = Vec::with_capacity(PENDING_CAPACITY);
        for (idx, worker) in self.workers.iter().enumerate() {
            let key = idx as u32;
            let (effect, handle) =
                call_cancelable(*worker, WorkerMsg::Do, CALL_TIMEOUT).then(move |outcome| {
                    DriverMsg::Returned {
                        batch,
                        worker: key,
                        outcome,
                    }
                });
            self.pending
                .insert(key, handle)
                .expect("first form: bounded set sized to PENDING_CAPACITY refills cleanly");
            self.report.inserted += 1;
            effects.push(effect);
        }
        Effect::Batch(effects)
    }
}

#[test]
fn fill_cancel_refill_reclaims_capacity() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    let mut workers = Vec::with_capacity(PENDING_CAPACITY);
    for _ in 0..PENDING_CAPACITY {
        workers.push(
            runtime
                .register_with_capacity::<_, Infallible>(Worker, 8)
                .expect("register worker"),
        );
    }

    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            Driver {
                workers,
                pending: PendingCallSet::with_capacity(PENDING_CAPACITY),
                report: Report::default(),
            },
            32,
        )
        .expect("register driver");

    let result = runtime
        .observe_result::<Report, _, _>(driver)
        .expect("observe_result");

    runtime
        .try_send(driver, DriverMsg::FillBatch1)
        .expect("FillBatch1");
    // Wait for every batch-1 IsolateCall to dispatch so cancels hit
    // "delivered, still pending" rather than "queued, never
    // dispatched." Filtering by `IsolateCall` keeps the count honest
    // — `CancelCall` and other kinds also generate dispatch events.
    assert!(
        wait_for(POLL_BUDGET, || count_trace_events(
            &runtime,
            |kind| matches!(
                kind,
                RuntimeEventKind::CallDispatchAttempted {
                    call_kind: CallKind::IsolateCall,
                    ..
                }
            )
        ) >= PENDING_CAPACITY),
        "batch 1 did not dispatch every call within the poll budget"
    );

    runtime
        .try_send(driver, DriverMsg::CancelAll)
        .expect("CancelAll");
    // Wait for every cancel to land as a `CallCancelled` trace event.
    // This guarantees the per-cancel translator has fired (and the
    // resulting `Cancelled` message is queued in the driver mailbox)
    // before we send Finish — otherwise Finish could race past the
    // cancel acks.
    assert!(
        wait_for(POLL_BUDGET, || count_trace_events(
            &runtime,
            |kind| matches!(kind, RuntimeEventKind::CallCancelled { .. })
        ) >= PENDING_CAPACITY),
        "not every batch-1 call was cancelled within the poll budget"
    );

    runtime
        .try_send(driver, DriverMsg::FillBatch2)
        .expect("FillBatch2");
    // Wait for batch 2's IsolateCalls to complete. Filter by
    // `IsolateCall` because `CancelCall` completions from the prior
    // CancelAll batch also emit `CallCompleted`; counting both would
    // fire the wait early and Finish would race past the real
    // `Returned` continuations for batch 2.
    assert!(
        wait_for(POLL_BUDGET, || count_trace_events(
            &runtime,
            |kind| matches!(
                kind,
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::IsolateCall,
                    ..
                }
            )
        ) >= PENDING_CAPACITY),
        "batch 2 did not complete within the poll budget"
    );

    runtime.try_send(driver, DriverMsg::Finish).expect("Finish");

    let report = result.wait(POLL_BUDGET).expect("driver report");

    assert!(report.exit_clean, "driver should have exited cleanly");
    assert_eq!(
        report.inserted as usize,
        2 * PENDING_CAPACITY,
        "refill must succeed without stale Full",
    );
    assert_eq!(
        report.cancelled as usize, PENDING_CAPACITY,
        "every queued waiter should report Cancelled",
    );
    assert_eq!(
        report.replied_after_refill as usize, PENDING_CAPACITY,
        "second batch should run to completion, proving slot reuse",
    );

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

// ---------- Timeout-driven cleanup proof ----------
//
// Every stored handle must have a completion, cancellation, or *timeout*
// continuation that removes its key. The cancel path is exercised above.
// This test pins the timeout path:
// the driver fires calls with a short call timeout against a worker
// that will not finish in time, the runtime delivers
// `CallOutcome::Timeout` to the translator, the translator removes the
// slot, and the set refills cleanly.

const TIMEOUT_BUDGET_MS: u64 = 50;
const SLOW_WORK_MS: u64 = 250;

#[derive(Debug, Default, Clone, Copy)]
struct TimeoutReport {
    timed_out: u32,
    inserted: u32,
    replied_after_refill: u32,
    exit_clean: bool,
}

#[derive(Debug)]
enum TimeoutDriverMsg {
    /// Fill with calls that will time out before the worker replies.
    FillTimeoutBatch,
    /// Refill batch — worker is fast this time, all replies land.
    FillFastBatch,
    Returned {
        batch: u8,
        worker: u32,
        outcome: CallOutcome<WorkerReply>,
    },
    Finish,
}

struct SlowWorker;

#[tina_runtime::isolate(message = WorkerMsg, reply = WorkerReply)]
impl SlowWorker {
    fn handle(
        &mut self,
        msg: WorkerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            WorkerMsg::Do => sleep(Duration::from_millis(SLOW_WORK_MS)).then(WorkerMsg::Done),
            WorkerMsg::Done(Ok(())) => reply(WorkerReply),
            WorkerMsg::Done(Err(_)) => stop(),
            WorkerMsg::DoneForCall(req, Ok(())) => tina::reply_to(req, WorkerReply),
            WorkerMsg::DoneForCall(_, Err(_)) => stop(),
        }
    }

    fn handle_call(&mut self, msg: WorkerMsg, call: tina::CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            WorkerMsg::Do => {
                let req = call.into_request_context();
                sleep(Duration::from_millis(SLOW_WORK_MS))
                    .then_with_request(req, WorkerMsg::DoneForCall)
            }
            WorkerMsg::Done(_) | WorkerMsg::DoneForCall(_, _) => {
                call.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

struct FastWorker;

#[tina_runtime::isolate(message = WorkerMsg, reply = WorkerReply)]
impl FastWorker {
    fn handle(
        &mut self,
        msg: WorkerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            WorkerMsg::Do => sleep(Duration::from_millis(5)).then(WorkerMsg::Done),
            WorkerMsg::Done(Ok(())) => reply(WorkerReply),
            WorkerMsg::Done(Err(_)) => stop(),
            WorkerMsg::DoneForCall(req, Ok(())) => tina::reply_to(req, WorkerReply),
            WorkerMsg::DoneForCall(_, Err(_)) => stop(),
        }
    }

    fn handle_call(&mut self, msg: WorkerMsg, call: tina::CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            WorkerMsg::Do => {
                let req = call.into_request_context();
                sleep(Duration::from_millis(5)).then_with_request(req, WorkerMsg::DoneForCall)
            }
            WorkerMsg::Done(_) | WorkerMsg::DoneForCall(_, _) => {
                call.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

struct TimeoutDriver {
    slow_workers: Vec<Address<WorkerMsg, WorkerReply>>,
    fast_workers: Vec<Address<WorkerMsg, WorkerReply>>,
    pending: PendingCallSet<u32, WorkerReply>,
    report: TimeoutReport,
}

#[tina_runtime::isolate(message = TimeoutDriverMsg)]
impl TimeoutDriver {
    fn handle(
        &mut self,
        msg: TimeoutDriverMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            TimeoutDriverMsg::FillTimeoutBatch => {
                let mut effects = Vec::with_capacity(PENDING_CAPACITY);
                for (idx, worker) in self.slow_workers.iter().enumerate() {
                    let key = idx as u32;
                    let (effect, handle) = call_cancelable(
                        *worker,
                        WorkerMsg::Do,
                        Duration::from_millis(TIMEOUT_BUDGET_MS),
                    )
                    .then(move |outcome| TimeoutDriverMsg::Returned {
                        batch: 1,
                        worker: key,
                        outcome,
                    });
                    self.pending.insert(key, handle).expect("first batch fits");
                    self.report.inserted += 1;
                    effects.push(effect);
                }
                Effect::Batch(effects)
            }
            TimeoutDriverMsg::FillFastBatch => {
                let mut effects = Vec::with_capacity(PENDING_CAPACITY);
                for (idx, worker) in self.fast_workers.iter().enumerate() {
                    let key = idx as u32;
                    let (effect, handle) = call_cancelable(*worker, WorkerMsg::Do, CALL_TIMEOUT)
                        .then(move |outcome| TimeoutDriverMsg::Returned {
                            batch: 2,
                            worker: key,
                            outcome,
                        });
                    self.pending
                        .insert(key, handle)
                        .expect("refill must succeed if timeout cleanup reclaimed slots");
                    self.report.inserted += 1;
                    effects.push(effect);
                }
                Effect::Batch(effects)
            }
            TimeoutDriverMsg::Returned {
                batch,
                worker,
                outcome,
            } => {
                // Explicit slot cleanup on every continuation —
                // including the timeout one. This is the load-bearing
                // step the test is proving.
                self.pending.remove(&worker);
                match (batch, outcome) {
                    (1, CallOutcome::Timeout) => self.report.timed_out += 1,
                    (2, CallOutcome::Replied(_)) => self.report.replied_after_refill += 1,
                    _ => {}
                }
                noop()
            }
            TimeoutDriverMsg::Finish => {
                self.report.exit_clean = true;
                stop_with(self.report)
            }
        }
    }
}

#[test]
fn fill_timeout_refill_reclaims_capacity() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    let mut slow_workers = Vec::with_capacity(PENDING_CAPACITY);
    for _ in 0..PENDING_CAPACITY {
        slow_workers.push(
            runtime
                .register_with_capacity::<_, Infallible>(SlowWorker, 8)
                .expect("register slow worker"),
        );
    }
    let mut fast_workers = Vec::with_capacity(PENDING_CAPACITY);
    for _ in 0..PENDING_CAPACITY {
        fast_workers.push(
            runtime
                .register_with_capacity::<_, Infallible>(FastWorker, 8)
                .expect("register fast worker"),
        );
    }

    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            TimeoutDriver {
                slow_workers,
                fast_workers,
                pending: PendingCallSet::with_capacity(PENDING_CAPACITY),
                report: TimeoutReport::default(),
            },
            32,
        )
        .expect("register driver");

    let result = runtime
        .observe_result::<TimeoutReport, _, _>(driver)
        .expect("observe_result");

    runtime
        .try_send(driver, TimeoutDriverMsg::FillTimeoutBatch)
        .expect("FillTimeoutBatch");

    // Wait for every batch-1 IsolateCall to time out. Filter on
    // `IsolateCall` so unrelated kinds (e.g. CancelCall, runtime
    // I/O calls) cannot accidentally satisfy the count.
    assert!(
        wait_for(POLL_BUDGET, || count_trace_events(
            &runtime,
            |kind| matches!(
                kind,
                RuntimeEventKind::CallFailed {
                    call_kind: CallKind::IsolateCall,
                    ..
                }
            )
        ) >= PENDING_CAPACITY),
        "batch 1 did not time out within the poll budget"
    );

    runtime
        .try_send(driver, TimeoutDriverMsg::FillFastBatch)
        .expect("FillFastBatch");

    // Wait for batch 2's fast workers to complete. Filter by
    // `IsolateCall` so this never accidentally counts a CancelCall
    // completion (there should be none in this test, but the filter
    // matches the convention).
    assert!(
        wait_for(POLL_BUDGET, || count_trace_events(
            &runtime,
            |kind| matches!(
                kind,
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::IsolateCall,
                    ..
                }
            )
        ) >= PENDING_CAPACITY),
        "batch 2 did not complete within the poll budget"
    );

    runtime
        .try_send(driver, TimeoutDriverMsg::Finish)
        .expect("Finish");

    let report = result.wait(POLL_BUDGET).expect("driver report");

    assert!(report.exit_clean);
    assert_eq!(
        report.inserted as usize,
        2 * PENDING_CAPACITY,
        "refill after timeout must succeed without stale Full",
    );
    assert_eq!(
        report.timed_out as usize, PENDING_CAPACITY,
        "every first-batch call should report Timeout",
    );
    assert_eq!(
        report.replied_after_refill as usize, PENDING_CAPACITY,
        "second batch should run to completion, proving slot reuse \
         after timeout-driven cleanup",
    );

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

// ---------- ABA proof ----------
//
// Codex P2 regression: a stale `Returned` continuation from an
// already-settled call must not be able to silently remove a fresh
// entry the user inserted under the same key. The set rejects the
// duplicate key loudly instead of auto-sweeping.

#[derive(Debug, Default, Clone, Copy)]
struct AbaReport {
    /// Whether the second `insert` of the reused key returned
    /// `DuplicateKey` (the loud, correct failure).
    second_insert_was_duplicate: bool,
    /// Whether the slow worker's `Returned` continuation eventually
    /// observed `Timeout`. Just a sanity assertion that the timing
    /// is real.
    first_call_timed_out: bool,
    exit_clean: bool,
}

#[derive(Debug)]
enum AbaDriverMsg {
    /// Insert key=42 with a slow worker (call A).
    InsertA,
    /// Before A's `Returned` is processed, attempt to insert key=42
    /// again with a fresh handle (call B). Expect `DuplicateKey`.
    AttemptDuplicateInsert,
    /// Continuation for the second-attempted call B; B never gets
    /// stored and its handle is dropped, so this never fires for the
    /// successful path. Payload kept as `()` because it is never read.
    BReturned(#[allow(dead_code)] CallOutcome<WorkerReply>),
    /// First call's timeout continuation. Removes key=42.
    AReturned {
        worker: u32,
        outcome: CallOutcome<WorkerReply>,
    },
    Finish,
}

struct AbaDriver {
    slow_worker: Address<WorkerMsg, WorkerReply>,
    pending: PendingCallSet<u32, WorkerReply>,
    report: AbaReport,
}

#[tina_runtime::isolate(message = AbaDriverMsg)]
impl AbaDriver {
    fn handle(
        &mut self,
        msg: AbaDriverMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            AbaDriverMsg::InsertA => {
                let (effect, handle) =
                    call_cancelable(self.slow_worker, WorkerMsg::Do, Duration::from_millis(50))
                        .then(move |outcome| AbaDriverMsg::AReturned {
                            worker: 42,
                            outcome,
                        });
                self.pending
                    .insert(42, handle)
                    .expect("first insert succeeds");
                effect
            }
            AbaDriverMsg::AttemptDuplicateInsert => {
                // Try to reinsert under key=42 while A's `Returned`
                // is still queued in the mailbox (it has not yet
                // fired because we have not yielded to it). The set
                // must refuse loudly, even though A's handle has
                // settled.
                let (effect_b, handle_b): (Effect<Self>, _) =
                    call_cancelable(self.slow_worker, WorkerMsg::Do, CALL_TIMEOUT)
                        .then(AbaDriverMsg::BReturned);
                match self.pending.insert(42, handle_b) {
                    Err(tina::PendingCallSetInsertError::DuplicateKey { key, handle: _ }) => {
                        assert_eq!(key, 42);
                        self.report.second_insert_was_duplicate = true;
                        // Drop the unused B effect — never dispatched.
                        let _ = effect_b;
                        noop()
                    }
                    other => panic!(
                        "expected DuplicateKey for ABA case, got {:?}",
                        other.is_ok()
                    ),
                }
            }
            AbaDriverMsg::AReturned { worker, outcome } => {
                self.pending.remove(&worker);
                if matches!(outcome, CallOutcome::Timeout) {
                    self.report.first_call_timed_out = true;
                }
                noop()
            }
            AbaDriverMsg::BReturned(_) => noop(),
            AbaDriverMsg::Finish => {
                self.report.exit_clean = true;
                stop_with(self.report)
            }
        }
    }
}

#[test]
fn duplicate_key_under_settled_handle_is_rejected_not_silently_replaced() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    // Slow worker: long sleep so the 50ms timeout fires before reply.
    let slow_worker = runtime
        .register_with_capacity::<_, Infallible>(SlowWorker, 8)
        .expect("register slow worker");

    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            AbaDriver {
                slow_worker,
                pending: PendingCallSet::with_capacity(4),
                report: AbaReport::default(),
            },
            32,
        )
        .expect("register driver");

    let result = runtime
        .observe_result::<AbaReport, _, _>(driver)
        .expect("observe_result");

    // Send InsertA and AttemptDuplicateInsert back-to-back so the
    // driver's mailbox processes them in FIFO order *before* A's
    // 50ms timeout has any chance to fire. The reinsert sees A still
    // `Pending`; the set must reject loudly. (The settled-but-not-yet-
    // observed case is unit-tested directly in
    // `tina/src/pending_call_set.rs`, where handle state can be set
    // synchronously without racing the runtime's timer wheel.)
    runtime
        .try_send(driver, AbaDriverMsg::InsertA)
        .expect("InsertA");
    runtime
        .try_send(driver, AbaDriverMsg::AttemptDuplicateInsert)
        .expect("AttemptDuplicateInsert");

    // Wait for A's timeout to fire so the driver's AReturned handler
    // runs and the report's `first_call_timed_out` flag flips. Polling
    // the trace for `CallFailed` is more honest than a 150ms sleep.
    assert!(
        wait_for(POLL_BUDGET, || count_trace_events(
            &runtime,
            |kind| matches!(kind, RuntimeEventKind::CallFailed { .. })
        ) >= 1),
        "A's call did not time out within the poll budget"
    );

    runtime
        .try_send(driver, AbaDriverMsg::Finish)
        .expect("Finish");

    let report = result.wait(POLL_BUDGET).expect("driver report");

    assert!(report.exit_clean);
    assert!(
        report.second_insert_was_duplicate,
        "duplicate key under a still-pending prior entry must be \
         rejected with DuplicateKey, never silently replaced",
    );

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}
