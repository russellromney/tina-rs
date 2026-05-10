//! Bounded fixed-capacity set of caller-owned [`CallHandle`]s.
//!
//! `PendingCallSet` is an isolate-state helper for the common pattern
//! "I own N outstanding calls; on completion / timeout / cancel /
//! owner-stop I need to remove this one and reclaim its slot." It is
//! not a workflow runner: insertion, completion, and cancel are all
//! explicit, and a stored handle has no `Drop` magic — every entry is
//! removed by the user when the matching reply, timeout, or cancel
//! continuation fires.
//!
//! # Hard rules
//!
//! - **Bounded.** Backed by a `Vec` with a fixed capacity set at
//!   construction. `insert` returns [`PendingCallSetInsertError::Full`]
//!   when the table is at capacity.
//! - **Explicit cleanup.** Storing a handle does not register a
//!   continuation; the user routes their reply translator's outcome
//!   back into [`PendingCallSet::remove`] (or
//!   [`PendingCallSet::drain`] for owner-stop / cancel-all).
//! - **No `Drop` removal.** Dropping the set drops the stored handles;
//!   this lets the underlying calls run to completion (the existing
//!   `CallHandle` rule). Cancel-all is a separate explicit action.
//! - **Duplicate keys.** Insertion with a key already present returns
//!   [`PendingCallSetInsertError::DuplicateKey`] and the set is
//!   unchanged. The handle is returned to the caller for visible
//!   handling.
//!
//! # Cancel-all pattern
//!
//! The set deliberately does *not* know how to build a [`crate::Effect`]
//! itself — that would force this crate to depend on the runtime's
//! `cancel_call`. Instead, drain the set in user code and pair each
//! handle with a `cancel_call(handle).reply(...)` effect (defined in
//! `tina_runtime`, which this crate cannot import). The shape is:
//!
//! ```text
//! // Inside a handler:
//! let mut effects = Vec::with_capacity(self.calls.len());
//! for (_, handle) in self.calls.drain() {
//!     effects.push(cancel_call(handle).reply(|_| Msg::Cancelled));
//! }
//! batch(effects)
//! ```
//!
//! Every cancelled call still reports a typed [`crate::CancelOutcome`]
//! back through the user's translator.
//!
//! The value-type API itself is runnable in isolation — this doctest
//! compiles and runs without a runtime, exercising every `tina`-side
//! method:
//!
//! ```
//! use std::any::TypeId;
//! use std::sync::Arc;
//! use tina::{CallHandle, CallHandleShared, PendingCallSet, runtime_internal};
//!
//! fn make_handle<R: 'static>() -> CallHandle<R> {
//!     // Runtime-internal mint; ordinary user code receives handles
//!     // from `tina_runtime::call_with_handle(...).reply(...)`.
//!     let shared = Arc::new(CallHandleShared::new(TypeId::of::<R>()));
//!     runtime_internal::call_handle_from_shared::<R>(shared)
//! }
//!
//! let mut calls: PendingCallSet<u32, ()> = PendingCallSet::with_capacity(4);
//! calls.insert(1, make_handle()).map_err(|_| ()).unwrap();
//! calls.insert(2, make_handle()).map_err(|_| ()).unwrap();
//! assert_eq!(calls.len(), 2);
//!
//! // Completion path: explicit `remove`.
//! let _settled = calls.remove(&1).expect("present");
//! assert_eq!(calls.len(), 1);
//!
//! // Cancel-all path: `drain` empties the set; the user pairs each
//! // handle with a `cancel_call(...).reply(...)` effect.
//! let drained: Vec<_> = calls.drain().collect();
//! assert_eq!(drained.len(), 1);
//! assert!(calls.is_empty());
//! ```
//!
//! # Settled / cancelled slots are reclaimed on insert
//!
//! [`PendingCallSet::insert`] sweeps any entries whose handle reports
//! [`CallHandleState::Settled`] or [`CallHandleState::Cancelled`]
//! before checking capacity. This matches the
//! `tina_runtime::PendingReplies::try_insert` shape — the inverse
//! helper for deferred reply slots, which sweeps `Closed` slots before
//! admit — and means the set cannot become silently full just because
//! a translator forgot to call [`PendingCallSet::remove`].
//!
//! The sweep is foreground only: it runs when the user calls a method
//! that asks "is there room?" Nothing sweeps in the background, no
//! timer fires, no `Drop` magic touches the table. The contract
//! "a stored handle becomes free space when the call settles" is
//! visible: the runtime sets the handle's state on settle/cancel, and
//! the next admission honors it.
//!
//! Calling [`PendingCallSet::remove`] from a `Returned` translator is
//! still recommended — it reclaims the slot eagerly and keeps the
//! semantics aligned with the message the user just handled. But
//! forgetting the `remove` no longer leaks the slot until the set is
//! dropped.

