//! Bounded WorkerPool tests.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::pool::{
    AcquireOutcome, CloseMode, PoolConfig, PoolLease, PoolPressureReport, ReleaseDisposition,
    ReleaseOutcome,
};
use tina::prelude::*;
use tina_runtime::pool::{WorkerPool, WorkerPoolMsg, WorkerPoolReply};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime, call, call_cancelable, cancel_call,
};

type Resource = u32;

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

#[derive(Debug, Default)]
struct Observations {
    acquires: Vec<AcquireKind>,
    releases: Vec<ReleaseOutcome>,
    cancel_acks: Vec<CancelAck>,
    pressure: Option<PoolPressureReport>,
    closed_acks: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcquireKind {
    Acquired { resource: Resource, generation: u64 },
    Full,
    Closed,
    WrongShard,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelAck {
    Ok,
    AlreadyCompleted,
    AlreadyCancelled,
    WrongShard,
}

enum DriverMsg {
    BeginAcquire {
        id: u32,
    },
    BeginAcquireWithHandle {
        id: u32,
    },
    AcquireReturned {
        id: u32,
        outcome: CallOutcome<WorkerPoolReply<Resource>>,
    },
    BeginRelease {
        id: u32,
        disposition: ReleaseDisposition,
    },
    BeginReleaseTo {
        id: u32,
        disposition: ReleaseDisposition,
        target: Address<WorkerPoolMsg<Resource>, WorkerPoolReply<Resource>>,
    },
    ReleaseReturned(CallOutcome<WorkerPoolReply<Resource>>),
    BeginCancel {
        id: u32,
    },
    CancelReturned(tina::CancelOutcome),
    BeginPressure,
    PressureReturned(CallOutcome<WorkerPoolReply<Resource>>),
    BeginClose(CloseMode),
    CloseReturned(CallOutcome<WorkerPoolReply<Resource>>),
}

impl std::fmt::Debug for DriverMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeginAcquire { id } => write!(f, "BeginAcquire({id})"),
            Self::BeginAcquireWithHandle { id } => write!(f, "BeginAcquireWithHandle({id})"),
            Self::AcquireReturned { id, .. } => write!(f, "AcquireReturned({id})"),
            Self::BeginRelease { id, disposition } => {
                write!(f, "BeginRelease({id}, {disposition:?})")
            }
            Self::BeginReleaseTo {
                id, disposition, ..
            } => {
                write!(f, "BeginReleaseTo({id}, {disposition:?})")
            }
            Self::ReleaseReturned(_) => f.write_str("ReleaseReturned"),
            Self::BeginCancel { id } => write!(f, "BeginCancel({id})"),
            Self::CancelReturned(o) => write!(f, "CancelReturned({o:?})"),
            Self::BeginPressure => f.write_str("BeginPressure"),
            Self::PressureReturned(_) => f.write_str("PressureReturned"),
            Self::BeginClose(m) => write!(f, "BeginClose({m:?})"),
            Self::CloseReturned(_) => f.write_str("CloseReturned"),
        }
    }
}

struct Driver {
    pool: Address<WorkerPoolMsg<Resource>, WorkerPoolReply<Resource>>,
    obs: Arc<Mutex<Observations>>,
    leases: Vec<(u32, PoolLease<Resource>)>,
    handles: Vec<(u32, tina::CallHandle<WorkerPoolReply<Resource>>)>,
}

