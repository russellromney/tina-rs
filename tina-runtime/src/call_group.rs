//! Bounded named helper for explicit isolate-call races.
//!
//! `CallGroup` is state and report vocabulary, not a scheduler. Users
//! still build and return the visible Tina effects. The copied path is
//! [`CallGroup::start_cancelable`]:
//!
//! - it reserves a [`CallGroupToken`];
//! - starts a [`crate::call_cancelable`] branch;
//! - stores the returned [`tina::CallHandle`];
//! - routes the reply continuation back with the token.
//!
//! The lower-level gears remain available when a service needs to split those
//! steps:
//!
//! - reserve a [`CallGroupToken`] for each branch;
//! - start each branch with [`crate::call_cancelable`];
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
use crate::call::{CancelableCall, RuntimeCall};

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

/// Reasons [`CallGroup::start_cancelable`] may reject a branch before
/// returning its effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallGroupStartError<K> {
    /// Every slot is already live or reserved.
    Full {
        /// Rejected branch key.
        key: K,
    },
    /// A live branch already uses this key.
    DuplicateKey {
        /// Rejected branch key.
        key: K,
    },
}

/// A loser that should be cancelled by user code.
#[derive(Debug)]
#[must_use = "turn this into cancel_call(handle).then(...) or handle it visibly"]
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
    /// A first-success race already reached its winner and this reply
    /// does not name one of the losers currently being cancelled.
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
        self.reset_completed_if_empty();
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
        self.reset_completed_if_empty();
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

    /// Builds, stores, and returns one cancelable branch effect.
    ///
    /// This is the copy path for first-success races:
    ///
    /// 1. reserve the generation token;
    /// 2. build the cancelable call continuation carrying that token;
    /// 3. store the handle under `key`;
    /// 4. return the effect only after storage succeeds.
    ///
    /// The helper does not schedule the effect and does not cancel anything by
    /// itself. Loser cancellation still comes back through
    /// [`record_reply`](Self::record_reply) as explicit
    /// [`CallGroupCancelRequest`] values.
    pub fn start_cancelable<I, M, T, F>(
        &mut self,
        key: K,
        call: CancelableCall<T, R>,
        translator: F,
    ) -> Result<tina::Effect<I>, CallGroupStartError<K>>
    where
        I: tina::Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(K, CallGroupToken, CallOutcome<R>) -> M + 'static,
        K: Clone + 'static,
        M: 'static,
        T: Send + 'static,
        R: 'static,
    {
        self.reset_completed_if_empty();
        if self.entries.iter().any(|entry| entry.key == key) {
            return Err(CallGroupStartError::DuplicateKey { key });
        }
        let token = self
            .reserve_token()
            .map_err(|CallGroupReserveError::Full| CallGroupStartError::Full {
                key: key.clone(),
            })?;
        let continuation_key = key.clone();
        let (effect, handle) =
            call.then(move |outcome| translator(continuation_key, token, outcome));
        let reserved_pos = self
            .reserved_tokens
            .iter()
            .position(|reserved| *reserved == token)
            .expect("start_cancelable just reserved this token");
        let _ = self.reserved_tokens.swap_remove(reserved_pos);
        self.entries.push(Entry { key, token, handle });
        Ok(effect)
    }

    fn reset_completed_if_empty(&mut self) {
        if self.entries.is_empty() && matches!(self.race_state, RaceState::Complete) {
            self.branch_outcomes.clear();
            self.cancel_outcomes.clear();
            self.expected_cancels.clear();
            self.race_state = RaceState::Collecting;
        }
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
            if self
                .expected_cancels
                .iter()
                .any(|expected| expected.key == key && expected.token == token)
                && !self
                    .branch_outcomes
                    .iter()
                    .any(|outcome| outcome.token == token)
            {
                assert!(
                    self.branch_outcomes.len() < self.capacity,
                    "CallGroup branch outcome storage exceeded fixed capacity",
                );
                self.branch_outcomes.push(CallGroupBranchOutcome {
                    key,
                    token,
                    outcome,
                    classified_success: false,
                });
                return Ok(CallGroupReplyStep {
                    cancel_losers: Vec::new(),
                    report_ready: self.report_ready(),
                });
            }
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
            CallOutcome::Full
            | CallOutcome::Closed
            | CallOutcome::Timeout
            | CallOutcome::Rejected(_) => false,
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
        if drained.is_empty() && self.expected_cancels.is_empty() {
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
            pending: self.entries.len() + self.reserved_tokens.len() + self.expected_cancels.len(),
            complete: matches!(self.race_state, RaceState::Complete),
        }
    }

    fn drain_entries_for_cancel(&mut self) -> Vec<CallGroupCancelRequest<K, R>>
    where
        K: Clone,
    {
        let mut requests = Vec::with_capacity(self.entries.len());
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

/// Generation token used by [`CallJoinSet`] branches.
pub type CallJoinToken = CallGroupToken;

/// Generation token used by [`CallSelectSet`] branches.
pub type CallSelectToken = CallGroupToken;

#[derive(Debug)]
struct SetEntry<K, R> {
    key: K,
    token: CallGroupToken,
    handle: CallHandle<R>,
}

#[derive(Debug)]
struct ExpectedSetCancel<K> {
    key: K,
    token: CallGroupToken,
}

/// One named terminal branch reply observed by join/select helpers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSetBranchOutcome<K, R> {
    /// Branch key supplied by user code.
    pub key: K,
    /// Generation token for this exact branch incarnation.
    pub token: CallGroupToken,
    /// Runtime call outcome returned by the branch.
    pub outcome: CallOutcome<R>,
    /// Whether a business-success classifier accepted this reply.
    ///
    /// [`CallJoinSet::record_reply`] always sets this to `true` (any
    /// reply completes the branch; join-all does not filter). Use
    /// [`CallJoinSet::record_classified_reply`] to apply a predicate on
    /// `CallOutcome::Replied` payloads instead.
    pub classified_success: bool,
}

/// One named cancel outcome observed by join/select helpers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSetCancelOutcome<K> {
    /// Branch key supplied by user code.
    pub key: K,
    /// Generation token for this exact branch incarnation.
    pub token: CallGroupToken,
    /// Outcome returned by `cancel_call`.
    pub outcome: CancelOutcome,
}

