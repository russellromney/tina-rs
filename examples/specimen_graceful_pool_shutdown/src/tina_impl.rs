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

use tina::pool::{AcquireFailure, CloseMode, PoolConfig, PoolLease, ReleaseDisposition};
use tina::prelude::*;
use tina_runtime::pool::{
    WorkerPool, WorkerPoolMsg, WorkerPoolReply, acquire_result_effect, close_effect,
    release_result_effect,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, SleepReply, ThreadedRuntime, sleep,
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
            WorkerMsg::Do => sleep(self.work).then(WorkerMsg::Done),
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

#[derive(Debug)]
enum DriverMsg {
    Begin,
    AcquireReturned {
        job: u32,
        result: Result<PoolLease<WorkerHandle>, AcquireFailure>,
    },
    WorkerReturned {
        job: u32,
        outcome: CallOutcome<WorkerReply>,
    },
    ReleaseReturned {
        job: u32,
    },
    CloseReturned(CallOutcome<WorkerPoolReply<WorkerHandle>>),
    Shutdown,
}

struct Driver {
    pool: Address<WorkerPoolMsg<WorkerHandle>, WorkerPoolReply<WorkerHandle>>,
    outcome: DriverOutcome,
    jobs: HashMap<u32, JobState>,
    expected: usize,
    shutdown_close_observed: bool,
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
                let mut effects = Vec::with_capacity(CALLERS);
                for j in 0..CALLERS as u32 {
                    self.jobs.insert(j, JobState::Acquiring);
                    effects.push(acquire_result_effect(
                        self.pool,
                        CALL_TIMEOUT,
                        move |result| DriverMsg::AcquireReturned { job: j, result },
                    ));
                }
                Effect::Batch(effects)
            }
            DriverMsg::AcquireReturned { job, result } => match result {
                Ok(lease) => {
                    let worker = *lease.handle();
                    self.jobs.insert(job, JobState::Working { lease });
                    tina_runtime::call(worker, WorkerMsg::Do, CALL_TIMEOUT)
                        .then(move |outcome| DriverMsg::WorkerReturned { job, outcome })
                }
                Err(AcquireFailure::Closed) | Err(AcquireFailure::Full) => {
                    self.jobs.remove(&job);
                    self.outcome.closed += 1;
                    self.maybe_finish()
                }
                Err(_) => {
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
                let succeeded = matches!(outcome, CallOutcome::Replied(_));
                let disposition = if succeeded {
                    ReleaseDisposition::Reuse
                } else {
                    ReleaseDisposition::Retire
                };
                self.jobs.insert(job, JobState::Releasing);
                if succeeded {
                    self.outcome.completed += 1;
                } else {
                    self.outcome.failed += 1;
                }
                release_result_effect(lease, self.pool, disposition, CALL_TIMEOUT, move |_| {
                    DriverMsg::ReleaseReturned { job }
                })
            }
            DriverMsg::ReleaseReturned { job } => {
                self.jobs.remove(&job);
                self.maybe_finish()
            }
            DriverMsg::Shutdown => close_effect(
                self.pool,
                CloseMode::Drain,
                CALL_TIMEOUT,
                DriverMsg::CloseReturned,
            ),
            DriverMsg::CloseReturned(outcome) => {
                self.shutdown_close_observed = close_was_observed(&outcome);
                self.maybe_finish()
            }
        }
    }
}

impl Driver {
    fn maybe_finish(&mut self) -> Effect<Self> {
        let total = self.outcome.completed + self.outcome.closed + self.outcome.failed;
        if total >= self.expected && self.shutdown_close_observed {
            stop_with(self.outcome)
        } else {
            noop()
        }
    }
}

fn close_was_observed(outcome: &CallOutcome<WorkerPoolReply<WorkerHandle>>) -> bool {
    matches!(outcome, CallOutcome::Replied(WorkerPoolReply::Closed))
}

pub fn run() -> anyhow::Result<Report> {
    let runtime = Arc::new(ThreadedRuntime::try_new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    )?);
    let shutdown = runtime.shutdown_handle();

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

    let pool: WorkerPool<WorkerHandle, SingleShard> =
        WorkerPool::new(PoolConfig::new(WORKERS, CALLERS), workers);
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
                shutdown_close_observed: false,
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

    let terminal = shutdown.request_and_wait_report(Duration::from_secs(5))?;
    drop(runtime);
    terminal.ensure_clean()?;

    Ok(Report {
        callers: CALLERS,
        completed: outcome.completed,
        closed: outcome.closed,
        failed: outcome.failed,
        shutdown_close_observed: true,
        exit_clean: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_observed_requires_real_closed_reply() {
        assert!(close_was_observed(&CallOutcome::Replied(
            WorkerPoolReply::Closed
        )));
        assert!(!close_was_observed(&CallOutcome::Timeout));
        assert!(!close_was_observed(&CallOutcome::Full));
        assert!(!close_was_observed(&CallOutcome::Closed));
        assert!(!close_was_observed(&CallOutcome::Rejected(
            tina::CallRejectedReason::UnsupportedMessage,
        )));
    }
}
