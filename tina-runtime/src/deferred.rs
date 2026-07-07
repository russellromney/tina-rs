//! Runtime-owned bookkeeping for deferred reply slots.
//!
//! Slot-id allocation and the per-message pending-capture queue live in
//! [`tina::DeferredSlotRegistry`]. This module owns the *promoted*
//! slot table — slots whose handler turn finished and which now wait
//! for either a `reply_to` effect, a caller-close signal, or a sweep.
//!
//! Promise box belongs near runtime. RPC and bridges merely use box.

use std::sync::Arc;

use tina::{DeferredSlotShared, Effect, Isolate, IsolateId, reply_to, stop};

use crate::call::CallId;
use crate::trace::DeferredSlotId;

/// Where a deferred reply must be routed.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DeferredRouting {
    /// Caller lives on the same shard. Reply settles via the local
    /// pending isolate call.
    Local,

    /// Caller lives on another shard. Reply travels through the remote
    /// reply path.
    ///
    Remote {
        requester: crate::RegisteredAddress,
        cause: crate::CauseId,
    },
}

/// One promoted deferred slot the runtime is tracking.
pub(crate) struct DeferredSlotRecord {
    pub slot_id: DeferredSlotId,
    pub call_id: CallId,
    pub capturing_isolate: IsolateId,
    pub shared: Arc<DeferredSlotShared>,
    pub routing: DeferredRouting,
}

/// Promoted-slot table. Lives on the runtime; not shared.
#[derive(Default)]
pub(crate) struct PromotedSlots {
    slots: Vec<DeferredSlotRecord>,
}

impl PromotedSlots {
    pub fn push(&mut self, record: DeferredSlotRecord) {
        self.slots.push(record);
    }

    /// True when no slots are tracked. Lets the per-step sweep skip its
    /// scan entirely on shards that hold no promoted deferred replies.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Pop and return the slot record matching the given shared handle.
    pub fn take_by_handle(
        &mut self,
        shared: &Arc<DeferredSlotShared>,
    ) -> Option<DeferredSlotRecord> {
        let pos = self
            .slots
            .iter()
            .position(|s| Arc::ptr_eq(&s.shared, shared))?;
        // swap_remove: order does not matter, slots are looked up by id.
        Some(self.slots.swap_remove(pos))
    }

    /// Pop the slot tracking a given local call id.
    ///
    /// Only takes slots whose routing is [`DeferredRouting::Local`].
    /// Remote-routed slots can carry a `call_id` minted on another
    /// shard, so a local caller-liveness sweep must ignore them. Remote
    /// deferred slots are closed by their own routing path or isolate-stop
    /// cleanup, not by local pending-call lookup.
    pub fn take_by_local_call_id(&mut self, call_id: CallId) -> Option<DeferredSlotRecord> {
        let pos = self
            .slots
            .iter()
            .position(|s| matches!(s.routing, DeferredRouting::Local) && s.call_id == call_id)?;
        debug_assert!(matches!(self.slots[pos].routing, DeferredRouting::Local));
        Some(self.slots.swap_remove(pos))
    }

    /// Sweep slots whose only remaining strong reference is the
    /// promoted table's. Returns dropped records so the caller can
    /// emit terminal events.
    ///
    /// Drain every slot captured by the given isolate. Called when an
    /// isolate stops so the runtime can emit terminal facts eagerly
    /// instead of waiting for the user-side Rc to drop with the
    /// isolate's state.
    pub fn take_by_isolate(&mut self, isolate: IsolateId) -> Vec<DeferredSlotRecord> {
        let mut taken = Vec::new();
        let mut i = 0;
        while i < self.slots.len() {
            if self.slots[i].capturing_isolate == isolate {
                // swap_remove moves the tail into `i`; re-check that slot.
                taken.push(self.slots.swap_remove(i));
            } else {
                i += 1;
            }
        }
        taken
    }

    /// Single pass: dropping one record's `Rc` cannot cascade into
    /// dropping another record's `Rc` because they are independent
    /// allocations.
    pub fn sweep_dropped(&mut self) -> Vec<DeferredSlotRecord> {
        let mut dropped = Vec::new();
        let mut i = 0;
        while i < self.slots.len() {
            if Arc::strong_count(&self.slots[i].shared) <= 1 {
                // swap_remove moves the tail into `i`; re-check that slot.
                dropped.push(self.slots.swap_remove(i));
            } else {
                i += 1;
            }
        }
        dropped
    }
}

// ---------------------------------------------------------------------------
// PendingReplies: bounded helper for services that hold many promises.
//
// Mailbox holds messages. Pending box holds promises. Both need caps.
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicU64, Ordering};

use tina::{DeferredReply, DeferredSlotState};

fn mint_pending_replies_seq() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Bounded, named container for many in-flight deferred reply slots.
///
/// A frontend isolate (pool, sharded service, bridge worker) typically
/// captures one [`DeferredReply`] per inbound caller and stores it
/// keyed by the worker request id. `PendingReplies` is the blessed
/// container for that pattern: it has a hard cap, an explicit key
/// type, and visible counters.
///
/// First-form storage is a fixed-capacity slot table. The vector is
/// pre-sized at construction and never grows; an admission failure
/// returns [`InsertError::Full`] and bumps the full-reject counter.
///
/// Live ownership rule: the runtime owns caller liveness truth; this
/// helper owns reclaim for slots it holds. Admission first sweeps
/// non-Open slots so timed-out callers do not occupy capacity.
///
/// `K: PartialEq` is enough — admission and lookup both use linear
/// scans because the table is small and bounded. There is no `Hash`
/// requirement on the key, and the constant factor stays predictable.
/// Pick caps in the tens to low hundreds; if you need more, you
/// probably want a sharded table, not a bigger pending box.
pub struct PendingReplies<K, R> {
    capacity: usize,
    slots: Vec<Option<PendingReplyEntry<K, R>>>,
    /// Slot occupancy generation. Bumped each time a slot transitions to
    /// occupied. Tickets carry the generation they were issued under, so
    /// a stale ticket against a reused slot is rejected.
    generations: Vec<u64>,
    high_water: usize,
    full_rejects: u64,
    reclaimed: u64,
    taken: u64,
    duplicate_keys: u64,
    capacity_name: String,
    capacity_mode: tina::capacity::CapacityMode,
}

impl<K, R> std::fmt::Debug for PendingReplies<K, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let live = self.slots.iter().filter(|s| s.is_some()).count();
        f.debug_struct("PendingReplies")
            .field("capacity", &self.capacity)
            .field("len", &live)
            .field("high_water", &self.high_water)
            .field("full_rejects", &self.full_rejects)
            .field("reclaimed", &self.reclaimed)
            .field("taken", &self.taken)
            .field("duplicate_keys", &self.duplicate_keys)
            .finish()
    }
}

struct PendingReplyEntry<K, R> {
    key: K,
    reply: DeferredReply<R>,
}

/// Why an admission attempt failed. The caller key and slot are
/// returned so the user code can decide what to do (drop, log, retry
/// later policy).
#[derive(Debug)]
pub enum InsertError<K, R> {
    /// Pending box is at capacity and no slot could be reclaimed.
    Full(K, DeferredReply<R>),
    /// A live entry already exists for the same key.
    DuplicateKey(K, DeferredReply<R>),
}

/// Why [`PendingReplies::try_capture`] could not produce a slot.
#[derive(Debug)]
pub enum TryCaptureError {
    /// The current message has no caller (plain send) or the slot was
    /// already captured on this turn.
    NoCaller,
    /// The current call came from a different shard. First-form
    /// captures are local-only.
    CrossShardUnsupported,
    /// The pending box is at capacity even after sweeping abandoned
    /// slots.
    Full,
    /// An entry with the same key is already live.
    DuplicateKey,
}

