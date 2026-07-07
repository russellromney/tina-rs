//! Bounded observed broadcast helpers.
//!
//! These helpers are the copied Tina path for "send this to many sessions" or
//! "fan this event to many subscribers." The service chooses `max_targets`
//! before any effects exist, every attempted send goes through
//! [`send_observed`], and the owner receives one ordinary
//! continuation message per target.
//!
//! This is deliberately not a room framework. It is the small bounded kernel
//! room/session services can build on.

use std::error::Error;
use std::fmt;

use tina::{Address, Effect, Isolate};

use crate::{RuntimeCall, SendOutcome, send_observed};

/// Errors while building a bounded target list for a broadcast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BroadcastTargetsError {
    /// `max_targets` was zero. A zero-width broadcast is almost always a config
    /// bug; callers that truly want to do nothing can skip the helper.
    ZeroMaxTargets,
    /// More targets were observed than the configured service-owned cap.
    TooManyTargets {
        /// Service-owned maximum target count.
        max: usize,
        /// Count observed before construction stopped.
        attempted: usize,
    },
    /// A target key appeared more than once.
    DuplicateTarget {
        /// Index of the duplicate target in the iterator.
        index: usize,
    },
}

impl fmt::Display for BroadcastTargetsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroMaxTargets => f.write_str("broadcast max_targets must be positive"),
            Self::TooManyTargets { max, attempted } => {
                write!(
                    f,
                    "broadcast target count {attempted} exceeded configured max_targets {max}",
                )
            }
            Self::DuplicateTarget { index } => {
                write!(f, "broadcast target key at index {index} was duplicated")
            }
        }
    }
}

impl Error for BroadcastTargetsError {}

/// One keyed broadcast target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BroadcastTarget<K, M> {
    key: K,
    address: Address<M>,
}

impl<K, M> BroadcastTarget<K, M> {
    /// Build one target from any typed Tina address.
    pub fn new<R>(key: K, address: Address<M, R>) -> Self {
        Self {
            key,
            address: address.with_reply::<()>(),
        }
    }

    /// Target identity carried back through the report.
    pub fn key(&self) -> &K {
        &self.key
    }

    /// Target address.
    pub fn address(&self) -> Address<M> {
        self.address
    }
}

/// Service-owned bounded target list.
///
/// The broadcast effect helper accepts this type instead of a raw `Vec`, so a
/// request-sized list must pass through a service-owned cap before it can become
/// many runtime effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BroadcastTargets<K, M> {
    max_targets: usize,
    targets: Vec<BroadcastTarget<K, M>>,
}

impl<K, M> BroadcastTargets<K, M> {
    /// Build a bounded target list from keyed addresses.
    ///
    /// Stops at the first over-cap item and returns `TooManyTargets`; it does
    /// not keep walking an unbounded iterator just to count every extra item.
    pub fn try_from_iter<R>(
        max_targets: usize,
        iter: impl IntoIterator<Item = (K, Address<M, R>)>,
    ) -> Result<Self, BroadcastTargetsError>
    where
        K: Eq,
    {
        if max_targets == 0 {
            return Err(BroadcastTargetsError::ZeroMaxTargets);
        }
        let mut targets: Vec<BroadcastTarget<K, M>> = Vec::with_capacity(max_targets);
        for (index, (key, address)) in iter.into_iter().enumerate() {
            if targets.len() == max_targets {
                return Err(BroadcastTargetsError::TooManyTargets {
                    max: max_targets,
                    attempted: max_targets + 1,
                });
            }
            if targets.iter().any(|target| target.key == key) {
                return Err(BroadcastTargetsError::DuplicateTarget { index });
            }
            targets.push(BroadcastTarget::new(key, address));
        }
        Ok(Self {
            max_targets,
            targets,
        })
    }

    /// Configured service-owned cap.
    pub fn max_targets(&self) -> usize {
        self.max_targets
    }

    /// Admitted target count.
    pub fn len(&self) -> usize {
        self.targets.len()
    }

    /// True when the bounded list contains no targets.
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    /// Iterate over admitted targets.
    pub fn iter(&self) -> impl Iterator<Item = &BroadcastTarget<K, M>> {
        self.targets.iter()
    }

    /// Consume into target entries.
    pub fn into_vec(self) -> Vec<BroadcastTarget<K, M>> {
        self.targets
    }
}

impl<K, M> BroadcastTargets<K, M>
where
    K: Clone + Eq,
{
    /// Build a tracker over this target list.
    pub fn tracker(&self) -> BroadcastTracker<K> {
        BroadcastTracker::new(
            self.max_targets,
            self.targets.iter().map(|target| target.key.clone()),
        )
    }
}

/// One target's observed broadcast outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BroadcastOutcome<K> {
    /// Target identity supplied by the service.
    pub key: K,
    /// Runtime-observed send outcome.
    pub outcome: SendOutcome,
}

