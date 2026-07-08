//! WorkerPool resource-lifetime, health, and refill tests.
//!
//! User-shaped proofs for resource lifecycle maturity:
//!
//! - an idle resource retires and the report names *why*,
//! - a max-lifetime retire never hands the stale resource to a new
//!   caller,
//! - a caller health verdict retires a bad resource,
//! - a leased resource past max age is reported old, never stolen,
//! - fill / retire / refill proves capacity is reclaimed,
//! - a shutdown report names drain/force + leased counts.
//!
//! Time is logical: the pool is built at `t0` and every `Maintain { now }`
//! carries an explicit `now = t0 + delta`. No test sleeps for age.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tina::pool::{
    AcquireOutcome, CloseMode, PolicyCheckPoint, PoolConfig, PoolLease, PoolPressureReport,
    PoolShutdownReport, RefillOutcome, ReleaseDisposition, ReleaseOutcome, ResourceHealth,
    ResourceLifetime, ResourcePolicyReport, RetireReason,
};
use tina::prelude::*;
use tina_runtime::pool::{
    WorkerPool, WorkerPoolMsg, WorkerPoolReply, maintain_effect, refill_effect,
};
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime};

type Resource = u32;
type Rt = ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>;
type PoolAddr = Address<WorkerPoolMsg<Resource>, WorkerPoolReply<Resource>>;

const T: Duration = Duration::from_secs(5);

// Drives the `maintain_effect` / `refill_effect` sugar from inside an
// isolate (the effect helpers are isolate-side) and forwards each pool
// reply out a channel for the host to assert on.
enum HelperMsg {
    DoMaintain(Instant),
    DoRefill {
        id: u32,
        handle: Resource,
        now: Instant,
    },
    Done(CallOutcome<WorkerPoolReply<Resource>>),
}

struct HelperDriver {
    pool: PoolAddr,
    tx: std::sync::mpsc::Sender<WorkerPoolReply<Resource>>,
}

#[tina_runtime::isolate(message = HelperMsg)]
impl HelperDriver {
    fn handle(
        &mut self,
        msg: HelperMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            HelperMsg::DoMaintain(now) => maintain_effect(self.pool, now, T, HelperMsg::Done),
            HelperMsg::DoRefill { id, handle, now } => {
                refill_effect(self.pool, id, handle, now, T, HelperMsg::Done)
            }
            HelperMsg::Done(outcome) => {
                if let CallOutcome::Replied(reply) = outcome {
                    let _ = self.tx.send(reply);
                }
                noop()
            }
        }
    }
}

fn runtime() -> Arc<Rt> {
    Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ))
}

fn register(rt: &Rt, pool: WorkerPool<Resource, SingleShard>) -> PoolAddr {
    rt.register_with_capacity::<_, Infallible>(pool, 64)
        .expect("register pool")
}

fn reply(rt: &Rt, pool: PoolAddr, msg: WorkerPoolMsg<Resource>) -> WorkerPoolReply<Resource> {
    match rt.call_blocking(pool, msg, T).expect("call_blocking") {
        CallOutcome::Replied(r) => r,
        other => panic!("expected reply, got {other:?}"),
    }
}

fn acquire(rt: &Rt, pool: PoolAddr) -> AcquireOutcome<Resource> {
    match reply(rt, pool, WorkerPoolMsg::Acquire) {
        WorkerPoolReply::Acquire(outcome) => outcome,
        other => panic!("expected Acquire reply, got {other:?}"),
    }
}

fn acquire_lease(rt: &Rt, pool: PoolAddr) -> PoolLease<Resource> {
    match acquire(rt, pool) {
        AcquireOutcome::Acquired(lease) => lease,
        other => panic!("expected Acquired, got {other:?}"),
    }
}

fn release(
    rt: &Rt,
    pool: PoolAddr,
    lease: PoolLease<Resource>,
    disposition: ReleaseDisposition,
) -> ReleaseOutcome {
    match reply(rt, pool, WorkerPoolMsg::Release { lease, disposition }) {
        WorkerPoolReply::Release(outcome) => outcome,
        other => panic!("expected Release reply, got {other:?}"),
    }
}

fn maintain(rt: &Rt, pool: PoolAddr, now: Instant) -> ResourcePolicyReport {
    match reply(rt, pool, WorkerPoolMsg::Maintain { now }) {
        WorkerPoolReply::Maintenance(report) => report,
        other => panic!("expected Maintenance reply, got {other:?}"),
    }
}

