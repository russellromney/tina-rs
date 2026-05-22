//! Bounded worker pool isolate.
//!
//! Caller acquires with `call_cancelable(pool, WorkerPoolMsg::Acquire,
//! timeout).then(...)` and stores the [`tina::CallHandle`] to be able
//! to cancel the wait. `cancel_call(handle)` closes the caller-side
//! wait and marks the pool's deferred slot `Closed`. The pool sweeps
//! closed waiter slots on every incoming message; cancelled / timed-out
//! waiters are reclaimed without disturbing FIFO order of the rest.
//!
//! No pool-side waiter timeout. The caller's `call(...)` timeout is
//! the only deadline; a fired timeout closes the deferred slot, which
//! the next pool sweep reclaims (counted under `cancel_count`).
//!
//! # Cancel-race recovery
//!
//! When the pool dispatches an `Acquired` reply (immediate or via
//! `dispatch_to_next_waiter`) it stores an observer for the consumed
//! deferred slot under `(resource_id, generation)`. On every handler
//! turn the pool walks this set: a `Replied` slot was delivered
//! cleanly and is dropped from the set; a `Closed` slot was rejected
//! by the runtime (caller cancelled between dispatch and delivery), so
//! the pool returns the resource to Idle and bumps
//! `dispatch_recovered`. Without this back-channel a cancel race
//! would leak the resource forever.
//!
//! # Owner-stop behaviour
//!
//! When the pool's owner stops, the runtime cancels every pending
//! isolate call and delivers `CallCompletionRejected` to the caller.
//! The pool's deferred slots become `Closed`, but the pool itself
//! may not receive another handler turn before being dropped. This is
//! harmless: the waiter slab and counters are deallocated with the
//! isolate, and `live_waiter_count()` drops to zero. The
//! `cancel_count` may not increment for these unreclaimed slots,
//! so metrics read during shutdown should be treated as conservative
//! ("at most this many") rather than exact.

use std::convert::Infallible;
use std::marker::PhantomData;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use tina::pool::{
    AcquireFailure, AcquireOutcome, CloseMode, PolicyCheckPoint, PoolConfig, PoolId, PoolLease,
    PoolPressureReport, RefillOutcome, ReleaseDisposition, ReleaseFailure, ReleaseOutcome,
    ResourceLifetime, ResourcePolicyReport, RetireReason, RetiredResource,
    runtime_internal as pool_internal,
};
use tina::runtime_internal::{deferred_handle_ref, handle_shared};
use tina::{
    CallContext, Context, DeferredReply, DeferredSlotShared, DeferredSlotState, Effect, Isolate,
    Outbound, Shard, batch, noop, reply, reply_to,
};

use crate::call::RuntimeCall;

fn mint_pool_id() -> PoolId {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let raw = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nz = NonZeroU64::new(raw).expect("pool id counter wrapped to zero");
    // SAFETY: process-wide monotonic counter, used only here to
    // identify a pool we're constructing in `WorkerPool::new`. The
    // returned id is unique per pool instance for the process.
    unsafe { pool_internal::pool_id_from_raw(nz) }
}

/// Messages handled by [`WorkerPool`]. `H` is the resource handle.
pub enum WorkerPoolMsg<H>
where
    H: Send + 'static,
{
    /// Acquire one resource. Pool replies immediately with `Acquired` /
    /// `Full` / `Closed` / `WrongShard`, or parks the caller and
    /// replies later.
    Acquire,
    /// Return a lease.
    Release {
        /// The lease being returned.
        lease: PoolLease<H>,
        /// Caller's belief about resource health.
        disposition: ReleaseDisposition,
    },
    /// Stop new acquires; settle waiters as `Closed`. `Force` also
    /// marks outstanding leases stale.
    Close(CloseMode),
    /// Request a [`PoolPressureReport`] snapshot.
    PressureReport,
    /// Run a lifetime sweep at logical time `now`. Retires idle
    /// resources past the lifetime policy and reports leased resources
    /// past max age (reported, not stolen). No-op unless the pool was
    /// built with [`WorkerPool::with_lifetime`]. `now` is the owner's
    /// Tina clock (`ctx.now()`), not a wall clock the pool reads.
    Maintain {
        /// Logical time for this sweep.
        now: Instant,
    },
    /// Install a fresh resource handle into a retired slot to reclaim
    /// capacity. Owner-driven: the pool cannot build an `H`. A live
    /// (idle or leased) slot is never replaced.
    Refill {
        /// Slot to refill (a `ResourceId` the pool retired).
        resource_id: u32,
        /// Fresh resource handle.
        handle: H,
        /// Logical time the fresh resource was created.
        now: Instant,
    },
}

impl<H> std::fmt::Debug for WorkerPoolMsg<H>
where
    H: Send + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Acquire => f.write_str("Acquire"),
            Self::Release { disposition, .. } => f
                .debug_struct("Release")
                .field("disposition", disposition)
                .finish(),
            Self::Close(mode) => f.debug_tuple("Close").field(mode).finish(),
            Self::PressureReport => f.write_str("PressureReport"),
            Self::Maintain { now } => f.debug_struct("Maintain").field("now", now).finish(),
            Self::Refill { resource_id, .. } => f
                .debug_struct("Refill")
                .field("resource_id", resource_id)
                .finish(),
        }
    }
}

/// Reply payload covering every [`WorkerPoolMsg`] variant.
pub enum WorkerPoolReply<H>
where
    H: Send + 'static,
{
    /// Reply to [`WorkerPoolMsg::Acquire`].
    Acquire(AcquireOutcome<H>),
    /// Reply to [`WorkerPoolMsg::Release`].
    Release(ReleaseOutcome),
    /// Acknowledgement of [`WorkerPoolMsg::Close`].
    Closed,
    /// Reply to [`WorkerPoolMsg::PressureReport`].
    Pressure(PoolPressureReport),
    /// Reply to [`WorkerPoolMsg::Maintain`].
    Maintenance(ResourcePolicyReport),
    /// Reply to [`WorkerPoolMsg::Refill`].
    Refilled(RefillOutcome),
}

