//! Parked caller ownership composed with local concurrency admission.
//!
//! [`ConcurrencyPendingReplies`] owns both the [`crate::ConcurrencyLimit`]
//! and the bounded parked-reply table. Keeping those two pieces under one
//! owner is what makes deterministic cleanup possible without shared state:
//! permits are released on reply and retired on every abandon path.

use std::marker::PhantomData;

use tina::{DeferredReply, Effect, Isolate, RequestCall, RequestEffect, reply_to};

use crate::{
    AdmissionFailure, ConcurrencyLimit, ConcurrencyPermit, GuardedInsertError, GuardedParkError,
    GuardedParkTicket, GuardedPendingReplies, GuardedTakeError,
};

/// Move-only, generation-stamped ticket for one concurrency-charged caller.
pub struct ConcurrencyParkTicket<K>(GuardedParkTicket<K>);

impl<K> std::fmt::Debug for ConcurrencyParkTicket<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ConcurrencyParkTicket")
            .field(&self.0)
            .finish()
    }
}

/// A configured concurrency policy already has outstanding permits and
/// therefore cannot be transferred into a parked-caller owner safely.
#[derive(Debug)]
pub struct ConcurrencyPendingInitError {
    /// The unchanged policy, returned to its owner for settlement.
    pub limit: ConcurrencyLimit,
}

/// Snapshot of admission and parked-caller lifecycle truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConcurrencyPendingReport {
    /// The underlying concurrency policy report.
    pub admission: crate::AdmissionReport,
    /// Parked reply slots currently occupied.
    pub parked: usize,
    /// Highest parked reply count observed.
    pub parked_high_water: usize,
    /// Pending-table full rejections. Normally zero because the table and
    /// concurrency limit have the same capacity.
    pub pending_full_count: u64,
    /// Duplicate-key rejections.
    pub duplicate_key_count: u64,
    /// Callers reclaimed after their reply slot closed.
    pub caller_gone_count: u64,
    /// Permits released because a reply was intentionally produced.
    pub completed_count: u64,
    /// Permits retired without completion: caller gone, drain, owner drop, or
    /// rollback after a pending-table rejection.
    pub retired_count: u64,
}

impl ConcurrencyPendingReport {
    /// Whether policy and parked-slot live counts agree.
    pub fn counts_agree(&self) -> bool {
        self.admission.current == self.parked
    }
}

/// Why parking a split-service request failed.
pub enum ConcurrencyParkError<'a, K, I: Isolate> {
    /// The concurrency policy refused admission.
    Admission {
        /// Key returned unchanged.
        key: K,
        /// Caller authority returned unchanged.
        call: RequestCall<'a, I>,
        /// Typed admission failure and pressure report.
        failure: AdmissionFailure,
    },
    /// A live caller already occupies this key.
    DuplicateKey {
        /// Key returned unchanged.
        key: K,
        /// Caller authority returned unchanged.
        call: RequestCall<'a, I>,
    },
    /// The pending table refused despite concurrency admission. This is
    /// surfaced distinctly because hiding a count mismatch would make
    /// pressure reporting dishonest.
    PendingFull {
        /// Key returned unchanged.
        key: K,
        /// Caller authority returned unchanged.
        call: RequestCall<'a, I>,
    },
}

impl<K: std::fmt::Debug, I: Isolate> std::fmt::Debug for ConcurrencyParkError<'_, K, I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Admission { key, failure, .. } => f
                .debug_struct("ConcurrencyParkError::Admission")
                .field("key", key)
                .field("failure", failure)
                .finish(),
            Self::DuplicateKey { key, .. } => f
                .debug_struct("ConcurrencyParkError::DuplicateKey")
                .field("key", key)
                .finish(),
            Self::PendingFull { key, .. } => f
                .debug_struct("ConcurrencyParkError::PendingFull")
                .field("key", key)
                .finish(),
        }
    }
}

/// Why inserting an already captured reply failed.
#[derive(Debug)]
pub enum ConcurrencyInsertError<K, R> {
    /// The concurrency policy refused admission.
    Admission {
        /// Key returned unchanged.
        key: K,
        /// Reply slot returned unchanged.
        reply: DeferredReply<R>,
        /// Typed admission failure.
        failure: AdmissionFailure,
    },
    /// A live caller already occupies this key.
    DuplicateKey {
        /// Key returned unchanged.
        key: K,
        /// Reply slot returned unchanged.
        reply: DeferredReply<R>,
    },
    /// The pending table refused despite concurrency admission.
    PendingFull {
        /// Key returned unchanged.
        key: K,
        /// Reply slot returned unchanged.
        reply: DeferredReply<R>,
    },
}