fn refill(
    rt: &Rt,
    pool: PoolAddr,
    resource_id: u32,
    handle: Resource,
    now: Instant,
) -> RefillOutcome {
    match reply(
        rt,
        pool,
        WorkerPoolMsg::Refill {
            resource_id,
            handle,
            now,
        },
    ) {
        WorkerPoolReply::Refilled(outcome) => outcome,
        other => panic!("expected Refilled reply, got {other:?}"),
    }
}

fn pressure(rt: &Rt, pool: PoolAddr) -> PoolPressureReport {
    match reply(rt, pool, WorkerPoolMsg::PressureReport) {
        WorkerPoolReply::Pressure(report) => report,
        other => panic!("expected Pressure reply, got {other:?}"),
    }
}

fn close(rt: &Rt, pool: PoolAddr, mode: CloseMode) {
    match reply(rt, pool, WorkerPoolMsg::Close(mode)) {
        WorkerPoolReply::Closed => {}
        other => panic!("expected Closed reply, got {other:?}"),
    }
}

fn shutdown(rt: Arc<Rt>) {
    if let Ok(rt) = Arc::try_unwrap(rt) {
        let _ = rt.shutdown();
    }
}

#[test]
fn idle_retire_reports_why() {
    let rt = runtime();
    let t0 = Instant::now();
    let pool = register(
        &rt,
        WorkerPool::with_lifetime(
            PoolConfig::new(1, 0),
            vec![10],
            ResourceLifetime::idle_after(Duration::from_secs(5)),
            t0,
        ),
    );

    // Use the resource, then return it idle. Release resets the idle
    // clock; the first sweep re-stamps it.
    let lease = acquire_lease(&rt, pool);
    assert_eq!(
        release(&rt, pool, lease, ReleaseDisposition::Reuse),
        ReleaseOutcome::Released
    );

    // First sweep observes it idle and stamps the clock — nothing to
    // retire yet.
    let first = maintain(&rt, pool, t0 + Duration::from_secs(1));
    assert!(
        first.retired.is_empty(),
        "first sweep should stamp, not retire: {first:?}"
    );

    // Six seconds later it has been idle past the limit.
    let report = maintain(&rt, pool, t0 + Duration::from_secs(7));
    assert_eq!(report.retired.len(), 1, "{report:?}");
    let retired = report.retired[0];
    assert_eq!(retired.resource_id.get(), 0);
    assert_eq!(retired.reason, RetireReason::IdleTimeout);
    assert_eq!(retired.check_point, PolicyCheckPoint::ScheduledMaintenance);
    assert_eq!(report.available_after, 0);
    assert_eq!(report.retired_slots, 1);

    shutdown(rt);
}

#[test]
fn max_lifetime_retire_does_not_hand_stale_to_new_caller() {
    let rt = runtime();
    let t0 = Instant::now();
    let pool = register(
        &rt,
        WorkerPool::with_lifetime(
            PoolConfig::new(1, 0),
            vec![10],
            ResourceLifetime::max_age(Duration::from_secs(10)),
            t0,
        ),
    );

    // Borrow and return; the resource is idle again.
    let lease = acquire_lease(&rt, pool);
    assert_eq!(
        release(&rt, pool, lease, ReleaseDisposition::Reuse),
        ReleaseOutcome::Released
    );

    // Past max age. created_at is build time, so one sweep retires it.
    let report = maintain(&rt, pool, t0 + Duration::from_secs(11));
    assert_eq!(report.retired.len(), 1, "{report:?}");
    assert_eq!(report.retired[0].reason, RetireReason::MaxLifetime);

    // A new caller must NOT receive the stale resource. With no waiter
    // slots and the only resource retired, acquire sheds Full.
    assert!(matches!(acquire(&rt, pool), AcquireOutcome::Full));

    // The owner refills the slot with a fresh resource; now acquire
    // succeeds and hands out the fresh handle.
    assert_eq!(
        refill(&rt, pool, 0, 20, t0 + Duration::from_secs(11)),
        RefillOutcome::Refilled
    );
    let fresh = acquire_lease(&rt, pool);
    assert_eq!(*fresh.handle(), 20);
    assert_eq!(
        release(&rt, pool, fresh, ReleaseDisposition::Reuse),
        ReleaseOutcome::Released
    );

    shutdown(rt);
}

