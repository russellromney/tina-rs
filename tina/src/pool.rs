//! Bounded pool vocabulary.
//!
//! Pure data types. The concrete `WorkerPool` lives in `tina-runtime`.
//!
//! ```text
//! borrow thing.
//! use thing.
//! return thing, or retire thing.
//! if no thing, say Full.
//! if pool closed, say Closed.
//! if cross shard, say WrongShard.
//! never pretend thing came back by magic.
//! ```
//!
//! # Caller-side timeout
//!
//! There is no `AcquireOutcome::Timeout`. The pool does not enforce a
//! waiter deadline. When a caller's `call(pool, Acquire, timeout)`
//! expires, the runtime delivers `CallOutcome::Timeout` to the caller
//! and closes the pool's deferred reply slot. The pool's next sweep
//! reclaims the waiter slot and bumps `cancel_count` (cancel and
//! caller-timeout share the same in-pool reclaim path; the per-cause
//! breakdown is a runtime trace fact).
//!
//! # Pool mailbox is a second bound
//!
//! `max_waiters` caps parked callers, but the pool isolate's *mailbox
//! capacity* (set at registration) caps how many `Acquire` messages
//! can be in flight before a caller's `call(...)` returns
//! `CallOutcome::Full` from the runtime layer. Size the mailbox to
//! `>= max_waiters + burst` to avoid surprises.

use std::num::NonZeroU64;
use std::time::Duration;

/// Pool configuration. `capacity` is resource count; `max_waiters`
/// caps parked callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolConfig {
    /// Number of resources the pool owns. Must be > 0.
    pub capacity: usize,
    /// Maximum number of parked waiters. Zero means shed immediately
    /// when all resources are busy.
    pub max_waiters: usize,
}

impl PoolConfig {
    /// Construct. The concrete pool validates `capacity > 0` at build.
    pub const fn new(capacity: usize, max_waiters: usize) -> Self {
        Self {
            capacity,
            max_waiters,
        }
    }

    /// Roomy preset for examples and dev: capacity 4, 32 waiters.
    pub const fn dev() -> Self {
        Self::new(4, 32)
    }

    /// Tight preset for pressure tests: capacity 1, 4 waiters.
    pub const fn pressure() -> Self {
        Self::new(1, 4)
    }
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self::dev()
    }
}

/// Pool identity. Stamped into every lease so a release that names
/// the wrong pool is rejected. Constructed only via
/// [`runtime_internal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PoolId(NonZeroU64);

impl PoolId {
    /// Raw id.
    pub const fn get(self) -> NonZeroU64 {
        self.0
    }
}

/// Index of one resource slot inside a pool. Constructed only via
/// [`runtime_internal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResourceId(u32);

impl ResourceId {
    /// Raw index.
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Move-only lease for one borrowed resource.
///
/// `H` is the cheap-clone resource handle the user works with (e.g.
/// [`crate::Address`]). The lease holds an owned `H` plus identity:
/// pool id, resource id, generation. Release consumes the lease.
///
/// Not `Clone`, not `Copy`, no public constructor — only the runtime
/// can mint one. A lease is not a call-cancel handle and cannot
/// release a runtime call; a [`crate::CallHandle`] cannot release a
/// lease.
#[must_use = "PoolLease must be returned via Release or Retire — \
              dropping a lease leaks the resource until pool close"]
pub struct PoolLease<H> {
    pool_id: PoolId,
    resource_id: ResourceId,
    generation: u64,
    handle: H,
}

impl<H> PoolLease<H> {
    /// Pool this lease belongs to.
    pub fn pool_id(&self) -> PoolId {
        self.pool_id
    }

    /// Resource slot this lease names.
    pub fn resource_id(&self) -> ResourceId {
        self.resource_id
    }

    /// Generation under which this lease was minted.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Reference to the resource handle.
    pub fn handle(&self) -> &H {
        &self.handle
    }
}

impl<H: std::fmt::Debug> std::fmt::Debug for PoolLease<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolLease")
            .field("pool_id", &self.pool_id)
            .field("resource_id", &self.resource_id)
            .field("generation", &self.generation)
            .field("handle", &self.handle)
            .finish()
    }
}

/// Outcome of one acquire. Variants stay distinct.
#[must_use = "AcquireOutcome reports whether the pool actually handed \
              out a resource — ignoring it leaks the lease or hides \
              backpressure"]
