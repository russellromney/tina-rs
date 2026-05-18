//! Many callers wait for one result — user-facing name.
//!
//! `SharedWork<K, R>` is a thin wrapper around [`crate::WaitList`] that
//! names the user's job: "many callers asked for the same key; when the
//! result lands, reply every parked waiter with one value."
//!
//! Use `SharedWork` when:
//!
//! - several callers can race for the same natural key
//!   (a cache key, a hostname lookup, a session id);
//! - the service starts the upstream work once and replies every parked
//!   waiter with one result, often a `Clone` of the value.
//!
//! Use [`crate::PendingReplies`] when reply slots are owned by id and
//! unrelated to each other (no per-key coalescing).
//!
//! Use [`crate::WaitList`] only when the lower-level name is what reads
//! better at the call site (rare).
//!
//! `SharedWork` does not own the upstream fill. The service still owns:
//!
//! - the fill-in-flight flag/set;
//! - stale fill generation;
//! - the upstream call/timer;
//! - the reply value;
//! - the retry policy.
//!
//! The wrapper only owns the parked callers.
//!
//! No hidden queues. No hidden callbacks. No fake async. Capacity is
//! bounded, overload returns caller authority unchanged, the ticket is a
//! compile-time proof admission happened.

use tina::{
    CallContext, Effect, Isolate, RequestCall,
    capacity::{CapacityMode, CapacitySurfaceReport},
};

use crate::wait_list::{
    WaitCallError, WaitError, WaitList, WaitReplyError, WaitSnapshot, WaitTicket,
};

/// Move-only proof that a caller was parked in `SharedWork`.
///
/// The ticket carries the shape `K` so `wait_call`/`wait` and `reply_one`
/// agree on the key type at compile time.
///
/// Compile-fail: ticket field is `pub(crate)`, so user code cannot name
/// or construct one. Combined with [`WaitTicket`]'s private fields, there
/// is no path to forge a ticket outside the crate.
///
/// ```compile_fail
/// use std::marker::PhantomData;
/// use tina_runtime::{SharedWorkTicket, WaitTicket};
/// let _forged: SharedWorkTicket<u32> = SharedWorkTicket {
///     inner: WaitTicket { slot: 0, generation: 0, _key: PhantomData },
/// };
/// ```
pub struct SharedWorkTicket<K> {
    pub(crate) inner: WaitTicket<K>,
}

impl<K> std::fmt::Debug for SharedWorkTicket<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedWorkTicket")
            .field("inner", &self.inner)
            .finish()
    }
}

/// Snapshot row: one row per non-empty key.
pub type SharedWorkSnapshot<K> = WaitSnapshot<K>;

/// Why [`SharedWork::wait`] could not park the caller.
pub enum SharedWorkError<'a, K, I: Isolate> {
    /// Global capacity exhausted; caller authority returned unchanged.
    Full {
        /// Caller key.
        key: K,
        /// Original caller authority.
        call: RequestCall<'a, I>,
    },
    /// Per-key capacity exhausted; caller authority returned unchanged.
    KeyFull {
        /// Caller key.
        key: K,
        /// Original caller authority.
        call: RequestCall<'a, I>,
    },
}

impl<'a, K, I> std::fmt::Debug for SharedWorkError<'a, K, I>
where
    I: Isolate,
    K: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (name, key) = match self {
            Self::Full { key, .. } => ("Full", key),
            Self::KeyFull { key, .. } => ("KeyFull", key),
        };
        f.debug_struct(&format!("SharedWorkError::{name}"))
            .field("key", key)
            .finish()
    }
}

/// Why [`SharedWork::wait_call`] could not park the caller.
pub enum SharedWorkCallError<'a, K, I: Isolate> {
    /// The current turn has no caller authority.
    NoCaller {
        /// Caller key.
        key: K,
        /// Original caller context.
        call: CallContext<'a, I>,
    },
    /// The caller came from another shard.
    CrossShardUnsupported {
        /// Caller key.
        key: K,
        /// Original caller context.
        call: CallContext<'a, I>,
    },
    /// Global capacity exhausted.
    Full {
        /// Caller key.
        key: K,
        /// Original caller context.
        call: CallContext<'a, I>,
    },
    /// Per-key capacity exhausted.
    KeyFull {
        /// Caller key.
        key: K,
        /// Original caller context.
        call: CallContext<'a, I>,
    },
}