#[test]
fn never_leased_resource_retires_on_first_sweep() {
    let rt = runtime();
    let t0 = Instant::now();
    let pool = register(
        &rt,
        WorkerPool::with_lifetime(
            PoolConfig::new(1, 0),
            vec![10],
            ResourceLifetime::idle_after(Duration::from_secs(5)),
            t0,
        ),
    );

    // A resource idle since build (never leased) is stamped at build, so
    // a single sweep past the idle limit retires it — no warm-up sweep.
    let report = maintain(&rt, pool, t0 + Duration::from_secs(6));
    assert_eq!(report.retired.len(), 1, "{report:?}");
    assert_eq!(report.retired[0].reason, RetireReason::IdleTimeout);
    assert_eq!(report.available_after, 0);

    shutdown(rt);
}

#[test]
fn max_lifetime_takes_precedence_over_idle_in_reason() {
    let rt = runtime();
    let t0 = Instant::now();
    let pool = register(
        &rt,
        WorkerPool::with_lifetime(
            PoolConfig::new(1, 0),
            vec![10],
            ResourceLifetime::new(Some(Duration::from_secs(5)), Some(Duration::from_secs(10))),
            t0,
        ),
    );

    // At t0+12s the resource is past BOTH the idle (5s) and max-age (10s)
    // limits. Max age is the stronger reason and wins the report.
    let report = maintain(&rt, pool, t0 + Duration::from_secs(12));
    assert_eq!(report.retired.len(), 1, "{report:?}");
    assert_eq!(report.retired[0].reason, RetireReason::MaxLifetime);

    shutdown(rt);
}

#[test]
fn idle_only_policy_does_not_report_over_age_leased() {
    let rt = runtime();
    let t0 = Instant::now();
    let pool = register(
        &rt,
        WorkerPool::with_lifetime(
            PoolConfig::new(1, 0),
            vec![10],
            ResourceLifetime::idle_after(Duration::from_secs(5)),
            t0,
        ),
    );

    // Hold a lease far past any wall-clock age. With no max_lifetime,
    // leased resources are never flagged over-age — idle age does not
    // apply to a resource that is out on lease.
    let lease = acquire_lease(&rt, pool);
    let report = maintain(&rt, pool, t0 + Duration::from_secs(1000));
    assert!(report.over_age_leased.is_empty(), "{report:?}");
    assert!(report.retired.is_empty(), "{report:?}");
    assert_eq!(report.leased_after, 1);
    assert_eq!(
        release(&rt, pool, lease, ReleaseDisposition::Reuse),
        ReleaseOutcome::Released
    );

    shutdown(rt);
}

#[test]
fn multiple_resources_retired_in_one_sweep() {
    let rt = runtime();
    let t0 = Instant::now();
    let pool = register(
        &rt,
        WorkerPool::with_lifetime(
            PoolConfig::new(3, 0),
            vec![10, 11, 12],
            ResourceLifetime::idle_after(Duration::from_secs(5)),
            t0,
        ),
    );

    // All three idle since build; one sweep retires all of them and the
    // idle queue is pruned so none can be handed out afterward.
    let report = maintain(&rt, pool, t0 + Duration::from_secs(6));
    assert_eq!(report.retired.len(), 3, "{report:?}");
    assert_eq!(report.available_after, 0);
    assert_eq!(report.retired_slots, 3);
    assert!(
        report
            .retired
            .iter()
            .all(|r| r.reason == RetireReason::IdleTimeout)
    );
    let mut ids: Vec<u32> = report.retired.iter().map(|r| r.resource_id.get()).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![0, 1, 2]);
    assert!(matches!(acquire(&rt, pool), AcquireOutcome::Full));

    shutdown(rt);
}

#[test]
fn health_check_retires_bad_resource() {
    let rt = runtime();
    let pool = register(&rt, WorkerPool::new(PoolConfig::new(1, 0), vec![10]));

    let lease = acquire_lease(&rt, pool);

    // Caller probes the resource and decides it is bad. ResourceHealth
    // is the vocabulary; it maps onto the release disposition.
    let verdict = ResourceHealth::Retire;
    assert_eq!(verdict.disposition(), ReleaseDisposition::Retire);
    assert_eq!(
        release(&rt, pool, lease, verdict.disposition()),
        ReleaseOutcome::Retired
    );

    let p = pressure(&rt, pool);
    assert_eq!(p.retired_count, 1);
    assert_eq!(p.available, 0);
    assert_eq!(p.leased, 0);

    // A Healthy/Suspect verdict reuses instead of retiring.
    assert_eq!(
        ResourceHealth::Healthy.disposition(),
        ReleaseDisposition::Reuse
    );
    assert_eq!(
        ResourceHealth::Suspect.disposition(),
        ReleaseDisposition::Reuse
    );

    shutdown(rt);
}

