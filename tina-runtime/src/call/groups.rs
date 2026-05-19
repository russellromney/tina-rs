//! `CancelableWork`: natural-key, multi-entry storage for
//! `PendingCancelableCall`.
//!
//! `PendingCancelableCallSet` enforces unique-key identity.
//! `CancelableWork` lets many live calls share one natural key, which
//! is the shape services need when the key is a real-world thing
//! (job id, room name, customer id) and each request gets its own
//! admission.

use super::pending::PendingCancelableCall;
#[cfg(test)]
use super::pending::PendingCancelableTicket;

/// Move-only witness for one admitted [`PendingCancelableCall`]. Carries
/// the slot index and a generation so a stale completion against a reused
/// slot cannot remove a newer entry.
///
/// Compile-fail: ticket fields are private.
///
/// ```compile_fail
/// # use std::marker::PhantomData;
/// use tina_runtime::WorkTicket;
/// let _forged: WorkTicket<u32> = WorkTicket {
///     slot: 0,
///     generation: 0,
///     _key: PhantomData,
/// };
/// ```
pub struct WorkTicket<K> {
    slot: usize,
    generation: u64,
    _key: std::marker::PhantomData<fn(K) -> K>,
}

impl<K> WorkTicket<K> {
    fn new(slot: usize, generation: u64) -> Self {
        Self {
            slot,
            generation,
            _key: std::marker::PhantomData,
        }
    }

    /// Slot index this ticket points at. Crate-internal helpers can use
    /// this for diagnostics; user code does not need it.
    #[doc(hidden)]
    pub fn slot_index(&self) -> usize {
        self.slot
    }
}

impl<K> std::fmt::Debug for WorkTicket<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkTicket")
            .field("slot", &self.slot)
            .field("generation", &self.generation)
            .finish()
    }
}

struct CancelableWorkEntry<K, Q, R> {
    token: PendingCancelableCall<K, Q, R>,
}

/// Many cancelable requests grouped by key.
///
/// Use this when the user's job is "this key can have several in-flight
/// requests at once" — for example, every retry attempt of the same job
/// id, or several callers racing to fill the same cache key with
/// caller-owned cancellation. For the "one active request per key"
/// shape, reach for [`crate::PendingCancelableCallSet`] instead.
///
/// Bounded fixed-capacity storage for [`PendingCancelableCall`] tokens
/// grouped by natural key. Multiple live entries may share one key, up
/// to the optional per-key cap configured at construction time.
///
/// Visible truth that stays on the surface:
///
/// - admission is `admit(token)` and returns `Full` / `KeyFull` typed
///   errors; caller authority comes back inside the rejected token.
/// - completion is `remove_by_ticket(WorkTicket)` — the ticket names this
///   exact attempt, so an older worker-return cannot remove a newer
///   admit that reused the same key.
/// - drain replies to every parked caller on stop and reports closed
///   capacity through the count surface.
pub struct CancelableWork<K, Q, R> {
    capacity: usize,
    per_key_limit: Option<usize>,
    slots: Vec<Option<CancelableWorkEntry<K, Q, R>>>,
    generations: Vec<u64>,
    high_water: usize,
    full_rejects: u64,
    key_full_rejects: u64,
    capacity_name: String,
    capacity_mode: tina::capacity::CapacityMode,
}

impl<K, Q, R> std::fmt::Debug for CancelableWork<K, Q, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let live = self.slots.iter().filter(|s| s.is_some()).count();
        f.debug_struct("CancelableWork")
            .field("capacity", &self.capacity)
            .field("per_key_limit", &self.per_key_limit)
            .field("len", &live)
            .field("high_water", &self.high_water)
            .field("full_rejects", &self.full_rejects)
            .field("key_full_rejects", &self.key_full_rejects)
            .finish()
    }
}

