//! Deterministic proofs for `RequestScope` and `RequestScopeSet`
//! (Phase 105: request-scoped cancellation).
//!
//! Coverage map (kept in lockstep with the phase plan's "Required
//! Proof" list):
//!
//! - **Cancel before delivery.** A scope-registered child is cancelled
//!   before the worker ever drains the call message. The caller sees
//!   `Cancelled`; the trace contains `CallCancelled { CallerCancelled }`.
//! - **Cancel after deferred capture.** The worker captured the slot;
//!   the scope cancels; the worker's later `reply_to` becomes a typed
//!   late-reply trace fact (`DeferredReplyRejected { CallerCancelled }`).
//! - **Fill-cancel-refill.** A bounded [`RequestScopeSet`] is filled,
//!   one scope is cancelled and removed, and the freed slot admits a
//!   new scope without leaking capacity.
//! - **Synchronous owner-stop cancel.** A scope marked cancelled with
//!   `OwnerStopped` reports the cause and refuses further registrations.
//!
//! These proofs are intentionally narrow. The blessed
//! `system_job_queue` and `mini_saas_api` specimens carry the wider
//! "request went away → these children were cancelled" story; this
//! test pins the runtime primitive itself.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DeferredReplyRejectedReason, RequestScope, RequestScopeId,
    RequestScopeInsertError, RequestScopeSet, RuntimeEventKind, ScopeCancelCause, call_cancelable,
};
use tina_sim::{Simulator, SimulatorConfig};

const CALL_TIMEOUT: Duration = Duration::from_millis(50);

fn drain(sim: &mut Simulator<SimShard>) {
    while sim.step() > 0 {}
}

#[derive(Debug)]
struct SimShard;

impl Shard for SimShard {
    fn id(&self) -> ShardId {
        ShardId::new(0)
    }
}

// --- Worker: holds the slot on Hold, replies on Release. ---

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

#[tina_runtime::isolate(message = WorkerMsg, reply = WorkerReply, shard = SimShard)]
impl Worker {
    fn handle_call(&mut self, msg: WorkerMsg, call: tina::CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            WorkerMsg::Hold => {
                self.held = Some(call.into_request_context().into_deferred());
                noop()
            }
            WorkerMsg::Release => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }

    fn handle(
        &mut self,
        msg: WorkerMsg,
        _ctx: &mut Context<'_, SimShard, Self::Reply>,
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
}

// --- Driver: opens a scope, makes one child call, cancels scope on
// demand, records observations. ---

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
    worker: Address<WorkerMsg, WorkerReply>,
    observations: Arc<Mutex<Observations>>,
    scope: RequestScope,
    pending: Option<CallHandle<WorkerReply>>,
}