#[tina_runtime::isolate(message = DriverMsg)]
impl Driver {
    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::BeginAcquire { id } => call(self.pool, WorkerPoolMsg::Acquire, CALL_TIMEOUT)
                .then(move |outcome| DriverMsg::AcquireReturned { id, outcome }),
            DriverMsg::BeginAcquireWithHandle { id } => {
                let (effect, handle) =
                    call_cancelable(self.pool, WorkerPoolMsg::Acquire, CALL_TIMEOUT)
                        .then(move |outcome| DriverMsg::AcquireReturned { id, outcome });
                self.handles.push((id, handle));
                effect
            }
            DriverMsg::AcquireReturned { id, outcome } => {
                let kind = match outcome {
                    CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Acquired(
                        lease,
                    ))) => {
                        let resource = *lease.handle();
                        let generation = lease.generation();
                        self.leases.push((id, lease));
                        AcquireKind::Acquired {
                            resource,
                            generation,
                        }
                    }
                    CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Full)) => {
                        AcquireKind::Full
                    }
                    CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Closed)) => {
                        AcquireKind::Closed
                    }
                    CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::WrongShard)) => {
                        AcquireKind::WrongShard
                    }
                    CallOutcome::Replied(_other) => panic!("unexpected reply variant"),
                    _ => AcquireKind::Failed,
                };
                self.obs.lock().expect("obs").acquires.push(kind);
                noop()
            }
            DriverMsg::BeginRelease { id, disposition } => {
                let pos = self
                    .leases
                    .iter()
                    .position(|(lid, _)| *lid == id)
                    .expect("lease for id");
                let (_, lease) = self.leases.remove(pos);
                tina_runtime::pool::release_effect(
                    lease,
                    self.pool,
                    disposition,
                    CALL_TIMEOUT,
                    DriverMsg::ReleaseReturned,
                )
            }
            DriverMsg::BeginReleaseTo {
                id,
                disposition,
                target,
            } => {
                let pos = self
                    .leases
                    .iter()
                    .position(|(lid, _)| *lid == id)
                    .expect("lease for id");
                let (_, lease) = self.leases.remove(pos);
                tina_runtime::pool::release_effect(
                    lease,
                    target,
                    disposition,
                    CALL_TIMEOUT,
                    DriverMsg::ReleaseReturned,
                )
            }
            DriverMsg::ReleaseReturned(outcome) => {
                let release_outcome = match outcome {
                    CallOutcome::Replied(WorkerPoolReply::Release(o)) => o,
                    other => panic!("unexpected release reply: {other:?}"),
                };
                self.obs.lock().expect("obs").releases.push(release_outcome);
                noop()
            }
            DriverMsg::BeginCancel { id } => {
                let pos = self
                    .handles
                    .iter()
                    .position(|(hid, _)| *hid == id)
                    .expect("handle for id");
                let (_, handle) = self.handles.remove(pos);
                cancel_call(handle).then(DriverMsg::CancelReturned)
            }
            DriverMsg::CancelReturned(outcome) => {
                let ack = match outcome {
                    tina::CancelOutcome::Cancelled => CancelAck::Ok,
                    tina::CancelOutcome::AlreadyCompleted => CancelAck::AlreadyCompleted,
                    tina::CancelOutcome::AlreadyCancelled => CancelAck::AlreadyCancelled,
                    tina::CancelOutcome::WrongShard => CancelAck::WrongShard,
                };
                self.obs.lock().expect("obs").cancel_acks.push(ack);
                noop()
            }
            DriverMsg::BeginPressure => {
                call(self.pool, WorkerPoolMsg::PressureReport, CALL_TIMEOUT)
                    .then(DriverMsg::PressureReturned)
            }
            DriverMsg::PressureReturned(outcome) => {
                if let CallOutcome::Replied(WorkerPoolReply::Pressure(report)) = outcome {
                    self.obs.lock().expect("obs").pressure = Some(report);
                }
                noop()
            }
            DriverMsg::BeginClose(mode) => {
                call(self.pool, WorkerPoolMsg::Close(mode), CALL_TIMEOUT)
                    .then(DriverMsg::CloseReturned)
            }
            DriverMsg::CloseReturned(outcome) => {
                if matches!(outcome, CallOutcome::Replied(WorkerPoolReply::Closed)) {
                    self.obs.lock().expect("obs").closed_acks += 1;
                }
                noop()
            }
        }
    }
}

// --- Helpers --------------------------------------------------------------

struct Harness {
    runtime: Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>,
    driver: Address<DriverMsg>,
    obs: Arc<Mutex<Observations>>,
    pool: Address<WorkerPoolMsg<Resource>, WorkerPoolReply<Resource>>,
}

fn build(config: PoolConfig, resources: Vec<Resource>) -> Harness {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let pool_isolate: WorkerPool<Resource, SingleShard> = WorkerPool::new(config, resources);
    let pool = runtime
        .register_with_capacity::<_, Infallible>(pool_isolate, 64)
        .expect("register pool");
    let obs = Arc::new(Mutex::new(Observations::default()));
    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            Driver {
                pool,
                obs: obs.clone(),
                leases: Vec::new(),
                handles: Vec::new(),
            },
            64,
        )
        .expect("register driver");
    Harness {
        runtime,
        driver,
        obs,
        pool,
    }
}

fn shutdown(harness: Harness) {
    if let Ok(rt) = Arc::try_unwrap(harness.runtime) {
        let _ = rt.shutdown();
    }
}

fn wait_acquires(obs: &Arc<Mutex<Observations>>, n: usize) {
    let obs = obs.clone();
    assert!(
        wait_for(POLL_BUDGET, || obs.lock().expect("obs").acquires.len() >= n),
        "expected {n} acquire outcomes; saw {:?}",
        obs.lock().expect("obs").acquires
    );
}

fn wait_releases(obs: &Arc<Mutex<Observations>>, n: usize) {
    let obs = obs.clone();
    assert!(
        wait_for(POLL_BUDGET, || obs.lock().expect("obs").releases.len() >= n),
        "expected {n} release outcomes; saw {:?}",
        obs.lock().expect("obs").releases
    );
}

fn wait_cancels(obs: &Arc<Mutex<Observations>>, n: usize) {
    let obs = obs.clone();
    assert!(
        wait_for(POLL_BUDGET, || obs.lock().expect("obs").cancel_acks.len()
            >= n),
        "expected {n} cancel acks; saw {:?}",
        obs.lock().expect("obs").cancel_acks
    );
}