use crate::{CallHandle, CallHandleState};

/// Bounded slab of [`CallHandle`]s keyed by a user-chosen `RequestId`.
///
/// See the module docs for invariants and the cancel-all pattern.
#[derive(Debug)]
pub struct PendingCallSet<K, R> {
    entries: Vec<(K, CallHandle<R>)>,
    capacity: usize,
}

/// Reasons [`PendingCallSet::insert`] may refuse a handle.
#[derive(Debug)]
pub enum PendingCallSetInsertError<K, R> {
    /// The set is at its configured capacity. No room for another entry.
    /// The rejected `(key, handle)` is returned so the caller can decide
    /// how to surface the pressure.
    Full {
        /// The key the caller attempted to insert.
        key: K,
        /// The handle the caller attempted to insert.
        handle: CallHandle<R>,
    },
    /// A handle is already registered under this key. The previously
    /// stored handle is left in place; the rejected `(key, handle)` is
    /// returned. Duplicate keys are user errors — typically a
    /// reused-`RequestId` bug.
    DuplicateKey {
        /// The key the caller attempted to reinsert.
        key: K,
        /// The handle the caller attempted to insert.
        handle: CallHandle<R>,
    },
}

impl<K, R> PendingCallSet<K, R>
where
    K: PartialEq,
{
    /// Builds an empty set with the given fixed capacity.
    ///
    /// Panics if `capacity == 0`. A zero-capacity set could never hold
    /// a handle and would shape every `insert` to `Full`, which is a
    /// configuration bug rather than runtime backpressure.
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(
            capacity > 0,
            "PendingCallSet requires capacity > 0; a zero-capacity set rejects every insert",
        );
        Self {
            entries: Vec::with_capacity(capacity),
            capacity,
        }
    }

    /// Returns the configured capacity.
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns the number of stored entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether the set holds zero entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns whether the set is at capacity (next insert would
    /// return [`PendingCallSetInsertError::Full`]).
    pub fn is_full(&self) -> bool {
        self.entries.len() >= self.capacity
    }

    /// Returns whether `key` is currently stored.
    pub fn contains_key(&self, key: &K) -> bool {
        self.entries.iter().any(|(k, _)| k == key)
    }

    /// Inserts `handle` under `key`.
    ///
    /// Sweeps any entries whose handle is `Settled` or `Cancelled`
    /// before checking capacity, so a forgotten `remove` in a previous
    /// `Returned` translator does not silently make the set full. This
    /// matches the `tina_runtime::PendingReplies::try_insert` shape.
    ///
    /// Returns `Ok(())` on success. Returns
    /// [`PendingCallSetInsertError::Full`] when even after the sweep
    /// no slot is free, and [`PendingCallSetInsertError::DuplicateKey`]
    /// when `key` is already stored under a still-live handle. The
    /// rejected `(key, handle)` is returned in the error variant so
    /// the caller can cancel, store elsewhere, or drop it visibly.
    pub fn insert(
        &mut self,
        key: K,
        handle: CallHandle<R>,
    ) -> Result<(), PendingCallSetInsertError<K, R>> {
        // Reclaim slots whose handles already settled or were
        // cancelled. A `Returned` translator that forgot to call
        // `remove` no longer leaks; the next admission honors the
        // runtime's truth about each handle.
        self.sweep_terminal();

        if self.contains_key(&key) {
            return Err(PendingCallSetInsertError::DuplicateKey { key, handle });
        }
        if self.is_full() {
            return Err(PendingCallSetInsertError::Full { key, handle });
        }
        self.entries.push((key, handle));
        Ok(())
    }

    /// Removes and returns the handle for `key`, if any.
    ///
    /// Use this on the success / timeout / cancel continuation that
    /// settles the call. The freed slot is immediately reusable.
    pub fn remove(&mut self, key: &K) -> Option<CallHandle<R>> {
        let pos = self.entries.iter().position(|(k, _)| k == key)?;
        Some(self.entries.swap_remove(pos).1)
    }

    /// Empties the set, returning every stored `(key, handle)`.
    ///
    /// The blessed cancel-all pattern: drain the set, then build one
    /// `cancel_call(handle).reply(...)` effect per entry in user code.
    /// The set is empty after this call regardless of how the caller
    /// uses the returned iterator.
    pub fn drain(&mut self) -> std::vec::Drain<'_, (K, CallHandle<R>)> {
        self.entries.drain(..)
    }

    /// Iterates over the stored `(key, handle)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &CallHandle<R>)> {
        self.entries.iter().map(|(k, h)| (k, h))
    }

    /// Drops every entry whose handle is in a terminal state
    /// ([`CallHandleState::Settled`] or [`CallHandleState::Cancelled`])
    /// and returns the number of slots reclaimed.
    ///
    /// [`PendingCallSet::insert`] runs this automatically before
    /// checking capacity, so callers do not need to invoke it for the
    /// "next insert sees room again" guarantee. Call it explicitly
    /// when you want a fresh `len()` / `is_full()` reading without
    /// inserting first — e.g. before a pressure-report snapshot.
    pub fn sweep_terminal(&mut self) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|(_, handle)| matches!(handle.state(), CallHandleState::Pending));
        before - self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use std::any::TypeId;
    use std::sync::Arc;

    use super::*;
    use crate::{CallHandleShared, runtime_internal};

    fn make_handle<R: 'static>() -> CallHandle<R> {
        let shared = Arc::new(CallHandleShared::new(TypeId::of::<R>()));
        runtime_internal::call_handle_from_shared::<R>(shared)
    }

    #[test]
    fn fill_then_drain_then_refill() {
        let mut set: PendingCallSet<u64, ()> = PendingCallSet::with_capacity(4);
        for i in 0..4 {
            set.insert(i, make_handle()).map_err(|_| ()).unwrap();
        }
        assert!(set.is_full());
        assert_eq!(set.len(), 4);

        let drained: Vec<_> = set.drain().collect();
        assert_eq!(drained.len(), 4);
        assert!(set.is_empty());
        assert!(!set.is_full());

        // Refill — the set must accept new handles after drain.
        for i in 4..8 {
            set.insert(i, make_handle()).map_err(|_| ()).unwrap();
        }
        assert!(set.is_full());
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn full_returns_typed_error() {
        let mut set: PendingCallSet<u64, ()> = PendingCallSet::with_capacity(2);
        set.insert(1, make_handle()).map_err(|_| ()).unwrap();
        set.insert(2, make_handle()).map_err(|_| ()).unwrap();

        match set.insert(3, make_handle()) {
            Err(PendingCallSetInsertError::Full { key, handle: _ }) => assert_eq!(key, 3),
            _ => panic!("expected Full"),
        }
        assert_eq!(set.len(), 2);
    }

    /// User pattern that the sweep must rescue: every `Returned`
    /// translator that should call `remove` forgot to. The handles
    /// transition to `Settled` (the runtime sets that field on
    /// settle/cancel). The next `insert` must reclaim those slots
    /// rather than report a stale `Full`.
    #[test]
    fn settled_handles_are_reclaimed_on_next_insert() {
        let mut set: PendingCallSet<u64, ()> = PendingCallSet::with_capacity(2);
        set.insert(1, make_handle()).map_err(|_| ()).unwrap();
        set.insert(2, make_handle()).map_err(|_| ()).unwrap();
        assert!(set.is_full());

        // Simulate the runtime settling both calls without the user
        // ever calling `remove`.
        for (_, handle) in set.iter() {
            runtime_internal::call_handle_shared(handle).set_state(CallHandleState::Settled);
        }

        // Inserting a new handle must succeed: the sweep reclaims the
        // settled entries before checking capacity.
        set.insert(3, make_handle()).map_err(|_| ()).unwrap();
        // Both stale entries gone; only the fresh one remains.
        assert_eq!(set.len(), 1);
        assert!(set.contains_key(&3));
        assert!(!set.contains_key(&1));
        assert!(!set.contains_key(&2));
    }

    #[test]
    fn cancelled_handles_are_reclaimed_on_next_insert() {
        let mut set: PendingCallSet<u64, ()> = PendingCallSet::with_capacity(2);
        set.insert(1, make_handle()).map_err(|_| ()).unwrap();
        set.insert(2, make_handle()).map_err(|_| ()).unwrap();

        for (_, handle) in set.iter() {
            runtime_internal::call_handle_shared(handle).set_state(CallHandleState::Cancelled);
        }

        set.insert(3, make_handle()).map_err(|_| ()).unwrap();
        assert_eq!(set.len(), 1);
        assert!(set.contains_key(&3));
    }

    /// Pending handles must NOT be swept — only terminal states. A
    /// still-in-flight call would silently lose its slot if pending
    /// were swept.
    #[test]
    fn pending_handles_are_never_swept() {
        let mut set: PendingCallSet<u64, ()> = PendingCallSet::with_capacity(2);
        set.insert(1, make_handle()).map_err(|_| ()).unwrap();
        set.insert(2, make_handle()).map_err(|_| ()).unwrap();
        // Both still Pending. A third insert must report Full —
        // sweep reclaims zero, capacity is honest.
        match set.insert(3, make_handle()) {
            Err(PendingCallSetInsertError::Full { key, handle: _ }) => assert_eq!(key, 3),
            _ => panic!("expected Full when no handles are terminal"),
        }
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn explicit_sweep_terminal_returns_reclaimed_count() {
        let mut set: PendingCallSet<u64, ()> = PendingCallSet::with_capacity(4);
        for i in 0..4 {
            set.insert(i, make_handle()).map_err(|_| ()).unwrap();
        }

        // Settle two of the four.
        for (key, handle) in set.iter() {
            if *key < 2 {
                runtime_internal::call_handle_shared(handle).set_state(CallHandleState::Settled);
            }
        }

        let reclaimed = set.sweep_terminal();
        assert_eq!(reclaimed, 2);
        assert_eq!(set.len(), 2);
        // A second sweep is a no-op; nothing has changed state.
        assert_eq!(set.sweep_terminal(), 0);
    }

    #[test]
    fn duplicate_key_returns_typed_error_and_does_not_overwrite() {
        let mut set: PendingCallSet<u64, ()> = PendingCallSet::with_capacity(4);
        set.insert(7, make_handle()).map_err(|_| ()).unwrap();
        match set.insert(7, make_handle()) {
            Err(PendingCallSetInsertError::DuplicateKey { key, handle: _ }) => assert_eq!(key, 7),
            _ => panic!("expected DuplicateKey"),
        }
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn remove_clears_slot() {
        let mut set: PendingCallSet<u64, ()> = PendingCallSet::with_capacity(2);
        set.insert(1, make_handle()).map_err(|_| ()).unwrap();
        set.insert(2, make_handle()).map_err(|_| ()).unwrap();
        assert!(set.is_full());

        let _h = set.remove(&1).expect("present");
        assert_eq!(set.len(), 1);
        assert!(!set.is_full());

        // Slot reusable.
        set.insert(3, make_handle()).map_err(|_| ()).unwrap();
        assert!(set.is_full());
        assert!(set.contains_key(&2));
        assert!(set.contains_key(&3));
        assert!(!set.contains_key(&1));
    }

    #[test]
    fn remove_missing_returns_none() {
        let mut set: PendingCallSet<u64, ()> = PendingCallSet::with_capacity(2);
        set.insert(1, make_handle()).map_err(|_| ()).unwrap();
        assert!(set.remove(&99).is_none());
        assert_eq!(set.len(), 1);
    }

    #[test]
    #[should_panic(expected = "capacity > 0")]
    fn zero_capacity_panics() {
        let _set: PendingCallSet<u64, ()> = PendingCallSet::with_capacity(0);
    }
}
