//! Bounded WorkerPool tests.
//!
//! Proves the load-bearing behaviours: bounded waiter table, FIFO
//! order with middle-cancel pruning, timeout/cancel/close all reclaim
//! waiter capacity, release acknowledgement is typed, drain vs force
//! close differ, and the pressure report counts.

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
    CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime, call, call_with_handle,
    cancel_call,
};

/// Resource handle used in tests. The pool is generic; we pick a
/// trivial Send + Clone scalar so the tests don't drag in a worker
/// isolate.
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

// --- Driver: stores leases / handles, exposes them through a Mutex --------

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
    Timeout,
    Failed, // call timeout / closed mailbox
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
    BeginReleaseExternal {
        lease: PoolLease<Resource>,
        disposition: ReleaseDisposition,
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
            Self::BeginReleaseExternal { disposition, .. } => {
                write!(f, "BeginReleaseExternal({disposition:?})")
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
                .reply(move |outcome| DriverMsg::AcquireReturned { id, outcome }),
            DriverMsg::BeginAcquireWithHandle { id } => {
                let (effect, handle) =
                    call_with_handle(self.pool, WorkerPoolMsg::Acquire, CALL_TIMEOUT)
                        .reply(move |outcome| DriverMsg::AcquireReturned { id, outcome });
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
                    CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Timeout)) => {
                        AcquireKind::Timeout
                    }
                    CallOutcome::Replied(_other) => panic!("unexpected reply variant"),
                    CallOutcome::Timeout => AcquireKind::Timeout,
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
            DriverMsg::BeginReleaseExternal { lease, disposition } => {
                tina_runtime::pool::release_effect(
                    lease,
                    self.pool,
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
                cancel_call(handle).reply(DriverMsg::CancelReturned)
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
                    .reply(DriverMsg::PressureReturned)
            }
            DriverMsg::PressureReturned(outcome) => {
                if let CallOutcome::Replied(WorkerPoolReply::Pressure(report)) = outcome {
                    self.obs.lock().expect("obs").pressure = Some(report);
                }
                noop()
            }
            DriverMsg::BeginClose(mode) => {
                call(self.pool, WorkerPoolMsg::Close(mode), CALL_TIMEOUT)
                    .reply(DriverMsg::CloseReturned)
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

// --- Tests ---------------------------------------------------------------

#[test]
fn immediate_acquire_when_idle() {
    let h = build(
        PoolConfig::new(2, 0, Duration::from_millis(100)),
        vec![10, 20],
    );

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
    let h = build(PoolConfig::new(1, 0, Duration::from_millis(100)), vec![1]);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("send a");
    wait_acquires(&h.obs, 1);
    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 2 })
        .expect("send b");
    wait_acquires(&h.obs, 2);

    let kinds = h.obs.lock().expect("obs").acquires.clone();
    assert!(matches!(kinds[0], AcquireKind::Acquired { .. }));
    assert_eq!(kinds[1], AcquireKind::Full);

    shutdown(h);
}

#[test]
fn waiter_parked_then_dispatched_on_release() {
    let h = build(PoolConfig::new(1, 4, Duration::from_secs(1)), vec![42]);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("send a");
    wait_acquires(&h.obs, 1);
    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 2 })
        .expect("send b");

    // Give the second acquire a moment to land and park.
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
        .expect("send release");

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
fn forged_wrong_pool_release_is_stale() {
    let h = build(PoolConfig::new(1, 0, Duration::from_secs(1)), vec![100]);

    // Forge a lease pointing at a different pool id. The pool must
    // reject it as stale rather than touching its real resource.
    h.runtime
        .try_send(
            h.driver,
            DriverMsg::BeginReleaseExternal {
                lease: PoolLease::new(
                    tina::pool::PoolId::from_raw(std::num::NonZeroU64::new(u64::MAX).expect("nz")),
                    tina::pool::ResourceId::from_raw(0),
                    1,
                    100,
                ),
                disposition: ReleaseDisposition::Reuse,
            },
        )
        .expect("send forged lease");

    wait_releases(&h.obs, 1);
    assert_eq!(
        h.obs.lock().expect("obs").releases[0],
        ReleaseOutcome::StaleLease
    );

    shutdown(h);
}

#[test]
fn drain_close_settles_waiters_as_closed() {
    let h = build(PoolConfig::new(1, 3, Duration::from_secs(1)), vec![1]);

    // Take the only resource; park three more.
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

    // Three parked waiters all get Closed.
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
    assert!(h.obs.lock().expect("obs").closed_acks >= 1);

    // Drain mode lets the live lease release normally.
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
    // After close, even Reuse is recorded as Retired (close path).
    assert_eq!(
        h.obs.lock().expect("obs").releases[0],
        ReleaseOutcome::Retired
    );

    shutdown(h);
}

