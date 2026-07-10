//! Focused tests for [`tina_runtime::call_cancelable_request`].
//!
//! `call_cancelable_request` is the request-lane sibling of
//! [`tina_runtime::call_cancelable`]: it wraps the request in
//! `tina::ServiceMessage::Request` and accepts only a
//! `tina::ServiceRequestAddress`, so a split-service caller no longer
//! hand-wraps `WorkerMsg::Request(...)` at every dispatch site. These
//! tests prove the wrapped call reaches `handle_request` (not
//! `handle_event`) and that the returned `CallHandle` cancels exactly
//! like an ordinary `call_cancelable` handle.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeEventKind, SplitServiceHandle,
    ThreadedRuntime, call_cancelable_request, cancel_call,
};

const CALL_TIMEOUT: Duration = Duration::from_secs(5);

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

// --- Split-service worker: event lane is unused, request lane replies or
// holds on demand -------------------------------------------------------

#[derive(Debug)]
enum WorkerEvent {}

#[derive(Debug)]
enum WorkerRequest {
    Ping,
    Hold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkerReply;

#[derive(Debug, Default)]
struct Worker {
    held: Option<DeferredReply<WorkerReply>>,
}

#[tina_runtime::isolate(event = WorkerEvent, request = WorkerRequest, reply = WorkerReply)]
impl Worker {
    fn handle_event(
        &mut self,
        event: WorkerEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {}
    }

    fn handle_request(
        &mut self,
        request: WorkerRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            WorkerRequest::Ping => call.reply(WorkerReply),
            WorkerRequest::Hold => call.capture(|req| {
                self.held = Some(req.into_deferred());
                noop()
            }),
        }
    }
}

// --- Driver: dispatches through call_cancelable_request, stores the handle
// --------------------------------------------------------------------------

#[derive(Debug)]
enum DriverMsg {
    Begin(WorkerRequest),
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
    worker: tina::ServiceRequestAddress<WorkerEvent, WorkerRequest, WorkerReply>,
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
            DriverMsg::Begin(request) => {
                let (effect, handle) = call_cancelable_request(self.worker, request, CALL_TIMEOUT)
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
fn call_cancelable_request_dispatches_to_handle_request_and_replies() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let worker_addr = runtime
        .register_with_capacity::<Worker, Infallible>(Worker::default(), 8)
        .expect("worker");
    let worker = SplitServiceHandle::from_address(worker_addr).requests;

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

    runtime
        .try_send(driver, DriverMsg::Begin(WorkerRequest::Ping))
        .expect("begin");

    let obs_for_wait = observations.clone();
    assert!(
        wait_for(POLL_BUDGET, || {
            !obs_for_wait.lock().expect("obs lock").outcomes.is_empty()
        }),
        "driver never observed a reply over the request lane"
    );

    let obs = observations.lock().expect("obs lock").clone();
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }

    assert_eq!(
        obs.outcomes,
        [CallOutcome::Replied(WorkerReply)],
        "call_cancelable_request must dispatch to handle_request and carry the reply back"
    );
}

#[test]
fn call_cancelable_request_cancel_call_closes_pending_wait() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let worker_addr = runtime
        .register_with_capacity::<Worker, Infallible>(Worker::default(), 8)
        .expect("worker");
    let worker = SplitServiceHandle::from_address(worker_addr).requests;

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

    runtime
        .try_send(driver, DriverMsg::Begin(WorkerRequest::Hold))
        .expect("begin");
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

    assert_eq!(obs.cancels, [CancelOutcome::Cancelled]);
    assert!(
        events
            .iter()
            .any(|kind| matches!(kind, RuntimeEventKind::CallCancelled { .. })),
        "cancelling a call_cancelable_request handle must emit CallCancelled, got {events:?}"
    );
}