impl<H> std::fmt::Debug for WorkerPoolReply<H>
where
    H: Send + std::fmt::Debug + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Acquire(outcome) => f.debug_tuple("Acquire").field(outcome).finish(),
            Self::Release(outcome) => f.debug_tuple("Release").field(outcome).finish(),
            Self::Closed => f.write_str("Closed"),
            Self::Pressure(report) => f.debug_tuple("Pressure").field(report).finish(),
            Self::Maintenance(report) => f.debug_tuple("Maintenance").field(report).finish(),
            Self::Refilled(outcome) => f.debug_tuple("Refilled").field(outcome).finish(),
        }
    }
}

struct Waiter<H>
where
    H: Send + 'static,
{
    reply: DeferredReply<WorkerPoolReply<H>>,
}

enum ResourceState {
    Idle { next_generation: u64 },
    Leased { generation: u64 },
    // Carries the next generation so a refill into this slot keeps the
    // pool's generation counter monotonic. Reusing a low generation
    // after a refill would weaken the stale-lease / double-release
    // checks that key on generation order.
    Retired { next_generation: u64 },
}

/// One in-flight Acquired dispatch the pool needs to confirm landed.
struct InFlightDispatch {
    resource_id: u32,
    generation: u64,
    slot: Arc<DeferredSlotShared>,
}

#[derive(Default)]
struct PoolCounters {
    full: u64,
    cancel: u64,
    retired: u64,
    closed: u64,
    wrong_shard: u64,
    no_caller_drops: u64,
    dispatch_recovered: u64,
    high_water_waiters: usize,
}

/// Bounded worker pool isolate.
///
/// `H` is the resource handle (e.g. `tina::Address<WorkerMsg, WReply>`);
/// must be cheap-clone + `Send`. `S` is the shard the pool runs on.
///
/// # Mailbox sizing
///
/// `max_waiters` caps parked callers but not in-flight `Acquire`
/// messages. Register the pool with mailbox capacity
/// `>= max_waiters + expected burst` so the runtime layer doesn't
/// reject acquires as `CallOutcome::Full` before the pool's own
/// `AcquireOutcome::Full` path can engage.
pub struct WorkerPool<H, S>
where
    H: Send + Clone + 'static,
    S: Shard + 'static,
{
    pool_id: PoolId,
    config: PoolConfig,
    lifetime: Option<ResourceLifetime>,
    // Per-slot timestamps, populated only when `lifetime` is set.
    // `created_at` is set at build / refill and never moves — total age
    // is from creation. `idle_since` is cleared on lease and re-stamped
    // by the first maintenance sweep that sees the slot idle.
    created_at: Vec<Option<Instant>>,
    idle_since: Vec<Option<Instant>>,
    resources: Vec<Option<H>>,
    states: Vec<ResourceState>,
    idle: std::collections::VecDeque<u32>,
    waiter_slab: Vec<Option<Waiter<H>>>,
    free_waiter_slots: Vec<u32>,
    live_waiters: usize,
    waiter_queue: std::collections::VecDeque<u32>,
    in_flight: Vec<InFlightDispatch>,
    counters: PoolCounters,
    closed: Option<CloseMode>,
    _shard: PhantomData<fn() -> S>,
}