pub enum AcquireOutcome<H> {
    /// Got a resource.
    Acquired(PoolLease<H>),
    /// All resources busy, waiter table full (or `max_waiters == 0`).
    /// Caller did not occupy a waiter slot.
    Full,
    /// Pool is closed.
    Closed,
    /// Caller is on a different shard than the pool. First form does
    /// not support cross-shard pool use — the pool isolate runs on
    /// one shard and only same-shard callers can park as waiters.
    WrongShard,
}

impl<H> AcquireOutcome<H> {
    /// Map only the resource handle inside `Acquired`, preserving
    /// lease identity. Other variants pass through.
    pub fn map_acquired<U>(self, f: impl FnOnce(H) -> U) -> AcquireOutcome<U> {
        match self {
            Self::Acquired(lease) => {
                let new_handle = f(lease.handle);
                AcquireOutcome::Acquired(PoolLease {
                    pool_id: lease.pool_id,
                    resource_id: lease.resource_id,
                    generation: lease.generation,
                    handle: new_handle,
                })
            }
            Self::Full => AcquireOutcome::Full,
            Self::Closed => AcquireOutcome::Closed,
            Self::WrongShard => AcquireOutcome::WrongShard,
        }
    }

    /// Reason for non-acquired variants. `None` for `Acquired`.
    pub fn pressure_reason(&self) -> Option<PoolPressureReason> {
        match self {
            Self::Acquired(_) => None,
            Self::Full => Some(PoolPressureReason::Full),
            Self::Closed => Some(PoolPressureReason::Closed),
            Self::WrongShard => Some(PoolPressureReason::WrongShard),
        }
    }
}

impl<H: std::fmt::Debug> std::fmt::Debug for AcquireOutcome<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Acquired(lease) => f.debug_tuple("Acquired").field(lease).finish(),
            Self::Full => f.write_str("Full"),
            Self::Closed => f.write_str("Closed"),
            Self::WrongShard => f.write_str("WrongShard"),
        }
    }
}

/// Why an acquire did not yield a resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolPressureReason {
    /// Resources busy, waiter table full.
    Full,
    /// Pool closed.
    Closed,
    /// Caller on a different shard than the pool.
    WrongShard,
}

/// Caller-owned disposition for a release.
///
/// `Reuse` says caller believes the resource is healthy. `Retire`
/// says it is not. The pool may override `Reuse` to retire a known-bad
/// resource, but reports that override via [`ReleaseOutcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseDisposition {
    /// Caller believes resource is healthy. Pool reuses it.
    Reuse,
    /// Caller believes resource is unhealthy. Pool drops it.
    Retire,
}

/// Outcome of one release.
#[must_use = "ReleaseOutcome reports whether the pool actually \
              accepted the release — ignoring it can hide \
              double-release / stale-lease bugs"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseOutcome {
    /// Pool accepted; resource went back to idle (or to next waiter).
    Released,
    /// Pool accepted and dropped the resource. Caller asked for
    /// `Retire`, or pool overrode `Reuse` because it knew the
    /// resource was stale.
    Retired,
    /// Pool id, resource id, or generation did not match anything live.
    StaleLease,
    /// Pool already saw a release for this `(resource_id, generation)`.
    DoubleRelease,
    /// Pool was force-closed; outstanding leases are stale.
    PoolClosed,
}

/// How a pool close behaves.
///
/// `Drain`: stop new acquires, settle waiters as `Closed`, let
/// outstanding leases return normally (caller's `Reuse` is honored
/// as `Released`; the resource sits idle but cannot be re-acquired).
///
/// `Force`: stop new acquires, settle waiters as `Closed`, mark
/// outstanding leases stale — late releases get `PoolClosed` and the
/// pool retires the resource immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseMode {
    /// Soft close. Outstanding leases return normally.
    Drain,
    /// Hard close. Outstanding leases are stale.
    Force,
}

