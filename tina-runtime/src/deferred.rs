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
    /// Reserved: first form refuses cross-shard captures at
    /// `Context::take_reply_slot`, so this variant is currently
    /// unreachable from user code. The runtime keeps the shape so a
    /// later phase can lift the restriction without churning the
    /// type vocabulary.
    #[allow(dead_code)]
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

    /// Pop and return the slot record matching the given shared handle.
    pub fn take_by_handle(
        &mut self,
        shared: &Arc<DeferredSlotShared>,
    ) -> Option<DeferredSlotRecord> {
        let pos = self
            .slots
            .iter()
            .position(|s| Arc::ptr_eq(&s.shared, shared))?;
        Some(self.slots.remove(pos))
    }

    /// Pop the slot tracking a given local call id.
    ///
    /// Only safe for slots whose routing is [`DeferredRouting::Local`]:
    /// remote-routed slots use a `call_id` minted on the requester's
    /// shard and could shadow a local id. The first form refuses
    /// remote captures, so every promoted slot is currently `Local`;
    /// the assertion makes that invariant load-bearing.
    pub fn take_by_local_call_id(&mut self, call_id: CallId) -> Option<DeferredSlotRecord> {
        let pos = self
            .slots
            .iter()
            .position(|s| matches!(s.routing, DeferredRouting::Local) && s.call_id == call_id)?;
        Some(self.slots.remove(pos))
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
                taken.push(self.slots.remove(i));
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
                dropped.push(self.slots.remove(i));
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

use tina::{DeferredReply, DeferredSlotState};

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
    high_water: usize,
    full_rejects: u64,
    reclaimed: u64,
    duplicate_keys: u64,
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
        for _ in 0..capacity {
            slots.push(None);
        }
        Self {
            capacity,
            slots,
            high_water: 0,
            full_rejects: 0,
            reclaimed: 0,
            duplicate_keys: 0,
        }
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

        if let Some(slot) = self.slots.iter_mut().find(|s| s.is_none()) {
            *slot = Some(PendingReplyEntry { key, reply });
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

        if let Some(entry_slot) = self.slots.iter_mut().find(|s| s.is_none()) {
            *entry_slot = Some(PendingReplyEntry { key, reply: slot });
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
                return slot.take().map(|e| e.reply);
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

    /// Drain every live slot and produce one [`Effect::ReplyTo`] per
    /// slot, each carrying the same `value`.
    ///
    /// Use when the same terminal marker (e.g. `R::Closed`) goes to
    /// every pending caller. Slots are visited in internal-table
    /// order, which matches admission order only when the table has
    /// not been swept and reused; after a sweep, freed positions are
    /// reused before later still-open positions.
    ///
    /// Closed slots (caller went away before reply) are drained too;
    /// `reply_to` against a Closed slot lands as
    /// `CallReplyRejected { RequesterClosed }` in the trace. The
    /// helper does not pre-sweep — it preserves
    /// [`drain`](Self::drain)'s semantics.
    ///
    /// Compile-time typed: a `PendingReplies<K, R>` only produces
    /// `Effect<I>` when `I::Reply = R`. The caller picks `I` via
    /// turbofish (`pending.drain_replies::<Self>(R::Closed)`).
    ///
    /// No hidden `stop`. Pair with `stop()` or use
    /// [`drain_replies_into_stop`](Self::drain_replies_into_stop).
    ///
    /// Allocates one fresh `Vec<Effect<I>>` of length live-count and
    /// performs `live - 1` clones of `value` (the last slot consumes
    /// `value` directly).
    ///
    /// ```compile_fail
    /// # use tina_runtime::PendingReplies;
    /// # use tina::{Effect, Isolate, Outbound, SingleShard, Context, noop};
    /// # struct A; struct B;
    /// # impl Isolate for A {
    /// #     type Message=(); type Reply=u32;
    /// #     type Send=Outbound<std::convert::Infallible>;
    /// #     type Spawn=std::convert::Infallible;
    /// #     type Call=std::convert::Infallible;
    /// #     type Shard=SingleShard;
    /// #     fn handle(&mut self, _:(), _:&mut Context<'_,Self::Shard,Self::Reply>) -> Effect<Self> { noop() }
    /// # }
    /// # impl Isolate for B {
    /// #     type Message=(); type Reply=u64;  // !! u64 not u32
    /// #     type Send=Outbound<std::convert::Infallible>;
    /// #     type Spawn=std::convert::Infallible;
    /// #     type Call=std::convert::Infallible;
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
        let live = self.len();
        let mut out = Vec::with_capacity(live);
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

    /// Drain every live slot, computing the reply value per key.
    ///
    /// Use when the per-caller reply depends on the key, or when `R`
    /// is not `Clone`. Allocates one fresh `Vec<Effect<I>>` of length
    /// live-count.
    pub fn drain_replies_with<I, F>(&mut self, mut f: F) -> Vec<Effect<I>>
    where
        I: Isolate<Reply = R>,
        F: FnMut(K) -> R,
    {
        let live = self.len();
        let mut out = Vec::with_capacity(live);
        for slot in self.slots.iter_mut() {
            if let Some(entry) = slot.take() {
                out.push(reply_to::<I>(entry.reply, f(entry.key)));
            }
        }
        out
    }

    /// [`drain_replies`](Self::drain_replies) wrapped in
    /// [`Effect::Batch`]. Returns [`Effect::Noop`] when the box is
    /// empty.
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
    /// [`stop()`]. The method name says `stop` on purpose — nothing
    /// else in this module appends `stop()` for you.
    ///
    /// Empty box returns plain [`Effect::Stop`] (not a one-element
    /// batch). The caller still stops as advertised.
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
    /// [`Effect::Batch`]. Empty box returns [`Effect::Noop`].
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
    /// trailing [`stop()`]. Empty box returns plain [`Effect::Stop`].
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

#[cfg(test)]
mod pending_replies_tests {
    use super::*;
    use tina::runtime_internal::{deferred_from_handle, handle_from_shared};
    use tina::{DeferredSlotShared, DeferredSlotState};

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
        assert!(box_.take(&1).is_none());
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

    /// Minimal Isolate used to type-check effects produced by the
    /// drain helpers. The handler is unreachable in these tests.
    #[derive(Debug)]
    struct TestIso;

    impl tina::Isolate for TestIso {
        type Message = ();
        type Reply = u32;
        type Send = tina::Outbound<std::convert::Infallible>;
        type Spawn = std::convert::Infallible;
        type Call = std::convert::Infallible;
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
        assert_eq!(
            effects.len(),
            3,
            "closed slot present at drain time must still produce a ReplyTo"
        );
        assert!(box_.is_empty());
    }
}