/// Why inserting a captured reply with an auxiliary RAII guard failed.
#[derive(Debug)]
pub enum ConcurrencyGuardedInsertError<K, R, G> {
    /// The concurrency policy refused admission.
    Admission {
        /// Key returned unchanged.
        key: K,
        /// Reply slot returned unchanged.
        reply: DeferredReply<R>,
        /// Auxiliary guard returned unchanged.
        guard: G,
        /// Typed admission failure.
        failure: AdmissionFailure,
    },
    /// A live caller already occupies this key.
    DuplicateKey {
        /// Key returned unchanged.
        key: K,
        /// Reply slot returned unchanged.
        reply: DeferredReply<R>,
        /// Auxiliary guard returned unchanged.
        guard: G,
    },
    /// The pending table refused despite concurrency admission.
    PendingFull {
        /// Key returned unchanged.
        key: K,
        /// Reply slot returned unchanged.
        reply: DeferredReply<R>,
        /// Auxiliary guard returned unchanged.
        guard: G,
    },
}

/// Why a parked reply could not be settled.
#[derive(Debug)]
pub enum ConcurrencyReplyError<K, R> {
    /// The ticket names an empty slot.
    Missing {
        /// Reply value returned unchanged.
        reply: R,
        /// Key type witness.
        _key: PhantomData<fn(K) -> K>,
    },
    /// The ticket generation no longer names the current occupant.
    StaleTicket {
        /// Reply value returned unchanged.
        reply: R,
        /// Key type witness.
        _key: PhantomData<fn(K) -> K>,
    },
}

/// Bounded parked callers charged against one local concurrency policy.
///
/// The permit type is deliberately absent from the public methods. This
/// owner is the only code that can settle it against the issuing limit, so a
/// wrong-owner release is unrepresentable and owner drop cannot leak capacity.
pub struct ConcurrencyPendingReplies<K: PartialEq, R, G = ()> {
    limit: ConcurrencyLimit,
    pending: GuardedPendingReplies<K, R, (ConcurrencyPermit, G)>,
    completed_count: u64,
    retired_count: u64,
}

impl<K: PartialEq, R, G> std::fmt::Debug for ConcurrencyPendingReplies<K, R, G> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConcurrencyPendingReplies")
            .field("report", &self.report())
            .finish()
    }
}

impl<K: PartialEq, R, G> ConcurrencyPendingReplies<K, R, G> {
    /// Build from a configured, idle concurrency policy.
    ///
    /// A policy with outstanding permits is returned unchanged; accepting it
    /// would orphan permits outside this owner and make live counts diverge.
    #[allow(clippy::result_large_err)]
    pub fn try_from_limit(limit: ConcurrencyLimit) -> Result<Self, ConcurrencyPendingInitError> {
        let report = limit.report();
        if report.current != 0 {
            return Err(ConcurrencyPendingInitError { limit });
        }
        let pending = GuardedPendingReplies::with_capacity(report.capacity)
            .named(format!("{}.parked", report.surface));
        Ok(Self {
            limit,
            pending,
            completed_count: 0,
            retired_count: 0,
        })
    }

    /// Sweep caller-gone slots and retire their concurrency charges.
    pub fn sweep(&mut self) -> usize {
        let reclaimed = self.pending.reclaim_closed();
        let count = reclaimed.len();
        for (_, (permit, guard)) in reclaimed {
            self.retire(permit);
            drop(guard);
        }
        count
    }

    /// Insert an already captured deferred reply with an auxiliary RAII guard.
    #[allow(clippy::result_large_err)]
    pub fn insert_deferred_guarded(
        &mut self,
        key: K,
        reply: DeferredReply<R>,
        guard: G,
    ) -> Result<ConcurrencyParkTicket<K>, ConcurrencyGuardedInsertError<K, R, G>> {
        self.sweep();
        let permit = match self.limit.try_admit().into_admitted() {
            Ok(permit) => permit,
            Err(failure) => {
                return Err(ConcurrencyGuardedInsertError::Admission {
                    key,
                    reply,
                    guard,
                    failure,
                });
            }
        };
        match self
            .pending
            .insert_deferred_guarded(key, reply, (permit, guard))
        {
            Ok(ticket) => Ok(ConcurrencyParkTicket(ticket)),
            Err(GuardedInsertError::DuplicateKey {
                key,
                reply,
                guard: (permit, guard),
            }) => {
                self.retire(permit);
                Err(ConcurrencyGuardedInsertError::DuplicateKey { key, reply, guard })
            }
            Err(GuardedInsertError::Full {
                key,
                reply,
                guard: (permit, guard),
            }) => {
                self.retire(permit);
                Err(ConcurrencyGuardedInsertError::PendingFull { key, reply, guard })
            }
        }
    }