impl<H, S> WorkerPool<H, S>
where
    H: Send + Clone + 'static,
    S: Shard + 'static,
{
    /// Build a pool over a fixed list of resource handles. Panics if
    /// `resources` is empty or its length disagrees with `config.capacity`.
    ///
    /// No lifetime policy: `Maintain` is a no-op and resources are only
    /// retired by caller `Retire` or force-close.
    pub fn new(config: PoolConfig, resources: Vec<H>) -> Self {
        Self::build(config, resources, None, None)
    }

    /// Build a pool with an idle/max-age [`ResourceLifetime`] policy.
    ///
    /// `now` stamps every resource's creation time. The owner drives
    /// retirement with `Maintain { now }` (typically off a Tina timer);
    /// the pool never reads the wall clock itself. Panics under the
    /// same conditions as [`WorkerPool::new`].
    pub fn with_lifetime(
        config: PoolConfig,
        resources: Vec<H>,
        lifetime: ResourceLifetime,
        now: Instant,
    ) -> Self {
        Self::build(config, resources, Some(lifetime), Some(now))
    }

    fn build(
        config: PoolConfig,
        resources: Vec<H>,
        lifetime: Option<ResourceLifetime>,
        now: Option<Instant>,
    ) -> Self {
        assert!(
            !resources.is_empty(),
            "WorkerPool capacity must be > 0 (got empty resource list)"
        );
        assert_eq!(
            resources.len(),
            config.capacity,
            "config.capacity {} disagrees with resources.len() {}",
            config.capacity,
            resources.len()
        );

        let pool_id = mint_pool_id();
        let cap = config.capacity;
        let max_waiters = config.max_waiters;
        let mut idle = std::collections::VecDeque::with_capacity(cap);
        let mut states = Vec::with_capacity(cap);
        for i in 0..cap {
            idle.push_back(i as u32);
            states.push(ResourceState::Idle { next_generation: 1 });
        }
        let mut waiter_slab = Vec::with_capacity(max_waiters);
        let mut free_waiter_slots = Vec::with_capacity(max_waiters);
        for _ in 0..max_waiters {
            waiter_slab.push(None);
        }
        for idx in (0..max_waiters).rev() {
            free_waiter_slots.push(idx as u32);
        }
        let resources_opt: Vec<Option<H>> = resources.into_iter().map(Some).collect();
        // Resources start idle "now". Both timestamps anchor at build
        // time when a lifetime policy is present, else stay None.
        let created_at = vec![now; cap];
        let idle_since = vec![now; cap];
        Self {
            pool_id,
            config,
            lifetime,
            created_at,
            idle_since,
            resources: resources_opt,
            states,
            idle,
            waiter_slab,
            free_waiter_slots,
            live_waiters: 0,
            waiter_queue: std::collections::VecDeque::with_capacity(max_waiters),
            in_flight: Vec::new(),
            counters: PoolCounters::default(),
            closed: None,
            _shard: PhantomData,
        }
    }

    /// This pool's identity. Useful for diagnostic logging.
    pub fn pool_id(&self) -> PoolId {
        self.pool_id
    }

    /// Snapshot the current pressure state.
    pub fn pressure(&self) -> PoolPressureReport {
        let mut available = 0usize;
        let mut leased = 0usize;
        for s in &self.states {
            match s {
                ResourceState::Idle { .. } => available += 1,
                ResourceState::Leased { .. } => leased += 1,
                ResourceState::Retired { .. } => {}
            }
        }
        PoolPressureReport {
            capacity: self.config.capacity,
            available,
            leased,
            waiters: self.live_waiter_count(),
            max_waiters: self.config.max_waiters,
            high_water_waiters: self.counters.high_water_waiters,
            full_count: self.counters.full,
            closed_count: self.counters.closed,
            wrong_shard_count: self.counters.wrong_shard,
            cancel_count: self.counters.cancel,
            retired_count: self.counters.retired,
            no_caller_drops: self.counters.no_caller_drops,
            dispatch_recovered: self.counters.dispatch_recovered,
            closed: self.closed.is_some(),
        }
    }

    fn live_waiter_count(&self) -> usize {
        self.live_waiters
    }

    // Reclaim waiter slots whose deferred reply slot is no longer
    // Open. The runtime cannot distinguish caller-cancel vs
    // caller-timeout at the slot level (that lives in trace facts), so
    // both increment `cancel_count` here.
    fn sweep_waiters(&mut self) {
        let mut reclaimed_any = false;
        for idx in 0..self.waiter_slab.len() {
            let drop_it = self.waiter_slab[idx]
                .as_ref()
                .is_some_and(|w| w.reply.state() != DeferredSlotState::Open);
            if drop_it && self.remove_waiter_slot(idx as u32).is_some() {
                self.counters.cancel += 1;
                reclaimed_any = true;
            }
        }
        if reclaimed_any {
            let slab = &self.waiter_slab;
            self.waiter_queue
                .retain(|idx| slab[*idx as usize].is_some());
        }
    }

    // Walk in-flight dispatches: drop entries whose slot is Replied,
    // recover entries whose slot is Closed (caller cancelled between
    // our dispatch and the runtime delivering the reply).
    fn sweep_in_flight(&mut self) {
        let mut i = 0;
        while i < self.in_flight.len() {
            let state = self.in_flight[i].slot.state();
            match state {
                DeferredSlotState::Open => i += 1,
                DeferredSlotState::Replied => {
                    self.in_flight.swap_remove(i);
                }
                DeferredSlotState::Closed => {
                    let entry = self.in_flight.swap_remove(i);
                    self.recover_dispatched(entry.resource_id, entry.generation);
                }
            }
        }
    }

    fn recover_dispatched(&mut self, resource_id: u32, generation: u64) {
        let state = &mut self.states[resource_id as usize];
        match state {
            ResourceState::Leased { generation: g } if *g == generation => {
                *state = ResourceState::Idle {
                    next_generation: generation.saturating_add(1),
                };
                // Back to idle: re-stamp on the next maintenance sweep.
                self.idle_since[resource_id as usize] = None;
                self.idle.push_back(resource_id);
                self.counters.dispatch_recovered += 1;
            }
            // Released, retired, or otherwise advanced — nothing to recover.
            _ => {}
        }
    }

    fn alloc_waiter_slot(&mut self) -> u32 {
        self.free_waiter_slots
            .pop()
            .expect("alloc_waiter_slot called when slab is full")
    }

    fn store_waiter_slot(&mut self, slab_idx: u32, waiter: Waiter<H>) {
        debug_assert!(self.waiter_slab[slab_idx as usize].is_none());
        self.waiter_slab[slab_idx as usize] = Some(waiter);
        self.live_waiters += 1;
    }

    fn remove_waiter_slot(&mut self, slab_idx: u32) -> Option<Waiter<H>> {
        let waiter = self.waiter_slab[slab_idx as usize].take();
        if waiter.is_some() {
            self.live_waiters = self
                .live_waiters
                .checked_sub(1)
                .expect("WorkerPool live waiter count underflow");
            self.free_waiter_slots.push(slab_idx);
        }
        waiter
    }

    // Retire one slot: drop its handle, mark it Retired, clear its
    // lifetime timestamps, and bump the counter. Preserves the slot's
    // next generation so a later refill stays monotonic. Leaves the
    // idle queue alone — callers prune it (release never has the id
    // queued; the maintenance sweep prunes after the pass).
    fn retire_slot(&mut self, idx: u32) {
        let next_generation = match self.states[idx as usize] {
            ResourceState::Idle { next_generation } => next_generation,
            // The outstanding lease's generation is now spent.
            ResourceState::Leased { generation } => generation.saturating_add(1),
            ResourceState::Retired { next_generation } => next_generation,
        };
        self.states[idx as usize] = ResourceState::Retired { next_generation };
        self.resources[idx as usize] = None;
        self.created_at[idx as usize] = None;
        self.idle_since[idx as usize] = None;
        self.counters.retired += 1;
    }

    fn mint_lease(&mut self, resource_id: u32) -> PoolLease<H> {
        // Leaving idle: the idle clock restarts when it returns.
        self.idle_since[resource_id as usize] = None;
        let state = &mut self.states[resource_id as usize];
        let generation = match state {
            ResourceState::Idle { next_generation } => {
                let g = *next_generation;
                *state = ResourceState::Leased { generation: g };
                g
            }
            ResourceState::Leased { .. } => {
                panic!("mint_lease on already-leased resource_id={resource_id}")
            }
            ResourceState::Retired { .. } => {
                panic!("mint_lease on retired resource_id={resource_id}")
            }
        };
        let handle = self
            .resources
            .get(resource_id as usize)
            .and_then(|r| r.as_ref())
            .expect("resource present for non-retired slot")
            .clone();
        // SAFETY: this is the pool implementation that owns
        // `resource_id` under `self.pool_id`. The state-machine
        // guarantees `generation` was just minted for this resource
        // and is not duplicated against any outstanding lease — see
        // the `Idle { next_generation }` arm above which transitions
        // to `Leased { generation }` atomically with the lease mint.
        unsafe {
            pool_internal::lease_new(
                self.pool_id,
                pool_internal::resource_id_from_raw(resource_id),
                generation,
                handle,
            )
        }
    }

    fn track_dispatch(
        &mut self,
        resource_id: u32,
        generation: u64,
        slot: &DeferredReply<WorkerPoolReply<H>>,
    ) {
        let shared: Arc<DeferredSlotShared> = handle_shared(deferred_handle_ref(slot)).clone();
        self.in_flight.push(InFlightDispatch {
            resource_id,
            generation,
            slot: shared,
        });
    }

    fn dispatch_to_next_waiter(&mut self, resource_id: u32) -> Option<Effect<Self>> {
        self.sweep_waiters();
        while let Some(slab_idx) = self.waiter_queue.pop_front() {
            let waiter = match self.remove_waiter_slot(slab_idx) {
                Some(w) => w,
                None => continue,
            };
            let lease = self.mint_lease(resource_id);
            let generation = lease.generation();
            self.track_dispatch(resource_id, generation, &waiter.reply);
            return Some(reply_to::<Self>(
                waiter.reply,
                WorkerPoolReply::Acquire(AcquireOutcome::Acquired(lease)),
            ));
        }
        self.idle.push_back(resource_id);
        None
    }

    fn handle_acquire(&mut self, ctx: &mut Context<'_, S, WorkerPoolReply<H>>) -> Effect<Self> {
        if self.closed.is_some() {
            self.counters.closed += 1;
            return reply(WorkerPoolReply::Acquire(AcquireOutcome::Closed));
        }

        // Acquire-with-resource and acquire-as-waiter both need a
        // deferred slot so the in-flight tracker can observe the
        // delivery and recover the resource if the caller cancels.
        // `reply()` (immediate) gives no observable slot. So always
        // take_reply_slot first; classify the outcome after.
        let slot = match ctx.take_reply_slot() {
            Ok(s) => s,
            Err(tina::TakeReplySlotError::NoCaller) => {
                self.counters.no_caller_drops += 1;
                return noop();
            }
            Err(tina::TakeReplySlotError::CrossShardUnsupported) => {
                self.counters.wrong_shard += 1;
                // No slot to reply through; surface the typed outcome
                // via the runtime's normal reply path. CrossShard
                // means the caller's own shard handles the reply
                // locally on its side, not the pool's.
                return reply(WorkerPoolReply::Acquire(AcquireOutcome::WrongShard));
            }
        };

        self.handle_acquire_slot(slot)
    }

    fn handle_acquire_slot(&mut self, slot: DeferredReply<WorkerPoolReply<H>>) -> Effect<Self> {
        if let Some(resource_id) = self.idle.pop_front() {
            let lease = self.mint_lease(resource_id);
            let generation = lease.generation();
            self.track_dispatch(resource_id, generation, &slot);
            return reply_to::<Self>(
                slot,
                WorkerPoolReply::Acquire(AcquireOutcome::Acquired(lease)),
            );
        }

        if self.free_waiter_slots.is_empty() {
            self.counters.full += 1;
            return reply_to::<Self>(slot, WorkerPoolReply::Acquire(AcquireOutcome::Full));
        }

        let slab_idx = self.alloc_waiter_slot();
        self.store_waiter_slot(slab_idx, Waiter { reply: slot });
        self.waiter_queue.push_back(slab_idx);
        self.bump_waiter_high_water();
        noop()
    }

    fn bump_waiter_high_water(&mut self) {
        if self.live_waiters > self.counters.high_water_waiters {
            self.counters.high_water_waiters = self.live_waiters;
        }
    }

    fn handle_release(
        &mut self,
        lease: PoolLease<H>,
        disposition: ReleaseDisposition,
    ) -> Effect<Self> {
        let (lease_pool_id, lease_resource_id, lease_generation, _handle) =
            pool_internal::lease_into_parts(lease);

        if lease_pool_id != self.pool_id {
            return reply(WorkerPoolReply::Release(ReleaseOutcome::StaleLease));
        }
        let raw_idx = lease_resource_id.get();
        let Some(state) = self.states.get(raw_idx as usize) else {
            return reply(WorkerPoolReply::Release(ReleaseOutcome::StaleLease));
        };

        // Force-closed pools retire the resource on release and tell
        // the caller the lease is stale.
        if matches!(self.closed, Some(CloseMode::Force)) {
            if let ResourceState::Leased { generation } = state {
                if *generation == lease_generation {
                    self.retire_slot(raw_idx);
                }
            }
            return reply(WorkerPoolReply::Release(ReleaseOutcome::PoolClosed));
        }

        match state {
            ResourceState::Leased { generation } if *generation == lease_generation => {
                if matches!(disposition, ReleaseDisposition::Retire) {
                    self.retire_slot(raw_idx);
                    return reply(WorkerPoolReply::Release(ReleaseOutcome::Retired));
                }
                let next_generation = lease_generation.saturating_add(1);
                self.states[raw_idx as usize] = ResourceState::Idle { next_generation };
                // Back to idle: re-stamp on the next maintenance sweep.
                self.idle_since[raw_idx as usize] = None;
                if self.closed.is_some() {
                    // Drain mode: release is honored as Released, the
                    // resource sits Idle but cannot be re-acquired
                    // because new acquires are rejected.
                    return reply(WorkerPoolReply::Release(ReleaseOutcome::Released));
                }
                let mut effects = Vec::with_capacity(2);
                if let Some(handover) = self.dispatch_to_next_waiter(raw_idx) {
                    effects.push(handover);
                }
                effects.push(reply(WorkerPoolReply::Release(ReleaseOutcome::Released)));
                batch(effects)
            }
            ResourceState::Leased { generation } if *generation > lease_generation => {
                reply(WorkerPoolReply::Release(ReleaseOutcome::DoubleRelease))
            }
            // Lease generation ahead of pool record — should be unreachable.
            ResourceState::Leased { .. } => {
                reply(WorkerPoolReply::Release(ReleaseOutcome::StaleLease))
            }
            ResourceState::Idle { next_generation } if *next_generation > lease_generation => {
                reply(WorkerPoolReply::Release(ReleaseOutcome::DoubleRelease))
            }
            ResourceState::Idle { .. } => {
                reply(WorkerPoolReply::Release(ReleaseOutcome::StaleLease))
            }
            ResourceState::Retired { .. } => {
                reply(WorkerPoolReply::Release(ReleaseOutcome::Retired))
            }
        }
    }

    fn handle_close(&mut self, mode: CloseMode) -> Effect<Self> {
        // Idempotent. Force can upgrade Drain; not the other way.
        let upgraded_to_force = match (self.closed, mode) {
            (None, CloseMode::Force) => {
                self.closed = Some(CloseMode::Force);
                true
            }
            (None, CloseMode::Drain) => {
                self.closed = Some(CloseMode::Drain);
                false
            }
            (Some(CloseMode::Drain), CloseMode::Force) => {
                self.closed = Some(CloseMode::Force);
                true
            }
            _ => false,
        };

        let mut effects: Vec<Effect<Self>> = Vec::new();

        // Force-close retires every outstanding lease right now. The
        // public contract says force marks outstanding leases stale —
        // not "marks them stale once each caller eventually
        // releases." Late releases land on Retired state and get
        // PoolClosed without owning resource cleanup. Drops the
        // resource handle promptly so heavy H types (connections,
        // files) release.
        if upgraded_to_force {
            for idx in 0..self.states.len() {
                if matches!(self.states[idx], ResourceState::Leased { .. }) {
                    self.retire_slot(idx as u32);
                }
            }
            // In-flight dispatches whose resource we just retired
            // can never complete cleanly — drop their tracker entries
            // so the next sweep_in_flight doesn't double-recover.
            self.in_flight.clear();
            // Idle resources also become unreachable under Force.
            // They keep ResourceState::Idle so a stray release of an
            // already-returned generation still reports DoubleRelease
            // accurately, but they won't be handed out (the
            // closed.is_some() check at the top of handle_acquire
            // prevents that).
        }

        for idx in 0..self.waiter_slab.len() {
            if let Some(waiter) = self.remove_waiter_slot(idx as u32) {
                self.counters.closed += 1;
                effects.push(reply_to::<Self>(
                    waiter.reply,
                    WorkerPoolReply::Acquire(AcquireOutcome::Closed),
                ));
            }
        }
        self.waiter_queue.clear();

        effects.push(reply(WorkerPoolReply::Closed));
        batch(effects)
    }

    // Lifetime sweep at logical time `now`. Retires idle resources past
    // the policy; reports (does not retire) over-age leased resources.
    fn handle_maintain(&mut self, now: Instant) -> Effect<Self> {
        let mut report = ResourcePolicyReport::default();
        // Close owns shutdown semantics. A maintenance tick that races a
        // close must not retire slots — in particular, force-close keeps
        // idle slots in `Idle` state on purpose so a stray late release
        // still reports `DoubleRelease`; retiring them here would change
        // that to `Retired`. So report current shape and retire nothing.
        if self.closed.is_some() {
            self.fill_report_tail(&mut report);
            return reply(WorkerPoolReply::Maintenance(report));
        }
        let Some(lifetime) = self.lifetime else {
            // No policy: report current shape, retire nothing.
            self.fill_report_tail(&mut report);
            return reply(WorkerPoolReply::Maintenance(report));
        };

        let mut retired_any = false;
        for idx in 0..self.states.len() {
            match self.states[idx] {
                ResourceState::Idle { .. } => {
                    report.inspected += 1;
                    // First sweep that sees the slot idle stamps the
                    // idle clock; total age runs from creation.
                    let idle_since = *self.idle_since[idx].get_or_insert(now);
                    let idle_for = now.saturating_duration_since(idle_since);
                    let age = self.created_at[idx].map(|c| now.saturating_duration_since(c));

                    let over_age = match (lifetime.max_lifetime, age) {
                        (Some(max), Some(age)) => age >= max,
                        _ => false,
                    };
                    let over_idle = match lifetime.max_idle {
                        Some(max) => idle_for >= max,
                        None => false,
                    };
                    // Max-age takes precedence in the reason — it is the
                    // stronger reason to retire.
                    let reason = if over_age {
                        Some(RetireReason::MaxLifetime)
                    } else if over_idle {
                        Some(RetireReason::IdleTimeout)
                    } else {
                        None
                    };
                    if let Some(reason) = reason {
                        // SAFETY: idx < states.len(), so the raw resource
                        // id is in range for this pool.
                        let resource_id =
                            unsafe { pool_internal::resource_id_from_raw(idx as u32) };
                        self.retire_slot(idx as u32);
                        retired_any = true;
                        report.retired.push(RetiredResource {
                            resource_id,
                            reason,
                            check_point: PolicyCheckPoint::ScheduledMaintenance,
                        });
                    }
                }
                ResourceState::Leased { .. } => {
                    report.inspected += 1;
                    let age = self.created_at[idx].map(|c| now.saturating_duration_since(c));
                    let over_age = match (lifetime.max_lifetime, age) {
                        (Some(max), Some(age)) => age >= max,
                        _ => false,
                    };
                    if over_age {
                        // Reported old, never stolen. The caller still
                        // holds a valid lease.
                        let resource_id =
                            unsafe { pool_internal::resource_id_from_raw(idx as u32) };
                        report.over_age_leased.push(resource_id);
                    }
                }
                ResourceState::Retired { .. } => {}
            }
        }

        if retired_any {
            let states = &self.states;
            self.idle
                .retain(|i| matches!(states[*i as usize], ResourceState::Idle { .. }));
        }

        self.fill_report_tail(&mut report);
        reply(WorkerPoolReply::Maintenance(report))
    }

    // Fill the count tail of a policy report from current state.
    fn fill_report_tail(&self, report: &mut ResourcePolicyReport) {
        let mut available = 0usize;
        let mut leased = 0usize;
        let mut retired_slots = 0usize;
        for s in &self.states {
            match s {
                ResourceState::Idle { .. } => available += 1,
                ResourceState::Leased { .. } => leased += 1,
                ResourceState::Retired { .. } => retired_slots += 1,
            }
        }
        report.available_after = available;
        report.leased_after = leased;
        report.retired_slots = retired_slots;
    }

    // Install a fresh handle into a retired slot. Owner-driven refill;
    // never replaces a live (idle or leased) resource.
    fn handle_refill(&mut self, resource_id: u32, handle: H, now: Instant) -> Effect<Self> {
        if self.closed.is_some() {
            return reply(WorkerPoolReply::Refilled(RefillOutcome::Closed));
        }
        let next_generation = match self.states.get(resource_id as usize) {
            None => return reply(WorkerPoolReply::Refilled(RefillOutcome::UnknownResource)),
            Some(ResourceState::Retired { next_generation }) => *next_generation,
            // A live (idle or leased) slot is never replaced behind a
            // caller's back.
            Some(_) => return reply(WorkerPoolReply::Refilled(RefillOutcome::NotRetired)),
        };

        let idx = resource_id as usize;
        // Fresh resource starts idle "now". The generation continues from
        // where the retired slot left off, so leases minted before and
        // after the refill stay distinguishable by generation.
        self.states[idx] = ResourceState::Idle { next_generation };
        self.resources[idx] = Some(handle);
        self.created_at[idx] = self.lifetime.is_some().then_some(now);
        self.idle_since[idx] = self.lifetime.is_some().then_some(now);

        // A fresh idle resource may satisfy a parked waiter.
        // `dispatch_to_next_waiter` either hands it off or returns it to
        // the idle queue — never push it ourselves first.
        let mut effects = Vec::with_capacity(2);
        if let Some(handover) = self.dispatch_to_next_waiter(resource_id) {
            effects.push(handover);
        }
        effects.push(reply(WorkerPoolReply::Refilled(RefillOutcome::Refilled)));
        batch(effects)
    }
}