/// Completed broadcast report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BroadcastReport<K> {
    max_targets: usize,
    outcomes: Vec<BroadcastOutcome<K>>,
}

impl<K> BroadcastReport<K> {
    fn new(max_targets: usize, outcomes: Vec<BroadcastOutcome<K>>) -> Self {
        Self {
            max_targets,
            outcomes,
        }
    }

    /// Service-owned maximum target count used for the broadcast.
    pub fn max_targets(&self) -> usize {
        self.max_targets
    }

    /// Per-target outcomes in target-list order.
    pub fn outcomes(&self) -> &[BroadcastOutcome<K>] {
        &self.outcomes
    }

    /// Number of accepted targets.
    pub fn accepted(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|o| o.outcome.is_accepted())
            .count()
    }

    /// Number of full targets.
    pub fn full(&self) -> usize {
        self.outcomes.iter().filter(|o| o.outcome.is_full()).count()
    }

    /// Number of closed targets.
    pub fn closed(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|o| o.outcome.is_closed())
            .count()
    }

    /// True when every admitted target produced exactly one outcome.
    pub fn is_complete(&self) -> bool {
        self.outcomes.len() <= self.max_targets
            && self.accepted() + self.full() + self.closed() == self.outcomes.len()
    }

    /// Assert the report has exactly the expected number of outcomes and no
    /// unknown outcome category.
    pub fn assert_all_accounted_for(
        &self,
        expected_targets: usize,
    ) -> Result<(), BroadcastAssertError> {
        if self.outcomes.len() != expected_targets {
            return Err(BroadcastAssertError::CountMismatch {
                expected: expected_targets,
                actual: self.outcomes.len(),
            });
        }
        if !self.is_complete() {
            return Err(BroadcastAssertError::Incomplete);
        }
        Ok(())
    }
}

/// Assertion failure for a completed [`BroadcastReport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BroadcastAssertError {
    /// Report count did not match the caller's expected target count.
    CountMismatch {
        /// Expected outcome count.
        expected: usize,
        /// Actual outcome count.
        actual: usize,
    },
    /// Outcomes did not add up to the known `SendOutcome` categories.
    Incomplete,
}

impl fmt::Display for BroadcastAssertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CountMismatch { expected, actual } => {
                write!(
                    f,
                    "broadcast report has {actual} outcomes, expected {expected}"
                )
            }
            Self::Incomplete => f.write_str("broadcast report has unaccounted outcomes"),
        }
    }
}

impl Error for BroadcastAssertError {}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingBroadcastOutcome<K> {
    key: K,
    outcome: Option<SendOutcome>,
}

/// Bounded tracker for a broadcast in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BroadcastTracker<K> {
    max_targets: usize,
    pending: Vec<PendingBroadcastOutcome<K>>,
}

impl<K> BroadcastTracker<K>
where
    K: Clone + Eq,
{
    /// Build a tracker from the configured cap and admitted target keys.
    fn new(max_targets: usize, keys: impl IntoIterator<Item = K>) -> Self {
        Self {
            max_targets,
            pending: keys
                .into_iter()
                .map(|key| PendingBroadcastOutcome { key, outcome: None })
                .collect(),
        }
    }

    /// Number of target outcomes expected.
    pub fn expected(&self) -> usize {
        self.pending.len()
    }

    /// Number of outcomes already recorded.
    pub fn observed(&self) -> usize {
        self.pending.iter().filter(|p| p.outcome.is_some()).count()
    }

    /// Record one observed send. Returns `Ok(Some(report))` once every target
    /// has reported.
    pub fn record(
        &mut self,
        key: K,
        outcome: SendOutcome,
    ) -> Result<Option<BroadcastReport<K>>, BroadcastRecordError<K>> {
        let Some(slot) = self.pending.iter_mut().find(|p| p.key == key) else {
            return Err(BroadcastRecordError::UnknownTarget { key });
        };
        if slot.outcome.is_some() {
            return Err(BroadcastRecordError::DuplicateTarget { key });
        }
        slot.outcome = Some(outcome);
        if self.observed() != self.expected() {
            return Ok(None);
        }
        Ok(Some(self.report()))
    }

    /// Return a report if every target has already reported. This is useful
    /// for the empty-target case, where no continuation message will fire.
    pub fn report_if_complete(&self) -> Option<BroadcastReport<K>> {
        (self.observed() == self.expected()).then(|| self.report())
    }

    fn report(&self) -> BroadcastReport<K> {
        let outcomes = self
            .pending
            .iter()
            .map(|slot| BroadcastOutcome {
                key: slot.key.clone(),
                outcome: slot.outcome.expect("all outcomes observed"),
            })
            .collect();
        BroadcastReport::new(self.max_targets, outcomes)
    }
}