/// A pending branch that should be cancelled by owner code.
#[derive(Debug)]
#[must_use = "turn this into cancel_call(handle).then(...) or report it visibly"]
pub struct CallSetCancelRequest<K, R> {
    key: K,
    token: CallGroupToken,
    handle: CallHandle<R>,
}

impl<K, R> CallSetCancelRequest<K, R> {
    /// Returns the branch key.
    pub fn key(&self) -> &K {
        &self.key
    }

    /// Returns the branch generation token.
    pub const fn token(&self) -> CallGroupToken {
        self.token
    }

    /// Consumes the request into the parts needed to call `cancel_call`.
    pub fn into_parts(self) -> (K, CallGroupToken, CallHandle<R>) {
        (self.key, self.token, self.handle)
    }
}

/// Reasons inserting a branch into a join/select set can fail.
#[derive(Debug)]
pub enum CallSetInsertError<K, R> {
    /// Every slot is already live or reserved.
    Full {
        /// Rejected branch key.
        key: K,
        /// Rejected handle, returned so user code can cancel or report it.
        handle: CallHandle<R>,
    },
    /// A live branch already uses this key.
    DuplicateKey {
        /// Rejected branch key.
        key: K,
        /// Rejected handle, returned so user code can cancel or report it.
        handle: CallHandle<R>,
    },
    /// The token was not returned by `reserve_token`, or was already consumed.
    UnknownReservedToken {
        /// Rejected branch key.
        key: K,
        /// Rejected generation token.
        token: CallGroupToken,
        /// Rejected handle, returned so user code can cancel or report it.
        handle: CallHandle<R>,
    },
    /// A live branch already uses this token.
    DuplicateToken {
        /// Rejected branch key.
        key: K,
        /// Rejected generation token.
        token: CallGroupToken,
        /// Rejected handle, returned so user code can cancel or report it.
        handle: CallHandle<R>,
    },
}

/// Reasons `start_cancelable` can reject before returning an effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallSetStartError<K> {
    /// Every slot is already live or reserved.
    Full {
        /// Rejected branch key.
        key: K,
    },
    /// A live branch already uses this key.
    DuplicateKey {
        /// Rejected branch key.
        key: K,
    },
}

/// Reasons recording a branch reply can fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallSetRecordReplyError<K, R> {
    /// No live or cancel-pending branch matches this token.
    UnknownToken {
        /// Continuation key.
        key: K,
        /// Continuation token.
        token: CallGroupToken,
        /// Outcome that could not be recorded.
        outcome: CallOutcome<R>,
    },
    /// Token names a live branch, but the continuation carried another key.
    KeyMismatch {
        /// Continuation key.
        key: K,
        /// Continuation token.
        token: CallGroupToken,
        /// Outcome that could not be recorded.
        outcome: CallOutcome<R>,
    },
    /// Bounded result storage is full.
    StorageFull {
        /// Continuation key.
        key: K,
        /// Continuation token.
        token: CallGroupToken,
        /// Outcome that could not be recorded.
        outcome: CallOutcome<R>,
    },
}

/// Reasons recording a cancel outcome can fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallSetRecordCancelError<K> {
    /// No cancel is expected for this key/token pair.
    UnexpectedCancel {
        /// Cancel continuation key.
        key: K,
        /// Cancel continuation token.
        token: CallGroupToken,
        /// Outcome that could not be recorded.
        outcome: CancelOutcome,
    },
    /// Bounded cancel-result storage is full.
    StorageFull {
        /// Cancel continuation key.
        key: K,
        /// Cancel continuation token.
        token: CallGroupToken,
        /// Outcome that could not be recorded.
        outcome: CancelOutcome,
    },
}

/// Completed or partial report for "wait for every branch" workflows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallJoinReport<K, R> {
    /// Terminal branch replies and failures.
    pub branch_outcomes: Vec<CallSetBranchOutcome<K, R>>,
    /// Explicit owner-stop cancel outcomes.
    pub cancel_outcomes: Vec<CallSetCancelOutcome<K>>,
    /// Number of branches still live or awaiting cancel truth.
    pub pending: usize,
    /// Whether every branch has a reply or explicit cancel outcome.
    pub complete: bool,
}

/// Fixed-capacity named set for finite "join all calls" workflows.
#[derive(Debug)]
pub struct CallJoinSet<K, R> {
    entries: Vec<SetEntry<K, R>>,
    reserved_tokens: Vec<CallGroupToken>,
    expected_cancels: Vec<ExpectedSetCancel<K>>,
    branch_outcomes: Vec<CallSetBranchOutcome<K, R>>,
    cancel_outcomes: Vec<CallSetCancelOutcome<K>>,
    capacity: usize,
    next_generation: u64,
}

