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
use std::sync::Arc;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, CallReplyRejectedReason, DefaultThreadedMailboxFactory,
    DeferredReplyRejectedReason, RequestScope, RequestScopeId, RuntimeEventKind, ScopeCancelCause,
    SleepReply, ThreadedRuntime, call_cancelable_request, sleep,
};

/// Number of child rails the driver dispatches per request.
pub const FANOUT: u32 = 4;
/// Per-worker work duration. Long enough that none reply before cancel.
pub const WORK_MS: u64 = 80;
/// Latency between dispatch and cancel.
pub const CANCEL_AFTER_MS: u64 = 15;

const CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// What the run produces for the host to assert against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
pub struct WorkerReply;

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
            WorkerEvent::Wake(_) => {
                if let Some(slot) = self.held.take() {
                    tina::reply_to(slot, WorkerReply)
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
    Cancel,
    Finish,
    Returned(CallOutcome<WorkerReply>),
    /// Cancel ack from one rail. The fields are routed through this
    /// continuation so a richer report could distinguish per-rail
    /// outcomes; the smoke test only asserts on the *count* of acks.
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
            DriverMsg::Begin => self.dispatch_fanout(),
            DriverMsg::Cancel => self.cancel_scope(),
            DriverMsg::Returned(outcome) => {
                if matches!(outcome, CallOutcome::Replied(_)) && !self.cancel_fired {
                    self.rails_settled_before_cancel += 1;
                }
                noop()
            }
            DriverMsg::ChildCancelled { .. } => {
                self.cancel_acks += 1;
                noop()
            }
            DriverMsg::Finish => stop_with(Report {
                rails_total: self.rails_total,
                rails_settled_before_cancel: self.rails_settled_before_cancel,
                rails_pending_at_cancel: self.rails_pending_at_cancel,
                cause: self
                    .cancel_cause
                    .expect("cancel must fire before Finish"),
                late_rejected_in_trace: 0, // host fills this from the trace
                cancel_acks: self.cancel_acks,
            }),
        }
    }
}

impl Driver {
    fn dispatch_fanout(&mut self) -> Effect<Self> {
        let scope = RequestScope::with_child_cap(RequestScopeId::alloc(), FANOUT as usize);
        let mut effects = Vec::with_capacity(self.workers.len());
        for worker in &self.workers {
            let (effect, handle) =
                call_cancelable_request(*worker, WorkerRequest::Run, CALL_TIMEOUT)
                    .then(DriverMsg::Returned);
            // Scope is sole canceller for these rails; the worker-return
            // continuation still delivers `Returned` normally for any
            // reply that races ahead of cancel.
            scope
                .register("worker", handle)
                .expect("scope cap matches fanout");
            self.rails_total += 1;
            effects.push(effect);
        }
        self.scope = Some(scope);
        Effect::Batch(effects)
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
        effect
    }
}

// --- Run ------------------------------------------------------------------

/// Runs the specimen end-to-end against a live threaded runtime.
pub fn run() -> anyhow::Result<Report> {
    let runtime = Arc::new(ThreadedRuntime::try_new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    )?);

    let mut workers = Vec::with_capacity(FANOUT as usize);
    for _ in 0..FANOUT {
        workers.push(
            runtime
                .register_split_service::<Worker, WorkerEvent, WorkerRequest, Infallible>(
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

    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            Driver {
                workers,
                scope: None,
                rails_total: 0,
                rails_settled_before_cancel: 0,
                rails_pending_at_cancel: 0,
                cancel_cause: None,
                cancel_acks: 0,
                cancel_fired: false,
            },
            64,
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

    // Wait long enough that any late worker reply has fired and the
    // runtime has had time to record the typed rejection trace fact.
    std::thread::sleep(Duration::from_millis(WORK_MS + 60));

    runtime
        .try_send(driver, DriverMsg::Finish)
        .map_err(|e| anyhow::anyhow!("send Finish: {e:?}"))?;

    let mut report = result
        .wait(Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("driver did not produce a report: {e:?}"))?;

    // Drain a little longer for trace events that fire after the
    // driver stops. The driver lifecycle and the worker timer fire
    // are independent threads.
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
    let target = report
        .rails_total
        .saturating_sub(report.rails_settled_before_cancel);
    let drain_deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < drain_deadline {
        if count_rejected(&runtime.trace()) >= target {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    report.late_rejected_in_trace = count_rejected(&runtime.trace());

    let rt = Arc::try_unwrap(runtime)
        .map_err(|_| anyhow::anyhow!("runtime still had outstanding references at shutdown"))?;
    rt.shutdown()
        .map_err(|e| anyhow::anyhow!("runtime shutdown: {e:?}"))?;

    Ok(report)
}
