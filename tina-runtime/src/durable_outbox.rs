//! Bounded, restart-survivable outbox for local durable work.
//!
//! A service records work before doing it, restarts, and resumes or reports
//! the truth. [`DurableOutbox`] is the first form of that pattern: a bounded,
//! sync state machine that turns "work I intend to do" into a durable journal
//! record, hands back an apply-authorizing token only after the record lands,
//! and tracks which work is still outstanding so a restart can replay it.
//!
//! ## Honesty boundary
//!
//! This is **at-least-once**, not exactly-once. After recovery, work that was
//! durably recorded but not durably completed may run again. This is not a
//! durable mailbox, not a queue broker, and makes no distributed guarantee.
//! Tina owns the I/O: the outbox produces the bytes to append and consumes the
//! append result, but the isolate issues the actual [`crate::journal_append`] /
//! [`crate::journal_replay`] calls. That keeps the outbox fully testable
//! without a filesystem and keeps fsync truth in the runtime.
//!
//! ## Lifecycle
//!
//! ```text
//! enqueue(work) ──> DurableWork ──record(append_ok)──> RecordedWork
//!                        │                                   │
//!                  (append err)                           apply
//!                        ▼                                   ▼
//!                    RecordError                      ApplyStatus::Apply(work)
//!                  (work returned)                           │
//!                                              begin_complete / finish_complete
//!                                                            ▼
//!                                                      CommittedWork
//! ```
//!
//! - `enqueue` reserves a stable [`WorkId`] and frames the durable record.
//!   Returns [`OutboxFull`] (carrying the original work) at capacity.
//! - `record` confirms the durable append. Only a successful append yields a
//!   [`RecordedWork`]; a failed append or stale staged token returns the
//!   original work in [`RecordError`]. There is no way to obtain a
//!   `RecordedWork` without a successful record, so apply-before-record cannot
//!   be expressed.
//! - `apply` consumes the `RecordedWork` (so the same authorization cannot be
//!   applied twice) and returns the work to act on.
//! - `begin_complete` / `finish_complete` durably mark the work done. Marking
//!   the same id complete twice is the typed [`CompletionStart::AlreadyCompleted`],
//!   not a silent success.
//!
//! On restart, [`DurableOutbox::recover`] replays the journal into a fresh
//! outbox plus a [`RecoveryReport`]: pending work to replay (as ready-to-apply
//! [`RecordedWork`] tokens), the ids already completed, and a [`TailStatus`]
//! that separates a clean tail, a repaired truncated tail, and an uncertain
//! commit. A corrupt tail is rejected by name as [`RecoveryError::CorruptTail`].

use std::collections::BTreeSet;
use std::marker::PhantomData;

use crate::{CallError, JournalReplay, JournalReplayWarning};

/// Inner-record tag for an enqueue record.
const TAG_ENQUEUE: u8 = 0;
/// Inner-record tag for a completion record.
const TAG_COMPLETE: u8 = 1;
/// Tag byte plus a little-endian `u64` work id.
const FRAME_HEADER: usize = 1 + 8;

/// Stable identifier for one unit of durable work.
///
/// Assigned at [`DurableOutbox::enqueue`] and written into the durable record,
/// so it survives restart unchanged. Ids are strictly increasing and never
/// reused, even across recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkId(pub u64);

/// User payload that can be written to and read back from a durable record.
///
/// The blanket implementation for `Vec<u8>` treats bytes as their own
/// encoding. Application types implement this to journal their own shape.
pub trait DurablePayload: Sized {
    /// Encode the payload for the durable record.
    fn to_durable_bytes(&self) -> Vec<u8>;
    /// Decode a payload from a durable record. `None` means the record framing
    /// is intact (checksum already verified by the journal) but the bytes do
    /// not describe a valid payload; recovery treats this as a corrupt tail.
    fn from_durable_bytes(bytes: &[u8]) -> Option<Self>;
}

impl DurablePayload for Vec<u8> {
    fn to_durable_bytes(&self) -> Vec<u8> {
        self.clone()
    }

    fn from_durable_bytes(bytes: &[u8]) -> Option<Self> {
        Some(bytes.to_vec())
    }
}

/// Work staged for durable record but not yet recorded.
///
/// Carries the journal entry the caller must append. Dropping this without
/// recording loses the work (it was never durable), which the shutdown report
/// counts as abandoned.
#[must_use = "append the journal entry and call record(), or the work is never durable"]
#[derive(Debug)]
pub struct DurableWork<W> {
    id: WorkId,
    index: u64,
    payload: Vec<u8>,
    work: W,
}

impl<W> DurableWork<W> {
    /// The stable id assigned to this work.
    pub fn work_id(&self) -> WorkId {
        self.id
    }

    /// The journal index to append at. Pass to [`crate::journal_append`].
    pub fn journal_index(&self) -> u64 {
        self.index
    }

    /// The durable record bytes to append. Pass to [`crate::journal_append`].
    pub fn journal_bytes(&self) -> &[u8] {
        &self.payload
    }

    /// Borrow the staged work item.
    pub fn work(&self) -> &W {
        &self.work
    }
}