impl<K, R> CallJoinSet<K, R>
where
    K: PartialEq,
{
    /// Builds an empty join set with fixed capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(
            capacity > 0,
            "CallJoinSet requires capacity > 0; a zero-capacity set cannot name any wait",
        );
        Self {
            entries: Vec::with_capacity(capacity),
            reserved_tokens: Vec::with_capacity(capacity),
            expected_cancels: Vec::with_capacity(capacity),
            branch_outcomes: Vec::with_capacity(capacity),
            cancel_outcomes: Vec::with_capacity(capacity),
            capacity,
            next_generation: 1,
        }
    }

    /// Returns fixed branch capacity.
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns live pending branch count.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true when no branches are live.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns true when another branch would be rejected as full.
    pub fn is_full(&self) -> bool {
        self.entries.len() + self.reserved_tokens.len() >= self.capacity
    }

    /// Reserves a generation token before building a continuation.
    pub fn reserve_token(&mut self) -> Result<CallGroupToken, CallGroupReserveError> {
        if self.is_full() {
            return Err(CallGroupReserveError::Full);
        }
        let token = self.next_token();
        self.reserved_tokens.push(token);
        Ok(token)
    }

    /// Inserts one branch whose effect/handle were built manually.
    pub fn insert(
        &mut self,
        key: K,
        handle: CallHandle<R>,
    ) -> Result<CallGroupToken, CallSetInsertError<K, R>> {
        if self.entries.iter().any(|entry| entry.key == key) {
            return Err(CallSetInsertError::DuplicateKey { key, handle });
        }
        if self.is_full() {
            return Err(CallSetInsertError::Full { key, handle });
        }
        let token = self.next_token();
        self.entries.push(SetEntry { key, token, handle });
        Ok(token)
    }

    /// Inserts one branch using a previously reserved token.
    pub fn insert_reserved(
        &mut self,
        key: K,
        token: CallGroupToken,
        handle: CallHandle<R>,
    ) -> Result<(), CallSetInsertError<K, R>> {
        let Some(reserved_pos) = self
            .reserved_tokens
            .iter()
            .position(|reserved| *reserved == token)
        else {
            return Err(CallSetInsertError::UnknownReservedToken { key, token, handle });
        };
        if self.entries.iter().any(|entry| entry.key == key) {
            let _ = self.reserved_tokens.swap_remove(reserved_pos);
            return Err(CallSetInsertError::DuplicateKey { key, handle });
        }
        if self.entries.iter().any(|entry| entry.token == token) {
            let _ = self.reserved_tokens.swap_remove(reserved_pos);
            return Err(CallSetInsertError::DuplicateToken { key, token, handle });
        }
        let _ = self.reserved_tokens.swap_remove(reserved_pos);
        self.entries.push(SetEntry { key, token, handle });
        Ok(())
    }

    /// Builds, stores, and returns one cancelable branch effect.
    pub fn start_cancelable<I, M, T, F>(
        &mut self,
        key: K,
        call: CancelableCall<T, R>,
        translator: F,
    ) -> Result<tina::Effect<I>, CallSetStartError<K>>
    where
        I: tina::Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(K, CallGroupToken, CallOutcome<R>) -> M + 'static,
        K: Clone + 'static,
        M: 'static,
        T: Send + 'static,
        R: 'static,
    {
        if self.entries.iter().any(|entry| entry.key == key) {
            return Err(CallSetStartError::DuplicateKey { key });
        }
        let token = self
            .reserve_token()
            .map_err(|CallGroupReserveError::Full| CallSetStartError::Full { key: key.clone() })?;
        let continuation_key = key.clone();
        let (effect, handle) =
            call.then(move |outcome| translator(continuation_key, token, outcome));
        let reserved_pos = self
            .reserved_tokens
            .iter()
            .position(|reserved| *reserved == token)
            .expect("start_cancelable just reserved this token");
        let _ = self.reserved_tokens.swap_remove(reserved_pos);
        self.entries.push(SetEntry { key, token, handle });
        Ok(effect)
    }

    /// Records one terminal branch outcome.
    ///
    /// Any terminal outcome (reply, timeout, rejection, ...) completes
    /// the branch; there is no business-success filtering. Use
    /// [`record_classified_reply`](Self::record_classified_reply) when a
    /// branch should also be tagged against a business predicate.
    pub fn record_reply(
        &mut self,
        key: K,
        token: CallGroupToken,
        outcome: CallOutcome<R>,
    ) -> Result<bool, CallSetRecordReplyError<K, R>>
    where
        K: Clone,
    {
        self.record_reply_with_classification(key, token, outcome, true)
    }

    /// Records one terminal branch outcome tagged by a business-success
    /// classifier.
    ///
    /// `is_success` is called only for `CallOutcome::Replied`; every
    /// other outcome is tagged `classified_success: false`. Mirrors
    /// [`CallGroup::record_reply`]'s classifier, but `CallJoinSet` never
    /// cancels sibling branches on success: join-all still waits for
    /// every branch's terminal truth. Inspect
    /// [`CallSetBranchOutcome::classified_success`] on the finished
    /// report to find the business winner, if any.
    pub fn record_classified_reply<F>(
        &mut self,
        key: K,
        token: CallGroupToken,
        outcome: CallOutcome<R>,
        is_success: F,
    ) -> Result<bool, CallSetRecordReplyError<K, R>>
    where
        K: Clone,
        F: FnOnce(&R) -> bool,
    {
        let classified_success = match &outcome {
            CallOutcome::Replied(reply) => is_success(reply),
            CallOutcome::Full
            | CallOutcome::Closed
            | CallOutcome::Timeout
            | CallOutcome::Rejected(_) => false,
        };
        self.record_reply_with_classification(key, token, outcome, classified_success)
    }

    fn record_reply_with_classification(
        &mut self,
        key: K,
        token: CallGroupToken,
        outcome: CallOutcome<R>,
        classified_success: bool,
    ) -> Result<bool, CallSetRecordReplyError<K, R>>
    where
        K: Clone,
    {
        let branch = self.take_branch_for_reply(key, token, outcome, classified_success)?;
        if self.branch_outcomes.len() >= self.capacity {
            return Err(CallSetRecordReplyError::StorageFull {
                key: branch.key,
                token: branch.token,
                outcome: branch.outcome,
            });
        }
        self.branch_outcomes.push(branch);
        Ok(self.report_ready())
    }

    /// Drains every live branch for explicit owner-stop cancellation.
    pub fn drain_pending_for_cancel(&mut self) -> Vec<CallSetCancelRequest<K, R>>
    where
        K: Clone,
    {
        drain_set_entries_for_cancel(&mut self.entries, &mut self.expected_cancels)
    }

    /// Records one owner-stop cancel outcome.
    pub fn record_cancel(
        &mut self,
        key: K,
        token: CallGroupToken,
        outcome: CancelOutcome,
    ) -> Result<bool, CallSetRecordCancelError<K>> {
        record_set_cancel(
            &mut self.expected_cancels,
            &mut self.cancel_outcomes,
            self.capacity,
            key,
            token,
            outcome,
        )?;
        Ok(self.report_ready())
    }

    /// Returns whether every branch has terminal truth.
    pub fn report_ready(&self) -> bool {
        self.entries.is_empty()
            && self.reserved_tokens.is_empty()
            && self.expected_cancels.is_empty()
    }

    /// Consumes the set into a completed or partial report.
    pub fn into_report(self) -> CallJoinReport<K, R> {
        CallJoinReport {
            branch_outcomes: self.branch_outcomes,
            cancel_outcomes: self.cancel_outcomes,
            pending: self.entries.len() + self.reserved_tokens.len() + self.expected_cancels.len(),
            complete: self.entries.is_empty()
                && self.reserved_tokens.is_empty()
                && self.expected_cancels.is_empty(),
        }
    }

    fn next_token(&mut self) -> CallGroupToken {
        let token = CallGroupToken {
            generation: self.next_generation,
        };
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("CallJoinSet generation counter exhausted");
        token
    }

    fn take_branch_for_reply(
        &mut self,
        key: K,
        token: CallGroupToken,
        outcome: CallOutcome<R>,
        classified_success: bool,
    ) -> Result<CallSetBranchOutcome<K, R>, CallSetRecordReplyError<K, R>>
    where
        K: Clone,
    {
        if let Some(pos) = self.entries.iter().position(|entry| entry.token == token) {
            if self.entries[pos].key != key {
                return Err(CallSetRecordReplyError::KeyMismatch {
                    key,
                    token,
                    outcome,
                });
            }
            let entry = self.entries.swap_remove(pos);
            return Ok(CallSetBranchOutcome {
                key: entry.key,
                token,
                outcome,
                classified_success,
            });
        }
        if self
            .expected_cancels
            .iter()
            .any(|expected| expected.key == key && expected.token == token)
            && !self
                .branch_outcomes
                .iter()
                .any(|existing| existing.token == token)
        {
            return Ok(CallSetBranchOutcome {
                key,
                token,
                outcome,
                classified_success,
            });
        }
        Err(CallSetRecordReplyError::UnknownToken {
            key,
            token,
            outcome,
        })
    }
}

