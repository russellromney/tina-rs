//! Bounded worker pool isolate.
//!
//! Caller acquires with `call_with_handle(pool, WorkerPoolMsg::Acquire,
//! timeout).reply(...)` and stores the [`tina::CallHandle`] to be able
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

use std::convert::Infallible;
use std::marker::PhantomData;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tina::pool::{
    AcquireFailure, AcquireOutcome, CloseMode, PoolConfig, PoolId, PoolLease, PoolPressureReport,
    ReleaseDisposition, ReleaseFailure, ReleaseOutcome, runtime_internal as pool_internal,
};
use tina::runtime_internal::{deferred_handle_ref, handle_shared};
use tina::{
    Context, DeferredReply, DeferredSlotShared, DeferredSlotState, Effect, Isolate, Outbound,
    Shard, batch, noop, reply, reply_to,
};

use crate::call::RuntimeCall;

fn mint_pool_id() -> PoolId {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let raw = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nz = NonZeroU64::new(raw).expect("pool id counter wrapped to zero");
    pool_internal::pool_id_from_raw(nz)
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
    Retired,
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
    resources: Vec<Option<H>>,
    states: Vec<ResourceState>,
    idle: std::collections::VecDeque<u32>,
    waiter_slab: Vec<Option<Waiter<H>>>,
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
    pub fn new(config: PoolConfig, resources: Vec<H>) -> Self {
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
        for _ in 0..max_waiters {
            waiter_slab.push(None);
        }
        let resources_opt: Vec<Option<H>> = resources.into_iter().map(Some).collect();
        Self {
            pool_id,
            config,
            resources: resources_opt,
            states,
            idle,
            waiter_slab,
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
                ResourceState::Retired => {}
            }
        }
        PoolPressureReport {
            capacity: self.config.capacity,
            available,
            leased,
            waiters: self.live_waiter_count(),
            max_waiters: self.config.max_waiters,
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
        self.waiter_slab.iter().filter(|s| s.is_some()).count()
    }

    // Reclaim waiter slots whose deferred reply slot is no longer
    // Open. The runtime cannot distinguish caller-cancel vs
    // caller-timeout at the slot level (that lives in trace facts), so
    // both increment `cancel_count` here.
    fn sweep_waiters(&mut self) {
        let mut reclaimed_any = false;
        for slot in self.waiter_slab.iter_mut() {
            let drop_it = slot
                .as_ref()
                .is_some_and(|w| w.reply.state() != DeferredSlotState::Open);
            if drop_it {
                *slot = None;
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
                self.idle.push_back(resource_id);
                self.counters.dispatch_recovered += 1;
            }
            // Released, retired, or otherwise advanced — nothing to recover.
            _ => {}
        }
    }

    fn alloc_waiter_slot(&mut self) -> u32 {
        for (i, slot) in self.waiter_slab.iter().enumerate() {
            if slot.is_none() {
                return i as u32;
            }
        }
        unreachable!("alloc_waiter_slot called when slab is full");
    }

    fn mint_lease(&mut self, resource_id: u32) -> PoolLease<H> {
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
            ResourceState::Retired => {
                panic!("mint_lease on retired resource_id={resource_id}")
            }
        };
        let handle = self
            .resources
            .get(resource_id as usize)
            .and_then(|r| r.as_ref())
            .expect("resource present for non-retired slot")
            .clone();
        pool_internal::lease_new(
            self.pool_id,
            pool_internal::resource_id_from_raw(resource_id),
            generation,
            handle,
        )
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
            let waiter = match self.waiter_slab[slab_idx as usize].take() {
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

        if let Some(resource_id) = self.idle.pop_front() {
            let lease = self.mint_lease(resource_id);
            let generation = lease.generation();
            self.track_dispatch(resource_id, generation, &slot);
            return reply_to::<Self>(
                slot,
                WorkerPoolReply::Acquire(AcquireOutcome::Acquired(lease)),
            );
        }

        if self.live_waiter_count() >= self.config.max_waiters {
            self.counters.full += 1;
            return reply_to::<Self>(slot, WorkerPoolReply::Acquire(AcquireOutcome::Full));
        }

        let slab_idx = self.alloc_waiter_slot();
        self.waiter_slab[slab_idx as usize] = Some(Waiter { reply: slot });
        self.waiter_queue.push_back(slab_idx);
        noop()
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
                    self.states[raw_idx as usize] = ResourceState::Retired;
                    self.resources[raw_idx as usize] = None;
                    self.counters.retired += 1;
                }
            }
            return reply(WorkerPoolReply::Release(ReleaseOutcome::PoolClosed));
        }

        match state {
            ResourceState::Leased { generation } if *generation == lease_generation => {
                if matches!(disposition, ReleaseDisposition::Retire) {
                    self.states[raw_idx as usize] = ResourceState::Retired;
                    self.resources[raw_idx as usize] = None;
                    self.counters.retired += 1;
                    return reply(WorkerPoolReply::Release(ReleaseOutcome::Retired));
                }
                let next_generation = lease_generation.saturating_add(1);
                self.states[raw_idx as usize] = ResourceState::Idle { next_generation };
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
            ResourceState::Retired => reply(WorkerPoolReply::Release(ReleaseOutcome::Retired)),
        }
    }

    fn handle_close(&mut self, mode: CloseMode) -> Effect<Self> {
        // Idempotent. Force can upgrade Drain; not the other way.
        match (self.closed, mode) {
            (None, m) => self.closed = Some(m),
            (Some(CloseMode::Drain), CloseMode::Force) => self.closed = Some(CloseMode::Force),
            _ => {}
        }

        let mut effects: Vec<Effect<Self>> = Vec::new();
        for slot in self.waiter_slab.iter_mut() {
            if let Some(waiter) = slot.take() {
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
}

/// Build an [`Effect`] that acquires a resource from the pool.
///
/// Sugar over `call(pool, WorkerPoolMsg::Acquire, timeout).reply(...)`.
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
    crate::call::call(pool, WorkerPoolMsg::Acquire, timeout).reply(translator)
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
    crate::call::call_with_handle(pool, WorkerPoolMsg::Acquire, timeout).reply(translator)
}

/// Build an [`Effect`] that releases a lease back to the pool.
///
/// Sugar over `call(pool, WorkerPoolMsg::Release { ... }, timeout).reply(...)`.
/// No drop-magic, no hidden retry. Pool address and disposition are
/// visible at the call site.
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
    crate::call::call(pool, WorkerPoolMsg::Release { lease, disposition }, timeout)
        .reply(translator)
}

/// Sugar for `call(pool, WorkerPoolMsg::PressureReport, timeout).reply(...)`.
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
    crate::call::call(pool, WorkerPoolMsg::PressureReport, timeout).reply(translator)
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
    }
}

/// Sugar for `call(pool, WorkerPoolMsg::Close(mode), timeout).reply(...)`.
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
    crate::call::call(pool, WorkerPoolMsg::Close(mode), timeout).reply(translator)
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
    type Call = RuntimeCall<WorkerPoolMsg<H>>;
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
        }
    }
}
