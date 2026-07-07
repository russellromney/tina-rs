//! Negative-path proofs for `defer_scoped(...).try_admit(...)` and
//! `defer_scoped(...).reply(...)`.
//!
//! The blessed admission helper has four failure modes:
//!
//! 1. `ScopeCancelled` — scope was already cancelled before this child
//!    was admitted.
//! 2. `ScopeFull` — scope's child cap is exhausted.
//! 3. `Pending(Full)` — the `PendingCancelableCallSet` is full.
//! 4. `Pending(DuplicateKey)` — a token is already stored under this key.
//!
//! Each variant must carry the pending token back so the service can
//! recover caller authority via `PendingCancelableCall::into_request_context`
//! and answer the original caller deliberately. No stranded authority,
//! no ghost cancel coverage left on the scope after a rollback.
//!
//! These tests use `defer_scoped` end-to-end through a live simulator
//! driver so the proofs hold for the production code path, not just a
//! constructed type stub.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallContextScopeExt, CallOutcome, PendingCancelableCallSet, PendingCancelableInsertError,
    PendingCancelableTicket, RequestScope, RequestScopeId, ScopeCancelCause, ScopedAdmitError,
    call_cancelable,
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

// --- Worker: idle holder of the deferred slot. ---

#[derive(Debug)]
enum WorkerMsg {
    Hold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkerReply;

#[derive(Debug, Default)]
struct Worker;

#[tina_runtime::isolate(message = WorkerMsg, reply = WorkerReply, shard = SimShard)]
impl Worker {
    fn handle(
        &mut self,
        _msg: WorkerMsg,
        _ctx: &mut Context<'_, SimShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(&mut self, msg: WorkerMsg, call: tina::CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            WorkerMsg::Hold => {
                // Capture and never release; the host will not need a
                // real reply for these tests.
                let _ = call.into_request_context();
                noop()
            }
        }
    }
}

// --- Driver: exposes a Submit call and surfaces the typed
// `ScopedAdmitError` (or the recovered reply) to the host. ---

#[derive(Debug)]
#[allow(dead_code)] // Fields used in trace inspection but not asserted here.
enum DriverMsg {
    Submit(u32),
    Returned {
        key: u32,
        ticket: PendingCancelableTicket,
        outcome: CallOutcome<WorkerReply>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // `Admitted` is part of the contract surface even
// though no current test asserts it.
enum DriverReply {
    Admitted,
    ScopeFull { cap: usize },
    ScopeCancelled(ScopeCancelCause),
    PendingFull,
    PendingDuplicate,
}

struct Driver {
    worker: Address<WorkerMsg, WorkerReply>,
    scope: RequestScope,
    pending: PendingCancelableCallSet<u32, DriverReply, WorkerReply>,
}

#[tina_runtime::isolate(message = DriverMsg, reply = DriverReply, shard = SimShard)]
impl Driver {
    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, SimShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::Returned { .. } => noop(),
            DriverMsg::Submit(_) => noop(),
        }
    }

