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
//! the only deadline.

use std::convert::Infallible;
use std::marker::PhantomData;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};

use tina::pool::{
    AcquireOutcome, CloseMode, PoolConfig, PoolId, PoolLease, PoolPressureReport,
    ReleaseDisposition, ReleaseOutcome, ResourceId,
};
use tina::{
    Context, DeferredReply, DeferredSlotState, Effect, Isolate, Outbound, Shard, batch, noop,
    reply, reply_to,
};

use crate::call::RuntimeCall;

fn mint_pool_id() -> PoolId {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let raw = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nz = NonZeroU64::new(raw).expect("pool id counter wrapped to zero");
    PoolId::from_raw(nz)
}

/// Messages handled by [`WorkerPool`]. `H` is the resource handle.
pub enum WorkerPoolMsg<H>
where
    H: Send + 'static,
{
    /// Acquire one resource. Pool replies immediately with `Acquired` /
    /// `Full` / `Closed`, or parks the caller and replies later.
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

/// Bounded worker pool isolate.
///
/// `H` is the resource handle (e.g. `tina::Address<WorkerMsg, WReply>`);
/// must be cheap-clone + `Send`. `S` is the shard the pool runs on.
pub struct WorkerPool<H, S>
where
    H: Send + Clone + 'static,
    S: Shard + 'static,
{
    pool_id: PoolId,
    config: PoolConfig,
    resources: Vec<H>,
    states: Vec<ResourceState>,
    idle: std::collections::VecDeque<u32>,
    waiter_slab: Vec<Option<Waiter<H>>>,
    waiter_queue: std::collections::VecDeque<u32>,
    full_count: u64,
    timeout_count: u64,
    cancel_count: u64,
    retired_count: u64,
    closed_count: u64,
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
        Self {
            pool_id,
            config,
            resources,
            states,
            idle,
            waiter_slab,
            waiter_queue: std::collections::VecDeque::with_capacity(max_waiters),
            full_count: 0,
            timeout_count: 0,
            cancel_count: 0,
            retired_count: 0,
            closed_count: 0,
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
        let leased = self
            .states
            .iter()
            .filter(|s| matches!(s, ResourceState::Leased { .. }))
            .count();
        PoolPressureReport {
            capacity: self.config.capacity,
            available: self.idle.len(),
            leased,
            waiters: self.live_waiter_count(),
            max_waiters: self.config.max_waiters,
            full_count: self.full_count,
            timeout_count: self.timeout_count,
            cancel_count: self.cancel_count,
            retired_count: self.retired_count,
            closed_count: self.closed_count,
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
    fn sweep_waiters(&mut self) -> usize {
        let mut reclaimed = 0usize;
        let live_slab_indices: Vec<u32> = self
            .waiter_slab
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().map(|_| i as u32))
            .collect();
        for slab_idx in live_slab_indices {
            let entry = self.waiter_slab[slab_idx as usize]
                .as_ref()
                .expect("entry filtered as Some");
            if entry.reply.state() != DeferredSlotState::Open {
                self.waiter_slab[slab_idx as usize] = None;
                self.cancel_count += 1;
                reclaimed += 1;
            }
        }
        if reclaimed > 0 {
            let alive: std::collections::VecDeque<u32> = self
                .waiter_queue
                .iter()
                .copied()
                .filter(|idx| self.waiter_slab[*idx as usize].is_some())
                .collect();
            self.waiter_queue = alive;
        }
        reclaimed
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
        let handle = self.resources[resource_id as usize].clone();
        PoolLease::new(
            self.pool_id,
            ResourceId::from_raw(resource_id),
            generation,
            handle,
        )
    }

    fn dispatch_to_next_waiter(&mut self, resource_id: u32) -> Option<Effect<Self>> {
        self.sweep_waiters();
        while let Some(slab_idx) = self.waiter_queue.pop_front() {
            let waiter = match self.waiter_slab[slab_idx as usize].take() {
                Some(w) => w,
                None => continue,
            };
            let lease = self.mint_lease(resource_id);
            return Some(reply_to::<Self>(
                waiter.reply,
                WorkerPoolReply::Acquire(AcquireOutcome::Acquired(lease)),
            ));
        }
        self.idle.push_back(resource_id);
        None
    }

    fn handle_acquire(&mut self, ctx: &mut Context<'_, S, WorkerPoolReply<H>>) -> Effect<Self> {
        self.sweep_waiters();

        if self.closed.is_some() {
            return reply(WorkerPoolReply::Acquire(AcquireOutcome::Closed));
        }

        if let Some(resource_id) = self.idle.pop_front() {
            let lease = self.mint_lease(resource_id);
            return reply(WorkerPoolReply::Acquire(AcquireOutcome::Acquired(lease)));
        }

        if self.live_waiter_count() >= self.config.max_waiters {
            self.full_count += 1;
            return reply(WorkerPoolReply::Acquire(AcquireOutcome::Full));
        }

        match ctx.take_reply_slot() {
            Ok(slot) => {
                let slab_idx = self.alloc_waiter_slot();
                self.waiter_slab[slab_idx as usize] = Some(Waiter { reply: slot });
                self.waiter_queue.push_back(slab_idx);
                noop()
            }
            // Acquire delivered as plain `send`, not `call` — caller
            // can't get an outcome. Drop.
            Err(tina::TakeReplySlotError::NoCaller) => noop(),
            // Cross-shard captures aren't supported; surface as Full
            // so the caller picks a local-shard route.
            Err(tina::TakeReplySlotError::CrossShardUnsupported) => {
                self.full_count += 1;
                reply(WorkerPoolReply::Acquire(AcquireOutcome::Full))
            }
        }
    }

    fn handle_release(
        &mut self,
        lease: PoolLease<H>,
        disposition: ReleaseDisposition,
    ) -> Effect<Self> {
        let (lease_pool_id, lease_resource_id, lease_generation, _handle) = lease.into_parts();

        // Wrong-pool release is always stale.
        if lease_pool_id != self.pool_id {
            return reply(WorkerPoolReply::Release(ReleaseOutcome::StaleLease));
        }
        let raw_idx = lease_resource_id.get();
        let Some(state) = self.states.get(raw_idx as usize) else {
            return reply(WorkerPoolReply::Release(ReleaseOutcome::StaleLease));
        };

        // Force-closed pools reject every late release.
        if matches!(self.closed, Some(CloseMode::Force)) {
            return reply(WorkerPoolReply::Release(ReleaseOutcome::PoolClosed));
        }

        match state {
            ResourceState::Leased { generation } if *generation == lease_generation => {
                if matches!(disposition, ReleaseDisposition::Retire) || self.closed.is_some() {
                    self.states[raw_idx as usize] = ResourceState::Retired;
                    self.retired_count += 1;
                    return reply(WorkerPoolReply::Release(ReleaseOutcome::Retired));
                }
                self.states[raw_idx as usize] = ResourceState::Idle {
                    next_generation: lease_generation + 1,
                };
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
                self.closed_count += 1;
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
        self.sweep_waiters();
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