impl<K, R> PendingReplies<K, R>
where
    K: PartialEq,
{
    /// Creates an empty pending-replies box with a fixed capacity.
    ///
    /// Panics if `capacity` is zero — a zero-capacity box would always
    /// reject and is never the right shape.
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "PendingReplies capacity must be positive");
        let mut slots = Vec::with_capacity(capacity);
        let mut generations = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(None);
            generations.push(0);
        }
        let seq = mint_pending_replies_seq();
        Self {
            capacity,
            slots,
            generations,
            high_water: 0,
            full_rejects: 0,
            reclaimed: 0,
            taken: 0,
            duplicate_keys: 0,
            capacity_name: format!("pending_replies.{seq}"),
            capacity_mode: tina::capacity::CapacityMode::Fixed,
        }
    }

    /// Override the default capacity name.
    ///
    /// Default is `pending_replies.<n>` where `<n>` is a
    /// process-wide counter. Pin an explicit name for CI tests so
    /// a refactor that reorders construction cannot silently
    /// retarget the assertion.
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.capacity_name = name.into();
        self
    }

    /// Mark the slot cap as `Tuning`. Cap is still hard. The flag
    /// just says "report high water loudly".
    pub fn with_capacity_mode(mut self, mode: tina::capacity::CapacityMode) -> Self {
        self.capacity_mode = mode;
        self
    }

    /// Name carried in [`Self::capacity_report`].
    pub fn capacity_name(&self) -> &str {
        &self.capacity_name
    }

    /// Snapshot for the count surface. `live_len` -> current,
    /// `high_water` -> high water, `full_rejects` -> full count.
    ///
    /// `current` excludes slots whose caller already went away
    /// (state == `Closed`) and are awaiting a sweep — those would
    /// inflate the live count and the discovery line would
    /// overstate pressure.
    pub fn capacity_report(&self) -> tina::capacity::CapacitySurfaceReport {
        tina::capacity::CapacitySurfaceReport::count(
            self.capacity_name.clone(),
            self.capacity_mode.clone(),
            self.capacity,
            self.live_len(),
            self.high_water,
            self.full_rejects,
        )
    }

    /// Number of slots whose caller is still waiting. Filters out
    /// `Closed` (caller cancelled / timed out) which a later sweep
    /// will reclaim.
    fn live_len(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| {
                s.as_ref()
                    .is_some_and(|e| e.reply.state() == DeferredSlotState::Open)
            })
            .count()
    }

    /// Returns the maximum number of live promises this box may hold.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns the current number of live promises.
    pub fn len(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    /// Returns true when no live promises are stored.
    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(|s| s.is_none())
    }

    /// Highest live count observed since construction. Useful for
    /// sizing and pressure dashboards.
    pub fn high_water(&self) -> usize {
        self.high_water
    }

    /// Cumulative number of `Full` admission rejections.
    pub fn full_rejects(&self) -> u64 {
        self.full_rejects
    }

    /// Cumulative number of slots reclaimed because the caller went
    /// away before the service replied.
    pub fn reclaimed(&self) -> u64 {
        self.reclaimed
    }

    /// Cumulative number of slots explicitly removed by
    /// [`take`](Self::take).
    pub fn taken(&self) -> u64 {
        self.taken
    }

    /// Cumulative number of duplicate-key admission rejections.
    pub fn duplicate_keys(&self) -> u64 {
        self.duplicate_keys
    }

    /// Reclaim slots whose deferred reply is no longer Open. Returns
    /// the number of slots reclaimed.
    ///
    /// Called automatically before each
    /// [`try_insert`](Self::try_insert) admission check so timed-out
    /// callers do not stall new admissions.
    pub fn sweep(&mut self) -> usize {
        let mut reclaimed = 0;
        for slot in self.slots.iter_mut() {
            // Replied slots never live in PendingReplies because the
            // user must `take` a slot before passing it to `reply_to`.
            // Open slots stay; Closed slots get reclaimed.
            let take = matches!(
                slot.as_ref().map(|e| e.reply.state()),
                Some(DeferredSlotState::Closed)
            );
            if take {
                slot.take();
                reclaimed += 1;
                self.reclaimed += 1;
            }
        }
        reclaimed
    }

    /// Insert a new (key, slot) pair. Sweeps abandoned slots first.
    ///
    /// Returns [`InsertError::Full`] when capacity is exhausted, or
    /// [`InsertError::DuplicateKey`] when a live entry already holds
    /// the same key.
    pub fn try_insert(&mut self, key: K, reply: DeferredReply<R>) -> Result<(), InsertError<K, R>> {
        self.sweep();

        if self
            .slots
            .iter()
            .any(|s| s.as_ref().is_some_and(|e| e.key == key))
        {
            self.duplicate_keys += 1;
            return Err(InsertError::DuplicateKey(key, reply));
        }

        if let Some(idx) = self.slots.iter().position(|s| s.is_none()) {
            self.generations[idx] = self.generations[idx].wrapping_add(1);
            self.slots[idx] = Some(PendingReplyEntry { key, reply });
            let cur = self.len();
            if cur > self.high_water {
                self.high_water = cur;
            }
            return Ok(());
        }

        self.full_rejects += 1;
        Err(InsertError::Full(key, reply))
    }

    /// Capture the current caller in one call, sweeping first.
    ///
    /// Composes [`tina::Context::take_reply_slot`] with
    /// [`try_insert`](Self::try_insert): sweeps abandoned slots, checks
    /// admission, captures the caller only if the box can hold the
    /// new entry, and returns a typed error otherwise. The original
    /// caller is *not* consumed when admission fails — the handler can
    /// still return `Effect::Reply` with a Full marker.
    ///
    /// Use this instead of hand-rolling
    /// `sweep` / `len < cap` / `take_reply_slot` / `try_insert`.
    pub fn try_capture<S>(
        &mut self,
        ctx: &mut tina::Context<'_, S, R>,
        key: K,
    ) -> Result<(), TryCaptureError>
    where
        S: tina::Shard + ?Sized,
        R: 'static,
    {
        self.sweep();

        if self
            .slots
            .iter()
            .any(|s| s.as_ref().is_some_and(|e| e.key == key))
        {
            self.duplicate_keys += 1;
            return Err(TryCaptureError::DuplicateKey);
        }
        if self.len() >= self.capacity {
            self.full_rejects += 1;
            return Err(TryCaptureError::Full);
        }

        let slot = match ctx.take_reply_slot() {
            Ok(slot) => slot,
            Err(tina::TakeReplySlotError::NoCaller) => return Err(TryCaptureError::NoCaller),
            Err(tina::TakeReplySlotError::CrossShardUnsupported) => {
                return Err(TryCaptureError::CrossShardUnsupported);
            }
        };

        if let Some(idx) = self.slots.iter().position(|s| s.is_none()) {
            self.generations[idx] = self.generations[idx].wrapping_add(1);
            self.slots[idx] = Some(PendingReplyEntry { key, reply: slot });
            let cur = self.len();
            if cur > self.high_water {
                self.high_water = cur;
            }
            Ok(())
        } else {
            // Cannot reach: we checked capacity above and admission is
            // single-threaded inside one handler turn.
            unreachable!("admission contract violated")
        }
    }

    /// Remove and return the live slot for the given key, if any.
    pub fn take(&mut self, key: &K) -> Option<DeferredReply<R>> {
        for slot in self.slots.iter_mut() {
            let matches = slot.as_ref().is_some_and(|e| &e.key == key);
            if matches {
                let taken = slot.take().map(|e| e.reply);
                if taken.is_some() {
                    self.taken += 1;
                }
                return taken;
            }
        }
        None
    }

    /// Drain every live slot. Used at service stop so pending callers
    /// see a terminal Dropped fact when their slots are released.
    pub fn drain(&mut self) -> Vec<(K, DeferredReply<R>)> {
        let mut out = Vec::new();
        for slot in self.slots.iter_mut() {
            if let Some(entry) = slot.take() {
                out.push((entry.key, entry.reply));
            }
        }
        out
    }

    /// One [`Effect::ReplyTo`] per live slot, all with the same
    /// `value`. Slot order follows the internal table; after
    /// sweep+reuse this diverges from admission order. Closed slots
    /// present at drain time are drained too — the runtime records
    /// the resulting rejection. The helper does not pre-sweep.
    ///
    /// `PendingReplies<K, R>` only produces `Effect<I>` when
    /// `I::Reply = R`; pick `I` via turbofish. No hidden `stop`.
    ///
    /// ```compile_fail
    /// # use tina_runtime::PendingReplies;
    /// # use tina::{Effect, Isolate, Outbound, SingleShard, Context, noop};
    /// # struct A; struct B;
    /// # impl Isolate for A {
    /// #     type Message=(); type Reply=u32;
    /// #     type Send=Outbound<std::convert::Infallible>;
    /// #     type Spawn=std::convert::Infallible;
    /// #     type Io =std::convert::Infallible;
    /// #     type Shard=SingleShard;
    /// #     fn handle(&mut self, _:(), _:&mut Context<'_,Self::Shard,Self::Reply>) -> Effect<Self> { noop() }
    /// # }
    /// # impl Isolate for B {
    /// #     type Message=(); type Reply=u64;  // !! u64 not u32
    /// #     type Send=Outbound<std::convert::Infallible>;
    /// #     type Spawn=std::convert::Infallible;
    /// #     type Io =std::convert::Infallible;
    /// #     type Shard=SingleShard;
    /// #     fn handle(&mut self, _:(), _:&mut Context<'_,Self::Shard,Self::Reply>) -> Effect<Self> { noop() }
    /// # }
    /// // Mismatch: PendingReplies<_, u32> cannot produce Effect<B> (B::Reply = u64).
    /// fn _no(b: &mut PendingReplies<u32, u32>) -> Vec<Effect<B>> {
    ///     b.drain_replies(0)
    /// }
    /// ```
    pub fn drain_replies<I>(&mut self, value: R) -> Vec<Effect<I>>
    where
        I: Isolate<Reply = R>,
        R: Clone,
    {
        let mut out = Vec::with_capacity(self.slots.len());
        // R rides in `taken` so the last slot consumes `value` without
        // cloning. K drops per iteration with `entry`.
        let mut taken: Option<DeferredReply<R>> = None;
        for slot in self.slots.iter_mut() {
            if let Some(entry) = slot.take() {
                if let Some(prev) = taken.take() {
                    out.push(reply_to::<I>(prev, value.clone()));
                }
                taken = Some(entry.reply);
            }
        }
        if let Some(last) = taken {
            out.push(reply_to::<I>(last, value));
        }
        out
    }

    /// Per-key reply form of [`drain_replies`](Self::drain_replies).
    /// Use when the value depends on `K` or `R: !Clone`.
    ///
    /// ```compile_fail
    /// # use tina_runtime::PendingReplies;
    /// # use tina::{Effect, Isolate, Outbound, SingleShard, Context, noop};
    /// # struct B;
    /// # impl Isolate for B {
    /// #     type Message=(); type Reply=u64;
    /// #     type Send=Outbound<std::convert::Infallible>;
    /// #     type Spawn=std::convert::Infallible;
    /// #     type Io =std::convert::Infallible;
    /// #     type Shard=SingleShard;
    /// #     fn handle(&mut self, _:(), _:&mut Context<'_,Self::Shard,Self::Reply>) -> Effect<Self> { noop() }
    /// # }
    /// fn _no(b: &mut PendingReplies<u32, u32>) -> Vec<Effect<B>> {
    ///     b.drain_replies_with(|_k| 0u32)
    /// }
    /// ```
    pub fn drain_replies_with<I, F>(&mut self, mut f: F) -> Vec<Effect<I>>
    where
        I: Isolate<Reply = R>,
        F: FnMut(K) -> R,
    {
        let mut out = Vec::with_capacity(self.slots.len());
        for slot in self.slots.iter_mut() {
            if let Some(entry) = slot.take() {
                out.push(reply_to::<I>(entry.reply, f(entry.key)));
            }
        }
        out
    }

    /// [`drain_replies`](Self::drain_replies) wrapped in
    /// [`Effect::Batch`]; [`Effect::Noop`] on empty box.
    ///
    /// ```compile_fail
    /// # use tina_runtime::PendingReplies;
    /// # use tina::{Effect, Isolate, Outbound, SingleShard, Context, noop};
    /// # struct B;
    /// # impl Isolate for B {
    /// #     type Message=(); type Reply=u64;
    /// #     type Send=Outbound<std::convert::Infallible>;
    /// #     type Spawn=std::convert::Infallible;
    /// #     type Io =std::convert::Infallible;
    /// #     type Shard=SingleShard;
    /// #     fn handle(&mut self, _:(), _:&mut Context<'_,Self::Shard,Self::Reply>) -> Effect<Self> { noop() }
    /// # }
    /// fn _no(b: &mut PendingReplies<u32, u32>) -> Effect<B> {
    ///     b.drain_replies_into_effect(0u32)
    /// }
    /// ```
    pub fn drain_replies_into_effect<I>(&mut self, value: R) -> Effect<I>
    where
        I: Isolate<Reply = R>,
        R: Clone,
    {
        let effects = self.drain_replies::<I>(value);
        if effects.is_empty() {
            Effect::Noop
        } else {
            Effect::Batch(effects)
        }
    }

    /// [`drain_replies`](Self::drain_replies) plus a trailing
    /// [`stop()`]. Method name says `stop` on purpose — nothing
    /// else in this module appends one for you.
    ///
    /// Bimodal: empty box → plain [`Effect::Stop`]; non-empty →
    /// [`Effect::Batch`] of `N` replies + [`Effect::Stop`]. A handler
    /// that just returns the effect sees no difference; a caller
    /// that pattern-matches must handle both.
    ///
    /// ```compile_fail
    /// # use tina_runtime::PendingReplies;
    /// # use tina::{Effect, Isolate, Outbound, SingleShard, Context, noop};
    /// # struct B;
    /// # impl Isolate for B {
    /// #     type Message=(); type Reply=u64;
    /// #     type Send=Outbound<std::convert::Infallible>;
    /// #     type Spawn=std::convert::Infallible;
    /// #     type Io =std::convert::Infallible;
    /// #     type Shard=SingleShard;
    /// #     fn handle(&mut self, _:(), _:&mut Context<'_,Self::Shard,Self::Reply>) -> Effect<Self> { noop() }
    /// # }
    /// fn _no(b: &mut PendingReplies<u32, u32>) -> Effect<B> {
    ///     b.drain_replies_into_stop(0u32)
    /// }
    /// ```
    pub fn drain_replies_into_stop<I>(&mut self, value: R) -> Effect<I>
    where
        I: Isolate<Reply = R>,
        R: Clone,
    {
        let mut effects = self.drain_replies::<I>(value);
        if effects.is_empty() {
            return stop::<I>();
        }
        effects.push(stop::<I>());
        Effect::Batch(effects)
    }

    /// [`drain_replies_with`](Self::drain_replies_with) wrapped in
    /// [`Effect::Batch`]; [`Effect::Noop`] on empty box.
    ///
    /// ```compile_fail
    /// # use tina_runtime::PendingReplies;
    /// # use tina::{Effect, Isolate, Outbound, SingleShard, Context, noop};
    /// # struct B;
    /// # impl Isolate for B {
    /// #     type Message=(); type Reply=u64;
    /// #     type Send=Outbound<std::convert::Infallible>;
    /// #     type Spawn=std::convert::Infallible;
    /// #     type Io =std::convert::Infallible;
    /// #     type Shard=SingleShard;
    /// #     fn handle(&mut self, _:(), _:&mut Context<'_,Self::Shard,Self::Reply>) -> Effect<Self> { noop() }
    /// # }
    /// fn _no(b: &mut PendingReplies<u32, u32>) -> Effect<B> {
    ///     b.drain_replies_with_into_effect(|_k| 0u32)
    /// }
    /// ```
    pub fn drain_replies_with_into_effect<I, F>(&mut self, f: F) -> Effect<I>
    where
        I: Isolate<Reply = R>,
        F: FnMut(K) -> R,
    {
        let effects = self.drain_replies_with::<I, F>(f);
        if effects.is_empty() {
            Effect::Noop
        } else {
            Effect::Batch(effects)
        }
    }

    /// [`drain_replies_with`](Self::drain_replies_with) plus a
    /// trailing [`stop()`]. Same bimodal contract as
    /// [`drain_replies_into_stop`](Self::drain_replies_into_stop).
    ///
    /// ```compile_fail
    /// # use tina_runtime::PendingReplies;
    /// # use tina::{Effect, Isolate, Outbound, SingleShard, Context, noop};
    /// # struct B;
    /// # impl Isolate for B {
    /// #     type Message=(); type Reply=u64;
    /// #     type Send=Outbound<std::convert::Infallible>;
    /// #     type Spawn=std::convert::Infallible;
    /// #     type Io =std::convert::Infallible;
    /// #     type Shard=SingleShard;
    /// #     fn handle(&mut self, _:(), _:&mut Context<'_,Self::Shard,Self::Reply>) -> Effect<Self> { noop() }
    /// # }
    /// fn _no(b: &mut PendingReplies<u32, u32>) -> Effect<B> {
    ///     b.drain_replies_with_into_stop(|_k| 0u32)
    /// }
    /// ```
    pub fn drain_replies_with_into_stop<I, F>(&mut self, f: F) -> Effect<I>
    where
        I: Isolate<Reply = R>,
        F: FnMut(K) -> R,
    {
        let mut effects = self.drain_replies_with::<I, F>(f);
        if effects.is_empty() {
            return stop::<I>();
        }
        effects.push(stop::<I>());
        Effect::Batch(effects)
    }
}