#[test]
fn over_age_leased_is_reported_not_stolen() {
    let rt = runtime();
    let t0 = Instant::now();
    let pool = register(
        &rt,
        WorkerPool::with_lifetime(
            PoolConfig::new(1, 0),
            vec![10],
            ResourceLifetime::max_age(Duration::from_secs(10)),
            t0,
        ),
    );

    // Hold the lease across the sweep.
    let lease = acquire_lease(&rt, pool);

    let report = maintain(&rt, pool, t0 + Duration::from_secs(11));
    assert!(
        report.retired.is_empty(),
        "leased resource must not be retired: {report:?}"
    );
    assert_eq!(report.over_age_leased.len(), 1, "{report:?}");
    assert_eq!(report.over_age_leased[0].get(), 0);
    assert_eq!(report.leased_after, 1);

    // The lease is still valid — the sweep did not steal it.
    assert_eq!(
        release(&rt, pool, lease, ReleaseDisposition::Reuse),
        ReleaseOutcome::Released
    );

    shutdown(rt);
}

#[test]
fn fill_retire_refill_reclaims_capacity() {
    let rt = runtime();
    let pool = register(&rt, WorkerPool::new(PoolConfig::new(2, 0), vec![10, 11]));

    // Fill: both resources out on lease.
    let a = acquire_lease(&rt, pool);
    let b = acquire_lease(&rt, pool);
    assert_eq!(pressure(&rt, pool).leased, 2);

    // Retire one as unhealthy. Capacity is now effectively 1.
    let retired_id = a.resource_id().get();
    assert_eq!(
        release(&rt, pool, a, ReleaseDisposition::Retire),
        ReleaseOutcome::Retired
    );
    let p = pressure(&rt, pool);
    assert_eq!(p.leased, 1);
    assert_eq!(p.available, 0);
    // The dead slot does not consume admission forever, but capacity is
    // down until the owner refills.
    assert!(matches!(acquire(&rt, pool), AcquireOutcome::Full));

    // Refill the retired slot with a fresh resource: capacity reclaimed.
    let now = Instant::now();
    assert_eq!(
        refill(&rt, pool, retired_id, 99, now),
        RefillOutcome::Refilled
    );
    let c = acquire_lease(&rt, pool);
    assert_eq!(*c.handle(), 99);
    assert_eq!(pressure(&rt, pool).leased, 2);

    assert_eq!(
        release(&rt, pool, b, ReleaseDisposition::Reuse),
        ReleaseOutcome::Released
    );
    assert_eq!(
        release(&rt, pool, c, ReleaseDisposition::Reuse),
        ReleaseOutcome::Released
    );

    shutdown(rt);
}

#[test]
fn refill_keeps_generation_monotonic() {
    let rt = runtime();
    let pool = register(&rt, WorkerPool::new(PoolConfig::new(1, 0), vec![10]));

    // First life of the slot: lease at generation 1, then retire it.
    let lease = acquire_lease(&rt, pool);
    assert_eq!(lease.generation(), 1);
    let retired_id = lease.resource_id().get();
    assert_eq!(
        release(&rt, pool, lease, ReleaseDisposition::Retire),
        ReleaseOutcome::Retired
    );

    // Refilled slot must NOT reset to generation 1 — that would let a
    // stale generation-1 lease alias the reborn slot. The next lease
    // continues the monotonic counter.
    assert_eq!(
        refill(&rt, pool, retired_id, 20, Instant::now()),
        RefillOutcome::Refilled
    );
    let reborn = acquire_lease(&rt, pool);
    assert!(
        reborn.generation() > 1,
        "refilled slot reused generation {} — must stay monotonic",
        reborn.generation()
    );
    assert_eq!(
        release(&rt, pool, reborn, ReleaseDisposition::Reuse),
        ReleaseOutcome::Released
    );

    shutdown(rt);
}