/// Build an [`Effect`] that acquires a resource from the pool.
///
/// Sugar over `call(pool, WorkerPoolMsg::Acquire, timeout).then(...)`.
/// The translator receives the raw
/// `CallOutcome<WorkerPoolReply<H>>` so callers can inspect both the
/// transport layer (`Full` / `Closed` / `Timeout` from the runtime)
/// and the pool layer (the inner `WorkerPoolReply::Acquire(...)`)
/// independently.
///
/// Use [`acquire_result_effect`] when you want the layered outcome
/// folded into `Result<PoolLease<H>, AcquireFailure>` for you.
/// Use [`acquire_with_handle_effect`] when the caller wants a
/// [`tina::CallHandle`] for cancellation.
pub fn acquire_effect<I, H, F, M>(
    pool: tina::Address<WorkerPoolMsg<H>, WorkerPoolReply<H>>,
    timeout: std::time::Duration,
    translator: F,
) -> Effect<I>
where
    H: Send + 'static,
    I: Isolate<Message = M, Call = RuntimeCall<M>>,
    F: FnOnce(crate::call::CallOutcome<WorkerPoolReply<H>>) -> M + 'static,
    M: 'static,
{
    crate::call::call(pool, WorkerPoolMsg::Acquire, timeout).then(translator)
}

