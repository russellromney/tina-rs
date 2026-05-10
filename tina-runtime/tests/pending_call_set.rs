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
    CallOutcome, DefaultThreadedMailboxFactory, SleepReply, ThreadedRuntime, call_with_handle,
    cancel_call, sleep,
};

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
            WorkerMsg::Do => sleep(Duration::from_millis(WORK_MS)).reply(WorkerMsg::Done),
            WorkerMsg::Done(Ok(())) => reply(WorkerReply),
            WorkerMsg::Done(Err(_)) => stop(),
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
    /// Active batch tag; lets the test prove the *second* batch ran
    /// without confusing it with stragglers from the first.
    current_batch: u8,
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
                    effects.push(cancel_call(handle).reply(DriverMsg::Cancelled));
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
        self.current_batch = batch;
        let mut effects = Vec::with_capacity(PENDING_CAPACITY);
        for (idx, worker) in self.workers.iter().enumerate() {
            let key = idx as u32;
            let (effect, handle) =
                call_with_handle(*worker, WorkerMsg::Do, CALL_TIMEOUT).reply(move |outcome| {
                    DriverMsg::Returned {
                        batch,
                        worker: key,
                        outcome,
                    }
                });
            self.pending
                .insert(key, handle)
                .map_err(|_| ())
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
                current_batch: 0,
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
    // Let the workers start their slow sleep so cancel hits "delivered
    // but not yet replied" rather than "still queued."
    std::thread::sleep(Duration::from_millis(50));

    runtime
        .try_send(driver, DriverMsg::CancelAll)
        .expect("CancelAll");
    // Give cancels time to settle and reclaim capacity. CancelOutcome
    // arrives synchronously through the `.reply(...)` translator.
    std::thread::sleep(Duration::from_millis(20));

    runtime
        .try_send(driver, DriverMsg::FillBatch2)
        .expect("FillBatch2");

    // Wait long enough for the second batch's slow workers to finish.
    std::thread::sleep(Duration::from_millis(WORK_MS + 100));

    runtime.try_send(driver, DriverMsg::Finish).expect("Finish");

    let report = result.wait(Duration::from_secs(5)).expect("driver report");

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