/// Error while recording a broadcast outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BroadcastRecordError<K> {
    /// Outcome key was not part of the admitted target list.
    UnknownTarget {
        /// Unknown key.
        key: K,
    },
    /// Outcome key was already recorded.
    DuplicateTarget {
        /// Duplicate key.
        key: K,
    },
}

impl<K> fmt::Display for BroadcastRecordError<K>
where
    K: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTarget { key } => write!(f, "unknown broadcast target {key:?}"),
            Self::DuplicateTarget { key } => write!(f, "duplicate broadcast target {key:?}"),
        }
    }
}

impl<K> Error for BroadcastRecordError<K> where K: fmt::Debug {}

/// Build the observed-send effect batch for a bounded target list.
///
/// `make_message` runs once per admitted target. The continuation still returns
/// ordinary messages to the caller's isolate; the helper does not mutate user
/// state in a hidden callback.
pub fn broadcast_observed<I, K, M, P, MakeMessage, Continue>(
    targets: BroadcastTargets<K, M>,
    mut make_message: MakeMessage,
    continuation: Continue,
) -> Effect<I>
where
    I: Isolate<Message = P, Io = RuntimeCall<P>>,
    K: 'static,
    M: Send + 'static,
    P: 'static,
    MakeMessage: FnMut(&K) -> M,
    Continue: Fn(K, SendOutcome) -> P + Clone + 'static,
{
    let effects: Vec<Effect<I>> = targets
        .into_vec()
        .into_iter()
        .map(|target| -> Effect<I> {
            let key = target.key;
            let message = make_message(&key);
            let continuation = continuation.clone();
            send_observed(target.address, message).then(move |outcome| continuation(key, outcome))
        })
        .collect();
    tina::batch(effects)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Msg {}

    #[test]
    fn targets_reject_zero_and_over_cap_before_effects_exist() {
        let addr = Address::<Msg>::new(tina::ShardId::new(1), tina::IsolateId::new(2));
        assert_eq!(
            BroadcastTargets::<u8, Msg>::try_from_iter(0, [(1, addr)]),
            Err(BroadcastTargetsError::ZeroMaxTargets)
        );
        let err = BroadcastTargets::try_from_iter(2, [(1, addr), (2, addr), (3, addr)])
            .expect_err("third target exceeds service cap");
        assert_eq!(
            err,
            BroadcastTargetsError::TooManyTargets {
                max: 2,
                attempted: 3,
            }
        );
    }

    #[test]
    fn tracker_reports_in_target_order_and_rejects_bad_keys() {
        let mut tracker = BroadcastTracker::new(8, [10_u8, 20, 30]);
        assert_eq!(tracker.record(20, SendOutcome::Full).unwrap(), None);
        assert!(matches!(
            tracker.record(99, SendOutcome::Closed),
            Err(BroadcastRecordError::UnknownTarget { key: 99 })
        ));
        assert!(matches!(
            tracker.record(20, SendOutcome::Closed),
            Err(BroadcastRecordError::DuplicateTarget { key: 20 })
        ));
        assert_eq!(tracker.record(10, SendOutcome::Accepted).unwrap(), None);
        let report = tracker
            .record(30, SendOutcome::Closed)
            .unwrap()
            .expect("all targets reported");
        assert_eq!(report.accepted(), 1);
        assert_eq!(report.full(), 1);
        assert_eq!(report.closed(), 1);
        assert_eq!(report.max_targets(), 8);
        assert_eq!(
            report
                .outcomes()
                .iter()
                .map(|o| (o.key, o.outcome))
                .collect::<Vec<_>>(),
            vec![
                (10, SendOutcome::Accepted),
                (20, SendOutcome::Full),
                (30, SendOutcome::Closed),
            ],
        );
        report.assert_all_accounted_for(3).unwrap();
    }

    #[test]
    fn targets_reject_duplicate_keys() {
        let addr = Address::<Msg>::new(tina::ShardId::new(1), tina::IsolateId::new(2));
        let err = BroadcastTargets::try_from_iter(4, [(1, addr), (1, addr)])
            .expect_err("duplicate target keys wedge completion accounting");
        assert_eq!(err, BroadcastTargetsError::DuplicateTarget { index: 1 });
    }

    #[test]
    fn empty_tracker_can_report_without_a_continuation() {
        let targets =
            BroadcastTargets::<u8, Msg>::try_from_iter(4, Vec::<(u8, Address<Msg>)>::new())
                .unwrap();
        let tracker = targets.tracker();
        let report = tracker
            .report_if_complete()
            .expect("empty broadcast is complete immediately");
        assert_eq!(report.max_targets(), 4);
        assert_eq!(report.outcomes().len(), 0);
        report.assert_all_accounted_for(0).unwrap();
    }
}