/// Terminal outcome yielded by [`CallSelectSet`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectedCallOutcome<R> {
    /// Branch produced a runtime call outcome.
    Reply(CallOutcome<R>),
    /// Branch cancellation produced a cancel outcome.
    Cancel(CancelOutcome),
}

/// One completed named branch yielded by [`CallSelectSet`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedCall<K, R> {
    /// Branch key supplied by user code.
    pub key: K,
    /// Generation token for this exact branch incarnation.
    pub token: CallGroupToken,
    /// Terminal reply or cancel outcome.
    pub outcome: SelectedCallOutcome<R>,
}

/// Completed or partial report for "handle whichever branch finishes next".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSelectReport<K, R> {
    /// Recorded branch completions in arrival order.
    pub selected: Vec<SelectedCall<K, R>>,
    /// Explicit owner-stop cancel outcomes.
    pub cancel_outcomes: Vec<CallSetCancelOutcome<K>>,
    /// Number of branches still live or awaiting cancel truth.
    pub pending: usize,
    /// Whether every branch has terminal truth.
    pub complete: bool,
}

/// Result of [`CallSelectSet::record_classified_reply`].
#[derive(Debug)]
pub struct CallSelectClassifiedStep<K, R> {
    /// The branch just recorded, whether or not it classified as a win.
    pub selected: SelectedCall<K, R>,
    /// Whether the caller's classifier accepted this reply as the race
    /// winner.
    pub classified_success: bool,
    /// Every other still-live branch, drained for cancellation because
    /// `selected` was the classified winner. Always empty unless
    /// `classified_success` is true.
    pub cancel_losers: Vec<CallSetCancelRequest<K, R>>,
}

/// Fixed-capacity named set for finite "select next completed call" workflows.
#[derive(Debug)]
pub struct CallSelectSet<K, R> {
    entries: Vec<SetEntry<K, R>>,
    reserved_tokens: Vec<CallGroupToken>,
    expected_cancels: Vec<ExpectedSetCancel<K>>,
    selected: Vec<SelectedCall<K, R>>,
    cancel_outcomes: Vec<CallSetCancelOutcome<K>>,
    capacity: usize,
    next_generation: u64,
}