// ---------------------------------------------------------------------------
// Ticketed park/reply path. The ticket carries (slot_idx, generation) so a
// stale completion against a reused slot cannot remove a newer parked caller.
// ParkTicket has private fields and is not Copy, so a moved ticket cannot be
// used twice and user code cannot forge one.
// ---------------------------------------------------------------------------

use std::marker::PhantomData;

use tina::{CallContext, RequestCall, RequestContext, TakeReplySlotError};

/// Witness for a parked caller in a [`PendingReplies`] box.
///
/// `ParkTicket` is move-only and has private fields. User code can carry
/// the ticket forward in messages but cannot duplicate or forge one. A
/// stale ticket against a reused slot is rejected at runtime through the
/// generation stamp.
///
/// Compile-fail: ticket fields are private. User code cannot construct
/// one.
///
/// ```compile_fail
/// # use std::marker::PhantomData;
/// use tina_runtime::ParkTicket;
/// // Fields are private. This must not compile from outside the crate.
/// let _forged: ParkTicket<u32> = ParkTicket {
///     slot: 0,
///     generation: 0,
///     _key: PhantomData,
/// };
/// ```
///
/// Compile-fail: a moved ticket cannot be used twice.
///
/// ```compile_fail
/// # use tina_runtime::{PendingReplies, ParkTicket};
/// # fn ticket_from(_box: &mut PendingReplies<u32, u32>) -> ParkTicket<u32> {
/// #     unimplemented!()
/// # }
/// fn use_twice(b: &mut PendingReplies<u32, u32>) {
///     let ticket = ticket_from(b);
///     let _ = b.take_ticket(ticket);
///     // Second use: ticket already moved.
///     let _ = b.take_ticket(ticket);
/// }
/// ```
pub struct ParkTicket<K> {
    slot: usize,
    generation: u64,
    _key: PhantomData<fn(K) -> K>,
}

