//! Tina side using the bounded `WorkerPool`.
//!
//! `WORKERS` worker isolates sit behind a `WorkerPool` that owns
//! their addresses as resources. The driver fans out `CALLERS`
//! parallel `Acquire`s. Each acquired lease drives one worker call,
//! then the lease is returned with `Release`. On `Shutdown` the
//! driver sends `Close(Drain)` to the pool — every still-parked
//! caller gets a typed `Closed` reply, and outstanding leases drain
//! normally.

use std::convert::Infallible;
use std::time::Duration;

use tina::pool::{
    AcquireFailure, CloseMode, PoolConfig, PoolLease, ReleaseDisposition, ReleaseFailure,
};
use tina::prelude::*;
use tina_runtime::pool::{
    WorkerPool, WorkerPoolMsg, WorkerPoolReply, acquire_result_effect, close_effect,
    release_result_effect,
};
use tina_runtime::{
    BoundedItems, CallError, CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, SleepReply,
    bounded_batch, call_request, sleep,
};

use crate::{CALLERS, Report, SHUTDOWN_AFTER_MS, TinaTerminalCounts, WORK_MS, WORKERS};

const CALL_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

// --- Worker --------------------------------------------------------------

#[derive(Debug)]
enum WorkerRequest {
    Do,
}

#[derive(Debug)]
enum WorkerEvent {
    Done(RequestContext<WorkerReply>, SleepReply),
}

#[derive(Debug, Clone, Copy)]
enum WorkerReply {
    Completed,
    TimerFailed(CallError),
}

struct Worker {
    work: Duration,
}

#[tina_runtime::isolate(event = WorkerEvent, request = WorkerRequest, reply = WorkerReply)]
impl Worker {
    fn handle_event(
        &mut self,
        event: WorkerEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            WorkerEvent::Done(request, Ok(())) => reply_to(request, WorkerReply::Completed),
            WorkerEvent::Done(request, Err(error)) => {
                reply_to(request, WorkerReply::TimerFailed(error))
            }
        }
    }

    fn handle_request(
        &mut self,
        request: WorkerRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            WorkerRequest::Do => call
                .defer(sleep(self.work))
                .reply_service_event(WorkerEvent::Done),
        }
    }
}

// --- Driver --------------------------------------------------------------

type WorkerHandle = tina::ServiceRequestAddress<WorkerEvent, WorkerRequest, WorkerReply>;

#[derive(Debug, Default, Clone)]
struct DriverOutcome {
    completed: usize,
    closed: usize,
    failed: usize,
    terminals: TinaTerminalCounts,
    shutdown_close_observed: bool,
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
        result: Result<(), ReleaseFailure>,
    },
    CloseReturned(CallOutcome<WorkerPoolReply<WorkerHandle>>),
    Shutdown(SleepReply),
}