/// Build an [`Effect`] that acquires a resource and folds the
/// layered outcome through [`try_acquired`] before handing it to the
/// translator.
///
/// The translator receives `Result<PoolLease<H>, AcquireFailure>`.
/// Pool-layer outcomes (`Full` / `Closed` / `WrongShard`) and
/// transport-layer outcomes (`CallTimeout` / `CallFull` /
/// `CallClosed`) stay distinct as enum variants of `AcquireFailure`,
/// so this fold is no-loss.
///
/// Prefer this when the consumer does not need the raw
/// `WorkerPoolReply` shape. Use [`acquire_effect`] when you do.
pub fn acquire_result_effect<I, H, F, M>(
    pool: tina::Address<WorkerPoolMsg<H>, WorkerPoolReply<H>>,
    timeout: std::time::Duration,
    translator: F,
) -> Effect<I>
where
    H: Send + 'static,
    I: Isolate<Message = M, Call = RuntimeCall<M>>,
    F: FnOnce(Result<PoolLease<H>, AcquireFailure>) -> M + 'static,
    M: 'static,
{
    acquire_effect(pool, timeout, move |outcome| {
        translator(try_acquired(outcome))
    })
}

/// Build an `(Effect, CallHandle)` pair for cancellable acquire.
///
/// The caller stores the [`tina::CallHandle`] and later fires
/// `cancel_call(handle)` to close the wait. The pool's sweep on the
/// next handler turn reclaims the waiter slot.
pub fn acquire_with_handle_effect<I, H, F, M>(
    pool: tina::Address<WorkerPoolMsg<H>, WorkerPoolReply<H>>,
    timeout: std::time::Duration,
    translator: F,
) -> (Effect<I>, tina::CallHandle<WorkerPoolReply<H>>)
where
    H: Send + 'static,
    I: Isolate<Message = M, Call = RuntimeCall<M>>,
    F: FnOnce(crate::call::CallOutcome<WorkerPoolReply<H>>) -> M + 'static,
    M: 'static,
{
    crate::call::call_cancelable(pool, WorkerPoolMsg::Acquire, timeout).then(translator)
}