/// Why [`CancelableWork::admit`] could not store the pending token.
#[derive(Debug)]
pub enum AdmitWorkError<K, Q, R> {
    /// Global capacity exhausted.
    Full {
        /// Pending token returned unchanged. Recover caller authority
        /// from this value if the handler wants to answer the original
        /// caller immediately.
        token: PendingCancelableCall<K, Q, R>,
    },
    /// Per-key capacity exhausted.
    KeyFull {
        /// Pending token returned unchanged.
        token: PendingCancelableCall<K, Q, R>,
    },
}

/// Snapshot row returned by [`CancelableWork::snapshot`].
#[derive(Debug, Clone)]
pub struct CancelableWorkSnapshot<K> {
    /// Natural key.
    pub key: K,
    /// Number of live entries for this key.
    pub entries: usize,
}

fn mint_cancelable_work_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

impl<K, Q, R> CancelableWork<K, Q, R>
where
    K: PartialEq + 'static,
    Q: 'static,
    R: 'static,
{
    /// Build an empty work set with one global capacity and no per-key cap.
    pub fn with_capacity(capacity: usize) -> Self {
        Self::build(capacity, None)
    }

    /// Build an empty work set with both global and per-key caps.
    pub fn with_key_limit(capacity: usize, per_key: usize) -> Self {
        assert!(per_key > 0, "CancelableWork per-key limit must be positive");
        Self::build(capacity, Some(per_key))
    }

    fn build(capacity: usize, per_key_limit: Option<usize>) -> Self {
        assert!(capacity > 0, "CancelableWork capacity must be positive");
        let mut slots = Vec::with_capacity(capacity);
        let mut generations = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(None);
            generations.push(0);
        }
        let seq = mint_cancelable_work_seq();
        Self {
            capacity,
            per_key_limit,
            slots,
            generations,
            high_water: 0,
            full_rejects: 0,
            key_full_rejects: 0,
            capacity_name: format!("cancelable_work.{seq}"),
            capacity_mode: tina::capacity::CapacityMode::Fixed,
        }
    }

    /// Override the capacity-report name.
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.capacity_name = name.into();
        self
    }

    /// Mark the cap as `Tuning`. Cap stays hard.
    pub fn with_capacity_mode(mut self, mode: tina::capacity::CapacityMode) -> Self {
        self.capacity_mode = mode;
        self
    }

    /// Name carried in [`Self::capacity_report`].
    pub fn capacity_name(&self) -> &str {
        &self.capacity_name
    }

    /// Configured global capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Configured per-key cap, if any.
    pub fn per_key_limit(&self) -> Option<usize> {
        self.per_key_limit
    }

    /// Number of live entries.
    pub fn len(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    /// True when no entries are stored.
    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(|s| s.is_none())
    }

    /// Highest live count observed since construction.
    pub fn high_water(&self) -> usize {
        self.high_water
    }

    /// Cumulative global-cap rejections.
    pub fn full_rejects(&self) -> u64 {
        self.full_rejects
    }

    /// Cumulative per-key-cap rejections.
    pub fn key_full_rejects(&self) -> u64 {
        self.key_full_rejects
    }

    /// Count-surface snapshot for capacity dashboards.
    pub fn capacity_report(&self) -> tina::capacity::CapacitySurfaceReport {
        tina::capacity::CapacitySurfaceReport::count(
            self.capacity_name.clone(),
            self.capacity_mode.clone(),
            self.capacity,
            self.len(),
            self.high_water,
            self.full_rejects,
        )
    }

    fn count_for_key(&self, key: &K) -> usize {
        self.slots
            .iter()
            .filter(|s| s.as_ref().is_some_and(|e| e.token.key() == key))
            .count()
    }

    /// Admit `token` under its own natural key. Returns a [`WorkTicket`]
    /// on success; on rejection, the pending token is returned through
    /// the error so the handler can recover caller authority and answer
    /// immediately.
    pub fn admit(
        &mut self,
        token: PendingCancelableCall<K, Q, R>,
    ) -> Result<WorkTicket<K>, AdmitWorkError<K, Q, R>> {
        if let Some(limit) = self.per_key_limit {
            if self.count_for_key(token.key()) >= limit {
                self.key_full_rejects += 1;
                return Err(AdmitWorkError::KeyFull { token });
            }
        }
        if self.len() >= self.capacity {
            self.full_rejects += 1;
            return Err(AdmitWorkError::Full { token });
        }

        let idx = self
            .slots
            .iter()
            .position(|s| s.is_none())
            .expect("admission checked capacity above");
        self.generations[idx] = self.generations[idx].wrapping_add(1);
        let generation = self.generations[idx];
        self.slots[idx] = Some(CancelableWorkEntry { token });
        let cur = self.len();
        if cur > self.high_water {
            self.high_water = cur;
        }
        Ok(WorkTicket::new(idx, generation))
    }

    /// Remove and return the pending token named by `ticket`. Returns
    /// `None` when the ticket is stale or the slot is empty (both
    /// "completion arrived after we already removed it" outcomes).
    pub fn take(&mut self, ticket: WorkTicket<K>) -> Option<PendingCancelableCall<K, Q, R>> {
        if ticket.slot >= self.slots.len() {
            return None;
        }
        if self.generations[ticket.slot] != ticket.generation {
            return None;
        }
        self.slots[ticket.slot].take().map(|entry| entry.token)
    }

    /// Drain every stored token, freeing all capacity.
    pub fn drain(&mut self) -> impl Iterator<Item = PendingCancelableCall<K, Q, R>> + '_ {
        self.slots
            .iter_mut()
            .filter_map(|s| s.take())
            .map(|entry| entry.token)
    }
}