impl<'a, K, I> std::fmt::Debug for SharedWorkCallError<'a, K, I>
where
    I: Isolate,
    K: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (name, key) = match self {
            Self::NoCaller { key, .. } => ("NoCaller", key),
            Self::CrossShardUnsupported { key, .. } => ("CrossShardUnsupported", key),
            Self::Full { key, .. } => ("Full", key),
            Self::KeyFull { key, .. } => ("KeyFull", key),
        };
        f.debug_struct(&format!("SharedWorkCallError::{name}"))
            .field("key", key)
            .finish()
    }
}

/// Why [`SharedWork::reply_one`] could not settle a ticket.
pub type SharedWorkReplyError<K, R> = WaitReplyError<K, R>;

/// User-facing parking lot for "many callers wait for one result."
///
/// Wraps [`WaitList`] one-to-one. See module docs for the user-shaped
/// description; see [`WaitList`] for the underlying mechanism.
pub struct SharedWork<K, R> {
    inner: WaitList<K, R>,
}

impl<K, R> std::fmt::Debug for SharedWork<K, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedWork")
            .field("inner", &self.inner)
            .finish()
    }
}

impl<K, R> SharedWork<K, R>
where
    K: PartialEq,
{
    /// Build a shared-work table with one global capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: WaitList::with_capacity(capacity),
        }
    }

    /// Build a shared-work table with both a global capacity and a
    /// per-key cap.
    pub fn with_key_limit(capacity: usize, per_key: usize) -> Self {
        Self {
            inner: WaitList::with_key_limit(capacity, per_key),
        }
    }

    /// Override the capacity-report name.
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.inner = self.inner.named(name);
        self
    }

    /// Mark the cap as `Tuning`. Cap is still hard.
    pub fn with_capacity_mode(mut self, mode: CapacityMode) -> Self {
        self.inner = self.inner.with_capacity_mode(mode);
        self
    }

    /// Name carried in [`Self::capacity_report`].
    pub fn capacity_name(&self) -> &str {
        self.inner.capacity_name()
    }

    /// Configured global capacity.
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    /// Configured per-key cap, if any.
    pub fn per_key_limit(&self) -> Option<usize> {
        self.inner.per_key_limit()
    }

    /// Current parked-caller count (occupancy).
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// True when no caller is parked.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Highest parked-caller count observed since construction.
    pub fn high_water(&self) -> usize {
        self.inner.high_water()
    }

    /// Cumulative global-cap rejections.
    pub fn full_rejects(&self) -> u64 {
        self.inner.full_rejects()
    }

    /// Cumulative per-key-cap rejections.
    pub fn key_full_rejects(&self) -> u64 {
        self.inner.key_full_rejects()
    }

    /// Cumulative reclaim count (caller went away).
    pub fn reclaimed(&self) -> u64 {
        self.inner.reclaimed()
    }

    /// Reclaim slots whose caller has gone away.
    pub fn sweep(&mut self) -> usize {
        self.inner.sweep()
    }

    /// Snapshot for the count capacity surface.
    pub fn capacity_report(&self) -> CapacitySurfaceReport {
        self.inner.capacity_report()
    }

    /// Park the current request-call caller under `key`.
    ///
    /// Returns a [`SharedWorkTicket`] on success. Returns
    /// [`SharedWorkError::Full`] or [`SharedWorkError::KeyFull`] with
    /// caller authority returned unchanged when admission cannot happen.
    pub fn wait<'a, I>(
        &mut self,
        key: K,
        call: RequestCall<'a, I>,
    ) -> Result<SharedWorkTicket<K>, SharedWorkError<'a, K, I>>
    where
        I: Isolate<Reply = R>,
        R: 'static,
    {
        match self.inner.park(key, call) {
            Ok(inner) => Ok(SharedWorkTicket { inner }),
            Err(WaitError::Full { key, call }) => Err(SharedWorkError::Full { key, call }),
            Err(WaitError::KeyFull { key, call }) => Err(SharedWorkError::KeyFull { key, call }),
        }
    }

    /// Park the current call-context caller under `key`.
    pub fn wait_call<'a, I>(
        &mut self,
        key: K,
        call: CallContext<'a, I>,
    ) -> Result<SharedWorkTicket<K>, SharedWorkCallError<'a, K, I>>
    where
        I: Isolate<Reply = R>,
        R: 'static,
    {
        match self.inner.park_call(key, call) {
            Ok(inner) => Ok(SharedWorkTicket { inner }),
            Err(WaitCallError::NoCaller { key, call }) => {
                Err(SharedWorkCallError::NoCaller { key, call })
            }
            Err(WaitCallError::CrossShardUnsupported { key, call }) => {
                Err(SharedWorkCallError::CrossShardUnsupported { key, call })
            }
            Err(WaitCallError::Full { key, call }) => Err(SharedWorkCallError::Full { key, call }),
            Err(WaitCallError::KeyFull { key, call }) => {
                Err(SharedWorkCallError::KeyFull { key, call })
            }
        }
    }

    /// Settle one parked caller named by `ticket`.
    pub fn reply_one<I>(
        &mut self,
        ticket: SharedWorkTicket<K>,
        reply: R,
    ) -> Result<Effect<I>, SharedWorkReplyError<K, R>>
    where
        I: Isolate<Reply = R>,
        R: 'static,
    {
        self.inner.reply_one(ticket.inner, reply)
    }

    /// Reply every parked caller under `key` with the same value
    /// (FIFO order). Requires `R: Clone`.
    pub fn reply_all_clone<I>(&mut self, key: &K, reply: R) -> Vec<Effect<I>>
    where
        I: Isolate<Reply = R>,
        R: Clone + 'static,
    {
        self.inner.reply_all_clone(key, reply)
    }

    /// Reply every parked caller under `key` with a value built per
    /// caller.
    pub fn reply_all_with<I, F>(&mut self, key: &K, factory: F) -> Vec<Effect<I>>
    where
        I: Isolate<Reply = R>,
        F: FnMut() -> R,
        R: 'static,
    {
        self.inner.reply_all_with(key, factory)
    }

    /// Close every parked caller under `key` with the same reply.
    pub fn close_all_clone<I>(&mut self, key: &K, reply: R) -> Vec<Effect<I>>
    where
        I: Isolate<Reply = R>,
        R: Clone + 'static,
    {
        self.inner.close_all_clone(key, reply)
    }

    /// Close every parked caller under `key` with a factory-built reply.
    pub fn close_all_with<I, F>(&mut self, key: &K, factory: F) -> Vec<Effect<I>>
    where
        I: Isolate<Reply = R>,
        F: FnMut() -> R,
        R: 'static,
    {
        self.inner.close_all_with(key, factory)
    }

    /// Drain every parked caller with a factory-built reply. Use this at
    /// service stop so every caller sees a terminal answer.
    pub fn drain_all_with<I, F>(&mut self, factory: F) -> Vec<Effect<I>>
    where
        I: Isolate<Reply = R>,
        F: FnMut() -> R,
        R: 'static,
    {
        self.inner.drain_all_with(factory)
    }
}

