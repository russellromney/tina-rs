//! Tina side using the bounded `WorkerPool`.
//!
//! `WORKERS` worker isolates sit behind a `WorkerPool` that owns
//! their addresses as resources. The driver fans out `CALLERS`
//! parallel `Acquire`s. Each acquired lease drives one worker call,
//! then the lease is returned with `Release`. On `Shutdown` the
//! driver sends `Close(Drain)` to the pool — every still-parked
//! caller gets a typed `Closed` reply, and outstanding leases drain
//! normally.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use tina::pool::{AcquireOutcome, CloseMode, PoolConfig, PoolLease, ReleaseDisposition};
use tina::prelude::*;
use tina_runtime::pool::{WorkerPool, WorkerPoolMsg, WorkerPoolReply};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, SleepReply, ThreadedRuntime, call, sleep,
};

use crate::{CALLERS, Report, SHUTDOWN_AFTER_MS, WORK_MS, WORKERS};

const CALL_TIMEOUT: Duration = Duration::from_secs(5);

// --- Worker --------------------------------------------------------------

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

// --- Driver --------------------------------------------------------------

type WorkerHandle = Address<WorkerMsg, WorkerReply>;

#[derive(Debug, Default, Clone, Copy)]
struct DriverOutcome {
    completed: usize,
    closed: usize,
    failed: usize,
}

enum JobState {
    Acquiring,
    Working { lease: PoolLease<WorkerHandle> },
    Releasing,
}

enum DriverMsg {
    Begin,
    AcquireReturned {
        job: u32,
        outcome: CallOutcome<WorkerPoolReply<WorkerHandle>>,
    },
    WorkerReturned {
        job: u32,
        outcome: CallOutcome<WorkerReply>,
    },
    ReleaseReturned { job: u32 },
    CloseReturned,
    Shutdown,
}

impl std::fmt::Debug for DriverMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Begin => f.write_str("Begin"),
            Self::AcquireReturned { job, .. } => write!(f, "AcquireReturned({job})"),
            Self::WorkerReturned { job, .. } => write!(f, "WorkerReturned({job})"),
            Self::ReleaseReturned { job } => write!(f, "ReleaseReturned({job})"),
            Self::CloseReturned => f.write_str("CloseReturned"),
            Self::Shutdown => f.write_str("Shutdown"),
        }
    }
}

struct Driver {
    pool: Address<WorkerPoolMsg<WorkerHandle>, WorkerPoolReply<WorkerHandle>>,
    outcome: DriverOutcome,
    jobs: HashMap<u32, JobState>,
    expected: usize,
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
                let pool = self.pool;
                let mut effects = Vec::with_capacity(CALLERS);
                for j in 0..CALLERS as u32 {
                    self.jobs.insert(j, JobState::Acquiring);
                    effects.push(
                        call(pool, WorkerPoolMsg::Acquire, CALL_TIMEOUT)
                            .reply(move |outcome| DriverMsg::AcquireReturned { job: j, outcome }),
                    );
                }
                Effect::Batch(effects)
            }
            DriverMsg::AcquireReturned { job, outcome } => match outcome {
                CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Acquired(lease))) => {
                    let worker = *lease.handle();
                    self.jobs.insert(job, JobState::Working { lease });
                    call(worker, WorkerMsg::Do, CALL_TIMEOUT)
                        .reply(move |outcome| DriverMsg::WorkerReturned { job, outcome })
                }
                CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Closed)) => {
                    self.jobs.remove(&job);
                    self.outcome.closed += 1;
                    self.maybe_finish()
                }
                CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Full)) => {
                    self.jobs.remove(&job);
                    self.outcome.closed += 1;
                    self.maybe_finish()
                }
                CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Timeout)) => {
                    self.jobs.remove(&job);
                    self.outcome.failed += 1;
                    self.maybe_finish()
                }
                CallOutcome::Replied(_) => unreachable!("non-Acquire reply to Acquire call"),
                _ => {
                    self.jobs.remove(&job);
                    self.outcome.failed += 1;
                    self.maybe_finish()
                }
            },
            DriverMsg::WorkerReturned { job, outcome } => {
                let Some(state) = self.jobs.remove(&job) else {
                    return noop();
                };
                let JobState::Working { lease } = state else {
                    return noop();
                };
                let disposition = match &outcome {
                    CallOutcome::Replied(_) => ReleaseDisposition::Reuse,
                    _ => ReleaseDisposition::Retire,
                };
                let counted_completed = matches!(outcome, CallOutcome::Replied(_));
                self.jobs.insert(job, JobState::Releasing);
                if counted_completed {
                    self.outcome.completed += 1;
                } else {
                    self.outcome.failed += 1;
                }
                tina_runtime::pool::release_effect(
                    lease,
                    self.pool,
                    disposition,
                    CALL_TIMEOUT,
                    move |_| DriverMsg::ReleaseReturned { job },
                )
            }
            DriverMsg::ReleaseReturned { job } => {
                self.jobs.remove(&job);
                self.maybe_finish()
            }
            DriverMsg::Shutdown => {
                let pool = self.pool;
                call(pool, WorkerPoolMsg::Close(CloseMode::Drain), CALL_TIMEOUT)
                    .reply(|_| DriverMsg::CloseReturned)
            }
            DriverMsg::CloseReturned => noop(),
        }
    }
}

impl Driver {
    fn maybe_finish(&mut self) -> Effect<Self> {
        let total = self.outcome.completed + self.outcome.closed + self.outcome.failed;
        if total >= self.expected {
            stop_with(self.outcome)
        } else {
            noop()
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

    let pool: WorkerPool<WorkerHandle, SingleShard> = WorkerPool::new(
        PoolConfig::new(WORKERS, CALLERS, Duration::from_secs(5)),
        workers,
    );
    let pool_addr = runtime
        .register_with_capacity::<_, Infallible>(pool, 64)
        .map_err(|e| anyhow::anyhow!("register pool: {e:?}"))?;

    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            Driver {
                pool: pool_addr,
                outcome: DriverOutcome::default(),
                jobs: HashMap::new(),
                expected: CALLERS,
            },
            64,
        )
        .map_err(|e| anyhow::anyhow!("register driver: {e:?}"))?;

    let result = runtime
        .observe_result::<DriverOutcome, _, _>(driver)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    runtime
        .try_send(driver, DriverMsg::Begin)
        .map_err(|e| anyhow::anyhow!("send Begin: {e:?}"))?;

    std::thread::sleep(Duration::from_millis(SHUTDOWN_AFTER_MS));

    runtime
        .try_send(driver, DriverMsg::Shutdown)
        .map_err(|e| anyhow::anyhow!("send Shutdown: {e:?}"))?;

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
