//! Tina side. The driver fans out one IsolateCall per worker via
//! [`tina_runtime::call_cancelable_request`], stores each named wait in a bounded
//! [`CallGroup`], and on the host's `Cancel` signal drains the group
//! and cancels each entry explicitly with [`cancel_call`]. Worker
//! replies that arrive after cancellation are
//! rejected by the runtime as
//! `CallReplyRejected { CallerCancelled }` and never reach the
//! handler.

use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    BoundedItems, CallGroup, CallGroupToken, CallOutcome, CallReplyRejectedReason,
    DefaultThreadedMailboxFactory, DeferredReplyRejectedReason, LocalSystem, RuntimeEventKind,
    SleepReply, bounded_batch, call_cancelable_request, cancel_call, sleep,
};

use crate::{CANCEL_AFTER_MS, FANOUT, Report, WORK_MS};

const CALL_TIMEOUT: Duration = Duration::from_secs(2);

// --- Worker ---------------------------------------------------------------

/// Internal event: the sleep continuation for a call in flight.
#[derive(Debug)]
enum WorkerEvent {
    /// Sleep continuation carrying a call request context.
    DoneForCall(RequestContext<WorkerReply>, SleepReply),
}

/// Caller-authority request: the only thing an outside caller can ask.
#[derive(Debug)]
enum WorkerRequest {
    /// Kick off the slow work for this iteration.
    Do,
}

/// Worker reply payload. Unit-sized — the specimen counts arrivals,
/// not values.
#[derive(Debug, Clone, Copy)]
struct WorkerReply;

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
            WorkerEvent::DoneForCall(request, Ok(())) => tina::reply_to(request, WorkerReply),
            WorkerEvent::DoneForCall(_, Err(_)) => stop(),
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
                .reply_service_event(WorkerEvent::DoneForCall),
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
}

struct Driver {
    workers: Vec<tina::ServiceRequestAddress<WorkerEvent, WorkerRequest, WorkerReply>>,
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
                let workers = BoundedItems::try_from_iter(
                    FANOUT as usize,
                    self.workers.iter().copied().enumerate(),
                )
                .expect("worker fan-out is capped by FANOUT");
                bounded_batch(workers.map_effects(|(idx, worker)| {
                    self
                        .group
                        .start_cancelable(
                            idx as u32,
                            call_cancelable_request(worker, WorkerRequest::Do, CALL_TIMEOUT),
                            |worker, token, outcome| DriverMsg::Returned {
                                worker,
                                token,
                                outcome,
                            },
                        )
                        .expect("call group is capped by FANOUT")
                }))
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
                self.finish_if_settled()
            }
            DriverMsg::Cancel => {
                self.cancel_observed = true;
                let cancel_requests = self.group.drain_pending_for_cancel();
                if cancel_requests.is_empty() {
                    return self.finish_if_settled();
                }
                let cancel_requests =
                    BoundedItems::try_from_iter(FANOUT as usize, cancel_requests)
                        .expect("cancel fan-out is capped by FANOUT");
                bounded_batch(cancel_requests.map_effects(|request| {
                    let (worker, token, handle) = request.into_parts();
                    cancel_call(handle).then(move |outcome| DriverMsg::Cancelled {
                        worker,
                        token,
                        outcome,
                    })
                }))
            }
            DriverMsg::Cancelled {
                worker,
                token,
                outcome,
            } => {
                if self.group.record_cancel(worker, token, outcome).is_err() {
                    self.group_error = true;
                }
                self.finish_if_settled()
            }
        }
    }
}

impl Driver {
    fn finish_if_settled(&self) -> Effect<Self> {
        if self.cancel_observed && self.group.report_ready() {
            stop_with(Report {
                replies_before_cancel: self.replies_before_cancel,
                replies_after_cancel: 0,
                cancel_observed: true,
                exit_clean: !self.group_error,
            })
        } else {
            noop()
        }
    }
}

// --- Run ------------------------------------------------------------------

pub fn run() -> anyhow::Result<Report> {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    Ok(app.run_to_shutdown_reported(Duration::from_secs(5), run_application)?)
}

fn run_application(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
) -> anyhow::Result<Report> {
    let mut workers = Vec::with_capacity(FANOUT as usize);
    for _ in 0..FANOUT {
        workers.push(
            app.register_split_service::<Worker, WorkerEvent, WorkerRequest, Infallible>(
                Worker {
                    work: Duration::from_millis(WORK_MS),
                },
                8,
            )
            .map_err(|e| anyhow::anyhow!("register worker: {e:?}"))?
            .requests,
        );
    }

    let driver = app
        .register_root::<_, Infallible>(
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

    let result = app
        .observe_result::<Report, _, _>(driver)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    app.try_send(driver, DriverMsg::Begin)
        .map_err(|e| anyhow::anyhow!("send Begin: {e:?}"))?;

    std::thread::sleep(Duration::from_millis(CANCEL_AFTER_MS));

    app.try_send(driver, DriverMsg::Cancel)
        .map_err(|e| anyhow::anyhow!("send Cancel: {e:?}"))?;

    let mut report = result
        .wait(Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("driver did not produce a report: {e:?}"))?;

    // Wait for the worker isolates to drain their late
    // `WorkerEvent::DoneForCall` sleep continuations and bounce them
    // through the runtime as typed
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
        if count_rejected(&app.trace()) >= target {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    report.replies_after_cancel = count_rejected(&app.trace());

    Ok(report)
}