/// Snapshot of pool pressure.
#[must_use = "PoolPressureReport carries pool-state truth — discarding \
              it hides backpressure visibility"]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PoolPressureReport {
    /// Configured resource capacity.
    pub capacity: usize,
    /// Resources currently idle (and not retired).
    pub available: usize,
    /// Resources currently out on lease.
    pub leased: usize,
    /// Callers parked waiting for a resource.
    pub waiters: usize,
    /// Configured waiter capacity.
    pub max_waiters: usize,
    /// Highest live `waiters` value since pool construction. Pair
    /// with `max_waiters` and `full_count` for a count-capacity
    /// snapshot.
    pub high_water_waiters: usize,
    /// Cumulative `Full` outcomes.
    pub full_count: u64,
    /// Cumulative `Closed` outcomes — both parked-waiter shutdowns
    /// and post-close acquires.
    pub closed_count: u64,
    /// Cumulative `WrongShard` outcomes.
    pub wrong_shard_count: u64,
    /// Cumulative waiters reclaimed because the caller cancelled via
    /// `cancel_call(handle)` or the caller's `call(...)` timeout
    /// fired before the pool replied.
    pub cancel_count: u64,
    /// Cumulative resources dropped via `Retire` or pool override.
    pub retired_count: u64,
    /// Cumulative `Acquire` messages received via plain `send` (no
    /// reply path) and dropped. A non-zero value means a caller is
    /// using the pool API wrong.
    pub no_caller_drops: u64,
    /// Cumulative resources reclaimed because the caller cancelled
    /// between the pool's dispatch and the runtime delivering the
    /// `Acquired` reply (the deferred-reply rejection path). Pool
    /// re-marks the resource as Idle.
    pub dispatch_recovered: u64,
    /// True once a [`CloseMode`] has been applied.
    pub closed: bool,
}

impl PoolPressureReport {
    /// Project this report onto the waiter count surface. Resource
    /// count is not capped at runtime (capacity is fixed at build),
    /// so only waiters need a capacity surface.
    ///
    /// The caller picks the surface name and mode. Pin an explicit
    /// name when CI tests assert on this surface.
    pub fn to_waiters_capacity_report(
        &self,
        name: impl Into<String>,
        mode: crate::capacity::CapacityMode,
    ) -> crate::capacity::CapacitySurfaceReport {
        crate::capacity::CapacitySurfaceReport::count(
            name,
            mode,
            self.max_waiters,
            self.waiters,
            self.high_water_waiters,
            self.full_count,
        )
    }
}

/// Idle-age and total-age retirement policy for pooled resources.
///
/// A pool built with a lifetime policy retires *idle* resources that
/// have sat idle at least `max_idle`, or have lived at least
/// `max_lifetime` overall. Retirement is driven by an explicit
/// `Maintain { now }` sweep — the pool never reads the wall clock on
/// its own, so idle/age timers are the owner's Tina timer, not a hidden
/// `Instant::now()` inside resource code.
///
/// Two honest limits:
///
/// - A *leased* resource past `max_lifetime` is reported old, never
///   stolen back from its caller. The owner sees it in
///   [`ResourcePolicyReport::over_age_leased`].
/// - Idle age is measured from the first maintenance sweep that
///   observes the resource idle, so the sweep cadence bounds idle
///   granularity. Run maintenance at least as often as `max_idle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResourceLifetime {
    /// Retire an idle resource once it has been idle at least this
    /// long. `None` disables idle retirement.
    pub max_idle: Option<Duration>,
    /// Retire a resource once its total age reaches this. Idle ones are
    /// retired on the next sweep; leased ones are reported, not stolen.
    /// `None` disables max-age retirement.
    pub max_lifetime: Option<Duration>,
}

impl ResourceLifetime {
    /// Construct from explicit idle / max-age limits.
    pub const fn new(max_idle: Option<Duration>, max_lifetime: Option<Duration>) -> Self {
        Self {
            max_idle,
            max_lifetime,
        }
    }

    /// Idle-retirement only: retire resources idle at least `d`.
    pub const fn idle_after(d: Duration) -> Self {
        Self {
            max_idle: Some(d),
            max_lifetime: None,
        }
    }

    /// Max-age only: retire resources older than `d`.
    pub const fn max_age(d: Duration) -> Self {
        Self {
            max_idle: None,
            max_lifetime: Some(d),
        }
    }

    /// True when no limit is set — the policy retires nothing on age.
    pub const fn is_unbounded(&self) -> bool {
        self.max_idle.is_none() && self.max_lifetime.is_none()
    }
}