    fn handle_call(&mut self, msg: DriverMsg, call: tina::CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            DriverMsg::Submit(key) => {
                let work = call_cancelable(self.worker, WorkerMsg::Hold, CALL_TIMEOUT);
                let admission = call.defer_scoped(&self.scope, "worker", work).try_admit(
                    &mut self.pending,
                    key,
                    |key, ticket, outcome| DriverMsg::Returned {
                        key,
                        ticket,
                        outcome,
                    },
                );
                match admission {
                    Ok(effect) => effect,
                    Err(ScopedAdmitError::ScopeCancelled { cause, token }) => {
                        tina::reply_to::<Self>(
                            token.into_request_context(),
                            DriverReply::ScopeCancelled(cause),
                        )
                    }
                    Err(ScopedAdmitError::ScopeFull { cap, token }) => tina::reply_to::<Self>(
                        token.into_request_context(),
                        DriverReply::ScopeFull { cap },
                    ),
                    Err(ScopedAdmitError::Pending(PendingCancelableInsertError::Full {
                        token,
                    })) => tina::reply_to::<Self>(
                        token.into_request_context(),
                        DriverReply::PendingFull,
                    ),
                    Err(ScopedAdmitError::Pending(
                        PendingCancelableInsertError::DuplicateKey { token },
                    )) => tina::reply_to::<Self>(
                        token.into_request_context(),
                        DriverReply::PendingDuplicate,
                    ),
                }
            }
            DriverMsg::Returned { .. } => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

fn build(
    pending_cap: usize,
    scope_child_cap: usize,
) -> (
    Simulator<SimShard>,
    Address<DriverMsg, DriverReply>,
    Address<WorkerMsg, WorkerReply>,
    RequestScope,
) {
    let mut sim = Simulator::new(SimShard, SimulatorConfig::default());
    let worker = sim.register_with_mailbox_capacity(Worker, 8);
    let scope = RequestScope::with_child_cap(RequestScopeId::alloc(), scope_child_cap);
    let driver = sim.register_with_mailbox_capacity(
        Driver {
            worker,
            scope: scope.clone(),
            pending: PendingCancelableCallSet::with_capacity(pending_cap),
        },
        32,
    );
    (sim, driver, worker, scope)
}

// Host-side helper that pretends to be an external caller: issues a
// call into the driver and captures the reply.
struct DriverHost {
    observations: Arc<Mutex<Vec<DriverReply>>>,
}

#[tina_runtime::isolate(message = DriverHostMsg, shard = SimShard)]
impl DriverHost {
    fn handle(
        &mut self,
        msg: DriverHostMsg,
        _ctx: &mut Context<'_, SimShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverHostMsg::Issue { target, key } => {
                tina_runtime::call(target, DriverMsg::Submit(key), Duration::from_millis(200))
                    .then(DriverHostMsg::Received)
            }
            DriverHostMsg::Received(outcome) => {
                if let CallOutcome::Replied(reply) = outcome {
                    self.observations.lock().expect("obs").push(reply);
                }
                noop()
            }
        }
    }
}

#[derive(Debug)]
enum DriverHostMsg {
    Issue {
        target: Address<DriverMsg, DriverReply>,
        key: u32,
    },
    Received(CallOutcome<DriverReply>),
}

fn issue(
    sim: &mut Simulator<SimShard>,
    driver: Address<DriverMsg, DriverReply>,
    host: Address<DriverHostMsg>,
    key: u32,
) {
    sim.try_send(
        host,
        DriverHostMsg::Issue {
            target: driver,
            key,
        },
    )
    .expect("send Issue");
    drain(sim);
}

#[allow(clippy::type_complexity)]
fn build_with_host(
    pending_cap: usize,
    scope_child_cap: usize,
) -> (
    Simulator<SimShard>,
    Address<DriverMsg, DriverReply>,
    Address<DriverHostMsg>,
    Arc<Mutex<Vec<DriverReply>>>,
    RequestScope,
) {
    let (mut sim, driver, _worker, scope) = build(pending_cap, scope_child_cap);
    let observations = Arc::new(Mutex::new(Vec::new()));
    let host = sim.register_with_mailbox_capacity(
        DriverHost {
            observations: observations.clone(),
        },
        16,
    );
    (sim, driver, host, observations, scope)
}

#[test]
fn try_admit_scope_cancelled_returns_token_caller_sees_reason() {
    let (mut sim, driver, host, obs, scope) = build_with_host(2, 2);
    // Cancel the scope before any admission.
    let _ = scope.cancel_synchronously(ScopeCancelCause::OwnerStopped);
    issue(&mut sim, driver, host, 1);
    let observations = obs.lock().expect("obs").clone();
    assert_eq!(
        observations,
        vec![DriverReply::ScopeCancelled(ScopeCancelCause::OwnerStopped)],
        "caller authority must be recovered with the original cause",
    );
}