struct Driver {
    pool: Address<WorkerPoolMsg<WorkerHandle>, WorkerPoolReply<WorkerHandle>>,
    outcome: DriverOutcome,
    jobs: [Option<JobState>; CALLERS],
    expected: usize,
    shutdown_close_observed: bool,
    shutdown_close_settled: bool,
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
                let jobs = BoundedItems::try_from_iter(CALLERS, 0..CALLERS as u32)
                    .expect("CALLERS is the driver-owned fanout bound");
                batch(vec![
                    bounded_batch(jobs.map_effects(|j| {
                        self.jobs[j as usize] = Some(JobState::Acquiring);
                        acquire_result_effect(self.pool, CALL_TIMEOUT, move |result| {
                            DriverMsg::AcquireReturned { job: j, result }
                        })
                    })),
                    sleep(Duration::from_millis(SHUTDOWN_AFTER_MS)).then(DriverMsg::Shutdown),
                ])
            }
            DriverMsg::AcquireReturned { job, result } => match result {
                Ok(lease) => {
                    let worker = *lease.handle();
                    self.jobs[job as usize] = Some(JobState::Working { lease });
                    call_request(worker, WorkerRequest::Do, CALL_TIMEOUT)
                        .then(move |outcome| DriverMsg::WorkerReturned { job, outcome })
                }
                Err(failure) => {
                    self.jobs[job as usize] = None;
                    match failure {
                        AcquireFailure::Full => {
                            self.outcome.terminals.acquire_full += 1;
                            self.outcome.failed += 1;
                        }
                        AcquireFailure::Closed => {
                            self.outcome.terminals.acquire_closed += 1;
                            self.outcome.closed += 1;
                        }
                        AcquireFailure::WrongShard => {
                            self.outcome.terminals.acquire_wrong_shard += 1;
                            self.outcome.failed += 1;
                        }
                        AcquireFailure::CallTimeout => {
                            self.outcome.terminals.acquire_call_timeout += 1;
                            self.outcome.failed += 1;
                        }
                        AcquireFailure::CallFull => {
                            self.outcome.terminals.acquire_call_full += 1;
                            self.outcome.failed += 1;
                        }
                        AcquireFailure::CallClosed => {
                            self.outcome.terminals.acquire_call_closed += 1;
                            self.outcome.failed += 1;
                        }
                        AcquireFailure::CallRejected(reason) => {
                            self.outcome.terminals.acquire_call_rejections.push(reason);
                            self.outcome.failed += 1;
                        }
                        AcquireFailure::WrongReply => {
                            self.outcome.terminals.acquire_wrong_reply += 1;
                            self.outcome.failed += 1;
                        }
                    }
                    self.maybe_finish()
                }
            },
            DriverMsg::WorkerReturned { job, outcome } => {
                let Some(state) = self.jobs[job as usize].take() else {
                    return noop();
                };
                let JobState::Working { lease } = state else {
                    return noop();
                };
                let succeeded = matches!(outcome, CallOutcome::Replied(WorkerReply::Completed));
                match outcome {
                    CallOutcome::Replied(WorkerReply::Completed) => self.outcome.completed += 1,
                    CallOutcome::Replied(WorkerReply::TimerFailed(error)) => {
                        self.outcome.terminals.worker_timer_failures.push(error);
                        self.outcome.failed += 1;
                    }
                    CallOutcome::Full => {
                        self.outcome.terminals.worker_full += 1;
                        self.outcome.failed += 1;
                    }
                    CallOutcome::Closed => {
                        self.outcome.terminals.worker_closed += 1;
                        self.outcome.failed += 1;
                    }
                    CallOutcome::Timeout => {
                        self.outcome.terminals.worker_timeout += 1;
                        self.outcome.failed += 1;
                    }
                    CallOutcome::Rejected(reason) => {
                        self.outcome.terminals.worker_rejections.push(reason);
                        self.outcome.failed += 1;
                    }
                }
                let disposition = if succeeded {
                    ReleaseDisposition::Reuse
                } else {
                    ReleaseDisposition::Retire
                };
                self.jobs[job as usize] = Some(JobState::Releasing);
                release_result_effect(lease, self.pool, disposition, CALL_TIMEOUT, move |result| {
                    DriverMsg::ReleaseReturned { job, result }
                })
            }
            DriverMsg::ReleaseReturned { job, result } => {
                if let Err(failure) = result {
                    match failure {
                        ReleaseFailure::Retired => self.outcome.terminals.release_retired += 1,
                        ReleaseFailure::StaleLease => {
                            self.outcome.terminals.release_stale_lease += 1
                        }
                        ReleaseFailure::DoubleRelease => {
                            self.outcome.terminals.release_double_release += 1
                        }
                        ReleaseFailure::PoolClosed => {
                            self.outcome.terminals.release_pool_closed += 1
                        }
                        ReleaseFailure::CallTimeout => {
                            self.outcome.terminals.release_call_timeout += 1
                        }
                        ReleaseFailure::CallFull => self.outcome.terminals.release_call_full += 1,
                        ReleaseFailure::CallClosed => {
                            self.outcome.terminals.release_call_closed += 1
                        }
                        ReleaseFailure::CallRejected(reason) => {
                            self.outcome.terminals.release_call_rejections.push(reason)
                        }
                        ReleaseFailure::WrongReply => {
                            self.outcome.terminals.release_wrong_reply += 1
                        }
                    }
                }
                self.jobs[job as usize] = None;
                self.maybe_finish()
            }
            DriverMsg::Shutdown(result) => {
                if let Err(error) = result {
                    self.outcome.terminals.shutdown_timer_failures.push(error);
                }
                close_effect(
                    self.pool,
                    CloseMode::Drain,
                    CALL_TIMEOUT,
                    DriverMsg::CloseReturned,
                )
            }
            DriverMsg::CloseReturned(outcome) => {
                self.shutdown_close_observed = close_was_observed(&outcome);
                self.outcome.shutdown_close_observed = self.shutdown_close_observed;
                self.shutdown_close_settled = true;
                match outcome {
                    CallOutcome::Replied(WorkerPoolReply::Closed) => {}
                    CallOutcome::Replied(_) => self.outcome.terminals.close_wrong_reply += 1,
                    CallOutcome::Full => self.outcome.terminals.close_full += 1,
                    CallOutcome::Closed => self.outcome.terminals.close_closed += 1,
                    CallOutcome::Timeout => self.outcome.terminals.close_timeout += 1,
                    CallOutcome::Rejected(reason) => {
                        self.outcome.terminals.close_rejections.push(reason)
                    }
                }
                self.maybe_finish()
            }
        }
    }
}