impl<K> ParkTicket<K> {
    fn new(slot: usize, generation: u64) -> Self {
        Self {
            slot,
            generation,
            _key: PhantomData,
        }
    }
}

impl<K> std::fmt::Debug for ParkTicket<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParkTicket")
            .field("slot", &self.slot)
            .field("generation", &self.generation)
            .finish()
    }
}

/// Why [`PendingReplies::park_request`] could not park the caller.
///
/// Both variants return the original [`RequestCall`] so the handler can
/// answer the caller immediately (typically with a typed `Full` or
/// rejection reply).
pub enum ParkError<'a, K, I: tina::Isolate> {
    /// The pending box is at capacity.
    Full {
        /// Caller key that was being parked.
        key: K,
        /// Original caller authority, returned unchanged.
        call: RequestCall<'a, I>,
    },
    /// A live entry already exists for this key.
    DuplicateKey {
        /// Caller key that conflicted with a live entry.
        key: K,
        /// Original caller authority, returned unchanged.
        call: RequestCall<'a, I>,
    },
}

impl<'a, K, I> std::fmt::Debug for ParkError<'a, K, I>
where
    I: tina::Isolate,
    K: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full { key, .. } => f.debug_struct("ParkError::Full").field("key", key).finish(),
            Self::DuplicateKey { key, .. } => f
                .debug_struct("ParkError::DuplicateKey")
                .field("key", key)
                .finish(),
        }
    }
}

/// Why [`PendingReplies::park_call`] could not park the caller.
///
/// `park_call` works at the lower [`CallContext`] level, so it also has
/// to surface `NoCaller` and `CrossShardUnsupported` from
/// [`TakeReplySlotError`].
pub enum ParkCallError<'a, K, I: tina::Isolate> {
    /// The current message had no caller authority on this turn.
    NoCaller {
        /// Caller key that was being parked.
        key: K,
        /// Original caller authority, returned unchanged.
        call: CallContext<'a, I>,
    },
    /// The caller came from another shard; deferred replies are local-only.
    CrossShardUnsupported {
        /// Caller key that was being parked.
        key: K,
        /// Original caller authority, returned unchanged.
        call: CallContext<'a, I>,
    },
    /// The pending box is at capacity.
    Full {
        /// Caller key that was being parked.
        key: K,
        /// Original caller authority, returned unchanged.
        call: CallContext<'a, I>,
    },
    /// A live entry already exists for this key.
    DuplicateKey {
        /// Caller key that conflicted with a live entry.
        key: K,
        /// Original caller authority, returned unchanged.
        call: CallContext<'a, I>,
    },
}

impl<'a, K, I> std::fmt::Debug for ParkCallError<'a, K, I>
where
    I: tina::Isolate,
    K: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoCaller { key, .. } => f
                .debug_struct("ParkCallError::NoCaller")
                .field("key", key)
                .finish(),
            Self::CrossShardUnsupported { key, .. } => f
                .debug_struct("ParkCallError::CrossShardUnsupported")
                .field("key", key)
                .finish(),
            Self::Full { key, .. } => f
                .debug_struct("ParkCallError::Full")
                .field("key", key)
                .finish(),
            Self::DuplicateKey { key, .. } => f
                .debug_struct("ParkCallError::DuplicateKey")
                .field("key", key)
                .finish(),
        }
    }
}

/// Why [`PendingReplies::take_ticket`] could not find the parked caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakeParkedError<K> {
    /// The slot is empty: the caller already replied, drained, or was
    /// swept after going away.
    Missing,
    /// The slot is occupied but by a newer caller. The ticket is stale.
    StaleTicket,
    #[doc(hidden)]
    _Phantom(PhantomData<fn(K) -> K>),
}

/// Why [`PendingReplies::reply_ticket`] could not settle the parked caller.
///
/// The reply value is returned so the user can decide what to do with it
/// (typically logged and dropped, since the caller already departed).
#[derive(Debug)]
pub enum ReplyParkedError<K, R> {
    /// The slot is empty.
    Missing {
        /// Reply value returned unchanged.
        reply: R,
        /// Phantom type witness for the ticket's key shape.
        #[doc(hidden)]
        _key: PhantomData<fn(K) -> K>,
    },
    /// The ticket generation does not match the current slot occupant.
    StaleTicket {
        /// Reply value returned unchanged.
        reply: R,
        /// Phantom type witness for the ticket's key shape.
        #[doc(hidden)]
        _key: PhantomData<fn(K) -> K>,
    },
}

