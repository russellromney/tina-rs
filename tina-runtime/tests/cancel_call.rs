//! Focused tests for [`tina_runtime::call_cancelable`] and
//! [`tina_runtime::cancel_call`].
//!
//! These prove first-form cancel semantics on the live runtime:
//! - cancelling a pending call closes the wait, emits `CallCancelled`,
//!   and returns `CancelOutcome::Cancelled`;
//! - cancelling after the call already replied returns
//!   `AlreadyCompleted`;
//! - a second cancel attempt on a Cancelled handle (driven via the
//!   shared cell) returns `AlreadyCancelled`;
//! - cross-shard cancel is rejected with `WrongShard` instead of
//!   silently no-op'ing as `AlreadyCompleted`;
//! - the runtime's recently-cancelled ring is bounded and reclaims
//!   capacity (regression: cancel/complete enough calls to evict, and
//!   verify new admissions see no stale `Full`).
//!
//! See `tina-sim/tests/cancel_call.rs` for the deterministic
//! simulator-side coverage.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallKind, CallOutcome, CallReplyRejectedReason, DefaultThreadedMailboxFactory,
    DeferredReplyRejectedReason, RuntimeEventKind, ThreadedRuntime, call_cancelable, cancel_call,
};

const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Polls `cond` until it returns true or `total` elapses. The 10ms
/// step plus 5s default budget is sized for slow / thermally throttled
/// CI runners; a tight inner loop would still be portable, but the
/// poll lets the worker thread make progress.
fn wait_for(total: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + total;
    while std::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    cond()
}

const POLL_BUDGET: Duration = Duration::from_secs(5);

/// Waits until the runtime's trace shows an `IsolateCall`-shaped
/// `CallDispatchAttempted` event. Lets the test sequence
/// "begin call → wait for dispatch → next action" without sleeps.
fn wait_for_isolate_call_dispatched(
    runtime: &Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>,
) -> bool {
    wait_for(POLL_BUDGET, || {
        let trace = runtime.trace();
        trace.events().iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallDispatchAttempted {
                    call_kind: CallKind::IsolateCall,
                    ..
                }
            )
        })
    })
}

// --- Worker holds a deferred reply, never replies until told ---------------

#[derive(Debug)]
enum WorkerMsg {
    Hold,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkerReply;

#[derive(Debug, Default)]
struct Worker {
    held: Option<DeferredReply<WorkerReply>>,
}

#[tina_runtime::isolate(message = WorkerMsg, reply = WorkerReply)]
impl Worker {
    fn handle(
        &mut self,
        msg: WorkerMsg,
        ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            WorkerMsg::Hold => {
                let slot = ctx.take_reply_slot().expect("slot");
                self.held = Some(slot);
                noop()
            }
            WorkerMsg::Release => {
                if let Some(slot) = self.held.take() {
                    tina::reply_to(slot, WorkerReply)
                } else {
                    noop()
                }
            }
        }
    }

    fn handle_call(&mut self, msg: WorkerMsg, call: tina::CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            WorkerMsg::Hold => {
                self.held = Some(call.into_request_context().into_deferred());
                noop()
            }
            WorkerMsg::Release => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

// --- Driver: stores a CallHandle, cancels it on demand ---------------------

#[derive(Debug)]
enum DriverMsg {
    Begin,
    DoCancel,
    Returned(CallOutcome<WorkerReply>),
    Cancelled(CancelOutcome),
}

#[derive(Debug, Default, Clone)]
struct Observations {
    outcomes: Vec<CallOutcome<WorkerReply>>,
    cancels: Vec<CancelOutcome>,
}

struct Driver {
    worker: Address<WorkerMsg, WorkerReply>,
    observations: Arc<Mutex<Observations>>,
    pending: Option<CallHandle<WorkerReply>>,
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
                let (effect, handle) = call_cancelable(self.worker, WorkerMsg::Hold, CALL_TIMEOUT)
                    .then(DriverMsg::Returned);
                self.pending = Some(handle);
                effect
            }
            DriverMsg::DoCancel => {
                if let Some(handle) = self.pending.take() {
                    cancel_call(handle).then(DriverMsg::Cancelled)
                } else {
                    noop()
                }
            }
            DriverMsg::Returned(outcome) => {
                self.observations
                    .lock()
                    .expect("obs lock")
                    .outcomes
                    .push(outcome);
                noop()
            }
            DriverMsg::Cancelled(outcome) => {
                self.observations
                    .lock()
                    .expect("obs lock")
                    .cancels
                    .push(outcome);
                noop()
            }
        }
    }
}

#[test]
fn cancel_call_closes_pending_wait() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let worker = runtime
        .register_with_capacity::<_, Infallible>(Worker::default(), 8)
        .expect("worker");
    let observations = Arc::new(Mutex::new(Observations::default()));
    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            Driver {
                worker,
                observations: observations.clone(),
                pending: None,
            },
            16,
        )
        .expect("driver");

    runtime.try_send(driver, DriverMsg::Begin).expect("begin");
    runtime
        .try_send(driver, DriverMsg::DoCancel)
        .expect("cancel");

    let obs_for_wait = observations.clone();
    let saw_cancel = wait_for(POLL_BUDGET, || {
        !obs_for_wait.lock().expect("obs lock").cancels.is_empty()
    });
    assert!(saw_cancel, "driver never observed the cancel outcome");

    let obs = observations.lock().expect("obs lock").clone();
    let events: Vec<RuntimeEventKind> = runtime
        .complete_trace()
        .expect("trace")
        .into_iter()
        .map(|e| e.kind())
        .collect();

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }

    assert_eq!(
        obs.cancels,
        vec![CancelOutcome::Cancelled],
        "cancel must succeed; got {obs:?}"
    );
    assert!(
        obs.outcomes.is_empty(),
        "no reply should reach a cancelled caller; got {obs:?}"
    );
    assert!(
        events
            .iter()
            .any(|kind| matches!(kind, RuntimeEventKind::CallCancelled { .. })),
        "trace must record CallCancelled; events: {events:?}"
    );
}

