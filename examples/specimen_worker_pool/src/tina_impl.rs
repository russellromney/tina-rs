use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, PendingReplies, PendingRepliesTryCaptureError,
    SleepReply, ThreadedRuntime, call, sleep,
};

use crate::{CLIENTS, MAX_PENDING, Report, WORKERS, expected_for};

const CALL_TIMEOUT: Duration = Duration::from_secs(5);

// --- Worker -------------------------------------------------------------

#[derive(Debug)]
enum WorkerMsg {
    Do(u64),
    Done(SleepReply, u64),
}

#[derive(Debug, Clone, Copy)]
struct WorkerReply(u64);

struct Worker {
    id: u64,
    work: Duration,
}

#[tina_runtime::isolate(message = WorkerMsg, reply = WorkerReply)]
impl Worker {
    fn handle(
        &mut self,
        msg: WorkerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            // Vary the wait by id so replies arrive out of dispatch order.
            WorkerMsg::Do(payload) => {
                let id = self.id;
                sleep(self.work).reply(move |reply| WorkerMsg::Done(reply, payload + id))
            }
            WorkerMsg::Done(Ok(()), result) => reply(WorkerReply(result)),
            WorkerMsg::Done(Err(_), _) => stop(),
        }
    }
}

// --- Frontend -----------------------------------------------------------

#[derive(Debug)]
enum FrontendMsg {
    Submit(u64),
    WorkerDone(u64, CallOutcome<WorkerReply>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontendReply {
    Result(u64),
    Full,
}

struct Frontend {
    workers: Vec<Address<WorkerMsg, WorkerReply>>,
    pending: PendingReplies<u64, FrontendReply>,
    next_qid: u64,
    next_worker: usize,
}

#[tina_runtime::isolate(message = FrontendMsg, reply = FrontendReply)]
impl Frontend {
    fn handle(
        &mut self,
        msg: FrontendMsg,
        ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            FrontendMsg::Submit(payload) => {
                let qid = self.next_qid;
                self.next_qid += 1;
                match self.pending.try_capture(ctx, qid) {
                    Ok(()) => {
                        let worker = self.workers[self.next_worker];
                        self.next_worker = (self.next_worker + 1) % self.workers.len();
                        call(worker, WorkerMsg::Do(payload), CALL_TIMEOUT)
                            .reply(move |outcome| FrontendMsg::WorkerDone(qid, outcome))
                    }
                    Err(PendingRepliesTryCaptureError::Full) => reply(FrontendReply::Full),
                    Err(other) => panic!("try_capture: {other:?}"),
                }
            }
            FrontendMsg::WorkerDone(qid, outcome) => {
                let Some(slot) = self.pending.take(&qid) else {
                    return noop();
                };
                match outcome {
                    CallOutcome::Replied(WorkerReply(v)) => {
                        reply_to(slot, FrontendReply::Result(v))
                    }
                    _ => reply_to(slot, FrontendReply::Full),
                }
            }
        }
    }
}

// --- Driver -------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
struct DriverOutcome {
    correct: usize,
    wrong: usize,
    failed: usize,
}

#[derive(Debug)]
enum DriverMsg {
    Begin,
    Returned(usize, CallOutcome<FrontendReply>),
}

struct Driver {
    frontend: Address<FrontendMsg, FrontendReply>,
    payloads: Vec<u64>,
    remaining: usize,
    outcome: DriverOutcome,
}

#[tina_runtime::isolate(message = DriverMsg)]
impl Driver {
    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::Begin => {
                let frontend = self.frontend;
                let calls: Vec<_> = self
                    .payloads
                    .iter()
                    .copied()
                    .enumerate()
                    .map(|(i, payload)| {
                        call(frontend, FrontendMsg::Submit(payload), CALL_TIMEOUT)
                            .reply(move |outcome| DriverMsg::Returned(i, outcome))
                    })
                    .collect();
                Effect::Batch(calls)
            }
            DriverMsg::Returned(i, outcome) => {
                let payload = self.payloads[i];
                // Round-robin worker assignment is deterministic.
                let worker_id = (i % WORKERS) as u64;
                let want = expected_for(payload, worker_id);
                match outcome {
                    CallOutcome::Replied(FrontendReply::Result(got)) if got == want => {
                        self.outcome.correct += 1;
                    }
                    CallOutcome::Replied(FrontendReply::Result(_)) => self.outcome.wrong += 1,
                    _ => self.outcome.failed += 1,
                }
                self.remaining -= 1;
                if self.remaining == 0 {
                    stop_with(self.outcome)
                } else {
                    noop()
                }
            }
        }
    }
}

pub fn run() -> anyhow::Result<Report> {
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);

    let mut workers = Vec::with_capacity(WORKERS);
    for w in 0..WORKERS as u64 {
        // Vary work time so replies are out of order.
        let work = Duration::from_millis(5 + (w * 7) % 20);
        workers.push(
            runtime
                .register_with_capacity::<_, Infallible>(Worker { id: w, work }, 16)
                .map_err(|e| anyhow::anyhow!("register worker: {e:?}"))?,
        );
    }

    let frontend = runtime
        .register_with_capacity::<_, Infallible>(
            Frontend {
                workers,
                pending: PendingReplies::with_capacity(MAX_PENDING),
                next_qid: 1,
                next_worker: 0,
            },
            32,
        )
        .map_err(|e| anyhow::anyhow!("register frontend: {e:?}"))?;

    let payloads: Vec<u64> = (0..CLIENTS as u64)
        .map(|c| c.wrapping_mul(11) + 1)
        .collect();
    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            Driver {
                frontend,
                payloads,
                remaining: CLIENTS,
                outcome: DriverOutcome::default(),
            },
            32,
        )
        .map_err(|e| anyhow::anyhow!("register driver: {e:?}"))?;

    let result = runtime
        .observe_result::<DriverOutcome, _, _>(driver)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;
    runtime
        .try_send(driver, DriverMsg::Begin)
        .map_err(|e| anyhow::anyhow!("kick driver: {e:?}"))?;
    let outcome = result
        .wait(Duration::from_secs(10))
        .map_err(|e| anyhow::anyhow!("driver finishes: {e:?}"))?;

    let _ = runtime.shutdown();

    Ok(Report {
        clients: CLIENTS,
        correct_replies: outcome.correct,
        wrong_replies: outcome.wrong,
        failed: outcome.failed,
        exit_clean: true,
    })
}
