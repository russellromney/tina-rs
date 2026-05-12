//! Bounded named helper for explicit isolate-call races.
//!
//! `CallGroup` is state and report vocabulary, not a scheduler. Users
//! still build and return the visible Tina effects:
//!
//! - reserve a [`CallGroupToken`] for each branch;
//! - start each branch with [`crate::call_with_handle`];
//! - store the returned [`tina::CallHandle`] with a user key;
//! - route each reply continuation back with the returned
//!   [`CallGroupToken`];
//! - when a first-success winner appears, return one explicit
//!   [`crate::cancel_call`] effect for each loser;
//! - wait for the named cancel outcomes before treating the report as
//!   complete.
//!
//! Use this helper for small, homogeneous fan-out where every branch
//! has the same reply type and the caller wants a named report. Do not
//! use it for heterogeneous reply sets (use a user enum), ordered
//! pipelines where stage boundaries matter, hidden retries,
//! idempotency policy, stream selection, or unbounded work queues.

use tina::{CallHandle, CancelOutcome};

use crate::CallOutcome;

/// Opaque generation returned by [`CallGroup::insert`].
///
/// Continuation messages must carry this token back to
/// [`CallGroup::record_reply`]. That prevents a stale continuation for
/// key `K` from removing a newer branch that reused the same key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use = "carry the token into the branch continuation"]
pub struct CallGroupToken {
    generation: u64,
}

impl CallGroupToken {
    /// Returns the monotonically assigned generation number.
    pub const fn generation(self) -> u64 {
        self.generation
    }
}

#[derive(Debug)]
struct Entry<K, R> {
    key: K,
    token: CallGroupToken,
    handle: CallHandle<R>,
}

#[derive(Debug)]
struct ExpectedCancel<K> {
    key: K,
    token: CallGroupToken,
}

/// Fixed-capacity group of named in-flight calls.
#[derive(Debug)]
pub struct CallGroup<K, R> {
    entries: Vec<Entry<K, R>>,
    reserved_tokens: Vec<CallGroupToken>,
    expected_cancels: Vec<ExpectedCancel<K>>,
    capacity: usize,
    next_generation: u64,
    branch_outcomes: Vec<CallGroupBranchOutcome<K, R>>,
    cancel_outcomes: Vec<CallGroupCancelOutcome<K>>,
    race_state: RaceState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RaceState {
    Collecting,
    CancellingLosers,
    Complete,
}

/// Reasons token reservation can fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallGroupReserveError {
    /// Every slot is already live or reserved.
    Full,
}

/// Reasons a branch insert can fail.
#[derive(Debug)]
pub enum CallGroupInsertError<K, R> {
    /// The group is full. The rejected key and handle are returned so
    /// the caller can surface pressure or cancel visibly.
    Full {
        /// Rejected branch key.
        key: K,
        /// Rejected call handle.
        handle: CallHandle<R>,
    },
    /// A live branch already uses this key.
    DuplicateKey {
        /// Rejected branch key.
        key: K,
        /// Rejected call handle.
        handle: CallHandle<R>,
    },
    /// The token was not produced by `reserve_token`, or it was
    /// already consumed by a prior insert.
    UnknownReservedToken {
        /// Rejected branch key.
        key: K,
        /// Rejected branch token.
        token: CallGroupToken,
        /// Rejected call handle.
        handle: CallHandle<R>,
    },
    /// A live branch already uses this token.
    DuplicateToken {
        /// Rejected branch key.
        key: K,
        /// Rejected branch token.
        token: CallGroupToken,
        /// Rejected call handle.
        handle: CallHandle<R>,
    },
}

/// A loser that should be cancelled by user code.
#[derive(Debug)]
#[must_use = "turn this into cancel_call(handle).reply(...) or handle it visibly"]
pub struct CallGroupCancelRequest<K, R> {
    key: K,
    token: CallGroupToken,
    handle: CallHandle<R>,
}

impl<K, R> CallGroupCancelRequest<K, R> {
    /// Returns the branch key.
    pub fn key(&self) -> &K {
        &self.key
    }

    /// Returns the branch token.
    pub const fn token(&self) -> CallGroupToken {
        self.token
    }

    /// Consumes the request into its parts.
    pub fn into_parts(self) -> (K, CallGroupToken, CallHandle<R>) {
        (self.key, self.token, self.handle)
    }
}