#[test]
fn try_admit_scope_full_returns_token_caller_sees_cap() {
    // Scope cap = 1; pending cap = 4. The first admission succeeds and
    // occupies the scope's only slot. The second admission must hit
    // ScopeFull and return the token; the caller sees the cap.
    let (mut sim, driver, host, obs, _scope) = build_with_host(4, 1);
    issue(&mut sim, driver, host, 1);
    issue(&mut sim, driver, host, 2);
    let observations = obs.lock().expect("obs").clone();
    // The first call is still in flight (worker holds the slot), so
    // we should see only the second reply observed so far.
    assert!(
        observations.contains(&DriverReply::ScopeFull { cap: 1 }),
        "scope full reply must reach the caller; got {observations:?}",
    );
}

#[test]
fn try_admit_pending_full_returns_token_caller_sees_pending_full() {
    // Pending cap = 1; scope cap = 8. First admission consumes the
    // pending slot. The second fails as `Pending(Full)`.
    // CRITICAL: the bug fix means the scope must *not* have grown after
    // the failed second admission. We assert this directly on the
    // scope's `registered()` count.
    let (mut sim, driver, host, obs, scope) = build_with_host(1, 8);
    issue(&mut sim, driver, host, 1);
    assert_eq!(
        scope.registered(),
        1,
        "first admission registered into scope",
    );

    issue(&mut sim, driver, host, 2);
    let observations = obs.lock().expect("obs").clone();
    assert!(
        observations.contains(&DriverReply::PendingFull),
        "pending-full reply must reach the caller; got {observations:?}",
    );
    assert_eq!(
        scope.registered(),
        1,
        "failed pending insert must not leak scope registration",
    );
}

#[test]
fn try_admit_pending_duplicate_returns_token_caller_sees_duplicate() {
    // Same key admitted twice. Pending rejects the second as
    // DuplicateKey. Scope must remain at 1 registration.
    let (mut sim, driver, host, obs, scope) = build_with_host(4, 8);
    issue(&mut sim, driver, host, 7);
    assert_eq!(scope.registered(), 1);

    issue(&mut sim, driver, host, 7);
    let observations = obs.lock().expect("obs").clone();
    assert!(
        observations.contains(&DriverReply::PendingDuplicate),
        "duplicate-key reply must reach the caller; got {observations:?}",
    );
    assert_eq!(
        scope.registered(),
        1,
        "duplicate-key admission must not leak scope registration",
    );
}

#[test]
fn try_admit_rollback_on_scope_register_after_pending_succeed() {
    // The interesting case: pending succeeds, then scope register
    // fails because the scope is at its child cap. The rollback path
    // must remove the pending entry so the caller can recover.
    //
    // We set: pending cap = 4 (plenty of room), scope cap = 1 (tight).
    // The driver pre-registers something into the scope through a
    // direct register_shared call so the scope is already at cap when
    // the first try_admit runs.
    let (mut sim, driver, host, obs, scope) = build_with_host(4, 1);
    // Pre-fill the scope so the first call hits the rollback path.
    let pre_shared = Arc::new(tina::CallHandleShared::new(std::any::TypeId::of::<
        WorkerReply,
    >()));
    scope
        .register_shared("pre", pre_shared)
        .expect("pre-register fits in cap=1");
    assert_eq!(scope.registered(), 1);

    issue(&mut sim, driver, host, 1);
    let observations = obs.lock().expect("obs").clone();
    assert!(
        observations.contains(&DriverReply::ScopeFull { cap: 1 }),
        "rollback path returns ScopeFull with token recovered; got {observations:?}",
    );
    // Scope still has only the pre-registered rail; the failed
    // admission did NOT add a second entry.
    assert_eq!(
        scope.registered(),
        1,
        "scope must not have grown past cap during rollback",
    );
}