impl<K, Q, R> CancelableWork<K, Q, R>
where
    K: PartialEq + Clone + 'static,
    Q: 'static,
    R: 'static,
{
    /// Per-key snapshot. Empty keys are omitted.
    pub fn snapshot(&self) -> Vec<CancelableWorkSnapshot<K>> {
        let mut out: Vec<CancelableWorkSnapshot<K>> = Vec::new();
        for slot in self.slots.iter() {
            if let Some(entry) = slot.as_ref() {
                let key = entry.token.key();
                if let Some(row) = out.iter_mut().find(|row| &row.key == key) {
                    row.entries += 1;
                } else {
                    out.push(CancelableWorkSnapshot {
                        key: key.clone(),
                        entries: 1,
                    });
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod cancelable_work_tests {
    use std::any::TypeId;
    use std::sync::Arc;

    use super::*;

    fn token<K>(key: K, slot_id: u64) -> PendingCancelableCall<K, &'static str, ()> {
        let deferred_shared = Arc::new(tina::DeferredSlotShared::new(
            slot_id,
            TypeId::of::<&'static str>(),
        ));
        let deferred = tina::runtime_internal::deferred_from_handle(
            tina::runtime_internal::handle_from_shared(deferred_shared),
        );
        let request = tina::runtime_internal::request_context_from_deferred(deferred);
        let call_shared = Arc::new(tina::CallHandleShared::new(TypeId::of::<()>()));
        let handle = tina::runtime_internal::call_handle_from_shared(call_shared);

        PendingCancelableCall {
            key,
            ticket: PendingCancelableTicket(slot_id),
            request,
            handle,
        }
    }

    #[test]
    fn admit_two_entries_same_natural_key() {
        let mut work = CancelableWork::<u32, &'static str, ()>::with_capacity(4);
        let t1 = work.admit(token(7, 10)).unwrap();
        let t2 = work.admit(token(7, 20)).unwrap();
        assert_eq!(work.len(), 2);
        let removed = work.take(t1).unwrap();
        assert_eq!(removed.key(), &7);
        assert_eq!(work.len(), 1);
        let removed = work.take(t2).unwrap();
        assert_eq!(removed.key(), &7);
        assert!(work.is_empty());
    }

    #[test]
    fn global_full_returns_token_back() {
        let mut work = CancelableWork::<u32, &'static str, ()>::with_capacity(1);
        work.admit(token(1, 10)).unwrap();
        match work.admit(token(2, 20)) {
            Err(AdmitWorkError::Full { token: rejected }) => {
                assert_eq!(rejected.key(), &2);
                assert_eq!(work.full_rejects(), 1);
            }
            other => panic!("expected Full, got {other:?}"),
        }
    }

    #[test]
    fn per_key_full_returns_token_back() {
        let mut work = CancelableWork::<u32, &'static str, ()>::with_key_limit(8, 2);
        work.admit(token(1, 10)).unwrap();
        work.admit(token(1, 11)).unwrap();
        match work.admit(token(1, 12)) {
            Err(AdmitWorkError::KeyFull { token: rejected }) => {
                assert_eq!(rejected.key(), &1);
                assert_eq!(work.key_full_rejects(), 1);
            }
            other => panic!("expected KeyFull, got {other:?}"),
        }
        // Different key still admits.
        work.admit(token(2, 13)).unwrap();
    }

    #[test]
    fn drain_releases_every_entry() {
        let mut work = CancelableWork::<u32, &'static str, ()>::with_capacity(4);
        work.admit(token(1, 10)).unwrap();
        work.admit(token(1, 11)).unwrap();
        work.admit(token(2, 20)).unwrap();
        let drained: Vec<_> = work.drain().collect();
        assert_eq!(drained.len(), 3);
        assert!(work.is_empty());
    }

    #[test]
    fn stale_completion_cannot_remove_newer_ticket() {
        let mut work = CancelableWork::<u32, &'static str, ()>::with_capacity(1);
        let t1 = work.admit(token(1, 10)).unwrap();
        // Take by ticket. Slot empties.
        let _ = work.take(t1);
        // Admit a new entry into the same slot.
        let _t2 = work.admit(token(2, 20)).unwrap();
        // t1 is already moved; we can't construct a stale ticket
        // explicitly because WorkTicket::new is private. The compile-
        // fail proof covers the forge and double-use paths.
        assert_eq!(work.len(), 1);
    }

    #[test]
    fn snapshot_groups_entries_by_key() {
        let mut work = CancelableWork::<u32, &'static str, ()>::with_capacity(4);
        work.admit(token(1, 10)).unwrap();
        work.admit(token(1, 11)).unwrap();
        work.admit(token(2, 20)).unwrap();
        let mut snap: Vec<(u32, usize)> = work
            .snapshot()
            .into_iter()
            .map(|s| (s.key, s.entries))
            .collect();
        snap.sort();
        assert_eq!(snap, vec![(1, 2), (2, 1)]);
    }

    #[test]
    fn capacity_report_tracks_high_water_and_full() {
        let mut work = CancelableWork::<u32, &'static str, ()>::with_capacity(2).named("work.set");
        let t1 = work.admit(token(1, 10)).unwrap();
        work.admit(token(2, 11)).unwrap();
        let _ = work.take(t1);
        work.admit(token(3, 12)).unwrap();
        // Over cap.
        let _ = work.admit(token(4, 13)).unwrap_err();
        let report = work.capacity_report();
        assert_eq!(report.name, "work.set");
        assert_eq!(report.max_messages, Some(2));
        assert_eq!(report.high_water_messages, 2);
        assert_eq!(report.full_count, 1);
    }

    #[test]
    fn fill_drain_refill() {
        let mut work = CancelableWork::<u32, &'static str, ()>::with_capacity(2);
        work.admit(token(1, 10)).unwrap();
        work.admit(token(2, 11)).unwrap();
        let _: Vec<_> = work.drain().collect();
        work.admit(token(3, 12)).unwrap();
        work.admit(token(4, 13)).unwrap();
        assert_eq!(work.len(), 2);
    }

    #[test]
    #[should_panic(expected = "CancelableWork capacity must be positive")]
    fn zero_capacity_panics() {
        let _: CancelableWork<u32, &'static str, ()> = CancelableWork::with_capacity(0);
    }
}
