//! Deterministic simulator parity for `tina_runtime::pool::WorkerPool`.
//!
//! The threaded-runtime tests live in `tina-runtime/tests/pool.rs`.
//! These mirror the load-bearing behaviours under the simulator so the
//! pool's lifecycle, FIFO order, and cancellation back-channel are
//! provable without thread scheduling.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::pool::{
    AcquireOutcome, CloseMode, PoolConfig, PoolLease, PoolPressureReport, ReleaseDisposition,
    ReleaseOutcome,
};
use tina::prelude::*;
use tina_runtime::pool::{WorkerPool, WorkerPoolMsg, WorkerPoolReply};
use tina_runtime::{CallOutcome, call, call_with_handle, cancel_call};
use tina_sim::{Simulator, SimulatorConfig};

const CALL_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
struct SimShard;

impl Shard for SimShard {
    fn id(&self) -> ShardId {
        ShardId::new(0)
    }
}

type Resource = u32;

#[derive(Debug, Default)]
struct Observations {
    acquires: Vec<AcquireKind>,
    releases: Vec<ReleaseOutcome>,
    cancel_acks: Vec<tina::CancelOutcome>,
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

#[tina_runtime::isolate(message = DriverMsg, shard = SimShard)]
impl Driver {
    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, SimShard, Self::Reply>,
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
                self.obs.lock().expect("obs").cancel_acks.push(outcome);
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

fn drain(sim: &mut Simulator<SimShard>) {
    while sim.step() > 0 {}
}

struct Sim {
    sim: Simulator<SimShard>,
    driver: Address<DriverMsg>,
    obs: Arc<Mutex<Observations>>,
}

fn build(config: PoolConfig, resources: Vec<Resource>) -> Sim {
    let mut sim = Simulator::new(SimShard, SimulatorConfig::default());
    let pool: WorkerPool<Resource, SimShard> = WorkerPool::new(config, resources);
    let pool_addr = sim.register_with_mailbox_capacity(pool, 64);
    let obs = Arc::new(Mutex::new(Observations::default()));
    let driver = sim.register_with_mailbox_capacity(
        Driver {
            pool: pool_addr,
            obs: obs.clone(),
            leases: Vec::new(),
            handles: Vec::new(),
        },
        64,
    );
    Sim { sim, driver, obs }
}

// --- Tests ---------------------------------------------------------------

#[test]
fn dst_immediate_acquire_and_release_round_trip() {
    let mut s = build(PoolConfig::new(1, 0), vec![42]);
    s.sim
        .try_send(s.driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("a");
    drain(&mut s.sim);
    let kinds = s.obs.lock().expect("obs").acquires.clone();
    assert!(matches!(
        kinds[0],
        AcquireKind::Acquired { resource: 42, .. }
    ));

    s.sim
        .try_send(
            s.driver,
            DriverMsg::BeginRelease {
                id: 1,
                disposition: ReleaseDisposition::Reuse,
            },
        )
        .expect("r");
    drain(&mut s.sim);
    assert_eq!(
        s.obs.lock().expect("obs").releases,
        vec![ReleaseOutcome::Released]
    );
}

#[test]
fn dst_cancel_reclaims_then_admits_new_waiter() {
    // Same load-bearing test as the threaded version, run
    // deterministically: fill waiters, cancel all via cancel_call,
    // verify a new acquire is admitted (not Full).
    let mut s = build(PoolConfig::new(1, 2), vec![1]);
    s.sim
        .try_send(s.driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("a1");
    drain(&mut s.sim);
    s.sim
        .try_send(s.driver, DriverMsg::BeginAcquireWithHandle { id: 2 })
        .expect("a2");
    drain(&mut s.sim);
    s.sim
        .try_send(s.driver, DriverMsg::BeginAcquireWithHandle { id: 3 })
        .expect("a3");
    drain(&mut s.sim);
    s.sim
        .try_send(s.driver, DriverMsg::BeginAcquire { id: 4 })
        .expect("a4 should be Full");
    drain(&mut s.sim);

    let acquires = s.obs.lock().expect("obs").acquires.clone();
    assert_eq!(acquires.len(), 2, "{acquires:?}");
    assert!(matches!(acquires[0], AcquireKind::Acquired { .. }));
    assert_eq!(acquires[1], AcquireKind::Full);

    s.sim
        .try_send(s.driver, DriverMsg::BeginCancel { id: 2 })
        .expect("cancel 2");
    s.sim
        .try_send(s.driver, DriverMsg::BeginCancel { id: 3 })
        .expect("cancel 3");
    drain(&mut s.sim);
    assert!(
        s.obs
            .lock()
            .expect("obs")
            .cancel_acks
            .iter()
            .all(|a| matches!(a, tina::CancelOutcome::Cancelled))
    );

    s.sim
        .try_send(s.driver, DriverMsg::BeginAcquire { id: 5 })
        .expect("a5 admitted");
    s.sim
        .try_send(
            s.driver,
            DriverMsg::BeginRelease {
                id: 1,
                disposition: ReleaseDisposition::Reuse,
            },
        )
        .expect("release 1");
    drain(&mut s.sim);

    let final_acquires = s.obs.lock().expect("obs").acquires.clone();
    assert_eq!(final_acquires.len(), 3, "{final_acquires:?}");
    assert!(
        matches!(final_acquires[2], AcquireKind::Acquired { .. }),
        "id=5 should land Acquired after sweep+release; got {:?}",
        final_acquires[2]
    );

    s.sim
        .try_send(s.driver, DriverMsg::BeginPressure)
        .expect("p");
    drain(&mut s.sim);
    let pr = s.obs.lock().expect("obs").pressure.expect("pressure");
    assert!(pr.cancel_count >= 2);
    assert_eq!(pr.full_count, 1);
    assert!(!pr.closed);
}

#[test]
fn dst_fifo_order_after_middle_cancel() {
    let mut s = build(PoolConfig::new(1, 3), vec![777]);
    s.sim
        .try_send(s.driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("a1");
    drain(&mut s.sim);
    for id in 2..=4 {
        s.sim
            .try_send(s.driver, DriverMsg::BeginAcquireWithHandle { id })
            .expect("park");
    }
    drain(&mut s.sim);
    s.sim
        .try_send(s.driver, DriverMsg::BeginCancel { id: 3 })
        .expect("cancel middle");
    drain(&mut s.sim);

    s.sim
        .try_send(
            s.driver,
            DriverMsg::BeginRelease {
                id: 1,
                disposition: ReleaseDisposition::Reuse,
            },
        )
        .expect("release 1");
    drain(&mut s.sim);
    let kinds = s.obs.lock().expect("obs").acquires.clone();
    assert!(matches!(kinds[1], AcquireKind::Acquired { .. }));

    s.sim
        .try_send(
            s.driver,
            DriverMsg::BeginRelease {
                id: 2,
                disposition: ReleaseDisposition::Reuse,
            },
        )
        .expect("release 2");
    drain(&mut s.sim);
    let kinds = s.obs.lock().expect("obs").acquires.clone();
    assert_eq!(kinds.len(), 3);
    assert!(matches!(kinds[2], AcquireKind::Acquired { .. }));
}

#[test]
fn dst_drain_close_settles_waiters_and_keeps_release() {
    let mut s = build(PoolConfig::new(1, 3), vec![1]);
    s.sim
        .try_send(s.driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("a1");
    drain(&mut s.sim);
    for id in 2..=4 {
        s.sim
            .try_send(s.driver, DriverMsg::BeginAcquire { id })
            .expect("park");
    }
    drain(&mut s.sim);
    s.sim
        .try_send(s.driver, DriverMsg::BeginClose(CloseMode::Drain))
        .expect("close");
    drain(&mut s.sim);
    let kinds = s.obs.lock().expect("obs").acquires.clone();
    let closed = kinds
        .iter()
        .filter(|k| matches!(k, AcquireKind::Closed))
        .count();
    assert_eq!(closed, 3);

    s.sim
        .try_send(
            s.driver,
            DriverMsg::BeginRelease {
                id: 1,
                disposition: ReleaseDisposition::Reuse,
            },
        )
        .expect("release in drain");
    drain(&mut s.sim);
    assert_eq!(
        s.obs.lock().expect("obs").releases,
        vec![ReleaseOutcome::Released]
    );
}

#[test]
fn dst_force_close_retires_outstanding_leases_immediately() {
    let mut s = build(PoolConfig::new(2, 0), vec![1, 2]);
    s.sim
        .try_send(s.driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("a1");
    s.sim
        .try_send(s.driver, DriverMsg::BeginAcquire { id: 2 })
        .expect("a2");
    drain(&mut s.sim);

    s.sim
        .try_send(s.driver, DriverMsg::BeginClose(CloseMode::Force))
        .expect("force");
    drain(&mut s.sim);

    // Pressure before any release: leased must already be 0.
    s.sim
        .try_send(s.driver, DriverMsg::BeginPressure)
        .expect("p");
    drain(&mut s.sim);
    let pr = s.obs.lock().expect("obs").pressure.expect("pressure");
    assert_eq!(pr.leased, 0, "force close must zero leased: {pr:?}");
    assert_eq!(pr.retired_count, 2);
    assert!(pr.closed);
}

#[test]
fn dst_force_close_zeroes_leased_on_release() {
    let mut s = build(PoolConfig::new(2, 0), vec![1, 2]);
    s.sim
        .try_send(s.driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("a1");
    s.sim
        .try_send(s.driver, DriverMsg::BeginAcquire { id: 2 })
        .expect("a2");
    drain(&mut s.sim);

    s.sim
        .try_send(s.driver, DriverMsg::BeginClose(CloseMode::Force))
        .expect("force");
    drain(&mut s.sim);

    s.sim
        .try_send(
            s.driver,
            DriverMsg::BeginRelease {
                id: 1,
                disposition: ReleaseDisposition::Reuse,
            },
        )
        .expect("r1");
    s.sim
        .try_send(
            s.driver,
            DriverMsg::BeginRelease {
                id: 2,
                disposition: ReleaseDisposition::Retire,
            },
        )
        .expect("r2");
    drain(&mut s.sim);

    assert_eq!(
        s.obs.lock().expect("obs").releases,
        vec![ReleaseOutcome::PoolClosed, ReleaseOutcome::PoolClosed]
    );

    s.sim
        .try_send(s.driver, DriverMsg::BeginPressure)
        .expect("p");
    drain(&mut s.sim);
    let pr = s.obs.lock().expect("obs").pressure.expect("pressure");
    assert_eq!(pr.leased, 0);
    assert_eq!(pr.retired_count, 2);
    assert!(pr.closed);
}

#[test]
fn dst_max_waiters_zero_sheds() {
    let mut s = build(PoolConfig::new(1, 0), vec![99]);
    s.sim
        .try_send(s.driver, DriverMsg::BeginAcquire { id: 1 })
        .expect("a1");
    drain(&mut s.sim);
    for id in 2..=5 {
        s.sim
            .try_send(s.driver, DriverMsg::BeginAcquire { id })
            .expect("shed");
    }
    drain(&mut s.sim);
    let kinds = s.obs.lock().expect("obs").acquires.clone();
    assert!(matches!(kinds[0], AcquireKind::Acquired { .. }));
    assert!(kinds[1..].iter().all(|k| matches!(k, AcquireKind::Full)));
}

#[test]
fn dst_force_can_upgrade_drain_but_not_reverse() {
    let mut s = build(PoolConfig::new(1, 0), vec![1]);
    s.sim
        .try_send(s.driver, DriverMsg::BeginClose(CloseMode::Drain))
        .expect("c1");
    drain(&mut s.sim);
    s.sim
        .try_send(s.driver, DriverMsg::BeginClose(CloseMode::Force))
        .expect("c2 upgrades");
    drain(&mut s.sim);
    s.sim
        .try_send(s.driver, DriverMsg::BeginClose(CloseMode::Drain))
        .expect("c3 ignored");
    drain(&mut s.sim);
    s.sim
        .try_send(s.driver, DriverMsg::BeginPressure)
        .expect("p");
    drain(&mut s.sim);
    let pr = s.obs.lock().expect("obs").pressure.expect("pressure");
    assert!(pr.closed);
    // closed_count: 0 since no waiters were parked.
    assert_eq!(pr.closed_count, 0);
}