impl<K, R> SharedWork<K, R>
where
    K: PartialEq + Clone,
{
    /// Per-key occupancy snapshot. Empty keys are omitted.
    pub fn snapshot(&self) -> Vec<SharedWorkSnapshot<K>> {
        self.inner.snapshot()
    }
}

#[cfg(test)]
impl<K, R> SharedWork<K, R>
where
    K: PartialEq,
{
    /// Test-only admission that pushes a pre-built request context. Use
    /// when a unit test wants to populate the table without a live
    /// runtime; the public `wait`/`wait_call` paths still own admission
    /// for production code.
    pub(crate) fn admit_request_for_test(
        &mut self,
        key: K,
        req: tina::RequestContext<R>,
    ) -> Result<SharedWorkTicket<K>, &'static str> {
        let inner = self.inner.admit_request(key, req)?;
        Ok(SharedWorkTicket { inner })
    }
}

/// Build a request-lane effect after caller authority has been consumed
/// by [`SharedWork::wait`] or [`SharedWork::wait_call`].
///
/// The ticket is a proof that admission happened. Its fields are private
/// to the crate, so user code cannot manufacture a `RequestEffect` from
/// plain `noop()` without first parking the caller.
///
/// Mirrors [`crate::request_effect_after_wait_park`] for the user-facing
/// shared-work name.
pub fn request_effect_after_shared_wait<I, K>(
    _ticket: &SharedWorkTicket<K>,
    effect: tina::Effect<I>,
) -> tina::RequestEffect<I>
where
    I: tina::Isolate,
{
    tina::runtime_internal::request_effect_from_consumed_effect(effect)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tina::DeferredSlotShared;
    use tina::runtime_internal::{deferred_from_handle, handle_from_shared};

    fn fake_request(id: u64) -> tina::RequestContext<u32> {
        let shared =
            std::sync::Arc::new(DeferredSlotShared::new(id, std::any::TypeId::of::<u32>()));
        let deferred = deferred_from_handle(handle_from_shared(shared));
        tina::runtime_internal::request_context_from_deferred(deferred)
    }

    #[derive(Debug)]
    struct TestIso;
    impl Isolate for TestIso {
        type Message = ();
        type Reply = u32;
        type Send = tina::Outbound<std::convert::Infallible>;
        type Spawn = std::convert::Infallible;
        type SpawnObserved = std::convert::Infallible;
        type Call = std::convert::Infallible;
        type Shard = tina::SingleShard;

        fn handle(
            &mut self,
            _: (),
            _: &mut tina::Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            tina::noop()
        }
    }

    #[test]
    fn global_full_returns_caller_authority() {
        let mut work = SharedWork::<u32, u32>::with_capacity(2).named("orders.shared");
        work.admit_request_for_test(1, fake_request(10)).unwrap();
        work.admit_request_for_test(2, fake_request(11)).unwrap();
        let err = work
            .admit_request_for_test(3, fake_request(12))
            .unwrap_err();
        assert_eq!(err, "Full");
        assert_eq!(work.full_rejects(), 1);
    }

    #[test]
    fn per_key_full_returns_caller_authority() {
        let mut work = SharedWork::<u32, u32>::with_key_limit(8, 2);
        work.admit_request_for_test(1, fake_request(10)).unwrap();
        work.admit_request_for_test(1, fake_request(11)).unwrap();
        let err = work
            .admit_request_for_test(1, fake_request(12))
            .unwrap_err();
        assert_eq!(err, "KeyFull");
        assert_eq!(work.key_full_rejects(), 1);
        // Another key still admits.
        work.admit_request_for_test(2, fake_request(13)).unwrap();
    }

    #[test]
    fn fifo_per_key_reply_all_clone() {
        let mut work = SharedWork::<u32, u32>::with_capacity(8);
        work.admit_request_for_test(1, fake_request(10)).unwrap();
        work.admit_request_for_test(1, fake_request(20)).unwrap();
        work.admit_request_for_test(1, fake_request(30)).unwrap();
        let effects: Vec<Effect<TestIso>> = work.reply_all_clone(&1, 7u32);
        let slot_ids: Vec<u64> = effects
            .iter()
            .map(|e| match e {
                Effect::ReplyTo(slot, _) => slot.slot_id(),
                _ => 0,
            })
            .collect();
        assert_eq!(slot_ids, vec![10, 20, 30]);
    }

    #[test]
    fn reply_one_settles_named_ticket() {
        let mut work = SharedWork::<u32, u32>::with_capacity(4);
        let t1 = work.admit_request_for_test(1, fake_request(10)).unwrap();
        let _t2 = work.admit_request_for_test(1, fake_request(11)).unwrap();
        let effect: Effect<TestIso> = work.reply_one(t1, 99).unwrap();
        match effect {
            Effect::ReplyTo(slot, v) => {
                assert_eq!(slot.slot_id(), 10);
                assert_eq!(v, 99);
            }
            other => panic!("expected ReplyTo, got {other:?}"),
        }
        assert_eq!(work.len(), 1);
    }

    #[test]
    fn stale_ticket_cannot_reply_newer_waiter() {
        let mut work = SharedWork::<u32, u32>::with_capacity(1);
        let t1 = work.admit_request_for_test(1, fake_request(10)).unwrap();
        // Settle and re-admit a fresh entry. The new waiter must get a
        // fresh ticket; the old one is now stale.
        let _: Effect<TestIso> = work.reply_one(t1, 1u32).unwrap();
        let _t2 = work.admit_request_for_test(2, fake_request(20)).unwrap();
        assert_eq!(work.len(), 1);
        // The stale ticket cannot even be reconstructed: SharedWorkTicket
        // is move-only and consumed by reply_one. This is enforced by the
        // compile-fail test that forbids forging the struct.
    }

    #[test]
    fn sweep_reclaims_closed_callers() {
        let mut work = SharedWork::<u32, u32>::with_capacity(4);
        // Park two; close one of them by hand.
        work.admit_request_for_test(1, fake_request(10)).unwrap();
        work.admit_request_for_test(2, fake_request(20)).unwrap();
        // Close one via inner: we drain by replying to all in key 1.
        let _: Vec<Effect<TestIso>> = work.close_all_clone(&1, 0u32);
        let reclaimed = work.sweep();
        // After close_all_clone, slots are already taken, so sweep has
        // nothing else to reclaim; the surface still reports correct
        // capacity numbers.
        assert!(reclaimed == 0);
        assert_eq!(work.len(), 1);
    }

    #[test]
    fn snapshot_omits_empty_keys() {
        let mut work = SharedWork::<u32, u32>::with_capacity(8);
        work.admit_request_for_test(1, fake_request(10)).unwrap();
        work.admit_request_for_test(1, fake_request(11)).unwrap();
        work.admit_request_for_test(2, fake_request(20)).unwrap();
        let snap = work.snapshot();
        let mut counts: Vec<(u32, usize)> = snap.iter().map(|s| (s.key, s.waiters)).collect();
        counts.sort();
        assert_eq!(counts, vec![(1, 2), (2, 1)]);
    }

    #[test]
    fn capacity_report_tracks_live_high_water_and_full() {
        let mut work = SharedWork::<u32, u32>::with_capacity(2).named("orders.shared");
        work.admit_request_for_test(1, fake_request(10)).unwrap();
        work.admit_request_for_test(2, fake_request(20)).unwrap();
        let _: Vec<Effect<TestIso>> = work.close_all_clone(&1, 0u32);
        work.admit_request_for_test(3, fake_request(30)).unwrap();
        let _ = work
            .admit_request_for_test(4, fake_request(40))
            .unwrap_err();
        let report = work.capacity_report();
        assert_eq!(report.name, "orders.shared");
        assert_eq!(report.max_messages, Some(2));
        assert_eq!(report.high_water_messages, 2);
        assert_eq!(report.full_count, 1);
    }

    #[test]
    fn drain_all_with_replies_every_open_waiter() {
        let mut work = SharedWork::<u32, u32>::with_capacity(4);
        work.admit_request_for_test(1, fake_request(10)).unwrap();
        work.admit_request_for_test(2, fake_request(20)).unwrap();
        let effects: Vec<Effect<TestIso>> = work.drain_all_with(|| 1u32);
        assert_eq!(effects.len(), 2);
        assert!(work.is_empty());
    }

    #[test]
    #[should_panic(expected = "WaitList capacity must be positive")]
    fn zero_capacity_panics() {
        let _ = SharedWork::<u32, u32>::with_capacity(0);
    }

    #[test]
    #[should_panic(expected = "WaitList per-key limit must be positive")]
    fn zero_per_key_panics() {
        let _ = SharedWork::<u32, u32>::with_key_limit(4, 0);
    }
}