/// One named branch reply observed by a group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallGroupBranchOutcome<K, R> {
    /// Branch key.
    pub key: K,
    /// Branch generation token.
    pub token: CallGroupToken,
    /// Runtime call outcome.
    pub outcome: CallOutcome<R>,
    /// Whether the caller's classifier accepted the reply as the race
    /// winner.
    pub classified_success: bool,
}

/// One named loser-cancel outcome observed by a group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallGroupCancelOutcome<K> {
    /// Branch key.
    pub key: K,
    /// Branch generation token.
    pub token: CallGroupToken,
    /// Outcome returned by `cancel_call`.
    pub outcome: CancelOutcome,
}

/// A completed or partial group report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallGroupReport<K, R> {
    /// Winning branch, if the first-success policy found one.
    pub winner: Option<CallGroupToken>,
    /// Named branch replies and failures observed by the group.
    pub branch_outcomes: Vec<CallGroupBranchOutcome<K, R>>,
    /// Named loser-cancel outcomes observed by the group.
    pub cancel_outcomes: Vec<CallGroupCancelOutcome<K>>,
    /// Number of branches still pending when the report was taken.
    pub pending: usize,
    /// Whether all required facts for the selected policy were
    /// observed.
    pub complete: bool,
}

/// Result of recording a branch reply.
#[derive(Debug)]
pub struct CallGroupReplyStep<K, R> {
    /// Losers to cancel because this reply produced the first success.
    pub cancel_losers: Vec<CallGroupCancelRequest<K, R>>,
    /// True when the group report is now complete.
    pub report_ready: bool,
}

/// Reasons recording a reply can fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallGroupRecordReplyError<K, R> {
    /// The token does not name any live branch. This is the typed
    /// stale-continuation path; it must not remove a newer same-key
    /// branch.
    UnknownToken {
        /// Branch key carried by the continuation.
        key: K,
        /// Branch token carried by the continuation.
        token: CallGroupToken,
        /// Reply outcome that could not be recorded.
        outcome: CallOutcome<R>,
    },
    /// The token named a live branch, but the continuation carried a
    /// different key.
    KeyMismatch {
        /// Branch key carried by the continuation.
        key: K,
        /// Branch token carried by the continuation.
        token: CallGroupToken,
        /// Reply outcome that could not be recorded.
        outcome: CallOutcome<R>,
    },
    /// A first-success race already reached its winner and drained
    /// losers for cancellation.
    RaceAlreadyWon {
        /// Branch key carried by the continuation.
        key: K,
        /// Branch token carried by the continuation.
        token: CallGroupToken,
        /// Reply outcome that could not be recorded.
        outcome: CallOutcome<R>,
    },
}

/// Reasons recording a cancel outcome can fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallGroupRecordCancelError<K> {
    /// No loser cancel is expected for this key/token pair, or the
    /// outcome was already recorded.
    UnexpectedCancel {
        /// Branch key carried by the cancel continuation.
        key: K,
        /// Branch token carried by the cancel continuation.
        token: CallGroupToken,
        /// Outcome that could not be recorded.
        outcome: CancelOutcome,
    },
    /// The bounded cancel-outcome storage is already full.
    StorageFull {
        /// Branch key carried by the cancel continuation.
        key: K,
        /// Branch token carried by the cancel continuation.
        token: CallGroupToken,
        /// Outcome that could not be recorded.
        outcome: CancelOutcome,
    },
}