/// A caller's health verdict for a leased resource, decided at an
/// explicit check point (before handoff, after release, or during
/// scheduled maintenance).
///
/// The caller owns the probe — a generic pool cannot inspect an
/// arbitrary `H`. Map the verdict onto a release with
/// [`ResourceHealth::disposition`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceHealth {
    /// Resource is good; reuse it.
    Healthy,
    /// Resource is questionable but still usable. First form treats
    /// this as reuse; the caller may downgrade to `Retire` itself.
    Suspect,
    /// Resource is bad; retire it.
    Retire,
}

impl ResourceHealth {
    /// Map this verdict onto the release disposition the pool acts on.
    /// `Healthy`/`Suspect` reuse; `Retire` drops the resource.
    pub fn disposition(self) -> ReleaseDisposition {
        match self {
            Self::Healthy | Self::Suspect => ReleaseDisposition::Reuse,
            Self::Retire => ReleaseDisposition::Retire,
        }
    }
}

/// Why a resource left the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetireReason {
    /// Idle at least the lifetime policy's `max_idle`.
    IdleTimeout,
    /// Total age reached the lifetime policy's `max_lifetime`.
    MaxLifetime,
    /// Caller released it as unhealthy (`ReleaseDisposition::Retire`).
    Unhealthy,
    /// Pool was force-closed with the lease outstanding.
    ForceClosed,
}

/// Where a lifetime / health decision was made. The report names the
/// point so the owner knows whether age was found by a scheduled sweep
/// or at the moment of handing / returning a resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyCheckPoint {
    /// A scheduled `Maintain { now }` sweep.
    ScheduledMaintenance,
    /// Just before handing a resource to a caller.
    BeforeHandoff,
    /// Just after a caller returned a resource.
    AfterRelease,
}

/// One resource the pool retired, named with the reason and the point
/// the decision was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetiredResource {
    /// Slot that was retired.
    pub resource_id: ResourceId,
    /// Why it was retired.
    pub reason: RetireReason,
    /// Where the decision happened.
    pub check_point: PolicyCheckPoint,
}

/// Result of a maintenance sweep.
///
/// Names which idle resources the sweep retired and why, and which
/// *leased* resources are past `max_lifetime` — reported old, never
/// stolen. `retired_slots` counts the empty slots an owner can
/// [`refill`](crate::pool) to reclaim capacity.
#[must_use = "ResourcePolicyReport names what maintenance retired — \
              discarding it hides which slots need refill"]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourcePolicyReport {
    /// Resources inspected (idle + leased, not already-retired slots).
    pub inspected: usize,
    /// Resources retired by this sweep, each with reason + check point.
    pub retired: Vec<RetiredResource>,
    /// Leased resources past `max_lifetime`. Reported old, not retired:
    /// the pool never steals a resource out from under its caller.
    pub over_age_leased: Vec<ResourceId>,
    /// Idle resources remaining after the sweep.
    pub available_after: usize,
    /// Leased resources after the sweep.
    pub leased_after: usize,
    /// Empty (retired) slots in the pool after the sweep. Refill these
    /// to restore capacity.
    pub retired_slots: usize,
}

/// Outcome of refilling a retired slot with a fresh resource handle.
///
/// Refill is owner-driven: the pool cannot build an `H`, so the owner
/// hands one back for a slot the pool retired. A live (idle or leased)
/// slot is never silently replaced.
#[must_use = "RefillOutcome reports whether the fresh resource was \
              actually installed"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefillOutcome {
    /// Slot was retired and is now idle with the fresh handle. Capacity
    /// reclaimed.
    Refilled,
    /// Named slot was not retired (idle or leased). Refill rejected so a
    /// live resource is never replaced behind a caller's back.
    NotRetired,
    /// `resource_id` is out of range for this pool.
    UnknownResource,
    /// Pool is closed; no refills accepted.
    Closed,
}

/// Typed shutdown summary built from a close mode plus a post-close
/// pressure snapshot. Uses the existing lifecycle words: drain, force,
/// closed, leased/leaked counts.
///
/// Build it with [`PoolShutdownReport::from_pressure`] after a
/// `Close(mode)` ack and a `PressureReport` snapshot.
#[must_use = "PoolShutdownReport carries the drain/force outcome — \
              discarding it hides whether leases were left outstanding"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolShutdownReport {
    /// How the pool was closed.
    pub mode: CloseMode,
    /// True once a close has been applied.
    pub closed: bool,
    /// Resources still out on lease. Under `Drain` these are waiting to
    /// return; under `Force` the pool retires them, so this should be
    /// zero.
    pub leased: usize,
    /// Idle resources still held (drained but not yet refused).
    pub available: usize,
    /// Cumulative resources retired over the pool's life.
    pub retired: u64,
    /// Callers still parked when the snapshot was taken.
    pub waiters: usize,
}