    /// Reply to a parked caller by generation-stamped ticket. The permit is
    /// released as a completion before the reply effect becomes observable.
    pub fn reply_ticket<I>(
        &mut self,
        ticket: ConcurrencyParkTicket<K>,
        reply: R,
    ) -> Result<Effect<I>, ConcurrencyReplyError<K, R>>
    where
        I: Isolate<Reply = R>,
        R: 'static,
    {
        match self.pending.take_ticket(ticket.0) {
            Ok((slot, (permit, guard))) => {
                self.release(permit);
                drop(guard);
                Ok(reply_to::<I>(slot, reply))
            }
            Err(GuardedTakeError::Missing) => Err(ConcurrencyReplyError::Missing {
                reply,
                _key: PhantomData,
            }),
            Err(GuardedTakeError::StaleTicket) => Err(ConcurrencyReplyError::StaleTicket {
                reply,
                _key: PhantomData,
            }),
            Err(GuardedTakeError::_Phantom(_)) => unreachable!(),
        }
    }

    fn take_by_key_for_reply(&mut self, key: &K) -> Option<(DeferredReply<R>, G)> {
        let (slot, (permit, guard)) = self.pending.take_by_key(key)?;
        self.release(permit);
        Some((slot, guard))
    }

    /// Reply by key, release the concurrency charge as completed, and drop the
    /// auxiliary guard. Prefer tickets where stale-key ABA is possible.
    pub fn reply_by_key<I>(&mut self, key: &K, reply: R) -> Option<Effect<I>>
    where
        I: Isolate<Reply = R>,
        R: 'static,
    {
        let (slot, guard) = self.take_by_key_for_reply(key)?;
        drop(guard);
        Some(reply_to::<I>(slot, reply))
    }

    /// Abandon a parked caller by key and retire its local charge. The closed
    /// reply slot and auxiliary guard are returned for explicit caller-owned
    /// cleanup or stage handoff.
    pub fn retire_by_key(&mut self, key: &K) -> Option<(DeferredReply<R>, G)> {
        let (slot, (permit, guard)) = self.pending.take_by_key(key)?;
        self.retire(permit);
        Some((slot, guard))
    }

    /// Drain all parked callers and retire their charges. Returned reply slots
    /// and auxiliary guards let the service perform explicit shutdown cleanup.
    pub fn drain(&mut self) -> Vec<(K, DeferredReply<R>, G)> {
        let entries = self.pending.drain();
        let mut replies = Vec::with_capacity(entries.len());
        for (key, reply, (permit, guard)) in entries {
            self.retire(permit);
            replies.push((key, reply, guard));
        }
        replies
    }

    /// Close future concurrency admissions.
    pub fn close(&mut self) {
        self.limit.close();
    }

    /// Snapshot pressure and lifecycle accounting.
    pub fn report(&self) -> ConcurrencyPendingReport {
        ConcurrencyPendingReport {
            admission: self.limit.report(),
            parked: self.pending.len(),
            parked_high_water: self.pending.high_water(),
            pending_full_count: self.pending.full_rejects(),
            duplicate_key_count: self.pending.duplicate_keys(),
            caller_gone_count: self.pending.reclaimed(),
            completed_count: self.completed_count,
            retired_count: self.retired_count,
        }
    }

    fn release(&mut self, permit: ConcurrencyPermit) {
        self.limit
            .release(permit)
            .expect("owned concurrency permit must release on its issuing limit");
        self.completed_count = self.completed_count.saturating_add(1);
    }

    fn retire(&mut self, permit: ConcurrencyPermit) {
        self.limit
            .retire(permit)
            .expect("owned concurrency permit must retire on its issuing limit");
        self.retired_count = self.retired_count.saturating_add(1);
    }
}

impl<K: PartialEq, R, G> Drop for ConcurrencyPendingReplies<K, R, G> {
    fn drop(&mut self) {
        let _ = self.drain();
    }
}

impl<K: PartialEq, R> ConcurrencyPendingReplies<K, R, ()> {
    /// Build a shed-on-full parked-caller owner with a fixed local capacity.
    pub fn with_capacity(surface: impl Into<crate::SurfaceName>, capacity: usize) -> Self {
        Self::try_from_limit(ConcurrencyLimit::with_capacity(surface, capacity))
            .expect("a newly constructed ConcurrencyLimit is idle")
    }

