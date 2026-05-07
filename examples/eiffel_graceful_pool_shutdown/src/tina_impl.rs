use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, PendingReplies, PendingRepliesTryCaptureError,
    SleepReply, ThreadedRuntime, call, sleep,
};

use crate::{CALLERS, MAX_PENDING, Report, SHUTDOWN_AFTER_MS, WORK_MS, WORKERS};

const CALL_TIMEOUT: Duration = Duration::from_secs(5);

// --- Worker -------------------------------------------------------------

#[derive(Debug)]
enum WorkerMsg {
    Do,
    Done(SleepReply),
}

#[derive(Debug, Clone, Copy)]
struct WorkerReply;

struct Worker {
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
            WorkerMsg::Do => sleep(self.work).reply(WorkerMsg::Done),
            WorkerMsg::Done(Ok(())) => reply(WorkerReply),
            WorkerMsg::Done(Err(_)) => stop(),
        }
    }
}

// --- Frontend -----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontendReply {
    Completed,
    Closed,
}

#[derive(Debug)]
enum FrontendMsg {
    Submit,
    WorkerDone(u64, CallOutcome<WorkerReply>),
    Shutdown,
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
            FrontendMsg::Submit => {
                let qid = self.next_qid;
                self.next_qid += 1;
                match self.pending.try_capture(ctx, qid) {
                    Ok(()) => {
                        let worker = self.workers[self.next_worker];
                        self.next_worker = (self.next_worker + 1) % self.workers.len();
                        call(worker, WorkerMsg::Do, CALL_TIMEOUT)
                            .reply(move |outcome| FrontendMsg::WorkerDone(qid, outcome))
                    }
                    Err(PendingRepliesTryCaptureError::Full) => reply(FrontendReply::Closed),
                    Err(other) => panic!("try_capture: {other:?}"),
                }
            }
            FrontendMsg::WorkerDone(qid, outcome) => {
                let Some(slot) = self.pending.take(&qid) else {
                    return noop();
                };
                match outcome {
                    CallOutcome::Replied(_) => reply_to(slot, FrontendReply::Completed),
                    _ => reply_to(slot, FrontendReply::Closed),
                }
            }
            FrontendMsg::Shutdown => {
                // Drain every pending caller as Closed before stopping.
                let drained = self.pending.drain();
                let mut effects = Vec::with_capacity(drained.len() + 1);
                for (_qid, slot) in drained {
                    effects.push(reply_to(slot, FrontendReply::Closed));
                }
                effects.push(stop());
                Effect::Batch(effects)
            }
        }
    }
}

// --- Driver -------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
struct DriverOutcome {
    completed: usize,
    closed: usize,
    failed: usize,
}

#[derive(Debug)]
enum DriverMsg {
    Begin,
    Returned(CallOutcome<FrontendReply>),
}

struct Driver {
    frontend: Address<FrontendMsg, FrontendReply>,
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
                let calls: Vec<_> = (0..CALLERS)
                    .map(|_| {
                        call(frontend, FrontendMsg::Submit, CALL_TIMEOUT).reply(DriverMsg::Returned)
                    })
                    .collect();
                Effect::Batch(calls)
            }
            DriverMsg::Returned(outcome) => {
                match outcome {
                    CallOutcome::Replied(FrontendReply::Completed) => self.outcome.completed += 1,
                    CallOutcome::Replied(FrontendReply::Closed) => self.outcome.closed += 1,
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
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    let mut workers = Vec::with_capacity(WORKERS);
    for _ in 0..WORKERS {
        workers.push(
            runtime
                .register_with_capacity::<_, Infallible>(
                    Worker {
                        work: Duration::from_millis(WORK_MS),
                    },
                    16,
                )
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
            64,
        )
        .map_err(|e| anyhow::anyhow!("register frontend: {e:?}"))?;
    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            Driver {
                frontend,
                remaining: CALLERS,
                outcome: DriverOutcome::default(),
            },
            64,
        )
        .map_err(|e| anyhow::anyhow!("register driver: {e:?}"))?;

    let result = runtime
        .observe_result::<DriverOutcome, _, _>(driver)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    runtime
        .try_send(driver, DriverMsg::Begin)
        .map_err(|e| anyhow::anyhow!("kick driver: {e:?}"))?;

    std::thread::sleep(Duration::from_millis(SHUTDOWN_AFTER_MS));

    // Shutdown rides the same bounded mailbox as regular Submits.
    // `send_observed_until` retries on `MailboxFull` / `IngressFull`
    // up to the deadline.
    let close_deadline = std::time::Instant::now() + Duration::from_secs(2);
    runtime
        .send_observed_until(frontend, close_deadline, Duration::from_millis(2), || {
            FrontendMsg::Shutdown
        })
        .map_err(|e| anyhow::anyhow!("shutdown send: {e:?}"))?;

    let outcome = result
        .wait(Duration::from_secs(10))
        .map_err(|e| anyhow::anyhow!("driver finishes: {e:?}"))?;

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    Ok(Report {
        callers: CALLERS,
        completed: outcome.completed,
        closed: outcome.closed,
        failed: outcome.failed,
        exit_clean: true,
    })
}
