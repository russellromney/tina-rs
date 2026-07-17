//! Specimen: one request, multiple child rails, one cancel.
//!
//! A driver isolate fans out [`FANOUT`] worker calls under one
//! [`RequestScope`], waits a short while, then cancels the scope. The
//! report says exactly what happened to each rail:
//!
//! - `rails_total` — rails dispatched.
//! - `rails_settled_before_cancel` — replies that arrived before cancel.
//! - `rails_pending_at_cancel` — waits the scope closed.
//! - `late_rejected_in_trace` — late worker replies recorded as typed
//!   `CallReplyRejected` / `DeferredReplyRejected` events (visible
//!   facts, never ghost deliveries).
//! - `cancel_acks` — cancel-completion messages the scope's translator
//!   produced.
//!
//! Workers that already accepted their call keep running on their own
//! schedule; that's the honesty the scope cancel promises. Nothing
//! pretends to kill the worker, and no counter lies.
//!
//! This specimen exercises [`RequestScope::register`] (typed handle
//! consumed into the scope; scope is sole canceller) and
//! [`RequestScope::cancel_into_effects`] (drain children, emit one
//! cancel effect per rail).

use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    BoundedItems, CallOutcome, CallReplyRejectedReason, DefaultThreadedMailboxFactory,
    DeferredReplyRejectedReason, LocalSystem, RequestScope, RequestScopeId, RuntimeEventKind,
    ScopeCancelCause, SleepReply, bounded_batch, call_cancelable_request, sleep,
};

/// Number of child rails the driver dispatches per request.
pub const FANOUT: u32 = 4;
/// Per-worker work duration. Long enough that none reply before cancel.
pub const WORK_MS: u64 = 80;
/// Latency between dispatch and cancel.
pub const CANCEL_AFTER_MS: u64 = 15;

const CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// What the run produces for the host to assert against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Rails dispatched in total.
    pub rails_total: u32,
    /// Rails that replied to the driver before the cancel fired.
    pub rails_settled_before_cancel: u32,
    /// Rails whose wait was still pending when the scope cancelled.
    pub rails_pending_at_cancel: u32,
    /// Cause recorded in the scope report.
    pub cause: ScopeCancelCause,
    /// Typed late-reply trace events
    /// (`CallReplyRejected { CallerCancelled }` or
    /// `DeferredReplyRejected { CallerCancelled }`).
    pub late_rejected_in_trace: u32,
    /// Number of cancel-ack messages the driver received from the
    /// scope's cancel translator.
    pub cancel_acks: u32,
    /// Exact outcomes delivered before the scope closed their waits.
    pub child_replied: u32,
    pub child_full: u32,
    pub child_closed: u32,
    pub child_timeout: u32,
    pub child_rejected: Vec<tina::CallRejectedReason>,
    /// Worker timer failures that arrived as domain replies.
    pub child_timer_failed: Vec<tina_runtime::CallError>,
    /// Exact cancellation acknowledgements, one per scope child.
    pub cancel_outcomes: Vec<tina::CancelOutcome>,
    /// Driver timer failures; empty on a clean run.
    pub driver_timer_failures: Vec<tina_runtime::CallError>,
}

// --- Worker: holds the slot, replies after a sleep ------------------------

/// Caller-authority request: the only thing an outside caller can ask.
#[derive(Debug)]
enum WorkerRequest {
    Run,
}

/// Internal event: sleep finished for a call in flight.
#[derive(Debug)]
#[allow(dead_code)] // `SleepReply` payload is part of the runtime
// contract; only the variant tag matters in this
// specimen.
enum WorkerEvent {
    Wake(SleepReply),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerReply {
    Completed,
    TimerFailed(tina_runtime::CallError),
}

struct Worker {
    work: Duration,
    held: Option<DeferredReply<WorkerReply>>,
}

#[tina_runtime::isolate(event = WorkerEvent, request = WorkerRequest, reply = WorkerReply)]
impl Worker {
    fn handle_event(
        &mut self,
        event: WorkerEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            WorkerEvent::Wake(outcome) => {
                if let Some(slot) = self.held.take() {
                    tina::reply_to(
                        slot,
                        match outcome {
                            Ok(()) => WorkerReply::Completed,
                            Err(error) => WorkerReply::TimerFailed(error),
                        },
                    )
                } else {
                    noop()
                }
            }
        }
    }