impl<K, R> CallSelectSet<K, R>
where
    K: PartialEq,
{
    /// Builds an empty select set with fixed capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(
            capacity > 0,
            "CallSelectSet requires capacity > 0; a zero-capacity set cannot name any wait",
        );
        Self {
            entries: Vec::with_capacity(capacity),
            reserved_tokens: Vec::with_capacity(capacity),
            expected_cancels: Vec::with_capacity(capacity),
            selected: Vec::with_capacity(capacity),
            cancel_outcomes: Vec::with_capacity(capacity),
            capacity,
            next_generation: 1,
        }
    }

    /// Returns fixed branch capacity.
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns live pending branch count.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true when no branches are live.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns true when another branch would be rejected as full.
    pub fn is_full(&self) -> bool {
        self.entries.len() + self.reserved_tokens.len() >= self.capacity
    }

    /// Reserves a generation token before building a continuation.
    pub fn reserve_token(&mut self) -> Result<CallGroupToken, CallGroupReserveError> {
        if self.is_full() {
            return Err(CallGroupReserveError::Full);
        }
        let token = self.next_token();
        self.reserved_tokens.push(token);
        Ok(token)
    }

    /// Inserts one branch whose effect/handle were built manually.
    pub fn insert(
        &mut self,
        key: K,
        handle: CallHandle<R>,
    ) -> Result<CallGroupToken, CallSetInsertError<K, R>> {
        if self.entries.iter().any(|entry| entry.key == key) {
            return Err(CallSetInsertError::DuplicateKey { key, handle });
        }
        if self.is_full() {
            return Err(CallSetInsertError::Full { key, handle });
        }
        let token = self.next_token();
        self.entries.push(SetEntry { key, token, handle });
        Ok(token)
    }

    /// Builds, stores, and returns one cancelable branch effect.
    pub fn start_cancelable<I, M, T, F>(
        &mut self,
        key: K,
        call: CancelableCall<T, R>,
        translator: F,
    ) -> Result<tina::Effect<I>, CallSetStartError<K>>
    where
        I: tina::Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(K, CallGroupToken, CallOutcome<R>) -> M + 'static,
        K: Clone + 'static,
        M: 'static,
        T: Send + 'static,
        R: 'static,
    {
        if self.entries.iter().any(|entry| entry.key == key) {
            return Err(CallSetStartError::DuplicateKey { key });
        }
        let token = self
            .reserve_token()
            .map_err(|CallGroupReserveError::Full| CallSetStartError::Full { key: key.clone() })?;
        let continuation_key = key.clone();
        let (effect, handle) =
            call.then(move |outcome| translator(continuation_key, token, outcome));
        let reserved_pos = self
            .reserved_tokens
            .iter()
            .position(|reserved| *reserved == token)
            .expect("start_cancelable just reserved this token");
        let _ = self.reserved_tokens.swap_remove(reserved_pos);
        self.entries.push(SetEntry { key, token, handle });
        Ok(effect)
    }

    /// Records and returns one terminal branch outcome.
    ///
    /// `R: Clone` is required because select workflows both return the
    /// completed branch immediately and retain the same bounded result in the
    /// partial/final report. Wrap heterogeneous or non-clone replies in a small
    /// user enum/handle that carries the cloneable report payload you need.
    pub fn record_reply(
        &mut self,
        key: K,
        token: CallGroupToken,
        outcome: CallOutcome<R>,
    ) -> Result<SelectedCall<K, R>, CallSetRecordReplyError<K, R>>
    where
        K: Clone,
        R: Clone,
    {
        let selected = if let Some(pos) = self.entries.iter().position(|entry| entry.token == token)
        {
            if self.entries[pos].key != key {
                return Err(CallSetRecordReplyError::KeyMismatch {
                    key,
                    token,
                    outcome,
                });
            }
            let entry = self.entries.swap_remove(pos);
            SelectedCall {
                key: entry.key,
                token,
                outcome: SelectedCallOutcome::Reply(outcome),
            }
        } else if self
            .expected_cancels
            .iter()
            .any(|expected| expected.key == key && expected.token == token)
            && !self.selected.iter().any(|existing| existing.token == token)
        {
            SelectedCall {
                key,
                token,
                outcome: SelectedCallOutcome::Reply(outcome),
            }
        } else {
            return Err(CallSetRecordReplyError::UnknownToken {
                key,
                token,
                outcome,
            });
        };
        if self.selected.len() >= self.capacity {
            return Err(CallSetRecordReplyError::StorageFull {
                key: selected.key,
                token: selected.token,
                outcome: match selected.outcome {
                    SelectedCallOutcome::Reply(outcome) => outcome,
                    SelectedCallOutcome::Cancel(_) => unreachable!("record_reply builds reply"),
                },
            });
        }
        self.selected.push(selected.clone());
        Ok(selected)
    }

    /// Records one terminal branch outcome under a business-success
    /// classifier, mirroring [`CallGroup::record_reply`]'s race
    /// semantics.
    ///
    /// `is_success` is called only for `CallOutcome::Replied`. When it
    /// returns `true`, this branch is the race winner: every other live
    /// branch is drained via
    /// [`drain_pending_for_cancel`](Self::drain_pending_for_cancel) and
    /// returned as `cancel_losers` for the caller to cancel explicitly —
    /// the same shape `CallGroup::record_reply` returns on a classified
    /// win. When it returns `false` (or the outcome is not a reply), the
    /// branch is recorded like a plain
    /// [`record_reply`](Self::record_reply) call and no sibling branch
    /// is touched; call this again for the next completion.
    ///
    /// [`record_reply`](Self::record_reply) keeps its existing "any
    /// reply completes the branch, nothing else is cancelled" behavior
    /// unchanged — this method is strictly additive and opt-in.
    pub fn record_classified_reply<F>(
        &mut self,
        key: K,
        token: CallGroupToken,
        outcome: CallOutcome<R>,
        is_success: F,
    ) -> Result<CallSelectClassifiedStep<K, R>, CallSetRecordReplyError<K, R>>
    where
        K: Clone,
        R: Clone,
        F: FnOnce(&R) -> bool,
    {
        let classified_success = match &outcome {
            CallOutcome::Replied(reply) => is_success(reply),
            CallOutcome::Full
            | CallOutcome::Closed
            | CallOutcome::Timeout
            | CallOutcome::Rejected(_) => false,
        };
        let selected = self.record_reply(key, token, outcome)?;
        let cancel_losers = if classified_success {
            self.drain_pending_for_cancel()
        } else {
            Vec::new()
        };
        Ok(CallSelectClassifiedStep {
            selected,
            classified_success,
            cancel_losers,
        })
    }

    /// Drains every live branch for explicit owner-stop cancellation.
    pub fn drain_pending_for_cancel(&mut self) -> Vec<CallSetCancelRequest<K, R>>
    where
        K: Clone,
    {
        drain_set_entries_for_cancel(&mut self.entries, &mut self.expected_cancels)
    }

    /// Records and returns one explicit owner-stop cancel outcome.
    pub fn record_cancel(
        &mut self,
        key: K,
        token: CallGroupToken,
        outcome: CancelOutcome,
    ) -> Result<SelectedCall<K, R>, CallSetRecordCancelError<K>>
    where
        K: Clone,
    {
        let Some(expected_pos) = self
            .expected_cancels
            .iter()
            .position(|expected| expected.key == key && expected.token == token)
        else {
            return Err(CallSetRecordCancelError::UnexpectedCancel {
                key,
                token,
                outcome,
            });
        };
        let already_selected = self.selected.iter().any(|selected| selected.token == token);
        if self.cancel_outcomes.len() >= self.capacity
            || (!already_selected && self.selected.len() >= self.capacity)
        {
            return Err(CallSetRecordCancelError::StorageFull {
                key,
                token,
                outcome,
            });
        }
        self.expected_cancels.swap_remove(expected_pos);
        self.cancel_outcomes.push(CallSetCancelOutcome {
            key: key.clone(),
            token,
            outcome,
        });
        if !already_selected {
            self.selected.push(SelectedCall {
                key: key.clone(),
                token,
                outcome: SelectedCallOutcome::Cancel(outcome),
            });
        }
        Ok(SelectedCall {
            key,
            token,
            outcome: SelectedCallOutcome::Cancel(outcome),
        })
    }

    /// Returns a partial report without consuming the set.
    pub fn partial_report(&self) -> CallSelectReport<K, R>
    where
        K: Clone,
        R: Clone,
    {
        CallSelectReport {
            selected: self.selected.clone(),
            cancel_outcomes: self.cancel_outcomes.clone(),
            pending: self.entries.len() + self.reserved_tokens.len() + self.expected_cancels.len(),
            complete: self.report_ready(),
        }
    }

    /// Returns whether every branch has terminal truth.
    pub fn report_ready(&self) -> bool {
        self.entries.is_empty()
            && self.reserved_tokens.is_empty()
            && self.expected_cancels.is_empty()
    }

    /// Consumes the set into a completed or partial report.
    pub fn into_report(self) -> CallSelectReport<K, R> {
        CallSelectReport {
            selected: self.selected,
            cancel_outcomes: self.cancel_outcomes,
            pending: self.entries.len() + self.reserved_tokens.len() + self.expected_cancels.len(),
            complete: self.entries.is_empty()
                && self.reserved_tokens.is_empty()
                && self.expected_cancels.is_empty(),
        }
    }

    fn next_token(&mut self) -> CallGroupToken {
        let token = CallGroupToken {
            generation: self.next_generation,
        };
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("CallSelectSet generation counter exhausted");
        token
    }
}