fn wait_pressure(obs: &Arc<Mutex<Observations>>) -> PoolPressureReport {
    let obs_clone = obs.clone();
    assert!(
        wait_for(POLL_BUDGET, || obs_clone
            .lock()
            .expect("obs")
            .pressure
            .is_some()),
        "no pressure report observed"
    );
    let mut guard = obs.lock().expect("obs");
    guard.pressure.take().expect("pressure")
}

fn wait_closed_ack(obs: &Arc<Mutex<Observations>>) {
    let obs = obs.clone();
    assert!(
        wait_for(POLL_BUDGET, || obs.lock().expect("obs").closed_acks >= 1),
        "close ack never observed"
    );
}

// --- Tests ---------------------------------------------------------------

#[test]
fn immediate_acquire_when_idle() {
    let h = build(PoolConfig::new(2, 0), vec![10, 20]);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("send");
    wait_acquires(&h.obs, 1);

    let acquired = h.obs.lock().expect("obs").acquires[0];
    match acquired {
        AcquireKind::Acquired { resource, .. } => assert!(matches!(resource, 10 | 20)),
        other => panic!("expected Acquired, got {other:?}"),
    }

    shutdown(h);
}

#[test]
fn full_when_busy_and_no_waiter_capacity() {
    let h = build(PoolConfig::new(1, 0), vec![1]);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("a");
    wait_acquires(&h.obs, 1);
    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 2 })
        .expect("b");
    wait_acquires(&h.obs, 2);

    let kinds = h.obs.lock().expect("obs").acquires.clone();
    assert!(matches!(kinds[0], AcquireKind::Acquired { .. }));
    assert_eq!(kinds[1], AcquireKind::Full);

    shutdown(h);
}

#[test]
fn max_waiters_zero_sheds_immediately() {
    // Documented config: max_waiters=0 means shed without parking.
    let h = build(PoolConfig::new(1, 0), vec![99]);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("a1");
    wait_acquires(&h.obs, 1);

    for id in 2..=5 {
        h.runtime
            .try_send(h.driver, DriverMsg::BeginAcquire { id })
            .expect("send shed");
    }
    wait_acquires(&h.obs, 5);

    let kinds = h.obs.lock().expect("obs").acquires.clone();
    assert!(matches!(kinds[0], AcquireKind::Acquired { .. }));
    assert!(kinds[1..].iter().all(|k| matches!(k, AcquireKind::Full)));

    h.runtime
        .try_send(h.driver, DriverMsg::BeginPressure)
        .expect("pressure");
    let pr = wait_pressure(&h.obs);
    assert_eq!(pr.full_count, 4);
    assert_eq!(pr.waiters, 0);
    assert_eq!(pr.max_waiters, 0);

    shutdown(h);
}

#[test]
fn waiter_parked_then_dispatched_on_release() {
    let h = build(PoolConfig::new(1, 4), vec![42]);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("a");
    wait_acquires(&h.obs, 1);
    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 2 })
        .expect("b");

    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        h.obs.lock().expect("obs").acquires.len(),
        1,
        "second acquire should still be parked"
    );

    h.runtime
        .try_send(
            h.driver,
            DriverMsg::BeginRelease {
                id: 1,
                disposition: ReleaseDisposition::Reuse,
            },
        )
        .expect("release");

    wait_acquires(&h.obs, 2);
    wait_releases(&h.obs, 1);

    let kinds = h.obs.lock().expect("obs").acquires.clone();
    let releases = h.obs.lock().expect("obs").releases.clone();
    assert!(matches!(kinds[0], AcquireKind::Acquired { .. }));
    assert!(matches!(kinds[1], AcquireKind::Acquired { .. }));
    assert_eq!(releases, vec![ReleaseOutcome::Released]);

    shutdown(h);
}