/// Build an [`Effect`] that releases a lease back to the pool.
///
/// Sugar over `call(pool, WorkerPoolMsg::Release { ... }, timeout).then(...)`.
/// The translator receives the raw
/// `CallOutcome<WorkerPoolReply<H>>`. No drop-magic, no hidden
/// retry. Pool address and disposition are visible at the call site.
///
/// Use [`release_result_effect`] when you want the layered outcome
/// folded into `Result<(), ReleaseFailure>` for you.
pub fn release_effect<I, H, F, M>(
    lease: PoolLease<H>,
    pool: tina::Address<WorkerPoolMsg<H>, WorkerPoolReply<H>>,
    disposition: ReleaseDisposition,
    timeout: std::time::Duration,
    translator: F,
) -> Effect<I>
where
    H: Send + 'static,
    I: Isolate<Message = M, Call = RuntimeCall<M>>,
    F: FnOnce(crate::call::CallOutcome<WorkerPoolReply<H>>) -> M + 'static,
    M: 'static,
{
    crate::call::call(pool, WorkerPoolMsg::Release { lease, disposition }, timeout).then(translator)
}

/// Build an [`Effect`] that releases a lease and folds the layered
/// outcome through [`try_released`] before handing it to the
/// translator.
///
/// The translator receives `Result<(), ReleaseFailure>`. Pool-layer
/// outcomes (`Retired` / `StaleLease` / `DoubleRelease` /
/// `PoolClosed`) and transport-layer outcomes (`CallTimeout` /
/// `CallFull` / `CallClosed`) stay distinct as variants of
/// `ReleaseFailure`.
///
/// Prefer this when the consumer does not need the raw
/// `WorkerPoolReply` shape. Use [`release_effect`] when you do.
pub fn release_result_effect<I, H, F, M>(
    lease: PoolLease<H>,
    pool: tina::Address<WorkerPoolMsg<H>, WorkerPoolReply<H>>,
    disposition: ReleaseDisposition,
    timeout: std::time::Duration,
    translator: F,
) -> Effect<I>
where
    H: Send + 'static,
    I: Isolate<Message = M, Call = RuntimeCall<M>>,
    F: FnOnce(Result<(), ReleaseFailure>) -> M + 'static,
    M: 'static,
{
    release_effect(lease, pool, disposition, timeout, move |outcome| {
        translator(try_released(outcome))
    })
}