impl<K, R> PendingReplies<K, R>
where
    K: PartialEq,
{
    /// Park the current request-call caller under `key`.
    ///
    /// On success returns a [`ParkTicket`] that must be carried into the
    /// continuation message. On failure, the original [`RequestCall`] is
    /// returned alongside the key so the handler can answer immediately.
    ///
    /// `RequestCall` is the split-service request shape and guarantees
    /// caller authority is present on this turn. Park admission is
    /// checked before the caller is consumed, so a `Full` or
    /// `DuplicateKey` rejection returns the original `RequestCall`.
    ///
    /// Compile-fail: a `PendingReplies<K, WrongReply>` cannot park a
    /// `RequestCall<'_, I>` whose isolate reply type is something else.
    ///
    /// ```compile_fail
    /// # use std::convert::Infallible;
    /// # use tina::prelude::*;
    /// # use tina_runtime::{PendingReplies, RuntimeCall};
    /// # struct Svc;
    /// # #[derive(Debug)] struct Req;
    /// # #[derive(Debug)] enum SvcMsg { Get(Req), _Tick }
    /// # impl Isolate for Svc {
    /// #     type Message = SvcMsg;
    /// #     type Reply = u64;
    /// #     type Send = tina::Outbound<Infallible>;
    /// #     type Spawn = Infallible;
    /// #     type SpawnObserved = Infallible;
    /// #     type Io = RuntimeCall<SvcMsg>;
    /// #     type Shard = tina::SingleShard;
    /// #     fn handle(&mut self, _m: SvcMsg, _ctx: &mut Context<'_, Self::Shard, u64>) -> Effect<Self> {
    /// #         tina::noop()
    /// #     }
    /// # }
    /// fn try_park(call: RequestCall<'_, Svc>, pending: &mut PendingReplies<u32, u32>) {
    ///     // Svc::Reply is u64; pending box holds u32. Cannot park.
    ///     let _ = pending.park_request(1, call);
    /// }
    /// ```
    pub fn park_request<'a, I>(
        &mut self,
        key: K,
        call: RequestCall<'a, I>,
    ) -> Result<ParkTicket<K>, ParkError<'a, K, I>>
    where
        I: tina::Isolate<Reply = R>,
        R: 'static,
    {
        self.sweep();

        if self
            .slots
            .iter()
            .any(|s| s.as_ref().is_some_and(|e| e.key == key))
        {
            self.duplicate_keys += 1;
            return Err(ParkError::DuplicateKey { key, call });
        }
        if self.live_admission_len() >= self.capacity {
            self.full_rejects += 1;
            return Err(ParkError::Full { key, call });
        }

        let req = Self::extract_request_context(call);
        Ok(self.store_request_context(key, req))
    }

    /// Park the current call-context caller under `key`.
    pub fn park_call<'a, I>(
        &mut self,
        key: K,
        call: CallContext<'a, I>,
    ) -> Result<ParkTicket<K>, ParkCallError<'a, K, I>>
    where
        I: tina::Isolate<Reply = R>,
        R: 'static,
    {
        self.sweep();

        if self
            .slots
            .iter()
            .any(|s| s.as_ref().is_some_and(|e| e.key == key))
        {
            self.duplicate_keys += 1;
            return Err(ParkCallError::DuplicateKey { key, call });
        }
        if self.live_admission_len() >= self.capacity {
            self.full_rejects += 1;
            return Err(ParkCallError::Full { key, call });
        }

        match call.try_into_request_context() {
            Ok(req) => Ok(self.store_request_context(key, req)),
            Err((call, TakeReplySlotError::NoCaller)) => Err(ParkCallError::NoCaller { key, call }),
            Err((call, TakeReplySlotError::CrossShardUnsupported)) => {
                Err(ParkCallError::CrossShardUnsupported { key, call })
            }
        }
    }

    /// Remove the parked caller named by `ticket`. Returns the underlying
    /// [`DeferredReply`] so the service can hand-roll its reply path.
    pub fn take_ticket(
        &mut self,
        ticket: ParkTicket<K>,
    ) -> Result<DeferredReply<R>, TakeParkedError<K>> {
        if ticket.slot >= self.slots.len() {
            return Err(TakeParkedError::StaleTicket);
        }
        if self.generations[ticket.slot] != ticket.generation {
            return Err(TakeParkedError::StaleTicket);
        }
        let Some(entry) = self.slots[ticket.slot].take() else {
            return Err(TakeParkedError::Missing);
        };
        Ok(entry.reply)
    }

    /// Settle the parked caller named by `ticket`, returning the
    /// corresponding [`Effect::ReplyTo`].
    ///
    /// `PendingReplies<K, R>` only produces `Effect<I>` when
    /// `I::Reply = R`. The matching `RequestContext` is reconstructed
    /// from the stored slot.
    pub fn reply_ticket<I>(
        &mut self,
        ticket: ParkTicket<K>,
        reply: R,
    ) -> Result<Effect<I>, ReplyParkedError<K, R>>
    where
        I: Isolate<Reply = R>,
        R: 'static,
    {
        if ticket.slot >= self.slots.len() {
            return Err(ReplyParkedError::StaleTicket {
                reply,
                _key: PhantomData,
            });
        }
        if self.generations[ticket.slot] != ticket.generation {
            return Err(ReplyParkedError::StaleTicket {
                reply,
                _key: PhantomData,
            });
        }
        let Some(entry) = self.slots[ticket.slot].take() else {
            return Err(ReplyParkedError::Missing {
                reply,
                _key: PhantomData,
            });
        };
        Ok(reply_to::<I>(entry.reply, reply))
    }

    /// Internal: place a `RequestContext<R>` into a free slot. Used by
    /// `park_call` (and indirectly `park_request`).
    fn store_request_context(&mut self, key: K, req: RequestContext<R>) -> ParkTicket<K> {
        let idx = self
            .slots
            .iter()
            .position(|s| s.is_none())
            .expect("admission already proved a free slot is available");
        self.generations[idx] = self.generations[idx].wrapping_add(1);
        let generation = self.generations[idx];
        self.slots[idx] = Some(PendingReplyEntry {
            key,
            reply: req.into_deferred(),
        });
        let cur = self.len();
        if cur > self.high_water {
            self.high_water = cur;
        }
        ParkTicket::new(idx, generation)
    }

    /// `live_len` filters by slot state; admission needs to count
    /// occupancy (including Closed slots still pending sweep) so we
    /// don't exceed `capacity` between sweeps. This is the same count
    /// as `len()` but pinned to admission semantics.
    fn live_admission_len(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }
}

impl<K, R> PendingReplies<K, R> {
    /// Internal helper that pulls the request context out of a
    /// `RequestCall` without forcing the user to settle the caller now.
    ///
    /// Cross-shard `RequestCall` would surface as a panic here; in
    /// practice the split-service request path is local-only on
    /// the request handler turn, so the conversion always succeeds.
    fn extract_request_context<I>(call: RequestCall<'_, I>) -> RequestContext<I::Reply>
    where
        I: tina::Isolate,
        I::Reply: 'static,
    {
        call.into_call_context().into_request_context()
    }
}

/// Build a `RequestEffect<I>` from an `Effect<I>` after caller authority
/// has been consumed by a bounded helper such as
/// [`PendingReplies::park_request`].
///
/// The ticket reference is taken purely as a type-level witness that
/// admission already happened: a caller cannot conjure one without going
/// through `park_request` (or one of its siblings), so this helper does
/// not open a hole in the safety rails. The ticket itself is borrowed,
/// not consumed.
pub fn request_effect_after_park<I, K>(
    _ticket: &ParkTicket<K>,
    effect: tina::Effect<I>,
) -> tina::RequestEffect<I>
where
    I: tina::Isolate,
{
    crate::call::request_effect_from_consumed_effect(effect)
}

#[cfg(test)]
mod pending_replies_tests {
    use super::*;
    use crate::RegisteredAddress;
    use crate::trace::{CauseId, EventId};
    use tina::runtime_internal::{deferred_from_handle, handle_from_shared};
    use tina::{AddressGeneration, DeferredSlotShared, DeferredSlotState, ShardId};

