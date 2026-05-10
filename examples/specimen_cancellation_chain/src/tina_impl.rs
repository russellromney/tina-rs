//! Tina side. The driver fans out one IsolateCall per worker via
//! [`call_with_handle`], stores each [`CallHandle`] in isolate state,
//! and on the host's `Cancel` signal cancels each one explicitly with
//! [`cancel_call`]. Worker replies that arrive after cancellation are
//! rejected by the runtime as
//! `CallReplyRejected { CallerCancelled }` and never reach the
//! handler.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, CallReplyRejectedReason, DefaultThreadedMailboxFactory, DeferredReplyRejectedReason,
    RuntimeEventKind, SleepReply, ThreadedRuntime, call_with_handle, cancel_call, sleep,
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

/// Worker reply payload. Unit-sized — the specimen counts arrivals,
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
    /// External cancel signal. Cancels each pending call by handle.
    Cancel,
    /// One cancel completed. The driver does not inspect the outcome
    /// here — the per-cancel truth lives in `CallCancelled` trace
    /// events, which the host reads after `run`.
    Cancelled,
    /// Stop the driver and emit the final report. Sent after the host
    /// has waited long enough for the cancellation chain to settle.
    Finish,
}

struct Driver {
    workers: Vec<Address<WorkerMsg, WorkerReply>>,
    pending: Vec<CallHandle<WorkerReply>>,
    replies_before_cancel: u32,
    cancel_observed: bool,
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
                let mut effects = Vec::with_capacity(self.workers.len());
                self.pending.reserve(self.workers.len());
                for worker in &self.workers {
                    let (effect, handle) =
                        call_with_handle(*worker, WorkerMsg::Do, CALL_TIMEOUT)
                            .reply(DriverMsg::Returned);
                    self.pending.push(handle);
                    effects.push(effect);
                }
                Effect::Batch(effects)
            }
            DriverMsg::Returned(outcome) => {
                if let CallOutcome::Replied(_) = outcome {
                    self.replies_before_cancel += 1;
                }
                noop()
            }
            DriverMsg::Cancel => {
                self.cancel_observed = true;
                let mut effects = Vec::with_capacity(self.pending.len());
                for handle in self.pending.drain(..) {
                    effects.push(cancel_call(handle).reply(|_| DriverMsg::Cancelled));
                }
                Effect::Batch(effects)
            }
            DriverMsg::Cancelled => noop(),
            DriverMsg::Finish => stop_with(Report {
                replies_before_cancel: self.replies_before_cancel,
                replies_after_cancel: 0,
                cancel_observed: self.cancel_observed,
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
                pending: Vec::new(),
                replies_before_cancel: 0,
                cancel_observed: false,
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
        .try_send(driver, DriverMsg::Cancel)
        .map_err(|e| anyhow::anyhow!("send Cancel: {e:?}"))?;

    // Give the worker sleep timers a chance to elapse so any "late"
    // worker replies fire while the driver is alive but its
    // pending-call slots have been cancelled. Those replies surface
    // as `CallReplyRejected { CallerCancelled }` events — not
    // delivered messages — which is the visible-truth invariant.
    std::thread::sleep(Duration::from_millis(WORK_MS + 50));

    runtime
        .try_send(driver, DriverMsg::Finish)
        .map_err(|e| anyhow::anyhow!("send Finish: {e:?}"))?;

    let mut report = result
        .wait(Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("driver did not produce a report: {e:?}"))?;

    // Wait for the worker isolates to drain their late `WorkerMsg::Done`
    // continuations and bounce them through the runtime as typed
    // rejection events before snapshotting the trace. Without this,
    // a thermally throttled CI runner can race: `result.wait` returns
    // when the *driver* stops, but the workers' SleepReply firings
    // are independent. Polling until either the count converges to
    // `FANOUT - replies_before_cancel` or a budget elapses keeps the
    // host from over-specifying timing.
    fn count_rejected(snapshot: &tina_runtime::TraceSnapshot) -> u32 {
        snapshot
            .events()
            .iter()
            .filter(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::CallReplyRejected {
                        reason: CallReplyRejectedReason::CallerCancelled,
                        ..
                    } | RuntimeEventKind::DeferredReplyRejected {
                        reason: DeferredReplyRejectedReason::CallerCancelled,
                        ..
                    }
                )
            })
            .count() as u32
    }
    let target = FANOUT.saturating_sub(report.replies_before_cancel);
    let drain_deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < drain_deadline {
        if count_rejected(&runtime.trace()) >= target {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    report.replies_after_cancel = count_rejected(&runtime.trace());

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    Ok(report)
}