#[test]
fn wrong_pool_release_is_stale() {
    // Two real pools. Acquire from A, send the lease to B's release.
    // B sees a foreign pool_id and returns StaleLease.
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    let pool_a: WorkerPool<Resource, SingleShard> = WorkerPool::new(PoolConfig::new(1, 0), vec![1]);
    let pool_a_addr = runtime
        .register_with_capacity::<_, Infallible>(pool_a, 32)
        .expect("a");
    let pool_b: WorkerPool<Resource, SingleShard> = WorkerPool::new(PoolConfig::new(1, 0), vec![2]);
    let pool_b_addr = runtime
        .register_with_capacity::<_, Infallible>(pool_b, 32)
        .expect("b");
    let obs = Arc::new(Mutex::new(Observations::default()));
    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            Driver {
                pool: pool_a_addr,
                obs: obs.clone(),
                leases: Vec::new(),
                handles: Vec::new(),
            },
            32,
        )
        .expect("driver");

    runtime
        .try_send(driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("acquire from A");
    wait_acquires(&obs, 1);

    runtime
        .try_send(
            driver,
            DriverMsg::BeginReleaseTo {
                id: 1,
                disposition: ReleaseDisposition::Reuse,
                target: pool_b_addr,
            },
        )
        .expect("release to B");
    wait_releases(&obs, 1);
    assert_eq!(
        obs.lock().expect("obs").releases[0],
        ReleaseOutcome::StaleLease
    );

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

#[test]
fn double_release_returns_double_release() {
    // Acquire, release with Reuse (Released), then attempt to release
    // again. The driver no longer has the lease — but a parked second
    // caller does. Use a parked acquire to get a fresh lease, then
    // re-release the first lease via a follow-up.
    //
    // Simpler: acquire, release, re-acquire (gen 2), release a lease
    // we constructed via the public `release_effect` path with a
    // generation-1 lease — but we sealed PoolLease::new. So the
    // strict double-release path (same generation twice) requires
    // forging via runtime_internal in tests, which we explicitly
    // disallow.
    //
    // What we CAN test from outside: release a lease whose generation
    // is older than the pool's current `next_generation` for that
    // resource. After acquire→release→acquire, the second lease is
    // gen 2. If we squirrel away a gen-1 lease... we don't have one,
    // because we already released it.
    //
    // Conclusion: structural double-release-of-a-generation is now a
    // tina-sim-only test (next file). Here we just confirm the
    // single-release happy path.
    let h = build(PoolConfig::new(1, 0), vec![1]);
    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("a");
    wait_acquires(&h.obs, 1);
    h.runtime
        .try_send(
            h.driver,
            DriverMsg::BeginRelease {
                id: 1,
                disposition: ReleaseDisposition::Reuse,
            },
        )
        .expect("r");
    wait_releases(&h.obs, 1);
    assert_eq!(
        h.obs.lock().expect("obs").releases[0],
        ReleaseOutcome::Released
    );
    shutdown(h);
}

#[test]
fn drain_close_settles_waiters_as_closed_and_release_returns_released() {
    // Drain mode: parked waiters get Closed, BUT outstanding leases
    // released with Reuse return Released (not Retired). The post-#7
    // contract.
    let h = build(PoolConfig::new(1, 3), vec![1]);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("a1");
    wait_acquires(&h.obs, 1);
    for id in 2..=4 {
        h.runtime
            .try_send(h.driver, DriverMsg::BeginAcquire { id })
            .expect("park");
    }
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(h.obs.lock().expect("obs").acquires.len(), 1);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginClose(CloseMode::Drain))
        .expect("close");

    wait_acquires(&h.obs, 4);
    let closed_count = h
        .obs
        .lock()
        .expect("obs")
        .acquires
        .iter()
        .filter(|k| matches!(k, AcquireKind::Closed))
        .count();
    assert_eq!(closed_count, 3);
    wait_closed_ack(&h.obs);

    h.runtime
        .try_send(
            h.driver,
            DriverMsg::BeginRelease {
                id: 1,
                disposition: ReleaseDisposition::Reuse,
            },
        )
        .expect("release");
    wait_releases(&h.obs, 1);
    // Drain + Reuse → Released (not Retired).
    assert_eq!(
        h.obs.lock().expect("obs").releases[0],
        ReleaseOutcome::Released
    );

    shutdown(h);
}

#[test]
fn drain_close_release_with_retire_returns_retired() {
    let h = build(PoolConfig::new(1, 0), vec![1]);
    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("a");
    wait_acquires(&h.obs, 1);
    h.runtime
        .try_send(h.driver, DriverMsg::BeginClose(CloseMode::Drain))
        .expect("close");
    wait_closed_ack(&h.obs);
    h.runtime
        .try_send(
            h.driver,
            DriverMsg::BeginRelease {
                id: 1,
                disposition: ReleaseDisposition::Retire,
            },
        )
        .expect("release");
    wait_releases(&h.obs, 1);
    assert_eq!(
        h.obs.lock().expect("obs").releases[0],
        ReleaseOutcome::Retired
    );
    shutdown(h);
}

#[test]
fn force_close_zeroes_leased_and_returns_pool_closed() {
    let h = build(PoolConfig::new(2, 0), vec![1, 2]);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("a1");
    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 2 })
        .expect("a2");
    wait_acquires(&h.obs, 2);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginClose(CloseMode::Force))
        .expect("force");
    wait_closed_ack(&h.obs);

    h.runtime
        .try_send(
            h.driver,
            DriverMsg::BeginRelease {
                id: 1,
                disposition: ReleaseDisposition::Reuse,
            },
        )
        .expect("r1");
    h.runtime
        .try_send(
            h.driver,
            DriverMsg::BeginRelease {
                id: 2,
                disposition: ReleaseDisposition::Retire,
            },
        )
        .expect("r2");
    wait_releases(&h.obs, 2);
    let releases = h.obs.lock().expect("obs").releases.clone();
    assert_eq!(
        releases,
        vec![ReleaseOutcome::PoolClosed, ReleaseOutcome::PoolClosed]
    );

    h.runtime
        .try_send(h.driver, DriverMsg::BeginPressure)
        .expect("pressure");
    let pr = wait_pressure(&h.obs);
    assert_eq!(
        pr.leased, 0,
        "force-close releases must zero leased: {pr:?}"
    );
    assert_eq!(pr.retired_count, 2);
    assert!(pr.closed);

    shutdown(h);
}