impl PoolShutdownReport {
    /// Fold a close mode and a post-close pressure snapshot into a
    /// shutdown summary.
    pub fn from_pressure(mode: CloseMode, pressure: &PoolPressureReport) -> Self {
        Self {
            mode,
            closed: pressure.closed,
            leased: pressure.leased,
            available: pressure.available,
            retired: pressure.retired_count,
            waiters: pressure.waiters,
        }
    }

    /// True when nothing is left out on lease — a drain has fully
    /// settled, or a force retired everything.
    pub fn drained(&self) -> bool {
        self.leased == 0
    }
}

/// Why an acquire did not yield a lease, after the bridge layer.
///
/// Returned by helpers that fold `CallOutcome<WorkerPoolReply<H>>`
/// into one flat result. Keeps the layered truth distinct: pool-level
/// outcomes (`Full`, `Closed`, `WrongShard`) are different from
/// transport-level outcomes (`CallTimeout`, `CallClosed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquireFailure {
    /// Pool said `Full` — no resource and no waiter slot.
    Full,
    /// Pool said `Closed`.
    Closed,
    /// Pool said `WrongShard`.
    WrongShard,
    /// Caller's `call(...)` timeout fired before the pool replied.
    CallTimeout,
    /// Pool isolate's mailbox refused the call.
    CallFull,
    /// Pool isolate is gone.
    CallClosed,
    /// Pool isolate rejected the call without a pool-level reply.
    CallRejected(crate::CallRejectedReason),
    /// Reply variant was not `Acquire(...)` — protocol error.
    WrongReply,
}

/// Why a release was not `Released`, after the bridge layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseFailure {
    /// Pool accepted but dropped the resource (Retire requested or
    /// pool override).
    Retired,
    /// Lease did not match anything live in the pool.
    StaleLease,
    /// Same `(resource_id, generation)` was already returned.
    DoubleRelease,
    /// Pool was force-closed.
    PoolClosed,
    /// Caller's `call(...)` timeout fired before the pool replied.
    CallTimeout,
    /// Pool isolate's mailbox refused the call.
    CallFull,
    /// Pool isolate is gone.
    CallClosed,
    /// Pool isolate rejected the call without a pool-level reply.
    CallRejected(crate::CallRejectedReason),
    /// Reply variant was not `Release(...)` — protocol error.
    WrongReply,
}

/// Capability-based internals exposed for runtime pool implementations.
///
/// [`PoolAuthority`] owns a process-unique [`PoolId`]. Its constructor does
/// not accept caller-provided identity, so even code that can name this module
/// cannot forge a lease that passes a different pool's identity check. A pool
/// implementation keeps its authority private and uses it to mint the
/// [`ResourceId`] and [`PoolLease`] values for that pool.
pub mod runtime_internal {
    use super::{PoolId, PoolLease, ResourceId};
    use std::num::NonZeroU64;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Unique authority for one pool's identity and lease constructors.
    ///
    /// This type is deliberately not `Clone`: the pool implementation owns the
    /// sole mint for its identity. Calling [`PoolAuthority::new`] creates a new
    /// identity rather than accepting one supplied by the caller.
    #[derive(Debug)]
    pub struct PoolAuthority {
        pool_id: PoolId,
    }

    impl PoolAuthority {
        /// Creates an authority with a process-unique pool identity.
        ///
        /// # Panics
        ///
        /// Panics if the process exhausts the non-zero `u64` identity space.
        pub fn new() -> Self {
            static NEXT_POOL_ID: AtomicU64 = AtomicU64::new(1);
            let mut raw = NEXT_POOL_ID.load(Ordering::Relaxed);
            loop {
                let next = raw.checked_add(1).expect("pool id space exhausted");
                match NEXT_POOL_ID.compare_exchange_weak(
                    raw,
                    next,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(observed) => raw = observed,
                }
            }
            let raw = NonZeroU64::new(raw).expect("pool id counter starts non-zero");
            Self {
                pool_id: PoolId(raw),
            }
        }

        /// Returns this authority's unique pool identity.
        pub const fn pool_id(&self) -> PoolId {
            self.pool_id
        }