    fn handle_request(
        &mut self,
        _request: WorkerRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        call.capture(move |req| {
            self.held = Some(req.into_deferred());
            sleep(self.work).then_service_event(WorkerEvent::Wake)
        })
    }
}

// --- Driver: builds the scope, fans out, cancels on demand ----------------

#[derive(Debug)]
enum DriverMsg {
    Begin,
    Cancel(SleepReply),
    Finish(SleepReply),
    Returned(CallOutcome<WorkerReply>),
    /// Cancel ack from one rail. The exact outcome is retained in the report.
    #[allow(dead_code)]
    ChildCancelled {
        scope: RequestScopeId,
        label: &'static str,
        outcome: CancelOutcome,
    },
}

struct Driver {
    workers: Vec<tina::ServiceRequestAddress<WorkerEvent, WorkerRequest, WorkerReply>>,
    scope: Option<RequestScope>,
    rails_total: u32,
    rails_settled_before_cancel: u32,
    rails_pending_at_cancel: u32,
    cancel_cause: Option<ScopeCancelCause>,
    cancel_acks: u32,
    child_replied: u32,
    child_full: u32,
    child_closed: u32,
    child_timeout: u32,
    child_rejected: Vec<tina::CallRejectedReason>,
    child_timer_failed: Vec<tina_runtime::CallError>,
    cancel_outcomes: Vec<CancelOutcome>,
    driver_timer_failures: Vec<tina_runtime::CallError>,
    finish_timer_settled: bool,
    /// Latched after Cancel so post-cancel replies do not inflate the
    /// "settled before cancel" counter — those should be impossible
    /// (the cancel closed the wait), but if anything sneaks through
    /// it is a regression, not a normal observation.
    cancel_fired: bool,
}

#[tina_runtime::isolate(message = DriverMsg, reply = Infallible)]
impl Driver {
    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::Begin => batch(vec![
                self.dispatch_fanout(),
                sleep(Duration::from_millis(CANCEL_AFTER_MS)).then(DriverMsg::Cancel),
            ]),
            DriverMsg::Cancel(outcome) => {
                if let Err(error) = outcome {
                    self.driver_timer_failures.push(error);
                }
                self.cancel_scope()
            }
            DriverMsg::Returned(outcome) => {
                if !self.cancel_fired {
                    self.rails_settled_before_cancel += 1;
                }
                match outcome {
                    CallOutcome::Replied(WorkerReply::Completed) => self.child_replied += 1,
                    CallOutcome::Replied(WorkerReply::TimerFailed(error)) => {
                        self.child_timer_failed.push(error)
                    }
                    CallOutcome::Full => self.child_full += 1,
                    CallOutcome::Closed => self.child_closed += 1,
                    CallOutcome::Timeout => self.child_timeout += 1,
                    CallOutcome::Rejected(reason) => self.child_rejected.push(reason),
                }
                noop()
            }
            DriverMsg::ChildCancelled { outcome, .. } => {
                self.cancel_acks += 1;
                self.cancel_outcomes.push(outcome);
                self.maybe_finish()
            }
            DriverMsg::Finish(outcome) => {
                if let Err(error) = outcome {
                    self.driver_timer_failures.push(error);
                }
                self.finish_timer_settled = true;
                self.maybe_finish()
            }
        }
    }
}