impl<K, R> CallGroup<K, R>
where
    K: PartialEq,
{
    /// Builds an empty group with fixed capacity.
    ///
    /// Panics when `capacity == 0`.
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(
            capacity > 0,
            "CallGroup requires capacity > 0; a zero-capacity group cannot name any wait",
        );
        Self {
            entries: Vec::with_capacity(capacity),
            reserved_tokens: Vec::with_capacity(capacity),
            expected_cancels: Vec::with_capacity(capacity),
            capacity,
            next_generation: 1,
            branch_outcomes: Vec::with_capacity(capacity),
            cancel_outcomes: Vec::with_capacity(capacity),
            race_state: RaceState::Collecting,
        }
    }

    /// Returns the fixed capacity.
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns the number of live pending branches.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true when no branches are live.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns true when the next insert would return `Full`.
    pub fn is_full(&self) -> bool {
        self.entries.len() + self.reserved_tokens.len() >= self.capacity
    }

    /// Inserts one named branch and returns its generation token.
    pub fn insert(
        &mut self,
        key: K,
        handle: CallHandle<R>,
    ) -> Result<CallGroupToken, CallGroupInsertError<K, R>> {
        if self.entries.is_empty() && matches!(self.race_state, RaceState::Complete) {
            self.branch_outcomes.clear();
            self.cancel_outcomes.clear();
            self.expected_cancels.clear();
            self.race_state = RaceState::Collecting;
        }
        if self.entries.iter().any(|entry| entry.key == key) {
            return Err(CallGroupInsertError::DuplicateKey { key, handle });
        }
        if self.is_full() {
            return Err(CallGroupInsertError::Full { key, handle });
        }
        let token = self.next_token();
        self.entries.push(Entry { key, token, handle });
        Ok(token)
    }

    /// Reserves a token before building a branch continuation.
    ///
    /// Use this with [`insert_reserved`](Self::insert_reserved) when
    /// the continuation message must contain the token that will later
    /// be used to record the reply.
    pub fn reserve_token(&mut self) -> Result<CallGroupToken, CallGroupReserveError> {
        if self.entries.len() + self.reserved_tokens.len() >= self.capacity {
            return Err(CallGroupReserveError::Full);
        }
        let token = self.next_token();
        self.reserved_tokens.push(token);
        Ok(token)
    }

    fn next_token(&mut self) -> CallGroupToken {
        let token = CallGroupToken {
            generation: self.next_generation,
        };
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("CallGroup generation counter exhausted");
        token
    }

    /// Inserts one named branch with a token previously returned by
    /// [`reserve_token`](Self::reserve_token).
    pub fn insert_reserved(
        &mut self,
        key: K,
        token: CallGroupToken,
        handle: CallHandle<R>,
    ) -> Result<(), CallGroupInsertError<K, R>> {
        if self.entries.is_empty() && matches!(self.race_state, RaceState::Complete) {
            self.branch_outcomes.clear();
            self.cancel_outcomes.clear();
            self.expected_cancels.clear();
            self.race_state = RaceState::Collecting;
        }
        let Some(reserved_pos) = self
            .reserved_tokens
            .iter()
            .position(|reserved| *reserved == token)
        else {
            return Err(CallGroupInsertError::UnknownReservedToken { key, token, handle });
        };
        if self.entries.iter().any(|entry| entry.key == key) {
            let _ = self.reserved_tokens.swap_remove(reserved_pos);
            return Err(CallGroupInsertError::DuplicateKey { key, handle });
        }
        if self.entries.iter().any(|entry| entry.token == token) {
            let _ = self.reserved_tokens.swap_remove(reserved_pos);
            return Err(CallGroupInsertError::DuplicateToken { key, token, handle });
        }
        let _ = self.reserved_tokens.swap_remove(reserved_pos);
        self.entries.push(Entry { key, token, handle });
        Ok(())
    }

    /// Records one branch reply under a first-success race policy.
    ///
    /// `is_success` is called only for `CallOutcome::Replied`.
    /// The first reply classified as success wins. All still-pending
    /// losers are returned to the caller as named cancel requests; the
    /// caller must return visible `cancel_call` effects and feed their
    /// outcomes back through [`record_cancel`](Self::record_cancel).
    pub fn record_reply<F>(
        &mut self,
        key: K,
        token: CallGroupToken,
        outcome: CallOutcome<R>,
        is_success: F,
    ) -> Result<CallGroupReplyStep<K, R>, CallGroupRecordReplyError<K, R>>
    where
        K: Clone,
        F: FnOnce(&R) -> bool,
    {
        if !matches!(self.race_state, RaceState::Collecting) {
            return Err(CallGroupRecordReplyError::RaceAlreadyWon {
                key,
                token,
                outcome,
            });
        }
        let Some(pos) = self.entries.iter().position(|entry| entry.token == token) else {
            return Err(CallGroupRecordReplyError::UnknownToken {
                key,
                token,
                outcome,
            });
        };
        if self.entries[pos].key != key {
            return Err(CallGroupRecordReplyError::KeyMismatch {
                key,
                token,
                outcome,
            });
        }

        let entry = self.entries.swap_remove(pos);
        let classified_success = match &outcome {
            CallOutcome::Replied(reply) => is_success(reply),
            CallOutcome::Full | CallOutcome::Closed | CallOutcome::Timeout => false,
        };
        assert!(
            self.branch_outcomes.len() < self.capacity,
            "CallGroup branch outcome storage exceeded fixed capacity",
        );
        self.branch_outcomes.push(CallGroupBranchOutcome {
            key: entry.key,
            token,
            outcome,
            classified_success,
        });

        if classified_success {
            let cancel_losers = self.drain_entries_for_cancel();
            let expected = cancel_losers.len();
            self.race_state = if expected == 0 {
                RaceState::Complete
            } else {
                RaceState::CancellingLosers
            };
            return Ok(CallGroupReplyStep {
                cancel_losers,
                report_ready: expected == 0,
            });
        }

        if self.entries.is_empty() {
            self.race_state = RaceState::Complete;
        }
        Ok(CallGroupReplyStep {
            cancel_losers: Vec::new(),
            report_ready: self.report_ready(),
        })
    }

    /// Records one loser-cancel outcome.
    pub fn record_cancel(
        &mut self,
        key: K,
        token: CallGroupToken,
        outcome: CancelOutcome,
    ) -> Result<bool, CallGroupRecordCancelError<K>> {
        let Some(expected_pos) = self
            .expected_cancels
            .iter()
            .position(|expected| expected.key == key && expected.token == token)
        else {
            return Err(CallGroupRecordCancelError::UnexpectedCancel {
                key,
                token,
                outcome,
            });
        };
        if self.cancel_outcomes.len() >= self.capacity {
            return Err(CallGroupRecordCancelError::StorageFull {
                key,
                token,
                outcome,
            });
        }
        self.expected_cancels.swap_remove(expected_pos);
        self.cancel_outcomes.push(CallGroupCancelOutcome {
            key,
            token,
            outcome,
        });
        if matches!(self.race_state, RaceState::CancellingLosers)
            && self.expected_cancels.is_empty()
        {
            self.race_state = RaceState::Complete;
        }
        Ok(self.report_ready())
    }

    /// Drains every live branch for explicit owner-stop cancellation.
    pub fn drain_pending_for_cancel(&mut self) -> Vec<CallGroupCancelRequest<K, R>>
    where
        K: Clone,
    {
        let drained = self.drain_entries_for_cancel();
        if drained.is_empty() {
            self.race_state = RaceState::Complete;
        } else {
            self.race_state = RaceState::CancellingLosers;
        }
        drained
    }

    /// Returns whether the policy has observed all required facts.
    pub fn report_ready(&self) -> bool {
        matches!(self.race_state, RaceState::Complete)
    }

    /// Consumes the group into a report.
    ///
    /// Taking a report before [`report_ready`](Self::report_ready) is
    /// true is allowed: the `pending` count and `complete` flag make
    /// the partial truth explicit.
    pub fn into_report(self) -> CallGroupReport<K, R> {
        let winner = self
            .branch_outcomes
            .iter()
            .find(|outcome| outcome.classified_success)
            .map(|outcome| outcome.token);
        CallGroupReport {
            winner,
            branch_outcomes: self.branch_outcomes,
            cancel_outcomes: self.cancel_outcomes,
            pending: self.entries.len(),
            complete: matches!(self.race_state, RaceState::Complete),
        }
    }

    fn drain_entries_for_cancel(&mut self) -> Vec<CallGroupCancelRequest<K, R>>
    where
        K: Clone,
    {
        let mut requests = Vec::with_capacity(self.entries.len());
        self.expected_cancels.clear();
        for entry in self.entries.drain(..) {
            self.expected_cancels.push(ExpectedCancel {
                key: entry.key.clone(),
                token: entry.token,
            });
            requests.push(CallGroupCancelRequest {
                key: entry.key,
                token: entry.token,
                handle: entry.handle,
            });
        }
        requests
    }
}