    /// Park split-service caller authority after concurrency admission.
    #[allow(clippy::result_large_err)]
    pub fn park_request<'a, I>(
        &mut self,
        key: K,
        call: RequestCall<'a, I>,
    ) -> Result<ConcurrencyParkTicket<K>, ConcurrencyParkError<'a, K, I>>
    where
        I: Isolate<Reply = R>,
        R: 'static,
    {
        self.sweep();
        let permit = match self.limit.try_admit().into_admitted() {
            Ok(permit) => permit,
            Err(failure) => return Err(ConcurrencyParkError::Admission { key, call, failure }),
        };
        match self.pending.park_request_guarded(key, call, (permit, ())) {
            Ok(ticket) => Ok(ConcurrencyParkTicket(ticket)),
            Err(GuardedParkError::DuplicateKey {
                key,
                call,
                guard: (permit, ()),
            }) => {
                self.retire(permit);
                Err(ConcurrencyParkError::DuplicateKey { key, call })
            }
            Err(GuardedParkError::Full {
                key,
                call,
                guard: (permit, ()),
            }) => {
                self.retire(permit);
                Err(ConcurrencyParkError::PendingFull { key, call })
            }
        }
    }

    /// Insert an already captured deferred reply after concurrency admission.
    #[allow(clippy::result_large_err)]
    pub fn insert_deferred(
        &mut self,
        key: K,
        reply: DeferredReply<R>,
    ) -> Result<ConcurrencyParkTicket<K>, ConcurrencyInsertError<K, R>> {
        match self.insert_deferred_guarded(key, reply, ()) {
            Ok(ticket) => Ok(ticket),
            Err(ConcurrencyGuardedInsertError::Admission {
                key,
                reply,
                failure,
                ..
            }) => Err(ConcurrencyInsertError::Admission {
                key,
                reply,
                failure,
            }),
            Err(ConcurrencyGuardedInsertError::DuplicateKey { key, reply, .. }) => {
                Err(ConcurrencyInsertError::DuplicateKey { key, reply })
            }
            Err(ConcurrencyGuardedInsertError::PendingFull { key, reply, .. }) => {
                Err(ConcurrencyInsertError::PendingFull { key, reply })
            }
        }
    }
}