impl Driver {
    fn maybe_finish(&mut self) -> Effect<Self> {
        if cancel_settlement_complete(
            self.finish_timer_settled,
            self.cancel_outcomes.len(),
            self.rails_pending_at_cancel,
        ) {
            stop_with(Report {
                rails_total: self.rails_total,
                rails_settled_before_cancel: self.rails_settled_before_cancel,
                rails_pending_at_cancel: self.rails_pending_at_cancel,
                cause: self.cancel_cause.expect("cancel must fire before Finish"),
                late_rejected_in_trace: 0,
                cancel_acks: self.cancel_acks,
                child_replied: self.child_replied,
                child_full: self.child_full,
                child_closed: self.child_closed,
                child_timeout: self.child_timeout,
                child_rejected: std::mem::take(&mut self.child_rejected),
                child_timer_failed: std::mem::take(&mut self.child_timer_failed),
                cancel_outcomes: std::mem::take(&mut self.cancel_outcomes),
                driver_timer_failures: std::mem::take(&mut self.driver_timer_failures),
            })
        } else {
            noop()
        }
    }
}

fn cancel_settlement_complete(
    finish_timer_settled: bool,
    cancel_outcomes: usize,
    rails_pending_at_cancel: u32,
) -> bool {
    finish_timer_settled && cancel_outcomes >= rails_pending_at_cancel as usize
}

impl Driver {
    fn dispatch_fanout(&mut self) -> Effect<Self> {
        let scope = RequestScope::with_child_cap(RequestScopeId::alloc(), FANOUT as usize);
        let workers = BoundedItems::try_from_iter(FANOUT as usize, self.workers.iter().copied())
            .expect("the worker registry is bounded by FANOUT");
        let effects = workers.map_effects(|worker| {
            let (effect, handle) =
                call_cancelable_request(worker, WorkerRequest::Run, CALL_TIMEOUT)
                    .then(DriverMsg::Returned);
            // Scope is sole canceller for these rails; the worker-return
            // continuation still delivers `Returned` normally for any
            // reply that races ahead of cancel.
            scope
                .register("worker", handle)
                .expect("scope cap matches fanout");
            self.rails_total += 1;
            effect
        });
        self.scope = Some(scope);
        bounded_batch(effects)
    }

    fn cancel_scope(&mut self) -> Effect<Self> {
        let scope = self.scope.as_ref().expect("scope built in Begin");
        let (report, effect) = scope.cancel_into_effect::<Self, _, _>(
            ScopeCancelCause::CallerCancelled,
            |scope, label, outcome| DriverMsg::ChildCancelled {
                scope,
                label,
                outcome,
            },
        );
        self.rails_pending_at_cancel = report.cancelled_count() as u32;
        self.cancel_cause = Some(report.cause);
        self.cancel_fired = true;
        batch(vec![
            effect,
            sleep(Duration::from_millis(WORK_MS + 60)).then(DriverMsg::Finish),
        ])
    }
}

// --- Run ------------------------------------------------------------------

/// Runs the specimen end-to-end against a live local system.
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
                    held: None,
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
                scope: None,
                rails_total: 0,
                rails_settled_before_cancel: 0,
                rails_pending_at_cancel: 0,
                cancel_cause: None,
                cancel_acks: 0,
                child_replied: 0,
                child_full: 0,
                child_closed: 0,
                child_timeout: 0,
                child_rejected: Vec::new(),
                child_timer_failed: Vec::new(),
                cancel_outcomes: Vec::with_capacity(FANOUT as usize),
                driver_timer_failures: Vec::new(),
                finish_timer_settled: false,
                cancel_fired: false,
            },
            64,
        )
        .map_err(|e| anyhow::anyhow!("register driver: {e:?}"))?;

    let result = app
        .observe_result::<Report, _, _>(driver)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    app.try_send(driver, DriverMsg::Begin)
        .map_err(|e| anyhow::anyhow!("send Begin: {e:?}"))?;

    let mut report = result
        .wait(Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("driver did not produce a report: {e:?}"))?;

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
    report.late_rejected_in_trace = count_rejected(&app.trace());

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::cancel_settlement_complete;

    #[test]
    fn finish_requires_timer_and_every_expected_cancel_ack() {
        assert!(!cancel_settlement_complete(false, 4, 4));
        assert!(!cancel_settlement_complete(true, 3, 4));
        assert!(cancel_settlement_complete(true, 4, 4));
        assert!(cancel_settlement_complete(true, 5, 4));
        assert!(cancel_settlement_complete(true, 0, 0));
    }
}