    fn fake_slot(id: u64) -> DeferredReply<u32> {
        let shared =
            std::sync::Arc::new(DeferredSlotShared::new(id, std::any::TypeId::of::<u32>()));
        deferred_from_handle(handle_from_shared(shared))
    }

    fn fake_slot_closed(id: u64) -> DeferredReply<u32> {
        let shared =
            std::sync::Arc::new(DeferredSlotShared::new(id, std::any::TypeId::of::<u32>()));
        shared.set_state(DeferredSlotState::Closed);
        deferred_from_handle(handle_from_shared(shared))
    }

    fn fake_shared(id: u64) -> std::sync::Arc<DeferredSlotShared> {
        std::sync::Arc::new(DeferredSlotShared::new(id, std::any::TypeId::of::<u32>()))
    }

    fn fake_promoted_record(
        slot_id: u64,
        call_id: u64,
        routing: DeferredRouting,
    ) -> DeferredSlotRecord {
        DeferredSlotRecord {
            slot_id: DeferredSlotId::new(slot_id),
            call_id: CallId::new(call_id),
            capturing_isolate: IsolateId::new(9),
            shared: fake_shared(slot_id),
            routing,
        }
    }

    #[test]
    fn try_insert_succeeds_until_full_then_returns_full() {
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(2);
        box_.try_insert(1, fake_slot(10)).unwrap();
        box_.try_insert(2, fake_slot(11)).unwrap();
        match box_.try_insert(3, fake_slot(12)) {
            Err(InsertError::Full(k, _)) => assert_eq!(k, 3),
            other => panic!("expected Full, got {other:?}"),
        }
        assert_eq!(box_.full_rejects(), 1);
        assert_eq!(box_.high_water(), 2);
        assert_eq!(box_.len(), 2);
    }

    #[test]
    fn take_by_local_call_id_ignores_remote_routed_slots() {
        let mut slots = PromotedSlots::default();
        let remote = fake_promoted_record(
            1,
            42,
            DeferredRouting::Remote {
                requester: RegisteredAddress {
                    shard: ShardId::new(2),
                    isolate: IsolateId::new(3),
                    generation: AddressGeneration::new(4),
                },
                cause: CauseId::new(EventId::new(5)),
            },
        );
        let remote_shared = std::sync::Arc::clone(&remote.shared);
        slots.push(remote);

        assert!(
            slots.take_by_local_call_id(CallId::new(42)).is_none(),
            "local caller cleanup must not consume a remote-routed deferred slot"
        );
        let remaining = slots
            .take_by_handle(&remote_shared)
            .expect("remote-routed slot remains tracked");
        assert!(matches!(remaining.routing, DeferredRouting::Remote { .. }));
    }

    #[test]
    fn promoted_slot_paths_do_not_shift_remove_inside_a_loop() {
        let source = include_str!("deferred.rs");
        // O(P^2): Vec::remove shifts the tail per drop. The promoted-slot
        // paths must compact with swap_remove instead. Needle is built at
        // runtime so this guard line does not match itself.
        let needle = format!("self.slots.{}(", "remove");
        assert!(
            !source.contains(&needle),
            "PromotedSlots must use swap_remove, not Vec::remove inside a loop"
        );
    }

    #[test]
    fn sweep_dropped_removes_exactly_dropped_and_keeps_live_resolvable() {
        // Drop-wave shape: promote P slots, drop most in one sweep. Sweep
        // must reclaim exactly the dropped ones and leave every live slot
        // still resolvable by its handle. Live = an external Arc clone is
        // held (strong_count > 1); dropped = only the record's Arc remains.
        let mut slots = PromotedSlots::default();

        const P: usize = 32;
        let mut live_handles = Vec::new();
        let mut live_slot_ids = Vec::new();
        let mut dropped_slot_ids = Vec::new();
        for i in 0..P as u64 {
            let record = fake_promoted_record(i, i, DeferredRouting::Local);
            if i % 4 == 0 {
                // Keep an external strong ref: this slot stays live.
                live_handles.push(std::sync::Arc::clone(&record.shared));
                live_slot_ids.push(i);
            } else {
                dropped_slot_ids.push(i);
            }
            slots.push(record);
        }

        let dropped = slots.sweep_dropped();
        let swept: std::collections::HashSet<u64> =
            dropped.iter().map(|r| r.slot_id.get()).collect();
        assert_eq!(
            swept,
            dropped_slot_ids.iter().copied().collect(),
            "sweep must reclaim exactly the slots whose caller went away"
        );

        // Every live slot is still tracked and resolvable by its handle.
        for handle in &live_handles {
            let record = slots
                .take_by_handle(handle)
                .expect("live slot must remain resolvable after a drop wave");
            assert!(live_slot_ids.contains(&record.slot_id.get()));
        }
        // After taking all live slots the table is empty.
        let leftover = slots.sweep_dropped();
        assert!(leftover.is_empty(), "no slots should remain after draining");
    }

    #[test]
    fn take_by_isolate_drains_only_matching_and_keeps_others_resolvable() {
        // swap_remove correctness for the isolate drain path: removing one
        // slot must not orphan another. Two isolates' slots interleave.
        let mut slots = PromotedSlots::default();
        let target = IsolateId::new(9); // fake_promoted_record uses isolate 9
        let mut target_handles = Vec::new();
        let mut other_handles = Vec::new();
        for i in 0..16u64 {
            let mut record = fake_promoted_record(i, i, DeferredRouting::Local);
            if i % 2 == 0 {
                record.capturing_isolate = target;
                target_handles.push(std::sync::Arc::clone(&record.shared));
            } else {
                record.capturing_isolate = IsolateId::new(99);
                other_handles.push(std::sync::Arc::clone(&record.shared));
            }
            slots.push(record);
        }

        let drained = slots.take_by_isolate(target);
        assert_eq!(drained.len(), target_handles.len());
        for record in &drained {
            assert_eq!(record.capturing_isolate, target);
        }
        // Other isolate's slots are untouched and still resolvable.
        for handle in &other_handles {
            assert!(
                slots.take_by_handle(handle).is_some(),
                "non-target slot must survive the isolate drain intact"
            );
        }
    }

    #[test]
    fn duplicate_key_is_rejected() {
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(4);
        box_.try_insert(7, fake_slot(1)).unwrap();
        match box_.try_insert(7, fake_slot(2)) {
            Err(InsertError::DuplicateKey(k, _)) => assert_eq!(k, 7),
            other => panic!("expected DuplicateKey, got {other:?}"),
        }
        assert_eq!(box_.duplicate_keys(), 1);
    }

    #[test]
    fn take_returns_and_removes_entry() {
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(2);
        box_.try_insert(1, fake_slot(10)).unwrap();
        let slot = box_.take(&1).expect("slot present");
        assert_eq!(slot.slot_id(), 10);
        assert_eq!(box_.taken(), 1);
        assert!(box_.take(&1).is_none());
        assert_eq!(box_.taken(), 1);
        assert_eq!(box_.len(), 0);
    }

    #[test]
    fn sweep_reclaims_closed_slots_before_admission() {
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(2);
        box_.try_insert(1, fake_slot_closed(10)).unwrap();
        box_.try_insert(2, fake_slot(11)).unwrap();
        // Box looks full but slot 1 is Closed (caller went away).
        // Admission must sweep then succeed.
        box_.try_insert(3, fake_slot(12))
            .expect("admission should succeed");
        assert_eq!(box_.reclaimed(), 1);
        assert_eq!(box_.len(), 2);
    }