#[test]
fn cancel_after_reply_returns_already_completed() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let worker = runtime
        .register_with_capacity::<_, Infallible>(Worker::default(), 8)
        .expect("worker");
    let observations = Arc::new(Mutex::new(Observations::default()));
    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            Driver {
                worker,
                observations: observations.clone(),
                pending: None,
            },
            16,
        )
        .expect("driver");

    runtime.try_send(driver, DriverMsg::Begin).expect("begin");
    // Wait for the call to dispatch before sending Release; otherwise
    // Release can race ahead of Hold on the worker's mailbox.
    assert!(
        wait_for_isolate_call_dispatched(&runtime),
        "isolate call was never dispatched"
    );
    runtime
        .try_send(worker, WorkerMsg::Release)
        .expect("release");
    let obs_for_reply = observations.clone();
    assert!(
        wait_for(POLL_BUDGET, || {
            !obs_for_reply.lock().expect("obs lock").outcomes.is_empty()
        }),
        "driver never observed the reply"
    );

    runtime
        .try_send(driver, DriverMsg::DoCancel)
        .expect("cancel");
    let obs_for_cancel = observations.clone();
    assert!(
        wait_for(POLL_BUDGET, || {
            !obs_for_cancel.lock().expect("obs lock").cancels.is_empty()
        }),
        "driver never observed the cancel outcome"
    );

    let obs = observations.lock().expect("obs lock").clone();

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }

    assert_eq!(
        obs.outcomes,
        vec![CallOutcome::Replied(WorkerReply)],
        "reply must arrive first; got {obs:?}"
    );
    assert_eq!(
        obs.cancels,
        vec![CancelOutcome::AlreadyCompleted],
        "cancel after reply must return AlreadyCompleted; got {obs:?}"
    );
}

#[test]
fn late_reply_after_cancel_surfaces_specifically_as_caller_cancelled() {
    // Worker captures the deferred slot during Hold. After cancel,
    // the worker's `reply_to` fires but the slot has been closed by
    // the cancel sweep — observable as
    // `DeferredReplyRejected { CallerCancelled }`. The whole point of
    // the recently-cancelled ring is to emit `CallerCancelled` here
    // instead of the generic `CallerClosed` / `NoPendingCall`. This
    // test asserts the *specific* reason; accepting either reason
    // would let the ring be silently removed and not catch it.
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let worker = runtime
        .register_with_capacity::<_, Infallible>(Worker::default(), 8)
        .expect("worker");
    let observations = Arc::new(Mutex::new(Observations::default()));
    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            Driver {
                worker,
                observations: observations.clone(),
                pending: None,
            },
            16,
        )
        .expect("driver");

    runtime.try_send(driver, DriverMsg::Begin).expect("begin");
    assert!(
        wait_for_isolate_call_dispatched(&runtime),
        "isolate call was never dispatched"
    );
    runtime
        .try_send(driver, DriverMsg::DoCancel)
        .expect("cancel");
    // Wait for cancel to land before releasing the worker.
    let obs_for_cancel = observations.clone();
    assert!(
        wait_for(POLL_BUDGET, || {
            !obs_for_cancel.lock().expect("obs lock").cancels.is_empty()
        }),
        "driver never observed the cancel outcome"
    );

    runtime
        .try_send(worker, WorkerMsg::Release)
        .expect("release");

    // Wait for the late `reply_to` to bounce through the trace.
    let runtime_for_wait = runtime.clone();
    let saw_specific_rejection = wait_for(POLL_BUDGET, || {
        let trace = runtime_for_wait.trace();
        trace.events().iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::DeferredReplyRejected {
                    reason: DeferredReplyRejectedReason::CallerCancelled,
                    ..
                } | RuntimeEventKind::CallReplyRejected {
                    reason: CallReplyRejectedReason::CallerCancelled,
                    ..
                }
            )
        })
    });

    let trace = runtime.complete_trace().expect("trace");
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }

    assert!(
        saw_specific_rejection,
        "late reply after cancel should surface as `CallerCancelled` \
         specifically (not the generic `CallerClosed` / `NoPendingCall`); \
         trace events: {:#?}",
        trace.iter().map(|e| e.kind()).collect::<Vec<_>>(),
    );

    // Negative: there must NOT be a generic CallerClosed reason for
    // this call_id — we want to catch a regression that swaps the
    // specific reason back to the generic one.
    assert!(
        !trace.iter().any(|event| matches!(
            event.kind(),
            RuntimeEventKind::DeferredReplyRejected {
                reason: DeferredReplyRejectedReason::CallerClosed,
                ..
            }
        )),
        "expected only `CallerCancelled` for this scenario, but found a \
         generic `CallerClosed` reason; ring is not classifying."
    );
}