/// Proof that work was durably recorded.
///
/// The only token that authorizes [`DurableOutbox::apply`]. It can be produced
/// only by [`DurableOutbox::record`] on a successful append or by
/// [`DurableOutbox::recover`] for work found still pending, so apply cannot run
/// before a durable record exists.
#[must_use = "apply the recorded work, or it stays pending until the next restart"]
#[derive(Debug)]
pub struct RecordedWork<W> {
    id: WorkId,
    work: W,
}

impl<W> RecordedWork<W> {
    /// The stable id of this recorded work.
    pub fn work_id(&self) -> WorkId {
        self.id
    }
}

/// Proof that recorded work was applied and its completion durably recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CommittedWork {
    id: WorkId,
}

impl CommittedWork {
    /// The stable id of the committed work.
    pub fn work_id(&self) -> WorkId {
        self.id
    }
}

/// The outbox was at capacity. The original work is returned unharmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxFull<W> {
    /// The work item that could not be enqueued.
    pub work: W,
}

/// A durable append failed. The original work is returned so the caller can
/// retry or surface the failure; no outbox state advanced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendFailed<W> {
    /// Why the durable append failed, in the runtime's own vocabulary.
    pub error: CallError,
    /// The work item that was not durably recorded.
    pub work: W,
}

/// A staged work token did not belong to this outbox anymore.
///
/// This is the stale-token guard for restart and recovery: an old
/// [`DurableWork`] must not be able to mint a new [`RecordedWork`] after the
/// outbox that staged it is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleWork<W> {
    /// The work item carried by the stale token.
    pub work: W,
}

/// Recording failed before a [`RecordedWork`] could be produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordError<W> {
    /// The durable append failed.
    Append(AppendFailed<W>),
    /// The staged token was stale or belonged to a different outbox.
    Stale(StaleWork<W>),
}

/// Outcome of [`DurableOutbox::apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyStatus<W> {
    /// Apply this work. It stays pending until a completion is durably
    /// recorded, so a crash before completion replays it (at-least-once).
    Apply(W),
    /// This work id was already durably completed. Skipped instead of applied;
    /// the duplicate is named, not silently dropped.
    DuplicateWork(WorkId),
}

/// A completion staged for durable record. Append its entry, then
/// [`DurableOutbox::finish_complete`].
#[must_use = "append the completion entry and call finish_complete(), or the work replays"]
#[derive(Debug)]
pub struct DurableCompletion {
    id: WorkId,
    index: u64,
    payload: Vec<u8>,
}

impl DurableCompletion {
    /// The id being completed.
    pub fn work_id(&self) -> WorkId {
        self.id
    }

    /// The journal index to append at.
    pub fn journal_index(&self) -> u64 {
        self.index
    }

    /// The completion record bytes to append.
    pub fn journal_bytes(&self) -> &[u8] {
        &self.payload
    }
}

/// Outcome of [`DurableOutbox::begin_complete`].
#[derive(Debug)]
pub enum CompletionStart {
    /// Append this completion record, then call `finish_complete`.
    Record(DurableCompletion),
    /// The id was already durably completed; marking complete is idempotent.
    AlreadyCompleted(WorkId),
    /// The id is not pending (never recorded, or unknown). Not a silent no-op.
    NotPending(WorkId),
}

/// A completion append failed. The work stays pending and replays next run.
#[derive(Debug)]
pub struct CompletionFailed {
    /// Why the completion append failed.
    pub error: CallError,
    /// The id whose completion was not durably recorded.
    pub work_id: WorkId,
}

/// State of the journal tail after recovery.
///
/// Separates the three non-rejecting outcomes. A corrupt tail does not appear
/// here: it is rejected up front as [`RecoveryError::CorruptTail`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailStatus {
    /// The journal ended on a complete record. Nothing to repair.
    Clean,
    /// The journal ended with an incomplete final record. The valid prefix was
    /// kept; `valid_prefix_len` is the byte length to truncate to before
    /// appending again. A warning, never reported as clean success.
    TruncatedTailRepaired {
        /// Byte length of the valid prefix to retain.
        valid_prefix_len: u64,
    },
    /// The most recent commit before this recovery could not confirm its final
    /// durability step (for example a parent-directory fsync that returned
    /// [`CallError::CommitUncertain`]). The records read are used, but the
    /// boundary is flagged so the operator and replay treat it as unknown.
    UncertainCommit,
}

/// Why recovery refused to rebuild the outbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryError {
    /// A complete record failed checksum or record-order validation. Recovery
    /// stops visibly rather than guessing past the corruption.
    CorruptTail,
    /// Replaying pending work would exceed the configured capacity. Replay is
    /// bounded; recovery refuses the moment the live backlog passes capacity
    /// rather than scanning an oversized log.
    OverCapacity {
        /// The configured capacity.
        capacity: usize,
        /// The live pending count when the bound was crossed (`capacity + 1`),
        /// not the full backlog — recovery stops before reading the rest.
        pending: usize,
    },
    /// The journal could not be read.
    Io,
}

