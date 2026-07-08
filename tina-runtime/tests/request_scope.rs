//! Live-runtime proof for `RequestScope`.
//!
//! Mirrors the deterministic proofs in `tina-sim/tests/request_scope.rs`
//! against the threaded runtime so we know the same invariants survive
//! out-of-order delivery and real schedulers.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RequestScope, RequestScopeId, RuntimeEventKind,
    ScopeCancelCause, ThreadedRuntime, call_cancelable,
};

const CALL_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_BUDGET: Duration = Duration::from_secs(5);

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

// --- Worker: holds the slot until Release ---

#[derive(Debug)]
#[allow(dead_code)] // Release is part of the worker contract; the live scope
// test exercises the cancel path so it is unused here.
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
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            WorkerMsg::Hold => noop(),
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

// --- Driver: registers two rails into one scope, cancels on demand. ---

#[derive(Debug)]
enum DriverMsg {
    Begin,
    CancelScope,
    Returned(CallOutcome<WorkerReply>),
    ChildCancelled(RequestScopeId, &'static str, CancelOutcome),
}

#[derive(Debug, Default, Clone)]
struct Observations {
    outcomes: Vec<CallOutcome<WorkerReply>>,
    cancel_acks: Vec<(RequestScopeId, &'static str, CancelOutcome)>,
}

struct Driver {
    worker_a: Address<WorkerMsg, WorkerReply>,
    worker_b: Address<WorkerMsg, WorkerReply>,
    scope: RequestScope,
    observations: Arc<Mutex<Observations>>,
    pending_a: Option<CallHandle<WorkerReply>>,
    pending_b: Option<CallHandle<WorkerReply>>,
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
                let (effect_a, handle_a) =
                    call_cancelable(self.worker_a, WorkerMsg::Hold, CALL_TIMEOUT)
                        .then(DriverMsg::Returned);
                let (effect_b, handle_b) =
                    call_cancelable(self.worker_b, WorkerMsg::Hold, CALL_TIMEOUT)
                        .then(DriverMsg::Returned);
                self.scope
                    .register_shared(
                        "rail_a",
                        tina::runtime_internal::call_handle_shared(&handle_a).clone(),
                    )
                    .expect("register a");
                self.scope
                    .register_shared(
                        "rail_b",
                        tina::runtime_internal::call_handle_shared(&handle_b).clone(),
                    )
                    .expect("register b");
                self.pending_a = Some(handle_a);
                self.pending_b = Some(handle_b);
                Effect::Batch(vec![effect_a, effect_b])
            }
            DriverMsg::CancelScope => {
                let (_report, effects) = self.scope.cancel_into_effects::<Self, _, _>(
                    ScopeCancelCause::ClientDisconnect,
                    DriverMsg::ChildCancelled,
                );
                let _ = self.pending_a.take();
                let _ = self.pending_b.take();
                Effect::Batch(effects)
            }
            DriverMsg::Returned(outcome) => {
                self.observations
                    .lock()
                    .expect("obs")
                    .outcomes
                    .push(outcome);
                noop()
            }
            DriverMsg::ChildCancelled(id, label, outcome) => {
                self.observations
                    .lock()
                    .expect("obs")
                    .cancel_acks
                    .push((id, label, outcome));
                noop()
            }
        }
    }
}

#[test]
fn scope_cancels_two_live_rails_with_one_call() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let worker_a = runtime
        .register_with_capacity::<_, Infallible>(Worker::default(), 8)
        .expect("worker a");
    let worker_b = runtime
        .register_with_capacity::<_, Infallible>(Worker::default(), 8)
        .expect("worker b");
    let observations = Arc::new(Mutex::new(Observations::default()));
    let scope = RequestScope::with_child_cap(RequestScopeId::alloc(), 4);
    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            Driver {
                worker_a,
                worker_b,
                scope: scope.clone(),
                observations: observations.clone(),
                pending_a: None,
                pending_b: None,
            },
            32,
        )
        .expect("driver");

    runtime.try_send(driver, DriverMsg::Begin).expect("begin");
    runtime
        .try_send(driver, DriverMsg::CancelScope)
        .expect("cancel");

    let obs_for_wait = observations.clone();
    let saw_both = wait_for(POLL_BUDGET, || {
        obs_for_wait.lock().expect("obs").cancel_acks.len() >= 2
    });
    assert!(saw_both, "driver never observed both cancel acks");

    let obs = observations.lock().expect("obs").clone();
    let events: Vec<RuntimeEventKind> = runtime
        .complete_trace()
        .expect("trace")
        .into_iter()
        .map(|e| e.kind())
        .collect();

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }

    assert_eq!(obs.cancel_acks.len(), 2, "one ack per registered rail");
    for (id, _label, outcome) in &obs.cancel_acks {
        assert_eq!(*id, scope.id());
        assert_eq!(*outcome, CancelOutcome::Cancelled);
    }
    let mut labels: Vec<&'static str> =
        obs.cancel_acks.iter().map(|(_, label, _)| *label).collect();
    labels.sort();
    assert_eq!(labels, vec!["rail_a", "rail_b"]);
    assert!(
        obs.outcomes.is_empty(),
        "no rail should reply after scope cancel; got {obs:?}",
    );
    let cancelled_count = events
        .iter()
        .filter(|kind| {
            matches!(
                kind,
                RuntimeEventKind::CallCancelled {
                    cause: tina::CancelCause::CallerCancelled,
                    ..
                }
            )
        })
        .count();
    assert!(
        cancelled_count >= 2,
        "expected at least two CallCancelled facts; events: {events:?}",
    );
    assert_eq!(
        scope.cancel_cause(),
        Some(ScopeCancelCause::ClientDisconnect)
    );
}