/// Build a request effect after [`ConcurrencyPendingReplies::park_request`]
/// consumed caller authority. The ticket is an unforgeable admission witness.
pub fn request_effect_after_concurrency_park<I, K>(
    ticket: &ConcurrencyParkTicket<K>,
    effect: Effect<I>,
) -> RequestEffect<I>
where
    I: Isolate,
{
    let _ = ticket;
    crate::call::request_effect_from_consumed_effect(effect)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tina::runtime_internal::{deferred_from_handle, handle_from_shared};
    use tina::{DeferredSlotShared, DeferredSlotState, SingleShard};

    fn fake_slot(id: u64) -> DeferredReply<u32> {
        let shared =
            std::sync::Arc::new(DeferredSlotShared::new(id, std::any::TypeId::of::<u32>()));
        deferred_from_handle(handle_from_shared(shared))
    }

    fn fake_closed_slot(id: u64) -> DeferredReply<u32> {
        let shared =
            std::sync::Arc::new(DeferredSlotShared::new(id, std::any::TypeId::of::<u32>()));
        shared.set_state(DeferredSlotState::Closed);
        deferred_from_handle(handle_from_shared(shared))
    }

    #[derive(Debug)]
    struct TestIso;

    impl Isolate for TestIso {
        type Message = ();
        type Reply = u32;
        type Send = tina::Outbound<std::convert::Infallible>;
        type Spawn = std::convert::Infallible;
        type SpawnObserved = std::convert::Infallible;
        type Io = std::convert::Infallible;
        type Fact = std::convert::Infallible;
        type Shard = SingleShard;

        fn handle(&mut self, _: (), _: &mut tina::Context<'_, SingleShard, u32>) -> Effect<Self> {
            tina::noop()
        }
    }

    #[test]
    fn full_duplicate_and_reply_keep_counts_honest() {
        let mut pending = ConcurrencyPendingReplies::with_capacity("test.parked", 2);
        let first = pending.insert_deferred(1, fake_slot(1)).unwrap();

        assert!(matches!(
            pending.insert_deferred(1, fake_slot(2)),
            Err(ConcurrencyInsertError::DuplicateKey { .. })
        ));
        let second = pending.insert_deferred(2, fake_slot(3)).unwrap();
        assert!(matches!(
            pending.insert_deferred(3, fake_slot(4)),
            Err(ConcurrencyInsertError::Admission {
                failure: AdmissionFailure::Full(_),
                ..
            })
        ));

        let _: Effect<TestIso> = pending.reply_ticket(first, 10).unwrap();
        let _: Effect<TestIso> = pending.reply_ticket(second, 11).unwrap();
        let report = pending.report();
        assert!(report.counts_agree());
        assert_eq!(report.parked, 0);
        assert_eq!(report.completed_count, 2);
        assert_eq!(report.retired_count, 1, "duplicate admission rolled back");
        assert_eq!(report.admission.full_count, 1);
        assert_eq!(report.pending_full_count, 0);
    }

    #[test]
    fn caller_gone_and_drain_retire_without_capacity_leaks() {
        let _guard = crate::local_permit::DROPPED_PERMIT_TEST_LOCK
            .lock()
            .expect("drop test lock");
        let before = crate::dropped_permit_count();
        let mut pending = ConcurrencyPendingReplies::with_capacity("test.lifecycle", 2);
        pending.insert_deferred(1, fake_closed_slot(1)).unwrap();
        pending.insert_deferred(2, fake_slot(2)).unwrap();

        assert_eq!(
            pending.sweep(),
            0,
            "the next admission already swept caller-gone"
        );
        let drained = pending.drain();
        assert_eq!(drained.len(), 1);
        let report = pending.report();
        assert!(report.counts_agree());
        assert_eq!(report.caller_gone_count, 1);
        assert_eq!(report.retired_count, 2);
        assert_eq!(report.completed_count, 0);
        drop(drained);
        drop(pending);
        assert_eq!(crate::dropped_permit_count(), before);
    }

    #[test]
    fn owner_drop_retires_every_live_permit() {
        let _guard = crate::local_permit::DROPPED_PERMIT_TEST_LOCK
            .lock()
            .expect("drop test lock");
        let before = crate::dropped_permit_count();
        {
            let mut pending = ConcurrencyPendingReplies::with_capacity("test.owner_drop", 2);
            pending.insert_deferred(1, fake_slot(1)).unwrap();
            pending.insert_deferred(2, fake_slot(2)).unwrap();
        }
        assert_eq!(crate::dropped_permit_count(), before);
    }

    #[test]
    fn precharged_limit_is_rejected_and_returned_for_correct_owner_release() {
        let mut limit = ConcurrencyLimit::with_capacity("test.precharged", 1);
        let permit = limit.try_admit().into_admitted().unwrap();
        let err = ConcurrencyPendingReplies::<u64, u32>::try_from_limit(limit)
            .expect_err("outstanding permit must prevent ownership transfer");
        let mut limit = err.limit;
        assert_eq!(limit.report().current, 1);
        limit.release(permit).unwrap();
        assert_eq!(limit.report().current, 0);
    }

    #[test]
    fn stale_generation_ticket_cannot_settle_reused_slot() {
        let mut pending = ConcurrencyPendingReplies::with_capacity("test.stale", 1);
        let stale = pending.insert_deferred(1, fake_slot(1)).unwrap();
        let (_slot, ()) = pending.retire_by_key(&1).unwrap();
        let current = pending.insert_deferred(2, fake_slot(2)).unwrap();

        assert!(matches!(
            pending.reply_ticket::<TestIso>(stale, 10),
            Err(ConcurrencyReplyError::StaleTicket { .. })
        ));
        let _: Effect<TestIso> = pending.reply_ticket(current, 11).unwrap();
        assert_eq!(pending.report().completed_count, 1);
        assert_eq!(pending.report().retired_count, 1);
    }

    #[test]
    fn wrong_owner_ticket_cannot_settle_matching_slot_generation() {
        let mut left = ConcurrencyPendingReplies::with_capacity("test.left", 1);
        let mut right = ConcurrencyPendingReplies::with_capacity("test.right", 1);
        let wrong_owner = left.insert_deferred(1, fake_slot(1)).unwrap();
        let right_ticket = right.insert_deferred(2, fake_slot(2)).unwrap();

        assert!(matches!(
            right.reply_ticket::<TestIso>(wrong_owner, 10),
            Err(ConcurrencyReplyError::StaleTicket { .. })
        ));
        let _: Effect<TestIso> = right.reply_ticket(right_ticket, 11).unwrap();
        assert_eq!(right.report().completed_count, 1);
        assert_eq!(left.report().parked, 1);
    }
}