/// Result of replaying a durable journal into a fresh outbox.
#[derive(Debug)]
pub struct RecoveryReport<W> {
    /// Tail condition of the recovered journal.
    pub tail_status: TailStatus,
    /// Work that was durably recorded but not durably completed. Each is a
    /// ready-to-apply [`RecordedWork`]; replaying it is at-least-once.
    pub pending: Vec<RecordedWork<W>>,
    /// Ids that were durably completed before the restart.
    pub completed: Vec<WorkId>,
}

/// Final accounting of durable work at shutdown.
///
/// `pending` and `abandoned` are bounded by the outbox capacity. `completed`
/// and `failed` are run totals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxShutdownReport {
    /// Recorded but not completed. Replays at-least-once next run.
    pub pending: Vec<WorkId>,
    /// Staged for record but never durably confirmed. Lost on shutdown; never
    /// was durable.
    pub abandoned: Vec<WorkId>,
    /// Work durably completed this run.
    pub completed: u64,
    /// Work whose durable record or completion append failed this run.
    pub failed: u64,
}

/// Durability confidence of the most recent commit observed before recovery.
///
/// A service that fsyncs the parent directory on every durable step has no
/// uncertainty window and passes [`CommitConfidence::Clean`]. A service that
/// snapshots/compacts passes [`CommitConfidence::Uncertain`] when its last
/// commit returned [`CallError::CommitUncertain`] (typically read back from a
/// commit fence it persisted), so recovery can flag the boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitConfidence {
    /// The last durable commit was confirmed.
    Clean,
    /// The last durable commit could not confirm its final step.
    Uncertain,
}

/// Bounded, restart-survivable record of local work to perform.
///
/// See the [module docs](self) for the lifecycle and the at-least-once honesty
/// boundary.
#[derive(Debug)]
pub struct DurableOutbox<W> {
    capacity: usize,
    next_work_id: u64,
    next_index: u64,
    /// Enqueued, durable append not yet confirmed.
    staged: BTreeSet<u64>,
    /// Durably recorded, not yet completed.
    pending: BTreeSet<u64>,
    /// Durably completed and above `completed_watermark`.
    completed: BTreeSet<u64>,
    /// Every id `<= completed_watermark` is fully accounted for (completed,
    /// failed, or never the lowest outstanding). Keeps `completed` bounded.
    completed_watermark: u64,
    completed_total: u64,
    failed_total: u64,
    _marker: PhantomData<fn() -> W>,
}