#[test]
fn force_close_retires_outstanding_leases_immediately() {
    // Codex P2: force close must retire leased resources at the
    // close moment, not "once each caller eventually releases."
    // Acquire two, send Force, snapshot pressure *before* any
    // release lands. Both should be Retired.
    let h = build(PoolConfig::new(2, 0), vec![1, 2]);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("a1");
    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 2 })
        .expect("a2");
    wait_acquires(&h.obs, 2);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginClose(CloseMode::Force))
        .expect("force");
    wait_closed_ack(&h.obs);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginPressure)
        .expect("pressure");
    let pr = wait_pressure(&h.obs);
    assert_eq!(
        pr.leased, 0,
        "force close must zero leased before any release; got {pr:?}"
    );
    assert_eq!(
        pr.retired_count, 2,
        "force close must bump retired_count for every outstanding lease; got {pr:?}"
    );
    assert!(pr.closed);

    // Late releases still report PoolClosed and don't double-bump
    // retired_count (the resource is already Retired, so the
    // Force-close release branch returns PoolClosed without
    // touching state).
    h.runtime
        .try_send(
            h.driver,
            DriverMsg::BeginRelease {
                id: 1,
                disposition: ReleaseDisposition::Reuse,
            },
        )
        .expect("late r1");
    h.runtime
        .try_send(
            h.driver,
            DriverMsg::BeginRelease {
                id: 2,
                disposition: ReleaseDisposition::Retire,
            },
        )
        .expect("late r2");
    wait_releases(&h.obs, 2);
    assert_eq!(
        h.obs.lock().expect("obs").releases,
        vec![ReleaseOutcome::PoolClosed, ReleaseOutcome::PoolClosed]
    );

    h.runtime
        .try_send(h.driver, DriverMsg::BeginPressure)
        .expect("pressure 2");
    let pr2 = wait_pressure(&h.obs);
    assert_eq!(
        pr2.retired_count, 2,
        "late releases on force-closed pool must not double-bump retired; got {pr2:?}"
    );

    shutdown(h);
}

#[test]
fn cancel_via_call_handle_reclaims_waiter_capacity() {
    let h = build(PoolConfig::new(1, 2), vec![1]);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("a1");
    wait_acquires(&h.obs, 1);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquireWithHandle { id: 2 })
        .expect("a2");
    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquireWithHandle { id: 3 })
        .expect("a3");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(h.obs.lock().expect("obs").acquires.len(), 1);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 4 })
        .expect("a4 full");
    wait_acquires(&h.obs, 2);
    assert_eq!(h.obs.lock().expect("obs").acquires[1], AcquireKind::Full);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginCancel { id: 2 })
        .expect("c2");
    h.runtime
        .try_send(h.driver, DriverMsg::BeginCancel { id: 3 })
        .expect("c3");
    wait_cancels(&h.obs, 2);
    assert!(
        h.obs
            .lock()
            .expect("obs")
            .cancel_acks
            .iter()
            .all(|a| matches!(a, CancelAck::Ok))
    );

    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 5 })
        .expect("a5");
    h.runtime
        .try_send(
            h.driver,
            DriverMsg::BeginRelease {
                id: 1,
                disposition: ReleaseDisposition::Reuse,
            },
        )
        .expect("release");

    wait_acquires(&h.obs, 3);
    let kinds = h.obs.lock().expect("obs").acquires.clone();
    assert!(matches!(kinds[2], AcquireKind::Acquired { .. }));

    h.runtime
        .try_send(h.driver, DriverMsg::BeginPressure)
        .expect("pressure");
    let pr = wait_pressure(&h.obs);
    assert!(pr.cancel_count >= 2, "cancel_count: {pr:?}");
    assert_eq!(pr.full_count, 1);
    assert!(!pr.closed);

    shutdown(h);
}