/// Sugar for `call(pool, WorkerPoolMsg::PressureReport, timeout).then(...)`.
pub fn pressure_effect<I, H, F, M>(
    pool: tina::Address<WorkerPoolMsg<H>, WorkerPoolReply<H>>,
    timeout: std::time::Duration,
    translator: F,
) -> Effect<I>
where
    H: Send + 'static,
    I: Isolate<Message = M, Call = RuntimeCall<M>>,
    F: FnOnce(crate::call::CallOutcome<WorkerPoolReply<H>>) -> M + 'static,
    M: 'static,
{
    crate::call::call(pool, WorkerPoolMsg::PressureReport, timeout).then(translator)
}

/// Fold a pool acquire reply into `Result<PoolLease<H>, AcquireFailure>`.
///
/// Collapses the three-layer match every consumer would otherwise
/// write: `CallOutcome` → `WorkerPoolReply::Acquire` →
/// `AcquireOutcome::Acquired`. Each non-Acquired outcome becomes a
/// distinct [`AcquireFailure`] variant; `Full` / `Closed` /
/// `WrongShard` stay distinguishable from transport-level failures.
pub fn try_acquired<H>(
    outcome: crate::call::CallOutcome<WorkerPoolReply<H>>,
) -> Result<PoolLease<H>, AcquireFailure>
where
    H: Send + 'static,
{
    use crate::call::CallOutcome;
    match outcome {
        CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Acquired(lease))) => {
            Ok(lease)
        }
        CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Full)) => {
            Err(AcquireFailure::Full)
        }
        CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Closed)) => {
            Err(AcquireFailure::Closed)
        }
        CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::WrongShard)) => {
            Err(AcquireFailure::WrongShard)
        }
        CallOutcome::Replied(_) => Err(AcquireFailure::WrongReply),
        CallOutcome::Timeout => Err(AcquireFailure::CallTimeout),
        CallOutcome::Full => Err(AcquireFailure::CallFull),
        CallOutcome::Closed => Err(AcquireFailure::CallClosed),
        CallOutcome::Rejected(reason) => Err(AcquireFailure::CallRejected(reason)),
    }
}

/// Fold a pool release reply into `Result<(), ReleaseFailure>`.
///
/// `Released` → `Ok(())`. Every other outcome becomes a typed
/// [`ReleaseFailure`].
pub fn try_released<H>(
    outcome: crate::call::CallOutcome<WorkerPoolReply<H>>,
) -> Result<(), ReleaseFailure>
where
    H: Send + 'static,
{
    use crate::call::CallOutcome;
    match outcome {
        CallOutcome::Replied(WorkerPoolReply::Release(ReleaseOutcome::Released)) => Ok(()),
        CallOutcome::Replied(WorkerPoolReply::Release(ReleaseOutcome::Retired)) => {
            Err(ReleaseFailure::Retired)
        }
        CallOutcome::Replied(WorkerPoolReply::Release(ReleaseOutcome::StaleLease)) => {
            Err(ReleaseFailure::StaleLease)
        }
        CallOutcome::Replied(WorkerPoolReply::Release(ReleaseOutcome::DoubleRelease)) => {
            Err(ReleaseFailure::DoubleRelease)
        }
        CallOutcome::Replied(WorkerPoolReply::Release(ReleaseOutcome::PoolClosed)) => {
            Err(ReleaseFailure::PoolClosed)
        }
        CallOutcome::Replied(_) => Err(ReleaseFailure::WrongReply),
        CallOutcome::Timeout => Err(ReleaseFailure::CallTimeout),
        CallOutcome::Full => Err(ReleaseFailure::CallFull),
        CallOutcome::Closed => Err(ReleaseFailure::CallClosed),
        CallOutcome::Rejected(reason) => Err(ReleaseFailure::CallRejected(reason)),
    }
}

/// Sugar for `call(pool, WorkerPoolMsg::Close(mode), timeout).then(...)`.
pub fn close_effect<I, H, F, M>(
    pool: tina::Address<WorkerPoolMsg<H>, WorkerPoolReply<H>>,
    mode: CloseMode,
    timeout: std::time::Duration,
    translator: F,
) -> Effect<I>
where
    H: Send + 'static,
    I: Isolate<Message = M, Call = RuntimeCall<M>>,
    F: FnOnce(crate::call::CallOutcome<WorkerPoolReply<H>>) -> M + 'static,
    M: 'static,
{
    crate::call::call(pool, WorkerPoolMsg::Close(mode), timeout).then(translator)
}

/// Sugar for `call(pool, WorkerPoolMsg::Maintain { now }, timeout).then(...)`.
///
/// Drive this off a Tina timer (`ctx.now()` for `now`). The reply is a
/// [`WorkerPoolReply::Maintenance`] carrying a
/// [`tina::pool::ResourcePolicyReport`] naming what the sweep retired.
pub fn maintain_effect<I, H, F, M>(
    pool: tina::Address<WorkerPoolMsg<H>, WorkerPoolReply<H>>,
    now: Instant,
    timeout: std::time::Duration,
    translator: F,
) -> Effect<I>
where
    H: Send + 'static,
    I: Isolate<Message = M, Call = RuntimeCall<M>>,
    F: FnOnce(crate::call::CallOutcome<WorkerPoolReply<H>>) -> M + 'static,
    M: 'static,
{
    crate::call::call(pool, WorkerPoolMsg::Maintain { now }, timeout).then(translator)
}