    #[test]
    fn drain_returns_all_live_slots() {
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(4);
        box_.try_insert(1, fake_slot(1)).unwrap();
        box_.try_insert(2, fake_slot(2)).unwrap();
        let drained = box_.drain();
        assert_eq!(drained.len(), 2);
        assert!(box_.is_empty());
    }

    #[test]
    #[should_panic]
    fn zero_capacity_panics() {
        let _ = PendingReplies::<u32, u32>::with_capacity(0);
    }

    #[test]
    fn capacity_report_uses_default_name_when_unnamed() {
        let box_ = PendingReplies::<u32, u32>::with_capacity(4);
        let report = box_.capacity_report();
        assert!(
            report.name.starts_with("pending_replies."),
            "default name should be dotted: {}",
            report.name
        );
        assert_eq!(report.mode, tina::capacity::CapacityMode::Fixed);
        assert_eq!(report.max_messages, Some(4));
        assert_eq!(report.current_messages, 0);
        assert_eq!(report.high_water_messages, 0);
        assert_eq!(report.full_count, 0);
    }

    #[test]
    fn capacity_report_named_overrides_default() {
        let box_ = PendingReplies::<u32, u32>::with_capacity(2).named("orders.pending");
        assert_eq!(box_.capacity_name(), "orders.pending");
        assert_eq!(box_.capacity_report().name, "orders.pending");
    }

    #[test]
    fn capacity_report_tracks_high_water_and_full() {
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(2).named("p");
        box_.try_insert(1, fake_slot(1)).unwrap();
        box_.try_insert(2, fake_slot(2)).unwrap();
        // High water hits 2, then we drop to 1 by taking, then push to
        // capacity again — high water must stick at 2.
        let _ = box_.take(&1);
        box_.try_insert(3, fake_slot(3)).unwrap();
        // 4th insert blocks — full.
        match box_.try_insert(4, fake_slot(4)) {
            Err(InsertError::Full(k, _)) => assert_eq!(k, 4),
            other => panic!("expected Full, got {other:?}"),
        }
        let report = box_.capacity_report();
        assert_eq!(report.current_messages, 2);
        assert_eq!(report.high_water_messages, 2);
        assert_eq!(report.full_count, 1);
    }

    #[test]
    fn capacity_report_excludes_closed_slots_awaiting_sweep() {
        // Two callers go in, both Open, high_water hits 2. Then
        // caller 1 goes away (state -> Closed) without a sweep.
        // capacity_report.current must drop to 1 — live count, not
        // occupancy. high_water is sticky at 2.
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(4).named("p");
        let shared1 =
            std::sync::Arc::new(DeferredSlotShared::new(10, std::any::TypeId::of::<u32>()));
        let slot1 = tina::runtime_internal::deferred_from_handle(
            tina::runtime_internal::handle_from_shared(shared1.clone()),
        );
        box_.try_insert(1, slot1).unwrap();
        box_.try_insert(2, fake_slot(11)).unwrap();
        // Now flip slot 1 to Closed. No sweep happens yet.
        shared1.set_state(DeferredSlotState::Closed);
        let report = box_.capacity_report();
        assert_eq!(
            report.current_messages, 1,
            "current should exclude closed slots awaiting sweep"
        );
        assert_eq!(report.high_water_messages, 2, "high water is sticky");
    }

    #[test]
    fn capacity_report_tuning_mode_still_has_hard_cap() {
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(1)
            .with_capacity_mode(tina::capacity::CapacityMode::Tuning)
            .named("discovery.box");
        box_.try_insert(1, fake_slot(1)).unwrap();
        // Tuning is "fixed-with-loud-flag" — the cap is real.
        assert!(matches!(
            box_.try_insert(2, fake_slot(2)),
            Err(InsertError::Full(_, _))
        ));
        let report = box_.capacity_report();
        assert_eq!(report.mode, tina::capacity::CapacityMode::Tuning);
        assert_eq!(report.full_count, 1);
    }

    /// Minimal Isolate used to type-check effects produced by the
    /// drain helpers. The handler is unreachable in these tests.
    #[derive(Debug)]
    struct TestIso;

    impl tina::Isolate for TestIso {
        type Message = ();
        type Reply = u32;
        type Send = tina::Outbound<std::convert::Infallible>;
        type Spawn = std::convert::Infallible;
        type SpawnObserved = std::convert::Infallible;
        type Io = std::convert::Infallible;
        type Fact = ::std::convert::Infallible;
        type Shard = tina::SingleShard;

        fn handle(
            &mut self,
            _: (),
            _: &mut tina::Context<'_, Self::Shard, Self::Reply>,
        ) -> tina::Effect<Self> {
            tina::noop()
        }
    }

    fn slot_id_of(effect: &tina::Effect<TestIso>) -> Option<u64> {
        match effect {
            tina::Effect::ReplyTo(slot, _) => Some(slot.slot_id()),
            _ => None,
        }
    }

    fn reply_value_of(effect: &tina::Effect<TestIso>) -> Option<u32> {
        match effect {
            tina::Effect::ReplyTo(_, v) => Some(*v),
            _ => None,
        }
    }

    #[test]
    fn drain_replies_emits_one_reply_to_per_slot() {
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(4);
        box_.try_insert(1, fake_slot(10)).unwrap();
        box_.try_insert(2, fake_slot(11)).unwrap();
        box_.try_insert(3, fake_slot(12)).unwrap();

        let effects: Vec<tina::Effect<TestIso>> = box_.drain_replies(99);
        assert_eq!(effects.len(), 3);
        for e in &effects {
            assert_eq!(reply_value_of(e), Some(99));
        }
        let mut ids: Vec<u64> = effects.iter().map(|e| slot_id_of(e).unwrap()).collect();
        ids.sort();
        assert_eq!(ids, vec![10, 11, 12]);
        assert!(box_.is_empty());
    }

    #[test]
    fn drain_replies_empty_returns_empty_vec() {
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(2);
        let effects: Vec<tina::Effect<TestIso>> = box_.drain_replies(7);
        assert!(effects.is_empty());
    }

    #[test]
    fn drain_replies_with_uses_per_key_value() {
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(4);
        box_.try_insert(1, fake_slot(10)).unwrap();
        box_.try_insert(2, fake_slot(11)).unwrap();

        let effects: Vec<tina::Effect<TestIso>> = box_.drain_replies_with(|k| k * 100);
        let pairs: Vec<(u64, u32)> = effects
            .iter()
            .map(|e| (slot_id_of(e).unwrap(), reply_value_of(e).unwrap()))
            .collect();
        // slot 10 carried key 1 -> 100; slot 11 carried key 2 -> 200
        assert!(pairs.contains(&(10, 100)));
        assert!(pairs.contains(&(11, 200)));
        assert!(box_.is_empty());
    }

    #[test]
    fn drain_replies_into_effect_empty_is_noop() {
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(2);
        let effect: tina::Effect<TestIso> = box_.drain_replies_into_effect(7);
        assert!(matches!(effect, tina::Effect::Noop));
    }

    #[test]
    fn drain_replies_into_effect_nonempty_is_batch_of_replies() {
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(2);
        box_.try_insert(1, fake_slot(10)).unwrap();
        box_.try_insert(2, fake_slot(11)).unwrap();
        let effect: tina::Effect<TestIso> = box_.drain_replies_into_effect(0);
        match effect {
            tina::Effect::Batch(items) => {
                assert_eq!(items.len(), 2);
                for item in &items {
                    assert!(matches!(item, tina::Effect::ReplyTo(_, _)));
                }
            }
            other => panic!("expected Batch, got {other:?}"),
        }
    }

    #[test]
    fn drain_replies_into_stop_appends_stop_after_replies() {
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(2);
        box_.try_insert(1, fake_slot(10)).unwrap();
        box_.try_insert(2, fake_slot(11)).unwrap();
        let effect: tina::Effect<TestIso> = box_.drain_replies_into_stop(0);
        match effect {
            tina::Effect::Batch(items) => {
                assert_eq!(items.len(), 3);
                assert!(matches!(items[0], tina::Effect::ReplyTo(_, _)));
                assert!(matches!(items[1], tina::Effect::ReplyTo(_, _)));
                assert!(matches!(items[2], tina::Effect::Stop));
            }
            other => panic!("expected Batch, got {other:?}"),
        }
    }