        /// Creates a resource identifier in this pool's identity domain.
        pub const fn resource_id(&self, raw: u32) -> ResourceId {
            ResourceId(raw)
        }

        /// Mints a move-only lease under this authority's pool identity.
        ///
        /// The owning pool state machine is responsible for issuing at most one
        /// live lease for each `(resource_id, generation)` pair. A lease minted
        /// by one authority cannot pass another pool's identity check.
        pub fn lease<H>(&self, resource_id: u32, generation: u64, handle: H) -> PoolLease<H> {
            PoolLease {
                pool_id: self.pool_id,
                resource_id: self.resource_id(resource_id),
                generation,
                handle,
            }
        }
    }

    impl Default for PoolAuthority {
        fn default() -> Self {
            Self::new()
        }
    }

    /// Consume a lease into its parts. Pools use this to validate
    /// releases. Destructuring a legitimately issued lease cannot break the
    /// pool invariant on its own.
    pub fn lease_into_parts<H>(lease: PoolLease<H>) -> (PoolId, ResourceId, u64, H) {
        (
            lease.pool_id,
            lease.resource_id,
            lease.generation,
            lease.handle,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_lifetime_constructors() {
        let idle = ResourceLifetime::idle_after(Duration::from_secs(5));
        assert_eq!(idle.max_idle, Some(Duration::from_secs(5)));
        assert_eq!(idle.max_lifetime, None);
        assert!(!idle.is_unbounded());

        let age = ResourceLifetime::max_age(Duration::from_secs(30));
        assert_eq!(age.max_idle, None);
        assert_eq!(age.max_lifetime, Some(Duration::from_secs(30)));
        assert!(!age.is_unbounded());

        let both =
            ResourceLifetime::new(Some(Duration::from_secs(5)), Some(Duration::from_secs(30)));
        assert_eq!(both.max_idle, Some(Duration::from_secs(5)));
        assert_eq!(both.max_lifetime, Some(Duration::from_secs(30)));
        assert!(!both.is_unbounded());

        // Default and the all-None case are unbounded — they retire
        // nothing on age.
        assert!(ResourceLifetime::default().is_unbounded());
        assert!(ResourceLifetime::new(None, None).is_unbounded());
    }

    #[test]
    fn resource_health_maps_to_disposition() {
        // Only an explicit Retire drops the resource; a Suspect verdict
        // still reuses (first form), so capacity is not shed on a hunch.
        assert_eq!(
            ResourceHealth::Healthy.disposition(),
            ReleaseDisposition::Reuse
        );
        assert_eq!(
            ResourceHealth::Suspect.disposition(),
            ReleaseDisposition::Reuse
        );
        assert_eq!(
            ResourceHealth::Retire.disposition(),
            ReleaseDisposition::Retire
        );
    }

    #[test]
    fn shutdown_report_drain_with_outstanding_leases() {
        let pressure = PoolPressureReport {
            capacity: 4,
            available: 1,
            leased: 2,
            waiters: 0,
            retired_count: 3,
            closed: true,
            ..PoolPressureReport::default()
        };
        let report = PoolShutdownReport::from_pressure(CloseMode::Drain, &pressure);
        assert_eq!(report.mode, CloseMode::Drain);
        assert!(report.closed);
        assert_eq!(report.leased, 2);
        assert_eq!(report.available, 1);
        assert_eq!(report.retired, 3);
        assert!(!report.drained(), "two leases still out — not drained");
    }

    #[test]
    fn shutdown_report_drained_when_nothing_leased() {
        let pressure = PoolPressureReport {
            capacity: 4,
            available: 4,
            leased: 0,
            closed: true,
            ..PoolPressureReport::default()
        };
        let report = PoolShutdownReport::from_pressure(CloseMode::Force, &pressure);
        assert!(report.drained());
    }

    #[test]
    fn pool_authorities_mint_distinct_identity_domains() {
        let first = runtime_internal::PoolAuthority::new();
        let second = runtime_internal::PoolAuthority::new();
        assert_ne!(first.pool_id(), second.pool_id());

        let lease = first.lease(7, 3, "resource");
        let (pool_id, resource_id, generation, handle) = runtime_internal::lease_into_parts(lease);
        assert_eq!(pool_id, first.pool_id());
        assert_eq!(resource_id, first.resource_id(7));
        assert_eq!(generation, 3);
        assert_eq!(handle, "resource");
    }
}
