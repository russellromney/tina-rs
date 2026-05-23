//! Tina side. The driver fans out one IsolateCall per worker via
//! [`call_cancelable`], stores each named wait in a bounded
//! [`CallGroup`], and on the host's `Cancel` signal drains the group
//! and cancels each entry explicitly with [`cancel_call`]. Worker
//! replies that arrive after cancellation are
//! rejected by the runtime as
//! `CallReplyRejected { CallerCancelled }` and never reach the
//! handler.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallGroup, CallGroupToken, CallOutcome, CallReplyRejectedReason, DefaultThreadedMailboxFactory,
    DeferredReplyRejectedReason, RuntimeEventKind, SleepReply, ThreadedRuntime, call_cancelable,
    cancel_call, sleep,
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
    /// Sleep continuation carrying a call request context.
    DoneForCall(RequestContext<WorkerReply>, SleepReply),
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
            WorkerMsg::Do => sleep(self.work).then(WorkerMsg::Done),
            // Pattern-match the SleepReply alias so a cancelled
            // sleep (runtime shutdown) becomes a reply with the
            // same shape rather than panic.
            WorkerMsg::Done(Ok(())) => reply(WorkerReply),
            WorkerMsg::Done(Err(_)) => stop(),
            WorkerMsg::DoneForCall(request, Ok(())) => tina::reply_to_request(request, WorkerReply),
            WorkerMsg::DoneForCall(_, Err(_)) => stop(),
        }
    }

    fn handle_call(&mut self, msg: WorkerMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            WorkerMsg::Do => call
                .defer(sleep(self.work))
                .reply(WorkerMsg::DoneForCall),
            WorkerMsg::Done(_) | WorkerMsg::DoneForCall(_, _) => {
                call.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

// --- Driver ---------------------------------------------------------------

#[derive(Debug)]
enum DriverMsg {
    Begin,
    /// Worker reply continuation. The key plus generation token names
    /// the exact `CallGroup` slot the driver removes on completion.
    Returned {
        worker: u32,
        token: CallGroupToken,
        outcome: CallOutcome<WorkerReply>,
    },
    /// External cancel signal. Drains the pending group and cancels each
    /// stored handle.
    Cancel,
    /// One cancel completed. The driver does not inspect the outcome
    /// here — the per-cancel truth lives in `CallCancelled` trace
    /// events, which the host reads after `run`.
    Cancelled {
        worker: u32,
        token: CallGroupToken,
        outcome: CancelOutcome,
    },
    /// Stop the driver and emit the final report. Sent after the host
    /// has waited long enough for the cancellation chain to settle.
    Finish,
}

struct Driver {
    workers: Vec<Address<WorkerMsg, WorkerReply>>,
    /// Bounded named wait group: one entry per worker, keyed by worker
    /// index. `with_capacity(FANOUT)` rejects extra inserts as `Full`
    /// rather than growing — a known-size fan-out fits exactly.
    group: CallGroup<u32, WorkerReply>,
    replies_before_cancel: u32,
    cancel_observed: bool,
    group_error: bool,
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
                for (idx, worker) in self.workers.iter().enumerate() {
                    let key = idx as u32;
                    let effect = self
                        .group
                        .start_cancelable(
                            key,
                            call_cancelable(*worker, WorkerMsg::Do, CALL_TIMEOUT),
                            |worker, token, outcome| DriverMsg::Returned {
                                worker,
                                token,
                                outcome,
                            },
                        )
                        .expect("group sized to FANOUT");
                    effects.push(effect);
                }
                Effect::Batch(effects)
            }
            DriverMsg::Returned {
                worker,
                token,
                outcome,
            } => {
                // Explicit slot cleanup on completion — Tina's
                // CallGroup has no Drop magic. A normal reply settled
                // the call; the slot is reusable.
                let replied = matches!(outcome, CallOutcome::Replied(_));
                if self
                    .group
                    .record_reply(worker, token, outcome, |_| false)
                    .is_err()
                {
                    self.group_error = true;
                }
                if replied {
                    self.replies_before_cancel += 1;
                }
                noop()
            }
            DriverMsg::Cancel => {
                self.cancel_observed = true;
                let cancel_requests = self.group.drain_pending_for_cancel();
                let mut effects = Vec::with_capacity(cancel_requests.len());
                // Drain the group; `cancel_call` is still explicit
                // user code, one named wait at a time.
                for request in cancel_requests {
                    let (worker, token, handle) = request.into_parts();
                    effects.push(cancel_call(handle).then(move |outcome| {
                        DriverMsg::Cancelled {
                            worker,
                            token,
                            outcome,
                        }
                    }));
                }
                Effect::Batch(effects)
            }
            DriverMsg::Cancelled {
                worker,
                token,
                outcome,
            } => {
                if self.group.record_cancel(worker, token, outcome).is_err() {
                    self.group_error = true;
                }
                noop()
            }
            DriverMsg::Finish => stop_with(Report {
                replies_before_cancel: self.replies_before_cancel,
                replies_after_cancel: 0,
                cancel_observed: self.cancel_observed,
                exit_clean: !self.group_error,
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
                group: CallGroup::with_capacity(FANOUT as usize),
                replies_before_cancel: 0,
                cancel_observed: false,
                group_error: false,
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

    let rt = Arc::try_unwrap(runtime)
        .map_err(|_| anyhow::anyhow!("runtime still had outstanding references at shutdown"))?;
    rt.shutdown()
        .map_err(|e| anyhow::anyhow!("runtime shutdown: {e:?}"))?;
    Ok(report)
}