    #[test]
    fn drain_replies_into_stop_empty_box_returns_plain_stop() {
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(2);
        let effect: tina::Effect<TestIso> = box_.drain_replies_into_stop(0);
        // No callers => no Batch wrapper. Plain Stop.
        assert!(
            matches!(effect, tina::Effect::Stop),
            "expected plain Stop, got {effect:?}"
        );
    }

    #[test]
    fn drain_replies_with_into_effect_empty_is_noop() {
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(2);
        let effect: tina::Effect<TestIso> = box_.drain_replies_with_into_effect(|k| k);
        assert!(matches!(effect, tina::Effect::Noop));
    }

    #[test]
    fn drain_replies_with_into_effect_nonempty_is_batch_of_per_key_replies() {
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(2);
        box_.try_insert(2, fake_slot(20)).unwrap();
        box_.try_insert(3, fake_slot(30)).unwrap();
        let effect: tina::Effect<TestIso> = box_.drain_replies_with_into_effect(|k| k * 10);
        match effect {
            tina::Effect::Batch(items) => {
                assert_eq!(items.len(), 2);
                let values: std::collections::HashSet<u32> = items
                    .iter()
                    .filter_map(|e| match e {
                        tina::Effect::ReplyTo(_, v) => Some(*v),
                        _ => None,
                    })
                    .collect();
                assert!(values.contains(&20));
                assert!(values.contains(&30));
            }
            other => panic!("expected Batch, got {other:?}"),
        }
    }

    #[test]
    fn drain_replies_with_into_stop_uses_per_key_value_then_stops() {
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(2);
        box_.try_insert(5, fake_slot(50)).unwrap();
        box_.try_insert(6, fake_slot(60)).unwrap();
        let effect: tina::Effect<TestIso> = box_.drain_replies_with_into_stop(|k| k + 1);
        match effect {
            tina::Effect::Batch(items) => {
                assert_eq!(items.len(), 3);
                assert!(matches!(items.last().unwrap(), tina::Effect::Stop));
                let mut got = std::collections::HashSet::new();
                for item in &items[..2] {
                    if let tina::Effect::ReplyTo(_, v) = item {
                        got.insert(*v);
                    }
                }
                assert!(got.contains(&6));
                assert!(got.contains(&7));
            }
            other => panic!("expected Batch, got {other:?}"),
        }
    }

    #[test]
    fn drain_replies_with_into_stop_empty_box_returns_plain_stop() {
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(2);
        let effect: tina::Effect<TestIso> = box_.drain_replies_with_into_stop(|k| k);
        assert!(
            matches!(effect, tina::Effect::Stop),
            "expected plain Stop, got {effect:?}"
        );
    }

    #[test]
    fn drain_replies_drains_closed_slots_too() {
        // drain_replies preserves drain()'s contract: Closed slots
        // present at drain time get a ReplyTo too. The runtime
        // records the resulting CallReplyRejected { RequesterClosed }
        // when reply_to runs.
        //
        // Insert order matters: try_insert sweeps Closed slots before
        // admitting. Inserting the Closed slot last means no later
        // admission triggers a sweep against it.
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(4);
        box_.try_insert(1, fake_slot(10)).unwrap();
        box_.try_insert(2, fake_slot(20)).unwrap();
        box_.try_insert(3, fake_slot_closed(30)).unwrap();

        let effects: Vec<tina::Effect<TestIso>> = box_.drain_replies(0);
        let mut slot_ids: Vec<u64> = effects.iter().map(|e| slot_id_of(e).unwrap()).collect();
        slot_ids.sort();
        assert_eq!(
            slot_ids,
            vec![10, 20, 30],
            "closed slot 30 must appear in the drained effects"
        );
        assert!(box_.is_empty());
    }

    // ------------------------------------------------------------------
    // Ticketed park-path tests. We exercise the lower-level
    // `store_request_context` directly (it is the same code path
    // park_call uses after passing TakeReplySlotError checks) so the
    // tests stay independent of a live runtime.
    // ------------------------------------------------------------------

    fn fake_request(id: u64) -> tina::RequestContext<u32> {
        tina::runtime_internal::request_context_from_deferred(fake_slot(id))
    }

    #[test]
    fn store_request_context_returns_ticket_and_reply_ticket_settles() {
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(4);
        let ticket = box_.store_request_context(1, fake_request(10));
        assert_eq!(box_.len(), 1);
        let effect: tina::Effect<TestIso> = box_.reply_ticket(ticket, 99).unwrap();
        match effect {
            tina::Effect::ReplyTo(slot, v) => {
                assert_eq!(slot.slot_id(), 10);
                assert_eq!(v, 99);
            }
            other => panic!("expected ReplyTo, got {other:?}"),
        }
        assert!(box_.is_empty());
    }

    #[test]
    fn take_ticket_returns_deferred_reply() {
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(2);
        let ticket = box_.store_request_context(7, fake_request(42));
        let slot = box_.take_ticket(ticket).expect("take ok");
        assert_eq!(slot.slot_id(), 42);
        assert!(box_.is_empty());
    }

    #[test]
    fn stale_ticket_rejected_after_slot_reuse() {
        // Park caller A, take by key (sim a stale completion path), then
        // park caller B which reuses the same slot. The old ticket must
        // not remove B.
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(1);
        let ticket_a = box_.store_request_context(1, fake_request(10));
        // Hand-roll a stale removal: take(&1) clears the slot without
        // consuming the ticket. Generation does not bump on take.
        let _ = box_.take(&1);
        // Now park a new caller; same slot index, new generation.
        let _ticket_b = box_.store_request_context(2, fake_request(20));
        // The old ticket must be rejected as stale.
        let err = box_.take_ticket(ticket_a).unwrap_err();
        assert!(matches!(err, TakeParkedError::StaleTicket));
        // B is still parked.
        assert_eq!(box_.len(), 1);
    }

    #[test]
    fn reply_ticket_stale_returns_reply_back() {
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(1);
        let ticket_a = box_.store_request_context(1, fake_request(10));
        let _ = box_.take(&1);
        let _ = box_.store_request_context(2, fake_request(20));
        let err = box_.reply_ticket::<TestIso>(ticket_a, 7).unwrap_err();
        match err {
            ReplyParkedError::StaleTicket { reply, .. } => assert_eq!(reply, 7),
            other => panic!("expected StaleTicket, got {other:?}"),
        }
    }

    #[test]
    fn take_ticket_missing_when_slot_drained() {
        // Park caller, drain (slot empty, generation unchanged), then
        // take_ticket -> Missing (not StaleTicket).
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(2);
        let ticket = box_.store_request_context(1, fake_request(10));
        let _ = box_.drain();
        let err = box_.take_ticket(ticket).unwrap_err();
        assert!(matches!(err, TakeParkedError::Missing));
    }

    #[test]
    fn reply_ticket_missing_returns_reply_back() {
        let mut box_ = PendingReplies::<u32, u32>::with_capacity(2);
        let ticket = box_.store_request_context(1, fake_request(10));
        let _ = box_.drain();
        let err = box_.reply_ticket::<TestIso>(ticket, 7).unwrap_err();
        match err {
            ReplyParkedError::Missing { reply, .. } => assert_eq!(reply, 7),
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    /// `ParkTicket` must be move-only — never `Copy` and never `Clone`.
    /// A static assertion makes regressions fail to compile.
    #[allow(dead_code)]
    fn _ticket_is_not_copy() {
        fn assert_not_copy<T: 'static>() {}
        // intentionally no Copy bound — this is just a witness.
        assert_not_copy::<ParkTicket<u32>>();
    }
}