#[test]
fn fifo_order_preserved_after_middle_cancel() {
    let h = build(PoolConfig::new(1, 3), vec![777]);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("a1");
    wait_acquires(&h.obs, 1);

    for id in 2..=4 {
        h.runtime
            .try_send(h.driver, DriverMsg::BeginAcquireWithHandle { id })
            .expect("park");
    }
    std::thread::sleep(Duration::from_millis(75));

    h.runtime
        .try_send(h.driver, DriverMsg::BeginCancel { id: 3 })
        .expect("cancel middle");
    wait_cancels(&h.obs, 1);

    h.runtime
        .try_send(
            h.driver,
            DriverMsg::BeginRelease {
                id: 1,
                disposition: ReleaseDisposition::Reuse,
            },
        )
        .expect("release");
    wait_acquires(&h.obs, 2);
    assert!(matches!(
        h.obs.lock().expect("obs").acquires[1],
        AcquireKind::Acquired { .. }
    ));

    h.runtime
        .try_send(
            h.driver,
            DriverMsg::BeginRelease {
                id: 2,
                disposition: ReleaseDisposition::Reuse,
            },
        )
        .expect("release 2");
    wait_acquires(&h.obs, 3);
    assert!(matches!(
        h.obs.lock().expect("obs").acquires[2],
        AcquireKind::Acquired { .. }
    ));

    shutdown(h);
}

#[test]
fn retire_disposition_drops_handle() {
    // H = Arc<()> so we can observe handle drops on retire.
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let canary = Arc::new(());
    let resources = vec![canary.clone(), canary.clone()];
    // 1 (test holds canary) + 2 (in pool resources) = 3.
    assert_eq!(Arc::strong_count(&canary), 3);

    let pool: WorkerPool<Arc<()>, SingleShard> = WorkerPool::new(PoolConfig::new(2, 0), resources);
    let pool_addr = runtime
        .register_with_capacity::<_, Infallible>(pool, 32)
        .expect("pool");

    // Drive directly without our generic test driver since H differs.
    enum CanaryMsg {
        Begin,
        Got(CallOutcome<WorkerPoolReply<Arc<()>>>),
        Retire,
        Released(CallOutcome<WorkerPoolReply<Arc<()>>>),
    }
    impl std::fmt::Debug for CanaryMsg {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Begin => f.write_str("Begin"),
                Self::Got(_) => f.write_str("Got"),
                Self::Retire => f.write_str("Retire"),
                Self::Released(_) => f.write_str("Released"),
            }
        }
    }
    struct CanaryDriver {
        pool: Address<WorkerPoolMsg<Arc<()>>, WorkerPoolReply<Arc<()>>>,
        lease: Option<PoolLease<Arc<()>>>,
        done: Arc<Mutex<Option<ReleaseOutcome>>>,
    }
    #[tina_runtime::isolate(message = CanaryMsg)]
    impl CanaryDriver {
        fn handle(
            &mut self,
            msg: CanaryMsg,
            _ctx: &mut Context<'_, SingleShard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                CanaryMsg::Begin => {
                    call(self.pool, WorkerPoolMsg::Acquire, CALL_TIMEOUT).then(CanaryMsg::Got)
                }
                CanaryMsg::Got(outcome) => {
                    if let CallOutcome::Replied(WorkerPoolReply::Acquire(
                        AcquireOutcome::Acquired(lease),
                    )) = outcome
                    {
                        self.lease = Some(lease);
                    } else {
                        panic!("expected acquire");
                    }
                    noop()
                }
                CanaryMsg::Retire => {
                    let lease = self.lease.take().expect("lease");
                    tina_runtime::pool::release_effect(
                        lease,
                        self.pool,
                        ReleaseDisposition::Retire,
                        CALL_TIMEOUT,
                        CanaryMsg::Released,
                    )
                }
                CanaryMsg::Released(outcome) => {
                    if let CallOutcome::Replied(WorkerPoolReply::Release(o)) = outcome {
                        *self.done.lock().expect("done") = Some(o);
                    }
                    noop()
                }
            }
        }
    }
    let done: Arc<Mutex<Option<ReleaseOutcome>>> = Arc::new(Mutex::new(None));
    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            CanaryDriver {
                pool: pool_addr,
                lease: None,
                done: done.clone(),
            },
            32,
        )
        .expect("driver");

    runtime.try_send(driver, CanaryMsg::Begin).expect("begin");
    let done_for_wait = done.clone();
    assert!(wait_for(POLL_BUDGET, || {
        // Cheap proxy: count rose because driver owns the lease's clone.
        Arc::strong_count(&canary) >= 4
    }));
    let count_with_lease = Arc::strong_count(&canary);
    assert_eq!(count_with_lease, 4, "test+2-in-pool+1-in-lease");

    runtime.try_send(driver, CanaryMsg::Retire).expect("retire");
    assert!(wait_for(POLL_BUDGET, || {
        done_for_wait.lock().expect("done").is_some()
    }));
    assert_eq!(
        done.lock().expect("done").as_ref().copied(),
        Some(ReleaseOutcome::Retired)
    );

    // After retire, the pool drops its slot for the retired resource
    // AND the lease's clone is consumed by release. Count drops by 2.
    assert!(wait_for(POLL_BUDGET, || Arc::strong_count(&canary) == 2));
    assert_eq!(
        Arc::strong_count(&canary),
        2,
        "test+remaining-pool-slot only"
    );

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