impl<W: DurablePayload> DurableOutbox<W> {
    /// Create an empty outbox bounded to `capacity` outstanding work items.
    ///
    /// `capacity` is the maximum number of staged-or-pending items at once.
    /// It is clamped to at least one.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            next_work_id: 1,
            next_index: 1,
            staged: BTreeSet::new(),
            pending: BTreeSet::new(),
            completed: BTreeSet::new(),
            completed_watermark: 0,
            completed_total: 0,
            failed_total: 0,
            _marker: PhantomData,
        }
    }

    /// The configured capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of outstanding items: staged plus pending.
    pub fn outstanding_len(&self) -> usize {
        self.staged.len() + self.pending.len()
    }

    /// Whether the outbox is at capacity.
    pub fn is_full(&self) -> bool {
        self.outstanding_len() >= self.capacity
    }

    /// Stage work for durable record.
    ///
    /// Returns [`OutboxFull`] carrying the original work when at capacity. On
    /// success the returned [`DurableWork`] carries the journal index and bytes
    /// to append; the work is not yet durable. Follow every `enqueue` with
    /// exactly one [`record`](Self::record) or [`abandon`](Self::abandon) —
    /// dropping the token leaks its capacity slot.
    pub fn enqueue(&mut self, work: W) -> Result<DurableWork<W>, OutboxFull<W>> {
        if self.is_full() {
            return Err(OutboxFull { work });
        }
        let id = WorkId(self.next_work_id);
        let index = self.next_index;
        self.next_work_id += 1;
        self.next_index += 1;
        self.staged.insert(id.0);
        let payload = frame_record(TAG_ENQUEUE, id, Some(&work.to_durable_bytes()));
        Ok(DurableWork {
            id,
            index,
            payload,
            work,
        })
    }

    /// Confirm the durable append for staged work.
    ///
    /// `append` is the result the runtime returned for the
    /// [`crate::journal_append`] of `staged.journal_bytes()`. On `Ok` the work
    /// becomes pending and a [`RecordedWork`] authorizes apply. On `Err`, or
    /// when the staged token is stale, the original work is returned in
    /// [`RecordError`] and no pending entry is fabricated.
    pub fn record(
        &mut self,
        staged: DurableWork<W>,
        append: Result<(), CallError>,
    ) -> Result<RecordedWork<W>, RecordError<W>> {
        // Only advance state for a token this outbox actually staged. A stale
        // token (for example one held across a `recover`, which returns a fresh
        // outbox) is not ours; do not fabricate a pending entry from it.
        let was_staged = self.staged.remove(&staged.id.0);
        match append {
            Ok(()) => {
                if was_staged {
                    self.pending.insert(staged.id.0);
                    Ok(RecordedWork {
                        id: staged.id,
                        work: staged.work,
                    })
                } else {
                    self.advance_watermark();
                    Err(RecordError::Stale(StaleWork { work: staged.work }))
                }
            }
            Err(error) => {
                if was_staged {
                    self.failed_total += 1;
                }
                self.advance_watermark();
                Err(RecordError::Append(AppendFailed {
                    error,
                    work: staged.work,
                }))
            }
        }
    }

    /// Abandon staged work without recording it, freeing its capacity slot and
    /// returning the original work.
    ///
    /// Stage with [`enqueue`](Self::enqueue), then exactly one of:
    /// [`record`](Self::record) (durable) or `abandon` (give up). Dropping the
    /// [`DurableWork`] instead leaks its capacity slot — the `#[must_use]` lint
    /// flags that.
    pub fn abandon(&mut self, staged: DurableWork<W>) -> W {
        self.staged.remove(&staged.id.0);
        self.advance_watermark();
        staged.work
    }

    /// Apply recorded work.
    ///
    /// Consumes the [`RecordedWork`], so the same authorization cannot be
    /// applied twice. Returns [`ApplyStatus::Apply`] with the work to act on,
    /// or [`ApplyStatus::DuplicateWork`] if the id was already completed (a
    /// defensive guard against a stale replayed token).
    pub fn apply(&mut self, recorded: RecordedWork<W>) -> ApplyStatus<W> {
        if self.is_completed(recorded.id) {
            return ApplyStatus::DuplicateWork(recorded.id);
        }
        ApplyStatus::Apply(recorded.work)
    }

    /// Begin marking work complete by staging a completion record.
    ///
    /// Idempotent by id: a second completion of the same id is
    /// [`CompletionStart::AlreadyCompleted`]. An unknown or not-yet-recorded id
    /// is [`CompletionStart::NotPending`].
    pub fn begin_complete(&mut self, id: WorkId) -> CompletionStart {
        if self.is_completed(id) {
            return CompletionStart::AlreadyCompleted(id);
        }
        if !self.pending.contains(&id.0) {
            return CompletionStart::NotPending(id);
        }
        let index = self.next_index;
        self.next_index += 1;
        let payload = frame_record(TAG_COMPLETE, id, None);
        CompletionStart::Record(DurableCompletion { id, index, payload })
    }

    /// Confirm the durable append for a completion.
    ///
    /// On `Ok` the work leaves pending and is recorded completed. On `Err` the
    /// work stays pending and replays next run; an
    /// [`CallError::CommitUncertain`] here is preserved verbatim so the caller
    /// can persist a commit fence for the next recovery.
    pub fn finish_complete(
        &mut self,
        completion: DurableCompletion,
        append: Result<(), CallError>,
    ) -> Result<CommittedWork, CompletionFailed> {
        match append {
            Ok(()) => {
                // Count and mark complete once. A second completion record for
                // the same id (the work already left pending) is idempotent: the
                // durable record is fine, but it is not a fresh completion.
                if self.pending.remove(&completion.id.0) {
                    self.mark_completed(completion.id);
                    self.completed_total += 1;
                }
                Ok(CommittedWork { id: completion.id })
            }
            Err(error) => {
                self.failed_total += 1;
                Err(CompletionFailed {
                    error,
                    work_id: completion.id,
                })
            }
        }
    }

    /// Rebuild an outbox from a recovered journal.
    ///
    /// `replay` is the result of [`crate::journal_replay`]. `commit` is the
    /// durability confidence of the most recent commit before this recovery
    /// (see [`CommitConfidence`]).
    ///
    /// Returns the rebuilt outbox and a [`RecoveryReport`], or a
    /// [`RecoveryError`] when the tail is corrupt or replay would exceed
    /// capacity. Records are folded in journal order: an enqueue makes work
    /// pending, a completion clears it and marks the id completed. Pending work
    /// is returned as ready-to-apply [`RecordedWork`].
    ///
    /// This returns a *fresh* outbox. Any staged [`DurableWork`] from a prior
    /// instance is stale against it; drop those tokens rather than recording
    /// them here.
    pub fn recover(
        capacity: usize,
        replay: Result<JournalReplay, CallError>,
        commit: CommitConfidence,
    ) -> Result<(Self, RecoveryReport<W>), RecoveryError> {
        let replay = match replay {
            Ok(replay) => replay,
            Err(CallError::CorruptRecord) => return Err(RecoveryError::CorruptTail),
            Err(_) => return Err(RecoveryError::Io),
        };

        let cap = capacity.max(1);
        let mut outbox = Self::new(cap);
        // Preserve durable order while reconstructing which work is still open.
        // `Vec` keeps insertion order for the report; the set guards membership.
        let mut pending_order: Vec<(WorkId, W)> = Vec::new();
        let mut pending_ids: BTreeSet<u64> = BTreeSet::new();
        let mut completed_order: Vec<WorkId> = Vec::new();
        let mut max_index = 0_u64;
        let mut max_work_id = 0_u64;

        for record in &replay.records {
            max_index = max_index.max(record.index);
            let Some((tag, id, payload)) = unframe_record(&record.bytes) else {
                return Err(RecoveryError::CorruptTail);
            };
            max_work_id = max_work_id.max(id.0);
            match tag {
                TAG_ENQUEUE => {
                    let Some(work) = W::from_durable_bytes(payload) else {
                        return Err(RecoveryError::CorruptTail);
                    };
                    if pending_ids.insert(id.0) {
                        pending_order.push((id, work));
                        // Bound replay: refuse the moment the live backlog would
                        // exceed capacity, instead of scanning an oversized log.
                        // `pending` is the count at that point (capacity + 1).
                        if pending_ids.len() > cap {
                            return Err(RecoveryError::OverCapacity {
                                capacity: cap,
                                pending: pending_ids.len(),
                            });
                        }
                    }
                }
                TAG_COMPLETE => {
                    if pending_ids.remove(&id.0) {
                        pending_order.retain(|(pending_id, _)| pending_id.0 != id.0);
                    }
                    if outbox.completed.insert(id.0) {
                        completed_order.push(id);
                    }
                }
                _ => return Err(RecoveryError::CorruptTail),
            }
        }

        outbox.next_index = max_index + 1;
        outbox.next_work_id = max_work_id + 1;
        outbox.pending = pending_ids;
        outbox.advance_watermark();

        let tail_status = match (commit, replay.warning) {
            (CommitConfidence::Uncertain, _) => TailStatus::UncertainCommit,
            (_, Some(JournalReplayWarning::TruncatedTail { valid_prefix_len })) => {
                TailStatus::TruncatedTailRepaired { valid_prefix_len }
            }
            (CommitConfidence::Clean, None) => TailStatus::Clean,
        };

        let pending = pending_order
            .into_iter()
            .map(|(id, work)| RecordedWork { id, work })
            .collect();

        Ok((
            outbox,
            RecoveryReport {
                tail_status,
                pending,
                completed: completed_order,
            },
        ))
    }

    /// Snapshot the outstanding work at shutdown.
    ///
    /// Names every category: pending (replays next run), abandoned (staged but
    /// never durable), and the completed/failed run totals.
    pub fn shutdown_report(&self) -> OutboxShutdownReport {
        OutboxShutdownReport {
            pending: self.pending.iter().copied().map(WorkId).collect(),
            abandoned: self.staged.iter().copied().map(WorkId).collect(),
            completed: self.completed_total,
            failed: self.failed_total,
        }
    }

    fn is_completed(&self, id: WorkId) -> bool {
        id.0 <= self.completed_watermark || self.completed.contains(&id.0)
    }

    fn mark_completed(&mut self, id: WorkId) {
        if id.0 > self.completed_watermark {
            self.completed.insert(id.0);
        }
        self.advance_watermark();
    }

    /// Advance the completed watermark past any prefix of ids that are below
    /// the lowest still-outstanding id, dropping them from `completed`. Keeps
    /// the `completed` set bounded by the number of out-of-order completions.
    fn advance_watermark(&mut self) {
        let lowest_outstanding = self
            .staged
            .iter()
            .chain(self.pending.iter())
            .copied()
            .min()
            .unwrap_or(self.next_work_id);
        let new_watermark = lowest_outstanding.saturating_sub(1);
        if new_watermark > self.completed_watermark {
            self.completed_watermark = new_watermark;
            self.completed.retain(|&id| id > new_watermark);
        }
    }
}

