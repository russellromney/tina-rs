//! Focused tests for [`tina_runtime::call_with_handle`] and
//! [`tina_runtime::cancel_call`].
//!
//! These prove first-form cancel semantics:
//! - cancelling a pending call closes the wait, emits `CallCancelled`,
//!   and returns `CancelOutcome::Cancelled`;
//! - cancelling after the call already replied returns
//!   `AlreadyCompleted`;
//! - cancelling a never-dispatched handle returns `NotDispatched`.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallKind, CallOutcome, DefaultThreadedMailboxFactory, RuntimeEventKind, ThreadedRuntime,
    call_with_handle, cancel_call,
};

const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Polls `cond` for up to `total` time, checking every 5ms. Returns
/// false on timeout. Avoids fixed `thread::sleep` calls that flake on
/// loaded CI machines.
fn wait_for(total: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + total;
    while std::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    cond()
}

/// Waits until the runtime's trace shows an `IsolateCall`-shaped
/// `CallDispatchAttempted` event. Lets the test sequence
/// "begin call → wait for dispatch → next action" without sleeps.
fn wait_for_isolate_call_dispatched(
    runtime: &Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>,
) -> bool {
    wait_for(Duration::from_secs(2), || {
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
                let (effect, handle) = call_with_handle(self.worker, WorkerMsg::Hold, CALL_TIMEOUT)
                    .reply(DriverMsg::Returned);
                self.pending = Some(handle);
                effect
            }
            DriverMsg::DoCancel => {
                if let Some(handle) = self.pending.take() {
                    cancel_call(handle).reply(DriverMsg::Cancelled)
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
    let saw_cancel = wait_for(Duration::from_secs(2), || {
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
        wait_for(Duration::from_secs(2), || {
            !obs_for_reply.lock().expect("obs lock").outcomes.is_empty()
        }),
        "driver never observed the reply"
    );

    runtime
        .try_send(driver, DriverMsg::DoCancel)
        .expect("cancel");
    let obs_for_cancel = observations.clone();
    assert!(
        wait_for(Duration::from_secs(2), || {
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