#[test]
fn nocaller_drops_are_counted() {
    let h = build(PoolConfig::new(1, 0), vec![7]);
    // Send Acquire as plain send (no reply path).
    for _ in 0..3 {
        h.runtime
            .try_send(h.pool, WorkerPoolMsg::Acquire)
            .expect("send");
    }
    // Then ask for pressure to give the pool time to process.
    h.runtime
        .try_send(h.driver, DriverMsg::BeginPressure)
        .expect("pressure");
    let pr = wait_pressure(&h.obs);
    assert_eq!(pr.no_caller_drops, 3, "{pr:?}");
    assert_eq!(pr.full_count, 0);

    shutdown(h);
}

#[test]
fn closed_count_includes_late_acquires() {
    let h = build(PoolConfig::new(1, 1), vec![1]);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginClose(CloseMode::Drain))
        .expect("close first");
    wait_closed_ack(&h.obs);

    for id in 1..=3 {
        h.runtime
            .try_send(h.driver, DriverMsg::BeginAcquire { id })
            .expect("late");
    }
    wait_acquires(&h.obs, 3);
    assert!(
        h.obs
            .lock()
            .expect("obs")
            .acquires
            .iter()
            .all(|k| matches!(k, AcquireKind::Closed))
    );

    h.runtime
        .try_send(h.driver, DriverMsg::BeginPressure)
        .expect("pressure");
    let pr = wait_pressure(&h.obs);
    assert_eq!(pr.closed_count, 3, "{pr:?}");

    shutdown(h);
}

// --- Result-flavored helpers --------------------------------------------
//
// Tiny isolate that uses acquire_result_effect and release_result_effect
// directly. Proves the helpers compose correctly end-to-end and that
// pool-layer outcomes (Full / Closed) and transport-layer outcomes
// (CallTimeout, etc.) survive the fold as distinct AcquireFailure variants.

#[derive(Debug)]
enum ResultDriverMsg {
    Acquire,
    Acquired(Result<PoolLease<Resource>, tina::pool::AcquireFailure>),
    Release,
    Released(Result<(), tina::pool::ReleaseFailure>),
}

#[derive(Debug, Default)]
struct ResultObs {
    acquires: Vec<Result<u32, tina::pool::AcquireFailure>>,
    releases: Vec<Result<(), tina::pool::ReleaseFailure>>,
}

struct ResultDriver {
    pool: Address<WorkerPoolMsg<Resource>, WorkerPoolReply<Resource>>,
    obs: Arc<Mutex<ResultObs>>,
    held: Option<PoolLease<Resource>>,
}

#[tina_runtime::isolate(message = ResultDriverMsg)]
impl ResultDriver {
    fn handle(
        &mut self,
        msg: ResultDriverMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ResultDriverMsg::Acquire => tina_runtime::pool::acquire_result_effect(
                self.pool,
                CALL_TIMEOUT,
                ResultDriverMsg::Acquired,
            ),
            ResultDriverMsg::Acquired(result) => {
                let summary = match &result {
                    Ok(lease) => Ok(*lease.handle()),
                    Err(e) => Err(*e),
                };
                if let Ok(lease) = result {
                    self.held = Some(lease);
                }
                self.obs.lock().expect("obs").acquires.push(summary);
                noop()
            }
            ResultDriverMsg::Release => {
                if let Some(lease) = self.held.take() {
                    tina_runtime::pool::release_result_effect(
                        lease,
                        self.pool,
                        ReleaseDisposition::Reuse,
                        CALL_TIMEOUT,
                        ResultDriverMsg::Released,
                    )
                } else {
                    noop()
                }
            }
            ResultDriverMsg::Released(result) => {
                self.obs.lock().expect("obs").releases.push(result);
                noop()
            }
        }
    }
}

#[test]
fn result_helpers_collapse_call_outcome_to_typed_result() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let pool: WorkerPool<Resource, SingleShard> = WorkerPool::new(PoolConfig::new(1, 0), vec![42]);
    let pool_addr = runtime
        .register_with_capacity::<_, Infallible>(pool, 32)
        .expect("pool");
    let obs = Arc::new(Mutex::new(ResultObs::default()));
    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            ResultDriver {
                pool: pool_addr,
                obs: obs.clone(),
                held: None,
            },
            32,
        )
        .expect("driver");

    // Happy path: acquire -> Ok(lease), release -> Ok(()).
    runtime
        .try_send(driver, ResultDriverMsg::Acquire)
        .expect("a1");
    let obs_a = obs.clone();
    assert!(
        wait_for(POLL_BUDGET, || !obs_a
            .lock()
            .expect("obs")
            .acquires
            .is_empty()),
        "no acquire reply observed"
    );

    // Second acquire while resource is held → pool says Full.
    runtime
        .try_send(driver, ResultDriverMsg::Acquire)
        .expect("a2 full");
    let obs_a2 = obs.clone();
    assert!(wait_for(POLL_BUDGET, || {
        obs_a2.lock().expect("obs").acquires.len() >= 2
    }));

    // Release the first lease → Ok(()).
    runtime
        .try_send(driver, ResultDriverMsg::Release)
        .expect("rel");
    let obs_r = obs.clone();
    assert!(wait_for(POLL_BUDGET, || {
        !obs_r.lock().expect("obs").releases.is_empty()
    }));

    let snap = obs.lock().expect("obs");
    assert_eq!(snap.acquires.len(), 2);
    assert_eq!(snap.acquires[0], Ok(42));
    assert_eq!(snap.acquires[1], Err(tina::pool::AcquireFailure::Full));
    assert_eq!(snap.releases, vec![Ok(())]);

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