#[test]
fn refill_resets_the_age_clock() {
    let rt = runtime();
    let t0 = Instant::now();
    let pool = register(
        &rt,
        WorkerPool::with_lifetime(
            PoolConfig::new(1, 0),
            vec![10],
            ResourceLifetime::max_age(Duration::from_secs(10)),
            t0,
        ),
    );

    // Original resource ages out and is retired.
    assert_eq!(
        maintain(&rt, pool, t0 + Duration::from_secs(11))
            .retired
            .len(),
        1
    );

    // Refill at t0+11s: the fresh resource's age starts there, not at t0.
    assert_eq!(
        refill(&rt, pool, 0, 20, t0 + Duration::from_secs(11)),
        RefillOutcome::Refilled
    );

    // 4s after refill it is still young — not retired.
    let young = maintain(&rt, pool, t0 + Duration::from_secs(15));
    assert!(
        young.retired.is_empty(),
        "refilled resource aged from t0, not refill: {young:?}"
    );
    assert_eq!(young.available_after, 1);

    // 11s after refill it is finally over max age again.
    let old = maintain(&rt, pool, t0 + Duration::from_secs(22));
    assert_eq!(old.retired.len(), 1, "{old:?}");
    assert_eq!(old.retired[0].reason, RetireReason::MaxLifetime);

    shutdown(rt);
}

#[test]
fn refill_rejects_live_unknown_and_closed() {
    let rt = runtime();
    let pool = register(&rt, WorkerPool::new(PoolConfig::new(2, 0), vec![10, 11]));
    let now = Instant::now();

    // Idle (live) slot: refusing keeps a live resource from being
    // silently replaced.
    assert_eq!(refill(&rt, pool, 0, 50, now), RefillOutcome::NotRetired);

    // Leased (live) slot: same.
    let lease = acquire_lease(&rt, pool);
    assert_eq!(
        refill(&rt, pool, lease.resource_id().get(), 50, now),
        RefillOutcome::NotRetired
    );

    // Out of range.
    assert_eq!(
        refill(&rt, pool, 99, 50, now),
        RefillOutcome::UnknownResource
    );

    // Closed pool refuses refills.
    let _ = release(&rt, pool, lease, ReleaseDisposition::Reuse);
    close(&rt, pool, CloseMode::Drain);
    assert_eq!(refill(&rt, pool, 0, 50, now), RefillOutcome::Closed);

    shutdown(rt);
}

#[test]
fn shutdown_report_drain_keeps_leased_visible() {
    let rt = runtime();
    let pool = register(&rt, WorkerPool::new(PoolConfig::new(2, 0), vec![10, 11]));

    let a = acquire_lease(&rt, pool);
    let b = acquire_lease(&rt, pool);

    close(&rt, pool, CloseMode::Drain);
    let p = pressure(&rt, pool);
    let report = PoolShutdownReport::from_pressure(CloseMode::Drain, &p);
    assert_eq!(report.mode, CloseMode::Drain);
    assert!(report.closed);
    assert_eq!(report.leased, 2);
    assert!(
        !report.drained(),
        "drain with leases out is not drained: {report:?}"
    );

    // Outstanding leases return normally under drain.
    assert_eq!(
        release(&rt, pool, a, ReleaseDisposition::Reuse),
        ReleaseOutcome::Released
    );
    assert_eq!(
        release(&rt, pool, b, ReleaseDisposition::Reuse),
        ReleaseOutcome::Released
    );

    let p = pressure(&rt, pool);
    let report = PoolShutdownReport::from_pressure(CloseMode::Drain, &p);
    assert_eq!(report.leased, 0);
    assert!(report.drained());

    shutdown(rt);
}

#[test]
fn shutdown_report_force_retires_and_counts() {
    let rt = runtime();
    let pool = register(&rt, WorkerPool::new(PoolConfig::new(2, 0), vec![10, 11]));

    let _a = acquire_lease(&rt, pool);
    let _b = acquire_lease(&rt, pool);

    close(&rt, pool, CloseMode::Force);
    let p = pressure(&rt, pool);
    let report = PoolShutdownReport::from_pressure(CloseMode::Force, &p);
    assert_eq!(report.mode, CloseMode::Force);
    assert!(report.closed);
    assert_eq!(
        report.leased, 0,
        "force retires outstanding leases: {report:?}"
    );
    assert_eq!(report.retired, 2);
    assert!(report.drained());

    shutdown(rt);
}