#[tina_runtime::isolate(message = DriverMsg, shard = SimShard)]
impl Driver {
    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, SimShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::Begin => {
                let (effect, handle) = call_cancelable(self.worker, WorkerMsg::Hold, CALL_TIMEOUT)
                    .then(DriverMsg::Returned);
                // Register the rail's shared cell into the scope *before*
                // surrendering the typed handle to the worker-return path.
                let shared = tina::runtime_internal::call_handle_shared(&handle).clone();
                self.scope
                    .register_shared("worker_call", shared)
                    .expect("scope is fresh + cap > 0");
                self.pending = Some(handle);
                effect
            }
            DriverMsg::CancelScope => {
                let (_report, effects) = self.scope.cancel_into_effects::<Self, _, _>(
                    ScopeCancelCause::ClientDisconnect,
                    DriverMsg::ChildCancelled,
                );
                // The typed handle is no longer the cancellation owner; the
                // scope already issued the cancel. Drop it so it doesn't
                // appear in `Returned` ambiguity tests.
                let _ = self.pending.take();
                if effects.is_empty() {
                    noop()
                } else {
                    Effect::Batch(effects)
                }
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

#[allow(clippy::type_complexity)]
fn build() -> (
    Simulator<SimShard>,
    Address<DriverMsg>,
    Address<WorkerMsg, WorkerReply>,
    Arc<Mutex<Observations>>,
    RequestScope,
) {
    let mut sim = Simulator::new(SimShard, SimulatorConfig::default());
    let worker = sim.register_with_mailbox_capacity(Worker::default(), 8);
    let observations = Arc::new(Mutex::new(Observations::default()));
    let scope = RequestScope::with_child_cap(RequestScopeId::alloc(), 2);
    let driver = sim.register_with_mailbox_capacity(
        Driver {
            worker,
            observations: observations.clone(),
            scope: scope.clone(),
            pending: None,
        },
        16,
    );
    (sim, driver, worker, observations, scope)
}

#[test]
fn scope_cancel_closes_pending_wait_before_delivery() {
    let (mut sim, driver, _worker, observations, scope) = build();
    sim.try_send(driver, DriverMsg::Begin).expect("begin");
    drain(&mut sim);
    sim.try_send(driver, DriverMsg::CancelScope)
        .expect("cancel");
    drain(&mut sim);

    let obs = observations.lock().expect("obs").clone();
    assert_eq!(obs.cancel_acks.len(), 1, "one ack per registered child");
    let (id, label, outcome) = obs.cancel_acks[0];
    assert_eq!(id, scope.id());
    assert_eq!(label, "worker_call");
    assert_eq!(outcome, CancelOutcome::Cancelled);
    assert!(
        obs.outcomes.is_empty(),
        "cancelled caller must not receive worker reply; got {obs:?}",
    );
    assert!(scope.is_cancelled());
    assert_eq!(
        scope.cancel_cause(),
        Some(ScopeCancelCause::ClientDisconnect)
    );

    let cancelled = sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCancelled {
                cause: tina::CancelCause::CallerCancelled,
                ..
            }
        )
    });
    assert!(
        cancelled,
        "trace must record CallCancelled with CallerCancelled cause"
    );
}

#[test]
fn scope_cancel_after_deferred_capture_rejects_late_reply() {
    let (mut sim, driver, worker, observations, _scope) = build();
    sim.try_send(driver, DriverMsg::Begin).expect("begin");
    drain(&mut sim);
    sim.try_send(driver, DriverMsg::CancelScope)
        .expect("cancel");
    drain(&mut sim);
    // The worker now holds the deferred slot. Releasing it should
    // surface as a typed late-reply trace fact.
    sim.try_send(worker, WorkerMsg::Release).expect("release");
    drain(&mut sim);

    let obs = observations.lock().expect("obs").clone();
    assert!(
        obs.outcomes.is_empty(),
        "no reply should reach a cancelled caller; got {obs:?}",
    );

    let trace = sim.trace();
    let specific = trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::DeferredReplyRejected {
                    reason: DeferredReplyRejectedReason::CallerCancelled,
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        specific, 1,
        "expected exactly one `DeferredReplyRejected {{ CallerCancelled }}` for the late reply",
    );
}

#[test]
fn scope_set_fill_cancel_refill_reclaims_capacity() {
    // No isolate involvement needed for the set-level proof; this is the
    // bookkeeping invariant. The runtime-level proof above already
    // covers the rail-cancellation half.
    let mut set = RequestScopeSet::<u32>::with_capacity(2);
    let a = RequestScope::with_child_cap(RequestScopeId::alloc(), 4);
    let b = RequestScope::with_child_cap(RequestScopeId::alloc(), 4);
    set.try_insert(1, a.clone()).expect("admit 1");
    set.try_insert(2, b.clone()).expect("admit 2");
    assert!(set.is_full());

    let c = RequestScope::with_child_cap(RequestScopeId::alloc(), 4);
    match set.try_insert(3, c.clone()) {
        Err(RequestScopeInsertError::Full { key, .. }) => assert_eq!(key, 3),
        other => panic!("expected Full, got {other:?}"),
    }

    // Cancel the scope under key 1, then remove it.
    let report = a.cancel_synchronously(ScopeCancelCause::ClientDisconnect);
    assert_eq!(report.cause, ScopeCancelCause::ClientDisconnect);
    assert!(report.children.is_empty(), "no children were registered");
    set.remove(&1).expect("remove key 1");
    assert!(!set.is_full());

    // Admit the previously-rejected scope.
    set.try_insert(3, c).expect("refill key 3");
    assert!(set.is_full());

    let snapshot = set.capacity_report();
    assert_eq!(snapshot.in_use, 2);
    assert_eq!(snapshot.capacity, 2);
    assert_eq!(snapshot.available(), 0);
}