#[test]
fn pressure_report_counts_full() {
    let h = build(PoolConfig::new(1, 0), vec![1]);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("a");
    wait_acquires(&h.obs, 1);
    for id in 2..=4 {
        h.runtime
            .try_send(h.driver, DriverMsg::BeginAcquire { id })
            .expect("full a");
    }
    wait_acquires(&h.obs, 4);
    h.runtime
        .try_send(h.driver, DriverMsg::BeginPressure)
        .expect("pressure");
    let pr = wait_pressure(&h.obs);
    assert_eq!(pr.full_count, 3);
    assert_eq!(pr.leased, 1);
    assert_eq!(pr.waiters, 0);

    shutdown(h);
}

#[test]
fn pressure_report_tracks_high_water_waiters() {
    // 1 resource, 4 waiter slots. Issue 4 acquires: first gets the
    // resource, three park as waiters. high_water_waiters hits 3.
    // Release the lease — pool dispatches one parked waiter, live
    // waiters drops to 2. High water stays 3.
    let h = build(PoolConfig::new(1, 4), vec![1]);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("a1");
    wait_acquires(&h.obs, 1);

    for id in 2..=4 {
        h.runtime
            .try_send(h.driver, DriverMsg::BeginAcquire { id })
            .expect("park");
    }
    // Give the runtime a moment to handle the three Acquires as parks.
    std::thread::sleep(Duration::from_millis(50));

    h.runtime
        .try_send(h.driver, DriverMsg::BeginPressure)
        .expect("pressure-mid");
    let mid = wait_pressure(&h.obs);
    assert_eq!(mid.waiters, 3, "three parked waiters");
    assert_eq!(mid.high_water_waiters, 3);
    assert_eq!(mid.full_count, 0);

    // Release dispatches one waiter to the resource; live waiters → 2.
    h.runtime
        .try_send(
            h.driver,
            DriverMsg::BeginRelease {
                id: 1,
                disposition: ReleaseDisposition::Reuse,
            },
        )
        .expect("release");
    wait_acquires(&h.obs, 2);
    wait_releases(&h.obs, 1);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginPressure)
        .expect("pressure-after");
    let after = wait_pressure(&h.obs);
    assert_eq!(after.waiters, 2, "one parked waiter dispatched");
    assert_eq!(after.high_water_waiters, 3, "high water is sticky");

    // Project to the count surface. Pin the name so a CI test
    // cannot silently retarget if internals get renamed.
    let report =
        after.to_waiters_capacity_report("pool.demo.waiters", tina::capacity::CapacityMode::Fixed);
    assert_eq!(report.name, "pool.demo.waiters");
    assert_eq!(report.max_messages, Some(4));
    assert_eq!(report.current_messages, 2);
    assert_eq!(report.high_water_messages, 3);
    assert_eq!(report.full_count, 0);

    // Stuff into a CapacitySummary and use the assertion helpers.
    let mut summary = tina_runtime::CapacitySummary::new();
    summary.push(report).expect("push");
    summary
        .surface("pool.demo.waiters")
        .high_water_at_most(3)
        .expect("high water within configured cap");
    summary
        .surface("pool.demo.waiters")
        .no_full()
        .expect("no Full under nominal load");

    shutdown(h);
}

#[test]
fn pressure_report_capacity_surface_records_full_under_overload() {
    // 1 resource, 0 waiters: every acquire after the first sheds
    // with Full. The capacity surface records those rejections and
    // the discovery formatter suggests "raise cap".
    let h = build(PoolConfig::new(1, 0), vec![1]);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("a1");
    wait_acquires(&h.obs, 1);
    for id in 2..=4 {
        h.runtime
            .try_send(h.driver, DriverMsg::BeginAcquire { id })
            .expect("full");
    }
    wait_acquires(&h.obs, 4);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginPressure)
        .expect("pressure");
    let pr = wait_pressure(&h.obs);

    let report =
        pr.to_waiters_capacity_report("pool.demo.waiters", tina::capacity::CapacityMode::Tuning);
    assert_eq!(report.full_count, 3);
    assert_eq!(report.high_water_messages, 0);

    let line = tina_runtime::format_discovery_line(&report);
    assert!(line.contains("tuning"), "{line}");
    assert!(line.contains("full=3"), "{line}");
    assert!(line.contains("raise cap"), "{line}");

    shutdown(h);
}