#[test]
fn maintain_on_closed_pool_retires_nothing() {
    let rt = runtime();
    let t0 = Instant::now();
    let pool = register(
        &rt,
        WorkerPool::with_lifetime(
            PoolConfig::new(1, 0),
            vec![10],
            ResourceLifetime::idle_after(Duration::from_secs(5)),
            t0,
        ),
    );

    let lease = acquire_lease(&rt, pool);
    assert_eq!(
        release(&rt, pool, lease, ReleaseDisposition::Reuse),
        ReleaseOutcome::Released
    );

    // Close owns shutdown. A maintenance tick that races the close must
    // not retire idle slots, even long past the idle limit.
    close(&rt, pool, CloseMode::Drain);
    let report = maintain(&rt, pool, t0 + Duration::from_secs(100));
    assert!(
        report.retired.is_empty(),
        "closed pool must not retire on maintain: {report:?}"
    );
    assert_eq!(report.available_after, 1);
    assert_eq!(report.retired_slots, 0);

    shutdown(rt);
}

#[test]
fn maintain_without_lifetime_policy_is_a_noop_report() {
    let rt = runtime();
    let pool = register(&rt, WorkerPool::new(PoolConfig::new(2, 0), vec![10, 11]));

    let report = maintain(&rt, pool, Instant::now());
    assert!(report.retired.is_empty());
    assert!(report.over_age_leased.is_empty());
    assert_eq!(report.available_after, 2);
    assert_eq!(report.leased_after, 0);

    shutdown(rt);
}

#[test]
fn refill_serves_parked_waiter() {
    let rt = runtime();
    let pool = register(&rt, WorkerPool::new(PoolConfig::new(1, 1), vec![10]));

    // Lease the only resource; a second acquire has nowhere to go but a
    // waiter slot.
    let a = acquire_lease(&rt, pool);
    let retired_id = a.resource_id().get();

    // A second thread acquires and parks (the resource is leased).
    let rt2 = Arc::clone(&rt);
    let waiter = std::thread::spawn(move || {
        match rt2.call_blocking(pool, WorkerPoolMsg::Acquire, Duration::from_secs(5)) {
            Ok(CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Acquired(lease)))) => {
                *lease.handle()
            }
            other => panic!("parked waiter expected Acquired, got {other:?}"),
        }
    });

    // Let the waiter park, then retire the leased resource and refill the
    // slot. The fresh resource is dispatched straight to the waiter.
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        release(&rt, pool, a, ReleaseDisposition::Retire),
        ReleaseOutcome::Retired
    );
    assert_eq!(
        refill(&rt, pool, retired_id, 77, Instant::now()),
        RefillOutcome::Refilled
    );

    let served = waiter.join().expect("waiter thread");
    assert_eq!(
        served, 77,
        "parked waiter received the freshly refilled resource"
    );

    shutdown(rt);
}

#[test]
fn maintain_and_refill_effect_helpers_drive_the_pool() {
    let rt = runtime();
    let t0 = Instant::now();
    let pool = register(
        &rt,
        WorkerPool::with_lifetime(
            PoolConfig::new(1, 0),
            vec![10],
            ResourceLifetime::idle_after(Duration::from_secs(5)),
            t0,
        ),
    );
    let (tx, results) = std::sync::mpsc::channel();
    let driver = rt
        .register_with_capacity::<_, Infallible>(HelperDriver { pool, tx }, 16)
        .expect("register helper driver");

    // maintain_effect: the idle resource (idle since build) is retired.
    rt.try_send(driver, HelperMsg::DoMaintain(t0 + Duration::from_secs(6)))
        .expect("send DoMaintain");
    match results.recv_timeout(T).expect("maintain reply") {
        WorkerPoolReply::Maintenance(report) => {
            assert_eq!(report.retired.len(), 1, "{report:?}");
            assert_eq!(report.retired_slots, 1);
        }
        other => panic!("expected Maintenance, got {other:?}"),
    }

    // refill_effect: refilling the retired slot reclaims capacity.
    rt.try_send(
        driver,
        HelperMsg::DoRefill {
            id: 0,
            handle: 20,
            now: t0 + Duration::from_secs(6),
        },
    )
    .expect("send DoRefill");
    match results.recv_timeout(T).expect("refill reply") {
        WorkerPoolReply::Refilled(outcome) => assert_eq!(outcome, RefillOutcome::Refilled),
        other => panic!("expected Refilled, got {other:?}"),
    }

    // The refilled slot is leasable again.
    let lease = acquire_lease(&rt, pool);
    assert_eq!(*lease.handle(), 20);
    let _ = release(&rt, pool, lease, ReleaseDisposition::Reuse);

    shutdown(rt);
}