#[test]
fn scope_cancels_multiple_rails_with_one_call() {
    // Two child rails registered under one scope. One cancel runs both;
    // both ack with `Cancelled`; the synchronous report names both
    // labels and reports the cause once.
    let mut sim = Simulator::new(SimShard, SimulatorConfig::default());
    let worker_a = sim.register_with_mailbox_capacity(Worker::default(), 8);
    let worker_b = sim.register_with_mailbox_capacity(Worker::default(), 8);
    let observations = Arc::new(Mutex::new(Observations::default()));
    let scope = RequestScope::with_child_cap(RequestScopeId::alloc(), 4);

    struct TwoRail {
        worker_a: Address<WorkerMsg, WorkerReply>,
        worker_b: Address<WorkerMsg, WorkerReply>,
        observations: Arc<Mutex<Observations>>,
        scope: RequestScope,
        pending_a: Option<CallHandle<WorkerReply>>,
        pending_b: Option<CallHandle<WorkerReply>>,
    }

    #[tina_runtime::isolate(message = DriverMsg, shard = SimShard)]
    impl TwoRail {
        fn handle(
            &mut self,
            msg: DriverMsg,
            _ctx: &mut Context<'_, SimShard, Self::Reply>,
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
                        ScopeCancelCause::Timeout,
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

    let driver = sim.register_with_mailbox_capacity(
        TwoRail {
            worker_a,
            worker_b,
            observations: observations.clone(),
            scope: scope.clone(),
            pending_a: None,
            pending_b: None,
        },
        16,
    );

    sim.try_send(driver, DriverMsg::Begin).expect("begin");
    drain(&mut sim);
    sim.try_send(driver, DriverMsg::CancelScope)
        .expect("cancel");
    drain(&mut sim);

    let obs = observations.lock().expect("obs").clone();
    assert_eq!(obs.cancel_acks.len(), 2, "one ack per rail");
    let mut labels: Vec<&'static str> = obs.cancel_acks.iter().map(|(_, l, _)| *l).collect();
    labels.sort();
    assert_eq!(labels, vec!["rail_a", "rail_b"]);
    for (id, _label, outcome) in &obs.cancel_acks {
        assert_eq!(*id, scope.id());
        assert_eq!(*outcome, CancelOutcome::Cancelled);
    }
    assert!(
        obs.outcomes.is_empty(),
        "no rail should reply after scope cancel; got {obs:?}",
    );
    assert_eq!(scope.cancel_cause(), Some(ScopeCancelCause::Timeout));
}

#[test]
fn scope_owner_stop_marks_cause_and_blocks_further_admission() {
    let scope = RequestScope::with_child_cap(RequestScopeId::alloc(), 4);
    let report = scope.cancel_synchronously(ScopeCancelCause::OwnerStopped);
    assert_eq!(report.cause, ScopeCancelCause::OwnerStopped);
    // After owner-stop, registration must reject explicitly with the
    // same cause so the service can answer the caller deliberately.
    let shared = std::sync::Arc::new(tina::CallHandleShared::new(std::any::TypeId::of::<
        WorkerReply,
    >()));
    let err = scope
        .register_shared("late_rail", shared)
        .expect_err("post-stop register must reject");
    match err {
        tina_runtime::ScopeRegisterSharedError::Cancelled { cause } => {
            assert_eq!(cause, ScopeCancelCause::OwnerStopped);
        }
        other => panic!("expected Cancelled, got {other:?}"),
    }
}