fn drain_set_entries_for_cancel<K, R>(
    entries: &mut Vec<SetEntry<K, R>>,
    expected_cancels: &mut Vec<ExpectedSetCancel<K>>,
) -> Vec<CallSetCancelRequest<K, R>>
where
    K: Clone,
{
    let mut requests = Vec::with_capacity(entries.len());
    for entry in entries.drain(..) {
        expected_cancels.push(ExpectedSetCancel {
            key: entry.key.clone(),
            token: entry.token,
        });
        requests.push(CallSetCancelRequest {
            key: entry.key,
            token: entry.token,
            handle: entry.handle,
        });
    }
    requests
}

fn record_set_cancel<K>(
    expected_cancels: &mut Vec<ExpectedSetCancel<K>>,
    cancel_outcomes: &mut Vec<CallSetCancelOutcome<K>>,
    capacity: usize,
    key: K,
    token: CallGroupToken,
    outcome: CancelOutcome,
) -> Result<(), CallSetRecordCancelError<K>>
where
    K: PartialEq,
{
    let Some(expected_pos) = expected_cancels
        .iter()
        .position(|expected| expected.key == key && expected.token == token)
    else {
        return Err(CallSetRecordCancelError::UnexpectedCancel {
            key,
            token,
            outcome,
        });
    };
    if cancel_outcomes.len() >= capacity {
        return Err(CallSetRecordCancelError::StorageFull {
            key,
            token,
            outcome,
        });
    }
    expected_cancels.swap_remove(expected_pos);
    cancel_outcomes.push(CallSetCancelOutcome {
        key,
        token,
        outcome,
    });
    Ok(())
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
    fn late_loser_reply_after_winner_is_recorded_not_rejected() {
        let mut group: CallGroup<u8, Reply> = CallGroup::with_capacity(2);
        let winner = group.insert(1, make_handle()).unwrap();
        let loser = group.insert(2, make_handle()).unwrap();

        let won = group
            .record_reply(1, winner, CallOutcome::Replied(Reply(10)), |reply| {
                reply.0 >= 10
            })
            .unwrap();
        assert_eq!(won.cancel_losers.len(), 1);
        assert!(!won.report_ready);

        let late = group
            .record_reply(2, loser, CallOutcome::Replied(Reply(9)), |reply| {
                reply.0 >= 10
            })
            .unwrap();
        assert!(late.cancel_losers.is_empty());
        assert!(!late.report_ready);

        assert!(
            group
                .record_cancel(2, loser, CancelOutcome::AlreadyCompleted)
                .unwrap(),
            "late reply plus observed cancel outcome completes the race"
        );
        let report = group.into_report();
        assert!(report.complete);
        assert_eq!(report.winner, Some(winner));
        assert_eq!(report.branch_outcomes.len(), 2);
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

    #[test]
    fn owner_stop_drain_keeps_existing_expected_cancel_truth() {
        let mut group: CallGroup<u8, Reply> = CallGroup::with_capacity(3);
        let winner = group.insert(1, make_handle()).unwrap();
        let loser = group.insert(2, make_handle()).unwrap();
        let step = group
            .record_reply(1, winner, CallOutcome::Replied(Reply(1)), |reply| {
                matches!(reply, Reply(1))
            })
            .unwrap();
        assert_eq!(step.cancel_losers.len(), 1);
        assert!(!group.report_ready());

        let drained = group.drain_pending_for_cancel();
        assert!(
            drained.is_empty(),
            "winner path already drained all live entries"
        );
        assert!(!group.report_ready());
        assert!(
            group
                .record_cancel(2, loser, CancelOutcome::Cancelled)
                .unwrap(),
            "expected loser cancel should still complete the group"
        );
    }

    #[test]
    fn race_report_pending_counts_reserved_tokens_and_expected_cancels() {
        let mut group: CallGroup<u8, Reply> = CallGroup::with_capacity(3);
        let _reserved = group.reserve_token().unwrap();
        let report = group.into_report();
        assert_eq!(report.pending, 1);
        assert!(!report.complete);

        let mut group: CallGroup<u8, Reply> = CallGroup::with_capacity(3);
        let winner = group.insert(1, make_handle()).unwrap();
        let loser = group.insert(2, make_handle()).unwrap();
        let won = group
            .record_reply(1, winner, CallOutcome::Replied(Reply(10)), |reply| {
                reply.0 >= 10
            })
            .unwrap();
        assert_eq!(won.cancel_losers.len(), 1);
        assert_eq!(won.cancel_losers[0].token(), loser);
        let report = group.into_report();
        assert_eq!(
            report.pending, 1,
            "expected loser-cancel truth is still pending"
        );
        assert!(!report.complete);
    }

    #[test]
    fn join_set_waits_for_every_terminal_outcome() {
        let mut set: CallJoinSet<&'static str, Reply> = CallJoinSet::with_capacity(2);
        let a = set.insert("a", make_handle()).unwrap();
        let b = set.insert("b", make_handle()).unwrap();

        assert!(
            !set.record_reply("a", a, CallOutcome::Replied(Reply(1)))
                .unwrap()
        );
        assert!(
            set.record_reply("b", b, CallOutcome::Timeout)
                .expect("second terminal branch completes join")
        );

        let report = set.into_report();
        assert!(report.complete);
        assert_eq!(report.pending, 0);
        assert_eq!(report.branch_outcomes.len(), 2);
    }

    #[test]
    fn join_set_owner_stop_cancels_and_records_late_reply_truth() {
        let mut set: CallJoinSet<&'static str, Reply> = CallJoinSet::with_capacity(1);
        let token = set.insert("slow", make_handle()).unwrap();
        let cancels = set.drain_pending_for_cancel();
        assert_eq!(cancels.len(), 1);
        assert!(!set.report_ready());

        assert!(
            !set.record_reply("slow", token, CallOutcome::Replied(Reply(9)))
                .unwrap(),
            "late reply is recorded but cancel outcome is still missing"
        );
        assert!(
            set.record_cancel("slow", token, CancelOutcome::AlreadyCompleted)
                .unwrap()
        );
        let report = set.into_report();
        assert!(report.complete);
        assert_eq!(report.branch_outcomes.len(), 1);
        assert_eq!(report.cancel_outcomes.len(), 1);
    }

    #[test]
    fn join_set_rejects_duplicate_and_wrong_key_without_losing_live_branch() {
        let mut set: CallJoinSet<&'static str, Reply> = CallJoinSet::with_capacity(2);
        let token = set.insert("primary", make_handle()).unwrap();

        match set.insert("primary", make_handle()) {
            Err(CallSetInsertError::DuplicateKey { key, handle: _ }) => {
                assert_eq!(key, "primary");
            }
            _ => panic!("expected duplicate key"),
        }
        match set.record_reply("wrong", token, CallOutcome::Timeout) {
            Err(CallSetRecordReplyError::KeyMismatch { key, token: t, .. }) => {
                assert_eq!(key, "wrong");
                assert_eq!(t, token);
            }
            _ => panic!("expected key mismatch"),
        }
        assert_eq!(set.len(), 1, "bad continuation must not remove branch");
        assert!(
            set.record_reply("primary", token, CallOutcome::Replied(Reply(1)))
                .unwrap()
        );
    }

    #[test]
    fn join_set_reserved_token_failure_releases_capacity() {
        let mut set: CallJoinSet<&'static str, Reply> = CallJoinSet::with_capacity(2);
        let _ = set.insert("live", make_handle()).unwrap();
        let reserved = set.reserve_token().unwrap();
        assert!(set.is_full());

        match set.insert_reserved("live", reserved, make_handle()) {
            Err(CallSetInsertError::DuplicateKey { key, handle: _ }) => assert_eq!(key, "live"),
            _ => panic!("expected duplicate key"),
        }
        assert!(!set.is_full(), "failed insert must release reserved slot");
        let fresh = set.reserve_token().unwrap();
        assert_ne!(reserved, fresh);
    }

    #[test]
    fn join_set_record_reply_tags_every_branch_classified_success() {
        let mut set: CallJoinSet<u8, Reply> = CallJoinSet::with_capacity(1);
        let token = set.insert(1, make_handle()).unwrap();

        assert!(
            set.record_reply(1, token, CallOutcome::Timeout).unwrap(),
            "single branch completes the join"
        );
        let report = set.into_report();
        assert_eq!(report.branch_outcomes.len(), 1);
        assert!(
            report.branch_outcomes[0].classified_success,
            "plain record_reply keeps the any-reply-completes default"
        );
    }

    #[test]
    fn join_set_record_classified_reply_only_tags_matching_replies() {
        let mut set: CallJoinSet<u8, Reply> = CallJoinSet::with_capacity(2);
        let low = set.insert(1, make_handle()).unwrap();
        let high = set.insert(2, make_handle()).unwrap();

        assert!(
            !set.record_classified_reply(1, low, CallOutcome::Replied(Reply(1)), |reply| {
                reply.0 >= 10
            })
            .unwrap()
        );
        assert!(
            set.record_classified_reply(2, high, CallOutcome::Replied(Reply(10)), |reply| {
                reply.0 >= 10
            })
            .unwrap()
        );

        let report = set.into_report();
        assert!(report.complete);
        let low_outcome = report
            .branch_outcomes
            .iter()
            .find(|outcome| outcome.key == 1)
            .unwrap();
        let high_outcome = report
            .branch_outcomes
            .iter()
            .find(|outcome| outcome.key == 2)
            .unwrap();
        assert!(
            !low_outcome.classified_success,
            "non-matching reply must not be tagged classified_success"
        );
        assert!(
            high_outcome.classified_success,
            "matching reply must be tagged classified_success"
        );

        // Non-reply outcomes never classify as success, even if the
        // predicate is written to ignore its input.
        let mut timeout_set: CallJoinSet<u8, Reply> = CallJoinSet::with_capacity(1);
        let timeout_token = timeout_set.insert(9, make_handle()).unwrap();
        assert!(
            timeout_set
                .record_classified_reply(9, timeout_token, CallOutcome::Timeout, |_| true)
                .unwrap()
        );
        let timeout_report = timeout_set.into_report();
        assert!(!timeout_report.branch_outcomes[0].classified_success);
    }

    #[test]
    fn join_set_report_pending_counts_reserved_tokens() {
        let mut set: CallJoinSet<&'static str, Reply> = CallJoinSet::with_capacity(2);
        let _reserved = set.reserve_token().unwrap();

        let report = set.into_report();
        assert_eq!(report.pending, 1);
        assert!(!report.complete);
    }

    #[test]
    fn select_set_returns_completed_branches_in_arrival_order_with_partial_report() {
        let mut set: CallSelectSet<&'static str, Reply> = CallSelectSet::with_capacity(2);
        let a = set.insert("a", make_handle()).unwrap();
        let b = set.insert("b", make_handle()).unwrap();

        let first = set
            .record_reply("b", b, CallOutcome::Replied(Reply(20)))
            .unwrap();
        assert_eq!(first.key, "b");
        assert_eq!(first.token, b);
        let partial = set.partial_report();
        assert!(!partial.complete);
        assert_eq!(partial.pending, 1);
        assert_eq!(partial.selected[0].key, "b");

        let second = set
            .record_reply("a", a, CallOutcome::Replied(Reply(10)))
            .unwrap();
        assert_eq!(second.key, "a");
        let report = set.into_report();
        assert!(report.complete);
        assert_eq!(report.selected.len(), 2);
        assert_eq!(report.selected[0].key, "b");
        assert_eq!(report.selected[1].key, "a");
    }

    #[test]
    fn select_set_cancel_is_explicit_selected_outcome_and_stale_token_rejected() {
        let mut set: CallSelectSet<&'static str, Reply> = CallSelectSet::with_capacity(1);
        let old = set.insert("same", make_handle()).unwrap();
        let cancel = set.drain_pending_for_cancel().pop().unwrap();
        assert_eq!(cancel.token(), old);
        let selected = set
            .record_cancel("same", old, CancelOutcome::Cancelled)
            .unwrap();
        assert_eq!(selected.key, "same");
        assert!(matches!(
            selected.outcome,
            SelectedCallOutcome::Cancel(CancelOutcome::Cancelled)
        ));

        let fresh = set.insert("same", make_handle()).unwrap();
        assert_ne!(old, fresh);
        match set.record_reply("same", old, CallOutcome::Timeout) {
            Err(CallSetRecordReplyError::UnknownToken { key, token, .. }) => {
                assert_eq!(key, "same");
                assert_eq!(token, old);
            }
            _ => panic!("expected stale select token to be rejected"),
        }
    }

    #[test]
    fn select_set_wrong_key_does_not_select_or_remove_branch() {
        let mut set: CallSelectSet<&'static str, Reply> = CallSelectSet::with_capacity(1);
        let token = set.insert("real", make_handle()).unwrap();
        match set.record_reply("wrong", token, CallOutcome::Replied(Reply(1))) {
            Err(CallSetRecordReplyError::KeyMismatch { key, token: t, .. }) => {
                assert_eq!(key, "wrong");
                assert_eq!(t, token);
            }
            _ => panic!("expected key mismatch"),
        }
        let partial = set.partial_report();
        assert!(partial.selected.is_empty());
        assert_eq!(partial.pending, 1);
        let selected = set
            .record_reply("real", token, CallOutcome::Replied(Reply(2)))
            .unwrap();
        assert_eq!(selected.key, "real");
    }

    #[test]
    fn select_set_late_reply_after_cancel_request_still_accepts_cancel_truth() {
        let mut set: CallSelectSet<&'static str, Reply> = CallSelectSet::with_capacity(1);
        let token = set.insert("slow", make_handle()).unwrap();
        let cancels = set.drain_pending_for_cancel();
        assert_eq!(cancels.len(), 1);

        let late = set
            .record_reply("slow", token, CallOutcome::Replied(Reply(7)))
            .unwrap();
        assert_eq!(late.key, "slow");
        assert!(!set.report_ready());

        let cancel = set
            .record_cancel("slow", token, CancelOutcome::AlreadyCompleted)
            .unwrap();
        assert!(matches!(
            cancel.outcome,
            SelectedCallOutcome::Cancel(CancelOutcome::AlreadyCompleted)
        ));
        let report = set.into_report();
        assert!(report.complete);
        assert_eq!(report.selected.len(), 1);
        assert_eq!(report.cancel_outcomes.len(), 1);
    }

    #[test]
    fn select_set_report_pending_counts_reserved_tokens() {
        let mut set: CallSelectSet<&'static str, Reply> = CallSelectSet::with_capacity(2);
        let _reserved = set.reserve_token().unwrap();

        let partial = set.partial_report();
        assert_eq!(partial.pending, 1);
        assert!(!partial.complete);
        let report = set.into_report();
        assert_eq!(report.pending, 1);
        assert!(!report.complete);
    }

    #[test]
    fn classified_reply_non_matching_reply_is_not_selected_as_winner() {
        let mut set: CallSelectSet<u8, Reply> = CallSelectSet::with_capacity(2);
        let slow = set.insert(1, make_handle()).unwrap();
        let fast = set.insert(2, make_handle()).unwrap();

        // "fast" arrives first but fails the business predicate: it
        // must not win, and no sibling should be cancelled.
        let step = set
            .record_classified_reply(2, fast, CallOutcome::Replied(Reply(0)), |reply| {
                reply.0 >= 10
            })
            .unwrap();
        assert!(!step.classified_success);
        assert!(
            step.cancel_losers.is_empty(),
            "a non-matching reply must not drain siblings"
        );
        assert_eq!(set.len(), 1, "the other branch is still live");

        // "slow" arrives second and matches: it is the winner.
        let step = set
            .record_classified_reply(1, slow, CallOutcome::Replied(Reply(10)), |reply| {
                reply.0 >= 10
            })
            .unwrap();
        assert!(step.classified_success);
        assert_eq!(step.selected.key, 1);
        assert!(
            step.cancel_losers.is_empty(),
            "no siblings remain once both branches have replied"
        );
    }

    #[test]
    fn classified_reply_matching_reply_wins_and_drains_siblings() {
        let mut set: CallSelectSet<u8, Reply> = CallSelectSet::with_capacity(2);
        let winner_token = set.insert(1, make_handle()).unwrap();
        let loser_token = set.insert(2, make_handle()).unwrap();

        let step = set
            .record_classified_reply(1, winner_token, CallOutcome::Replied(Reply(10)), |reply| {
                reply.0 >= 10
            })
            .unwrap();
        assert!(step.classified_success);
        assert_eq!(step.selected.key, 1);
        assert_eq!(step.cancel_losers.len(), 1);
        let (loser_key, drained_token, _handle) =
            step.cancel_losers.into_iter().next().unwrap().into_parts();
        assert_eq!(loser_key, 2);
        assert_eq!(drained_token, loser_token);
        assert!(set.is_empty(), "the loser was drained out of live entries");
        assert!(!set.report_ready(), "still waiting on the loser cancel");

        let cancel = set
            .record_cancel(2, loser_token, CancelOutcome::Cancelled)
            .unwrap();
        assert!(matches!(
            cancel.outcome,
            SelectedCallOutcome::Cancel(CancelOutcome::Cancelled)
        ));
        let report = set.into_report();
        assert!(report.complete);
        assert_eq!(report.selected.len(), 2);
        assert_eq!(report.cancel_outcomes.len(), 1);
    }

    #[test]
    fn classified_reply_ignores_non_reply_outcomes() {
        // A predicate that always returns true must still not classify
        // Timeout/Full/Closed/Rejected outcomes as a win: `is_success`
        // is only consulted for `CallOutcome::Replied`.
        let mut set: CallSelectSet<u8, Reply> = CallSelectSet::with_capacity(1);
        let token = set.insert(1, make_handle()).unwrap();
        let step = set
            .record_classified_reply(1, token, CallOutcome::Timeout, |_| true)
            .unwrap();
        assert!(!step.classified_success);
        assert!(step.cancel_losers.is_empty());
    }
}