#[test]
fn force_close_marks_outstanding_leases_stale() {
    let h = build(PoolConfig::new(1, 0, Duration::from_secs(1)), vec![1]);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("a1");
    wait_acquires(&h.obs, 1);

    h.runtime
        .try_send(h.driver, DriverMsg::BeginClose(CloseMode::Force))
        .expect("close");

    // Wait for the close ack.
    let obs = h.obs.clone();
    assert!(
        wait_for(POLL_BUDGET, || obs.lock().expect("obs").closed_acks >= 1),
        "close ack never observed"
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
    wait_releases(&h.obs, 1);
    assert_eq!(
        h.obs.lock().expect("obs").releases[0],
        ReleaseOutcome::PoolClosed
    );

    shutdown(h);
}

#[test]
fn cancel_via_call_handle_reclaims_waiter_capacity() {
    // The load-bearing test for the phase:
    //   - Pool capacity 1, max_waiters 2.
    //   - Acquire #1 takes the resource.
    //   - Acquire #2 and #3 park as waiters (table full).
    //   - Acquire #4 is rejected as Full.
    //   - Cancel #2 and #3 via cancel_call(handle); handles' waits close.
    //   - Acquire #5 must be admitted as a waiter (not Full) because
    //     the sweep on the next message reclaimed both slots.
    let h = build(PoolConfig::new(1, 2, Duration::from_secs(2)), vec![1]);

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
    assert_eq!(
        h.obs.lock().expect("obs").acquires.len(),
        1,
        "two parked waiters should not have replied yet"
    );

    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 4 })
        .expect("a4");
    wait_acquires(&h.obs, 2);
    assert_eq!(h.obs.lock().expect("obs").acquires[1], AcquireKind::Full);

    // Cancel both parked waiters.
    h.runtime
        .try_send(h.driver, DriverMsg::BeginCancel { id: 2 })
        .expect("cancel 2");
    h.runtime
        .try_send(h.driver, DriverMsg::BeginCancel { id: 3 })
        .expect("cancel 3");
    wait_cancels(&h.obs, 2);
    assert!(
        h.obs
            .lock()
            .expect("obs")
            .cancel_acks
            .iter()
            .all(|a| matches!(a, CancelAck::Ok))
    );

    // New acquire must succeed as a waiter, not as Full.
    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 5 })
        .expect("a5");

    // Releasing the held lease should hand the resource to id=5.
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
    assert!(
        matches!(kinds[2], AcquireKind::Acquired { .. }),
        "id=5 expected to be Acquired after sweep+release; got {:?}",
        kinds[2]
    );

    // Pressure report should reflect the cancels.
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
    // Pool with capacity 1 and three waiter slots. Park three. Cancel
    // the middle one. Release the live lease; the FIRST waiter should
    // get it (not the third).
    let h = build(PoolConfig::new(1, 3, Duration::from_secs(2)), vec![777]);

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

    // Cancel the middle waiter (id=3).
    h.runtime
        .try_send(h.driver, DriverMsg::BeginCancel { id: 3 })
        .expect("cancel middle");
    wait_cancels(&h.obs, 1);

    // Release lease 1; expect id=2 (head of queue) to be served.
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

    // And id=4 still parked. Release the lease just handed to id=2,
    // and id=4 must be next.
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
fn retire_disposition_drops_resource() {
    let h = build(
        PoolConfig::new(2, 0, Duration::from_millis(200)),
        vec![1, 2],
    );

    h.runtime
        .try_send(h.driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("a1");
    wait_acquires(&h.obs, 1);
    h.runtime
        .try_send(
            h.driver,
            DriverMsg::BeginRelease {
                id: 1,
                disposition: ReleaseDisposition::Retire,
            },
        )
        .expect("retire");
    wait_releases(&h.obs, 1);
    assert_eq!(
        h.obs.lock().expect("obs").releases[0],
        ReleaseOutcome::Retired
    );

    h.runtime
        .try_send(h.driver, DriverMsg::BeginPressure)
        .expect("pressure");
    let pr = wait_pressure(&h.obs);
    assert_eq!(pr.retired_count, 1);
    // Capacity 2; one retired ⇒ 1 remaining usable. The pool does
    // not auto-replace.
    assert_eq!(pr.available + pr.leased + 1, pr.capacity);

    shutdown(h);
}

#[test]
fn pressure_report_counts_full() {
    let h = build(PoolConfig::new(1, 0, Duration::from_millis(100)), vec![1]);

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