/// Sugar for `call(pool, WorkerPoolMsg::Refill { .. }, timeout).then(...)`.
///
/// Owner-driven capacity refill: hand a fresh resource handle back for a
/// slot the pool retired. The reply is a [`WorkerPoolReply::Refilled`].
pub fn refill_effect<I, H, F, M>(
    pool: tina::Address<WorkerPoolMsg<H>, WorkerPoolReply<H>>,
    resource_id: u32,
    handle: H,
    now: Instant,
    timeout: std::time::Duration,
    translator: F,
) -> Effect<I>
where
    H: Send + 'static,
    I: Isolate<Message = M, Call = RuntimeCall<M>>,
    F: FnOnce(crate::call::CallOutcome<WorkerPoolReply<H>>) -> M + 'static,
    M: 'static,
{
    crate::call::call(
        pool,
        WorkerPoolMsg::Refill {
            resource_id,
            handle,
            now,
        },
        timeout,
    )
    .then(translator)
}

// Manual Isolate impl: message and reply types are generic over H,
// which the runtime_isolate macro doesn't handle.
impl<H, S> Isolate for WorkerPool<H, S>
where
    H: Send + Clone + 'static,
    S: Shard + 'static,
{
    type Message = WorkerPoolMsg<H>;
    type Reply = WorkerPoolReply<H>;
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Call = RuntimeCall<WorkerPoolMsg<H>>;
    type Fact = Infallible;
    type Shard = S;

    fn handle(
        &mut self,
        msg: WorkerPoolMsg<H>,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        // Always sweep first: cancelled waiters and rejected
        // dispatches both need to be reclaimed before any state
        // decision in this turn.
        self.sweep_waiters();
        self.sweep_in_flight();
        match msg {
            WorkerPoolMsg::Acquire => self.handle_acquire(ctx),
            WorkerPoolMsg::Release { lease, disposition } => {
                self.handle_release(lease, disposition)
            }
            WorkerPoolMsg::Close(mode) => self.handle_close(mode),
            WorkerPoolMsg::PressureReport => reply(WorkerPoolReply::Pressure(self.pressure())),
            WorkerPoolMsg::Maintain { now } => self.handle_maintain(now),
            WorkerPoolMsg::Refill {
                resource_id,
                handle,
                now,
            } => self.handle_refill(resource_id, handle, now),
        }
    }

    fn handle_call(&mut self, msg: WorkerPoolMsg<H>, call: CallContext<'_, Self>) -> Effect<Self> {
        // Always sweep first: cancelled waiters and rejected
        // dispatches both need to be reclaimed before any state
        // decision in this turn.
        self.sweep_waiters();
        self.sweep_in_flight();
        match msg {
            WorkerPoolMsg::Acquire => {
                if self.closed.is_some() {
                    self.counters.closed += 1;
                    return call.reply(WorkerPoolReply::Acquire(AcquireOutcome::Closed));
                }
                let slot = call.into_request_context().into_deferred();
                self.handle_acquire_slot(slot)
            }
            WorkerPoolMsg::Release { lease, disposition } => {
                let _ = call;
                self.handle_release(lease, disposition)
            }
            WorkerPoolMsg::Close(mode) => {
                let _ = call;
                self.handle_close(mode)
            }
            WorkerPoolMsg::PressureReport => call.reply(WorkerPoolReply::Pressure(self.pressure())),
            WorkerPoolMsg::Maintain { now } => {
                let _ = call;
                self.handle_maintain(now)
            }
            WorkerPoolMsg::Refill {
                resource_id,
                handle,
                now,
            } => {
                let _ = call;
                self.handle_refill(resource_id, handle, now)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tina::DeferredSlotShared;
    use tina::runtime_internal::{deferred_from_handle, handle_from_shared};

    fn fake_slot(id: u64) -> DeferredReply<WorkerPoolReply<u32>> {
        let shared = Arc::new(DeferredSlotShared::new(
            id,
            std::any::TypeId::of::<WorkerPoolReply<u32>>(),
        ));
        deferred_from_handle(handle_from_shared(shared))
    }

    fn fake_closed_slot(id: u64) -> DeferredReply<WorkerPoolReply<u32>> {
        let shared = Arc::new(DeferredSlotShared::new(
            id,
            std::any::TypeId::of::<WorkerPoolReply<u32>>(),
        ));
        shared.set_state(DeferredSlotState::Closed);
        deferred_from_handle(handle_from_shared(shared))
    }

    fn pool_with_waiters(max_waiters: usize) -> WorkerPool<u32, tina::SingleShard> {
        WorkerPool::new(
            PoolConfig {
                capacity: 1,
                max_waiters,
            },
            vec![7],
        )
    }

    #[test]
    fn waiter_free_stack_reuses_reclaimed_slot_and_tracks_live_count() {
        let mut pool = pool_with_waiters(2);
        let a = pool.alloc_waiter_slot();
        pool.store_waiter_slot(
            a,
            Waiter {
                reply: fake_slot(10),
            },
        );
        let b = pool.alloc_waiter_slot();
        pool.store_waiter_slot(
            b,
            Waiter {
                reply: fake_slot(11),
            },
        );
        assert_eq!(pool.live_waiter_count(), 2);
        assert!(pool.free_waiter_slots.is_empty());

        let removed = pool.remove_waiter_slot(a).expect("slot a occupied");
        drop(removed);
        assert_eq!(pool.live_waiter_count(), 1);
        let reused = pool.alloc_waiter_slot();
        assert_eq!(reused, a, "reclaimed slot should be reused directly");
        assert_eq!(pool.live_waiter_count(), 1);
        assert_eq!(
            b, 1,
            "initial free stack should still allocate stable slots"
        );
    }

    #[test]
    fn waiter_sweep_reclaims_closed_slot_and_prunes_fifo_queue() {
        let mut pool = pool_with_waiters(3);
        for reply in [fake_slot(10), fake_closed_slot(11), fake_slot(12)] {
            let idx = pool.alloc_waiter_slot();
            pool.store_waiter_slot(idx, Waiter { reply });
            pool.waiter_queue.push_back(idx);
        }
        assert_eq!(pool.live_waiter_count(), 3);

        pool.sweep_waiters();

        assert_eq!(pool.live_waiter_count(), 2);
        assert_eq!(pool.counters.cancel, 1);
        let remaining: Vec<u32> = pool.waiter_queue.iter().copied().collect();
        assert_eq!(remaining, vec![0, 2]);
        assert!(pool.free_waiter_slots.contains(&1));
    }
}