/// Frame an inner durable record: tag, work id, optional payload.
fn frame_record(tag: u8, id: WorkId, payload: Option<&[u8]>) -> Vec<u8> {
    let body = payload.unwrap_or(&[]);
    let mut out = Vec::with_capacity(FRAME_HEADER + body.len());
    out.push(tag);
    out.extend_from_slice(&id.0.to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// Decode an inner durable record. `None` means the framing is malformed.
fn unframe_record(bytes: &[u8]) -> Option<(u8, WorkId, &[u8])> {
    if bytes.len() < FRAME_HEADER {
        return None;
    }
    let tag = bytes[0];
    let mut id_bytes = [0_u8; 8];
    id_bytes.copy_from_slice(&bytes[1..FRAME_HEADER]);
    let id = WorkId(u64::from_le_bytes(id_bytes));
    Some((tag, id, &bytes[FRAME_HEADER..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{JournalRecord, persistence::encode_journal_record};

    /// Drive a staged enqueue through a successful durable append.
    fn record_ok<W: DurablePayload>(outbox: &mut DurableOutbox<W>, work: W) -> RecordedWork<W> {
        let staged = outbox.enqueue(work).unwrap_or_else(|_| panic!("not full"));
        outbox
            .record(staged, Ok(()))
            .unwrap_or_else(|_| panic!("append ok"))
    }

    /// Build a journal replay from framed outbox records, as the runtime would
    /// return from `journal_replay`.
    fn replay_of(records: &[(u64, Vec<u8>)]) -> JournalReplay {
        let bytes: Vec<u8> = records
            .iter()
            .flat_map(|(index, payload)| {
                encode_journal_record(&JournalRecord {
                    index: *index,
                    bytes: payload.clone(),
                })
            })
            .collect();
        crate::persistence::replay_journal_bytes(&bytes).expect("clean replay")
    }

    #[test]
    fn full_outbox_returns_full_with_original_work() {
        let mut outbox: DurableOutbox<Vec<u8>> = DurableOutbox::new(1);
        let _staged = outbox.enqueue(b"first".to_vec()).expect("first fits");
        match outbox.enqueue(b"second".to_vec()) {
            Err(OutboxFull { work }) => assert_eq!(work, b"second".to_vec()),
            Ok(_) => panic!("expected Full at capacity"),
        }
    }

    #[test]
    fn append_failure_returns_original_work_and_frees_capacity() {
        let mut outbox: DurableOutbox<Vec<u8>> = DurableOutbox::new(1);
        let staged = outbox.enqueue(b"work".to_vec()).expect("fits");
        let failed = outbox
            .record(staged, Err(CallError::Io))
            .expect_err("append failed");
        let RecordError::Append(failed) = failed else {
            panic!("expected append failure");
        };
        assert_eq!(failed.error, CallError::Io);
        assert_eq!(failed.work, b"work".to_vec());
        // capacity freed; a fresh enqueue succeeds.
        assert!(outbox.enqueue(b"retry".to_vec()).is_ok());
        assert_eq!(outbox.shutdown_report().failed, 1);
    }

    #[test]
    fn stale_staged_token_cannot_mint_recorded_work() {
        let mut old_outbox: DurableOutbox<Vec<u8>> = DurableOutbox::new(1);
        let staged = old_outbox.enqueue(b"old".to_vec()).expect("fits");
        let mut fresh_outbox: DurableOutbox<Vec<u8>> = DurableOutbox::new(1);

        let err = fresh_outbox
            .record(staged, Ok(()))
            .expect_err("stale staged token rejected");
        let RecordError::Stale(stale) = err else {
            panic!("expected stale token");
        };
        assert_eq!(stale.work, b"old".to_vec());
        assert_eq!(fresh_outbox.shutdown_report().pending, Vec::<WorkId>::new());
        assert!(fresh_outbox.enqueue(b"new".to_vec()).is_ok());
    }

    #[test]
    fn stable_work_ids_increase_and_persist_payload() {
        let mut outbox: DurableOutbox<Vec<u8>> = DurableOutbox::new(8);
        let a = outbox.enqueue(b"a".to_vec()).unwrap();
        let b = outbox.enqueue(b"b".to_vec()).unwrap();
        assert_eq!(a.work_id(), WorkId(1));
        assert_eq!(b.work_id(), WorkId(2));
        // round-trip the framed record id + payload
        let (tag, id, payload) = unframe_record(a.journal_bytes()).unwrap();
        assert_eq!(tag, TAG_ENQUEUE);
        assert_eq!(id, WorkId(1));
        assert_eq!(payload, b"a");
    }

    #[test]
    fn apply_then_complete_is_idempotent_by_id() {
        let mut outbox: DurableOutbox<Vec<u8>> = DurableOutbox::new(4);
        let recorded = record_ok(&mut outbox, b"send".to_vec());
        let id = recorded.work_id();
        assert_eq!(outbox.apply(recorded), ApplyStatus::Apply(b"send".to_vec()));

        let completion = match outbox.begin_complete(id) {
            CompletionStart::Record(c) => c,
            other => panic!("expected Record, got {other:?}"),
        };
        let committed = outbox
            .finish_complete(completion, Ok(()))
            .expect("committed");
        assert_eq!(committed.work_id(), id);

        // second completion is idempotent, named, not silent.
        assert!(matches!(
            outbox.begin_complete(id),
            CompletionStart::AlreadyCompleted(found) if found == id
        ));
        assert_eq!(outbox.shutdown_report().completed, 1);
    }

    #[test]
    fn begin_complete_unknown_id_is_not_pending() {
        let mut outbox: DurableOutbox<Vec<u8>> = DurableOutbox::new(4);
        assert!(matches!(
            outbox.begin_complete(WorkId(99)),
            CompletionStart::NotPending(found) if found == WorkId(99)
        ));
    }

    #[test]
    fn completion_append_failure_keeps_work_pending() {
        let mut outbox: DurableOutbox<Vec<u8>> = DurableOutbox::new(4);
        let recorded = record_ok(&mut outbox, b"x".to_vec());
        let id = recorded.work_id();
        let _ = outbox.apply(recorded);
        let completion = match outbox.begin_complete(id) {
            CompletionStart::Record(c) => c,
            other => panic!("expected Record, got {other:?}"),
        };
        let failed = outbox
            .finish_complete(completion, Err(CallError::CommitUncertain))
            .expect_err("completion failed");
        assert_eq!(failed.error, CallError::CommitUncertain);
        assert_eq!(failed.work_id, id);
        // still pending, so it replays.
        assert_eq!(outbox.shutdown_report().pending, vec![id]);
    }

    #[test]
    fn recover_resumes_pending_and_lists_completed_without_replaying_it() {
        // Enqueue 1, complete 1, enqueue 2. Only 2 should replay.
        let records = vec![
            (1, frame_record(TAG_ENQUEUE, WorkId(1), Some(b"done"))),
            (2, frame_record(TAG_COMPLETE, WorkId(1), None)),
            (3, frame_record(TAG_ENQUEUE, WorkId(2), Some(b"pending"))),
        ];
        let (mut outbox, report) =
            DurableOutbox::<Vec<u8>>::recover(8, Ok(replay_of(&records)), CommitConfidence::Clean)
                .expect("recovered");
        assert_eq!(report.tail_status, TailStatus::Clean);
        assert_eq!(report.completed, vec![WorkId(1)]);
        assert_eq!(report.pending.len(), 1);
        assert_eq!(report.pending[0].work_id(), WorkId(2));

        // completed work (id 1) cannot be re-applied: it is not in pending and
        // ids continue past it.
        let recorded = report.pending.into_iter().next().unwrap();
        assert_eq!(
            outbox.apply(recorded),
            ApplyStatus::Apply(b"pending".to_vec())
        );
        // next enqueue gets a fresh id past the recovered max.
        let next = outbox.enqueue(b"new".to_vec()).unwrap();
        assert_eq!(next.work_id(), WorkId(3));
    }

    #[test]
    fn recover_rejects_corrupt_tail_by_name() {
        let err = DurableOutbox::<Vec<u8>>::recover(
            8,
            Err(CallError::CorruptRecord),
            CommitConfidence::Clean,
        )
        .expect_err("corrupt rejected");
        assert_eq!(err, RecoveryError::CorruptTail);
    }

    #[test]
    fn recover_reports_truncated_tail_repair_not_clean() {
        let mut bytes: Vec<u8> = encode_journal_record(&JournalRecord {
            index: 1,
            bytes: frame_record(TAG_ENQUEUE, WorkId(1), Some(b"keep")),
        });
        let valid_prefix_len = bytes.len() as u64;
        // partial trailing record
        bytes.extend_from_slice(&[7, 7, 7]);
        let replay = crate::persistence::replay_journal_bytes(&bytes).unwrap();
        let (_outbox, report) =
            DurableOutbox::<Vec<u8>>::recover(8, Ok(replay), CommitConfidence::Clean)
                .expect("recovered");
        assert_eq!(
            report.tail_status,
            TailStatus::TruncatedTailRepaired { valid_prefix_len }
        );
        assert_eq!(report.pending.len(), 1);
    }

    #[test]
    fn recover_flags_uncertain_commit_distinct_from_clean_and_corrupt() {
        let records = vec![(1, frame_record(TAG_ENQUEUE, WorkId(1), Some(b"x")))];
        let (_outbox, report) = DurableOutbox::<Vec<u8>>::recover(
            8,
            Ok(replay_of(&records)),
            CommitConfidence::Uncertain,
        )
        .expect("recovered");
        assert_eq!(report.tail_status, TailStatus::UncertainCommit);
        assert_ne!(report.tail_status, TailStatus::Clean);
    }

    #[test]
    fn recover_bails_at_capacity_plus_one_for_over_capacity_replay() {
        let records: Vec<(u64, Vec<u8>)> = (1..=4)
            .map(|i| (i, frame_record(TAG_ENQUEUE, WorkId(i), Some(b"x"))))
            .collect();
        let err =
            DurableOutbox::<Vec<u8>>::recover(2, Ok(replay_of(&records)), CommitConfidence::Clean)
                .expect_err("over capacity");
        // Bounded replay: refused at the third enqueue (capacity 2 + 1), not
        // after scanning all four.
        assert_eq!(
            err,
            RecoveryError::OverCapacity {
                capacity: 2,
                pending: 3
            }
        );
    }

    #[test]
    fn recover_within_capacity_with_interleaving_succeeds() {
        // Enqueue/complete interleaved so the live backlog never exceeds 2,
        // even though five items pass through a capacity-2 outbox.
        let records = vec![
            (1, frame_record(TAG_ENQUEUE, WorkId(1), Some(b"a"))),
            (2, frame_record(TAG_ENQUEUE, WorkId(2), Some(b"b"))),
            (3, frame_record(TAG_COMPLETE, WorkId(1), None)),
            (4, frame_record(TAG_ENQUEUE, WorkId(3), Some(b"c"))),
            (5, frame_record(TAG_COMPLETE, WorkId(2), None)),
            (6, frame_record(TAG_ENQUEUE, WorkId(4), Some(b"d"))),
        ];
        let (_outbox, report) =
            DurableOutbox::<Vec<u8>>::recover(2, Ok(replay_of(&records)), CommitConfidence::Clean)
                .expect("within capacity");
        let pending: Vec<u64> = report.pending.iter().map(|w| w.work_id().0).collect();
        // 3 and 4 remain, in durable order.
        assert_eq!(pending, vec![3, 4]);
    }

    #[test]
    fn recover_preserves_pending_durable_order() {
        let records = vec![
            (1, frame_record(TAG_ENQUEUE, WorkId(1), Some(b"first"))),
            (2, frame_record(TAG_ENQUEUE, WorkId(2), Some(b"second"))),
            (3, frame_record(TAG_ENQUEUE, WorkId(3), Some(b"third"))),
        ];
        let (mut outbox, report) =
            DurableOutbox::<Vec<u8>>::recover(8, Ok(replay_of(&records)), CommitConfidence::Clean)
                .expect("recovered");
        let order: Vec<Vec<u8>> = report
            .pending
            .into_iter()
            .map(|recorded| match outbox.apply(recorded) {
                ApplyStatus::Apply(work) => work,
                ApplyStatus::DuplicateWork(_) => panic!("unexpected duplicate"),
            })
            .collect();
        assert_eq!(
            order,
            vec![b"first".to_vec(), b"second".to_vec(), b"third".to_vec()]
        );
    }

    #[test]
    fn abandon_frees_capacity_and_returns_work() {
        let mut outbox: DurableOutbox<Vec<u8>> = DurableOutbox::new(1);
        let staged = outbox.enqueue(b"work".to_vec()).expect("fits");
        assert!(outbox.is_full());
        let work = outbox.abandon(staged);
        assert_eq!(work, b"work".to_vec());
        // slot reclaimed: a fresh enqueue fits, and nothing is left abandoned.
        assert!(!outbox.is_full());
        assert!(outbox.enqueue(b"next".to_vec()).is_ok());
        let report = outbox.shutdown_report();
        // the abandoned token freed its slot, so it is not reported as abandoned.
        assert!(!report.abandoned.contains(&WorkId(1)));
    }

    #[test]
    fn finish_complete_counts_completion_once_for_a_duplicate_record() {
        let mut outbox: DurableOutbox<Vec<u8>> = DurableOutbox::new(4);
        let recorded = record_ok(&mut outbox, b"x".to_vec());
        let id = recorded.work_id();
        let _ = outbox.apply(recorded);
        // Stage two completion records before either finishes (a re-issued
        // completion). Both durable appends succeed.
        let first = match outbox.begin_complete(id) {
            CompletionStart::Record(c) => c,
            other => panic!("expected Record, got {other:?}"),
        };
        outbox.finish_complete(first, Ok(())).expect("first commit");
        // The id already left pending; begin_complete is now idempotent.
        assert!(matches!(
            outbox.begin_complete(id),
            CompletionStart::AlreadyCompleted(found) if found == id
        ));
        // Completion counted exactly once despite the extra round.
        assert_eq!(outbox.shutdown_report().completed, 1);
    }

    #[test]
    fn recover_rejects_malformed_inner_framing_as_corrupt() {
        // intact journal checksum, but inner outbox framing too short.
        let records = vec![(1_u64, vec![TAG_ENQUEUE, 0, 0])];
        let err =
            DurableOutbox::<Vec<u8>>::recover(8, Ok(replay_of(&records)), CommitConfidence::Clean)
                .expect_err("malformed framing");
        assert_eq!(err, RecoveryError::CorruptTail);
    }

    #[test]
    fn duplicate_apply_of_completed_id_is_named() {
        // Defensive guard: a stale token for an already-completed id applies to
        // DuplicateWork, not silent success. Reachable only via the private
        // constructor since the type-state prevents it for users.
        let mut outbox: DurableOutbox<Vec<u8>> = DurableOutbox::new(4);
        let recorded = record_ok(&mut outbox, b"x".to_vec());
        let id = recorded.work_id();
        let _ = outbox.apply(recorded);
        let completion = match outbox.begin_complete(id) {
            CompletionStart::Record(c) => c,
            other => panic!("expected Record, got {other:?}"),
        };
        outbox.finish_complete(completion, Ok(())).unwrap();

        let stale = RecordedWork {
            id,
            work: b"x".to_vec(),
        };
        assert_eq!(outbox.apply(stale), ApplyStatus::DuplicateWork(id));
    }

    #[test]
    fn shutdown_report_names_pending_abandoned_completed_failed() {
        let mut outbox: DurableOutbox<Vec<u8>> = DurableOutbox::new(8);
        // completed
        let done = record_ok(&mut outbox, b"done".to_vec());
        let done_id = done.work_id();
        let _ = outbox.apply(done);
        if let CompletionStart::Record(c) = outbox.begin_complete(done_id) {
            outbox.finish_complete(c, Ok(())).unwrap();
        }
        // pending (recorded, not completed)
        let pending = record_ok(&mut outbox, b"pending".to_vec());
        let pending_id = pending.work_id();
        let _ = outbox.apply(pending);
        // failed append
        let staged = outbox.enqueue(b"fail".to_vec()).unwrap();
        let _ = outbox.record(staged, Err(CallError::Io));
        // abandoned: staged, never recorded
        let abandoned = outbox.enqueue(b"abandon".to_vec()).unwrap();
        let abandoned_id = abandoned.work_id();
        drop(abandoned); // never recorded; stays in the staged set

        let report = outbox.shutdown_report();
        assert_eq!(report.pending, vec![pending_id]);
        assert_eq!(report.abandoned, vec![abandoned_id]);
        assert_eq!(report.completed, 1);
        assert_eq!(report.failed, 1);
    }
}