impl<K, R> CallGroupReport<K, R> {
    /// Returns the first successful branch outcome, if any.
    pub fn winner_outcome(&self) -> Option<&CallGroupBranchOutcome<K, R>> {
        let winner = self.winner?;
        self.branch_outcomes
            .iter()
            .find(|outcome| outcome.token == winner)
    }
}

#[cfg(test)]
mod tests {
    use std::any::TypeId;
    use std::sync::Arc;

    use super::*;
    use tina::{CallHandleShared, runtime_internal};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Reply(u8);

    fn make_handle<R: 'static>() -> CallHandle<R> {
        let shared = Arc::new(CallHandleShared::new(TypeId::of::<R>()));
        runtime_internal::call_handle_from_shared::<R>(shared)
    }

    #[test]
    fn fill_then_next_insert_returns_full() {
        let mut group: CallGroup<u8, Reply> = CallGroup::with_capacity(2);
        let _ = group.insert(1, make_handle()).unwrap();
        let _ = group.insert(2, make_handle()).unwrap();

        match group.insert(3, make_handle()) {
            Err(CallGroupInsertError::Full { key, handle: _ }) => assert_eq!(key, 3),
            _ => panic!("expected Full"),
        }
        assert_eq!(group.len(), 2);
    }

    #[test]
    fn duplicate_key_returns_typed_error() {
        let mut group: CallGroup<u8, Reply> = CallGroup::with_capacity(2);
        let _ = group.insert(7, make_handle()).unwrap();

        match group.insert(7, make_handle()) {
            Err(CallGroupInsertError::DuplicateKey { key, handle: _ }) => assert_eq!(key, 7),
            _ => panic!("expected DuplicateKey"),
        }
        assert_eq!(group.len(), 1);
    }

    #[test]
    fn reserved_token_holds_capacity_and_is_one_shot() {
        let mut group: CallGroup<u8, Reply> = CallGroup::with_capacity(1);
        let token = group.reserve_token().unwrap();
        assert!(group.reserve_token().is_err());
        assert!(group.is_full());

        group
            .insert_reserved(1, token, make_handle())
            .map_err(|_| ())
            .unwrap();
        match group.insert_reserved(2, token, make_handle()) {
            Err(CallGroupInsertError::UnknownReservedToken {
                key,
                token: rejected,
                handle: _,
            }) => {
                assert_eq!(key, 2);
                assert_eq!(rejected, token);
            }
            _ => panic!("expected consumed token to be rejected"),
        }
    }

    #[test]
    fn failed_reserved_insert_does_not_leak_capacity() {
        let mut group: CallGroup<u8, Reply> = CallGroup::with_capacity(2);
        let _ = group.insert(1, make_handle()).unwrap();
        let token = group.reserve_token().unwrap();

        match group.insert_reserved(1, token, make_handle()) {
            Err(CallGroupInsertError::DuplicateKey { key, handle: _ }) => assert_eq!(key, 1),
            _ => panic!("expected DuplicateKey"),
        }
        assert!(!group.is_full());
        let fresh = group.reserve_token().unwrap();
        group
            .insert_reserved(2, fresh, make_handle())
            .map_err(|_| ())
            .unwrap();
    }

    #[test]
    fn old_token_cannot_be_reused_after_refill() {
        let mut group: CallGroup<u8, Reply> = CallGroup::with_capacity(1);
        let old = group.reserve_token().unwrap();
        group
            .insert_reserved(1, old, make_handle())
            .map_err(|_| ())
            .unwrap();
        let step = group
            .record_reply(1, old, CallOutcome::Replied(Reply(1)), |_| false)
            .unwrap();
        assert!(step.report_ready);

        match group.insert_reserved(1, old, make_handle()) {
            Err(CallGroupInsertError::UnknownReservedToken {
                key,
                token,
                handle: _,
            }) => {
                assert_eq!(key, 1);
                assert_eq!(token, old);
            }
            _ => panic!("expected old token reuse to be rejected"),
        }
        let new = group.reserve_token().unwrap();
        assert_ne!(old, new);
        group
            .insert_reserved(1, new, make_handle())
            .map_err(|_| ())
            .unwrap();
    }

    #[test]
    fn token_prevents_key_aba_after_refill() {
        let mut group: CallGroup<u8, Reply> = CallGroup::with_capacity(1);
        let old = group.insert(9, make_handle()).unwrap();
        let step = group
            .record_reply(9, old, CallOutcome::Replied(Reply(1)), |_| false)
            .unwrap();
        assert!(step.report_ready);

        let new = group.insert(9, make_handle()).unwrap();
        assert_ne!(old, new);

        match group.record_reply(9, old, CallOutcome::Timeout, |_| false) {
            Err(CallGroupRecordReplyError::UnknownToken { key, token, .. }) => {
                assert_eq!(key, 9);
                assert_eq!(token, old);
            }
            _ => panic!("expected stale token to be rejected"),
        }

        let step = group
            .record_reply(9, new, CallOutcome::Replied(Reply(2)), |_| true)
            .unwrap();
        assert!(step.report_ready);
        let report = group.into_report();
        assert_eq!(report.winner, Some(new));
    }

    #[test]
    fn first_success_drains_losers_and_waits_for_cancel_outcomes() {
        let mut group: CallGroup<u8, Reply> = CallGroup::with_capacity(3);
        let a = group.insert(1, make_handle()).unwrap();
        let b = group.insert(2, make_handle()).unwrap();
        let c = group.insert(3, make_handle()).unwrap();

        let failed = group
            .record_reply(1, a, CallOutcome::Replied(Reply(0)), |reply| reply.0 >= 10)
            .unwrap();
        assert!(failed.cancel_losers.is_empty());
        assert!(!failed.report_ready);

        let won = group
            .record_reply(2, b, CallOutcome::Replied(Reply(10)), |reply| reply.0 >= 10)
            .unwrap();
        assert_eq!(won.cancel_losers.len(), 1);
        assert!(!won.report_ready);
        let (loser_key, loser_token, _handle) =
            won.cancel_losers.into_iter().next().unwrap().into_parts();
        assert_eq!(loser_key, 3);
        assert_eq!(loser_token, c);

        assert!(
            group
                .record_cancel(loser_key, loser_token, CancelOutcome::Cancelled)
                .unwrap()
        );
        let report = group.into_report();
        assert!(report.complete);
        assert_eq!(report.winner, Some(b));
        assert_eq!(report.cancel_outcomes.len(), 1);
    }

    #[test]
    fn forged_and_duplicate_cancel_outcomes_do_not_complete_report() {
        let mut group: CallGroup<u8, Reply> = CallGroup::with_capacity(3);
        let winner = group.insert(1, make_handle()).unwrap();
        let loser_a = group.insert(2, make_handle()).unwrap();
        let loser_b = group.insert(3, make_handle()).unwrap();

        let won = group
            .record_reply(1, winner, CallOutcome::Replied(Reply(10)), |reply| {
                reply.0 >= 10
            })
            .unwrap();
        assert_eq!(won.cancel_losers.len(), 2);
        assert!(!won.report_ready);

        match group.record_cancel(99, loser_a, CancelOutcome::Cancelled) {
            Err(CallGroupRecordCancelError::UnexpectedCancel { key, token, .. }) => {
                assert_eq!(key, 99);
                assert_eq!(token, loser_a);
            }
            _ => panic!("expected forged cancel key to be rejected"),
        }
        assert!(!group.report_ready());

        assert!(
            !group
                .record_cancel(2, loser_a, CancelOutcome::Cancelled)
                .unwrap()
        );
        match group.record_cancel(2, loser_a, CancelOutcome::Cancelled) {
            Err(CallGroupRecordCancelError::UnexpectedCancel { key, token, .. }) => {
                assert_eq!(key, 2);
                assert_eq!(token, loser_a);
            }
            _ => panic!("expected duplicate cancel to be rejected"),
        }
        assert!(!group.report_ready());

        assert!(
            group
                .record_cancel(3, loser_b, CancelOutcome::Cancelled)
                .unwrap()
        );
        let report = group.into_report();
        assert!(report.complete);
        assert_eq!(report.cancel_outcomes.len(), 2);
    }

    #[test]
    fn fill_cancel_complete_then_refill_works() {
        let mut group: CallGroup<u8, Reply> = CallGroup::with_capacity(2);
        let a = group.insert(1, make_handle()).unwrap();
        let b = group.insert(2, make_handle()).unwrap();
        let cancels = group.drain_pending_for_cancel();
        assert_eq!(cancels.len(), 2);
        assert!(group.is_empty());
        assert!(!group.report_ready());
        for request in cancels {
            let (key, token, _handle) = request.into_parts();
            group
                .record_cancel(key, token, CancelOutcome::Cancelled)
                .unwrap();
        }
        assert!(group.report_ready());

        let c = group.insert(1, make_handle()).unwrap();
        let d = group.insert(2, make_handle()).unwrap();
        assert_ne!(a, c);
        assert_ne!(b, d);
        assert!(group.is_full());
    }
}