impl Driver {
    fn maybe_finish(&mut self) -> Effect<Self> {
        let total = self.outcome.completed + self.outcome.closed + self.outcome.failed;
        if total >= self.expected
            && self.jobs.iter().all(Option::is_none)
            && self.shutdown_close_settled
        {
            stop_with(self.outcome.clone())
        } else {
            noop()
        }
    }
}

fn close_was_observed(outcome: &CallOutcome<WorkerPoolReply<WorkerHandle>>) -> bool {
    matches!(outcome, CallOutcome::Replied(WorkerPoolReply::Closed))
}

pub fn run() -> anyhow::Result<Report> {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    Ok(app.run_to_shutdown_reported(SHUTDOWN_TIMEOUT, run_application)?)
}

fn run_application(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
) -> anyhow::Result<Report> {
    let mut workers = Vec::with_capacity(WORKERS);
    for _ in 0..WORKERS {
        workers.push(
            app.register_split_service::<Worker, WorkerEvent, WorkerRequest, Infallible>(
                Worker {
                    work: Duration::from_millis(WORK_MS),
                },
                16,
            )
            .map_err(|e| anyhow::anyhow!("register worker: {e:?}"))?
            .requests,
        );
    }

    let pool: WorkerPool<WorkerHandle, SingleShard> =
        WorkerPool::new(PoolConfig::new(WORKERS, CALLERS), workers);
    let pool_addr = app
        .register_root::<_, Infallible>(pool, 64)
        .map_err(|e| anyhow::anyhow!("register pool: {e:?}"))?;

    let driver = app
        .register_root::<_, Infallible>(
            Driver {
                pool: pool_addr,
                outcome: DriverOutcome::default(),
                jobs: std::array::from_fn(|_| None),
                expected: CALLERS,
                shutdown_close_observed: false,
                shutdown_close_settled: false,
            },
            64,
        )
        .map_err(|e| anyhow::anyhow!("register driver: {e:?}"))?;

    let result = app
        .observe_result::<DriverOutcome, _, _>(driver)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    app.try_send(driver, DriverMsg::Begin)
        .map_err(|e| anyhow::anyhow!("send Begin: {e:?}"))?;

    let outcome = result
        .wait(Duration::from_secs(10))
        .map_err(|e| anyhow::anyhow!("driver finishes: {e:?}"))?;

    Ok(Report {
        callers: CALLERS,
        completed: outcome.completed,
        closed: outcome.closed,
        failed: outcome.failed,
        shutdown_close_observed: outcome.shutdown_close_observed,
        exit_clean: true,
        tina_terminals: outcome.terminals,
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
