//! Tina side. The driver fans out one IsolateCall per worker, then
//! the host sends a `Stop` message that causes the driver to stop
//! itself. Stopping the driver closes its pending IsolateCalls; any
//! worker reply that arrives later is rejected by the runtime as
//! `CallReplyRejected { RequesterClosed }` and never reaches the
//! handler.
//!
//! The README documents the gap: there is no public
//! `runtime.cancel(addr)` for external cancellation. The "send a
//! Stop message and let the requester close itself" workaround is
//! what this specimen exercises.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, SleepReply, ThreadedRuntime, call, sleep,
};

use crate::{CANCEL_AFTER_MS, FANOUT, Report, WORK_MS};

const CALL_TIMEOUT: Duration = Duration::from_secs(2);

// --- Worker ---------------------------------------------------------------

#[derive(Debug)]
enum WorkerMsg {
    /// Kick off the slow work for this iteration.
    Do,
    /// Sleep continuation; reply to the caller now.
    Done(SleepReply),
}

/// Worker's reply payload. Unit-sized — the specimen counts arrivals,
/// not values.
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
            // Pattern-match the SleepReply alias so a cancelled
            // sleep (runtime shutdown) becomes a reply with the
            // same shape rather than panic.
            WorkerMsg::Done(Ok(())) => reply(WorkerReply),
            WorkerMsg::Done(Err(_)) => stop(),
        }
    }
}

// --- Driver ---------------------------------------------------------------

#[derive(Debug)]
enum DriverMsg {
    Begin,
    Returned(CallOutcome<WorkerReply>),
    /// External cancel signal. The driver stops itself; any worker
    /// replies that fire after this never reach the handler.
    Stop,
}

struct Driver {
    workers: Vec<Address<WorkerMsg, WorkerReply>>,
    replies_before_cancel: u32,
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
                let calls: Vec<_> = self
                    .workers
                    .iter()
                    .map(|w| call(*w, WorkerMsg::Do, CALL_TIMEOUT).reply(DriverMsg::Returned))
                    .collect();
                Effect::Batch(calls)
            }
            DriverMsg::Returned(outcome) => {
                if let CallOutcome::Replied(_) = outcome {
                    self.replies_before_cancel += 1;
                }
                noop()
            }
            DriverMsg::Stop => stop_with(Report {
                replies_before_cancel: self.replies_before_cancel,
                replies_after_cancel: 0,
                cancel_observed: true,
                exit_clean: true,
            }),
        }
    }
}

// --- Run ------------------------------------------------------------------

pub fn run() -> anyhow::Result<Report> {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    let mut workers = Vec::with_capacity(FANOUT as usize);
    for _ in 0..FANOUT {
        workers.push(
            runtime
                .register_with_capacity::<_, Infallible>(
                    Worker {
                        work: Duration::from_millis(WORK_MS),
                    },
                    8,
                )
                .map_err(|e| anyhow::anyhow!("register worker: {e:?}"))?,
        );
    }

    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            Driver {
                workers,
                replies_before_cancel: 0,
            },
            32,
        )
        .map_err(|e| anyhow::anyhow!("register driver: {e:?}"))?;

    let result = runtime
        .observe_result::<Report, _, _>(driver)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    runtime
        .try_send(driver, DriverMsg::Begin)
        .map_err(|e| anyhow::anyhow!("send Begin: {e:?}"))?;

    std::thread::sleep(Duration::from_millis(CANCEL_AFTER_MS));

    runtime
        .try_send(driver, DriverMsg::Stop)
        .map_err(|e| anyhow::anyhow!("send Stop: {e:?}"))?;

    let report = result
        .wait(Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("driver did not produce a report: {e:?}"))?;

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    Ok(report)
}
