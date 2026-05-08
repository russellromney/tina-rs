//! Reusable deterministic-simulation-testing helpers.
//!
//! This module is intentionally small. It gives Tina tests a common shape for
//! history-as-data runs, replay checks, deletion shrinking, and trace
//! invariants without becoming a general property-testing framework.

use std::fmt::Debug;

use tina::{AddressGeneration, IsolateId, ShardId};
use tina_runtime::{
    CallError, CallId, CauseId, EventId, RuntimeEvent, RuntimeEventKind, SendRejectedReason,
};

use crate::DurableImage;

/// One replayable generated or hand-authored operation history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History<Op> {
    name: &'static str,
    seed: u64,
    operations: Vec<Op>,
}

impl<Op> History<Op> {
    /// Creates a history with a stable name, seed, and operation list.
    pub fn new(name: &'static str, seed: u64, operations: Vec<Op>) -> Self {
        Self {
            name,
            seed,
            operations,
        }
    }

    /// Returns the workload name.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Returns the deterministic generation seed.
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    /// Returns the replayable operation list.
    pub fn operations(&self) -> &[Op] {
        &self.operations
    }

    /// Returns the number of operations in the history.
    pub fn len(&self) -> usize {
        self.operations.len()
    }

    /// Returns true when the history has no operations.
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    /// Returns a new history with the same name and seed but different
    /// operations.
    pub fn with_operations(&self, operations: Vec<Op>) -> Self {
        Self {
            name: self.name,
            seed: self.seed,
            operations,
        }
    }
}

/// Result of running one DST history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DstRun<Output, Artifact = crate::ReplayArtifact> {
    output: Output,
    artifact: Artifact,
}

impl<Output, Artifact> DstRun<Output, Artifact> {
    /// Creates a run result from a semantic output projection and replay
    /// artifact.
    pub fn new(output: Output, artifact: Artifact) -> Self {
        Self { output, artifact }
    }

    /// Returns the semantic output projection.
    pub const fn output(&self) -> &Output {
        &self.output
    }

    /// Returns the replay artifact.
    pub const fn artifact(&self) -> &Artifact {
        &self.artifact
    }

    /// Splits the run into its output and artifact.
    pub fn into_parts(self) -> (Output, Artifact) {
        (self.output, self.artifact)
    }
}

/// Runs one history twice and returns both runs.
pub fn run_twice_same_history<Op, Output, Artifact, Runner>(
    history: &History<Op>,
    mut runner: Runner,
) -> (DstRun<Output, Artifact>, DstRun<Output, Artifact>)
where
    Runner: FnMut(&History<Op>) -> DstRun<Output, Artifact>,
{
    let first = runner(history);
    let second = runner(history);
    (first, second)
}

/// Runs one history twice, asserts exact replay equality, and returns the
/// first run for additional test-specific checks.
pub fn assert_replays<Op, Output, Artifact, Runner>(
    history: &History<Op>,
    runner: Runner,
) -> DstRun<Output, Artifact>
where
    Op: Debug,
    Output: PartialEq + Debug,
    Artifact: PartialEq + Debug,
    Runner: FnMut(&History<Op>) -> DstRun<Output, Artifact>,
{
    let (first, second) = run_twice_same_history(history, runner);
    assert_eq!(
        first,
        second,
        "DST replay drift in {} seed {} history_len {} ops {:#?}",
        history.name(),
        history.seed(),
        history.len(),
        history.operations()
    );
    first
}

/// Deletion-only shrinker settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShrinkConfig {
    /// Maximum candidate deletions attempted before returning the current
    /// shrunk history.
    pub max_attempts: usize,
}

impl Default for ShrinkConfig {
    fn default() -> Self {
        Self { max_attempts: 1024 }
    }
}

/// One deletion-shrunk failing history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShrunkFailure<Op> {
    original: History<Op>,
    shrunk: History<Op>,
    reason: String,
    attempts: usize,
}

impl<Op> ShrunkFailure<Op> {
    /// Returns the original failing history.
    pub const fn original(&self) -> &History<Op> {
        &self.original
    }

    /// Returns the shrunk failing history.
    pub const fn shrunk(&self) -> &History<Op> {
        &self.shrunk
    }

    /// Returns the failure reason supplied by the caller.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// Returns how many candidate deletions were tried.
    pub const fn attempts(&self) -> usize {
        self.attempts
    }
}

/// Deletion-shrinks a history while `still_fails` remains true.
pub fn delete_shrink<Op, F>(
    history: &History<Op>,
    config: ShrinkConfig,
    reason: impl Into<String>,
    mut still_fails: F,
) -> ShrunkFailure<Op>
where
    Op: Clone,
    F: FnMut(&History<Op>) -> bool,
{
    let mut current = history.clone();
    let mut index = 0;
    let mut attempts = 0;
    while index < current.operations.len() && attempts < config.max_attempts {
        let mut candidate_ops = current.operations.clone();
        candidate_ops.remove(index);
        let candidate = current.with_operations(candidate_ops);
        attempts += 1;
        if still_fails(&candidate) {
            current = candidate;
            index = 0;
        } else {
            index += 1;
        }
    }

    ShrunkFailure {
        original: history.clone(),
        shrunk: current,
        reason: reason.into(),
        attempts,
    }
}

/// One reusable invariant failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvariantViolation {
    invariant: &'static str,
    event_id: Option<EventId>,
    reason: String,
}

impl InvariantViolation {
    /// Creates an invariant violation.
    pub fn new(
        invariant: &'static str,
        event_id: Option<EventId>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            invariant,
            event_id,
            reason: reason.into(),
        }
    }

    /// Returns the invariant name.
    pub const fn invariant(&self) -> &'static str {
        self.invariant
    }

    /// Returns the event closest to the failure, when there is one.
    pub const fn event_id(&self) -> Option<EventId> {
        self.event_id
    }

    /// Returns the human-readable failure reason.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// Reusable trace invariant set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvariantSuite {
    /// Check that event IDs increase by one from the first event.
    pub monotonic_events: bool,
    /// Check that causal links point backward and at existing events.
    pub causal_links: bool,
    /// Check that every send attempt has an accepted or rejected outcome.
    pub send_settlement: bool,
    /// Check that every runtime-call attempt settles.
    pub call_settlement: bool,
    /// Check that stopped isolates do not run later handlers.
    pub no_handler_after_stop: bool,
    /// Check that abandoned messages have a visible causal link.
    pub no_untraced_abandonment: bool,
}

impl InvariantSuite {
    /// Returns the standard Tina trace invariant set.
    pub const fn standard() -> Self {
        Self {
            monotonic_events: true,
            causal_links: true,
            send_settlement: true,
            call_settlement: true,
            no_handler_after_stop: true,
            no_untraced_abandonment: true,
        }
    }

    /// Checks all enabled invariants against one trace.
    pub fn check(self, events: &[RuntimeEvent]) -> Result<(), InvariantViolation> {
        if self.monotonic_events {
            events_are_monotonic(events)?;
        }
        if self.causal_links {
            causes_point_backward(events)?;
        }
        if self.send_settlement {
            send_attempts_settle(events)?;
        }
        if self.call_settlement {
            call_attempts_settle(events)?;
        }
        if self.no_handler_after_stop {
            no_handler_after_stop(events)?;
        }
        if self.no_untraced_abandonment {
            no_untraced_abandonment(events)?;
        }
        Ok(())
    }

    /// Panics with a useful message if any enabled invariant fails.
    pub fn assert(self, events: &[RuntimeEvent]) {
        if let Err(violation) = self.check(events) {
            panic!(
                "DST invariant {} failed near {:?}: {}",
                violation.invariant(),
                violation.event_id(),
                violation.reason()
            );
        }
    }
}

impl Default for InvariantSuite {
    fn default() -> Self {
        Self::standard()
    }
}

/// Checks that event IDs increase by one in trace order.
pub fn events_are_monotonic(events: &[RuntimeEvent]) -> Result<(), InvariantViolation> {
    let mut previous = None;
    for event in events {
        if let Some(previous) = previous {
            if event.id().get() != previous + 1 {
                return Err(InvariantViolation::new(
                    "events_are_monotonic",
                    Some(event.id()),
                    format!("event id {} followed {}", event.id().get(), previous),
                ));
            }
        }
        previous = Some(event.id().get());
    }
    Ok(())
}

/// Checks that every cause points backward and at an event in the trace.
pub fn causes_point_backward(events: &[RuntimeEvent]) -> Result<(), InvariantViolation> {
    for event in events {
        if let Some(cause) = event.cause() {
            if cause.event() >= event.id() {
                return Err(InvariantViolation::new(
                    "causes_point_backward",
                    Some(event.id()),
                    format!("cause {:?} does not point backward", cause),
                ));
            }
            if !events
                .iter()
                .any(|candidate| candidate.id() == cause.event())
            {
                return Err(InvariantViolation::new(
                    "causes_point_backward",
                    Some(event.id()),
                    format!("cause {:?} points at no event in trace", cause),
                ));
            }
        }
    }
    Ok(())
}

/// Checks that every send attempt has a same-target accepted or rejected
/// outcome caused by the attempt.
pub fn send_attempts_settle(events: &[RuntimeEvent]) -> Result<(), InvariantViolation> {
    for event in events {
        let RuntimeEventKind::SendDispatchAttempted {
            target_shard,
            target_isolate,
            target_generation,
        } = event.kind()
        else {
            continue;
        };
        let cause = Some(CauseId::new(event.id()));
        if !events.iter().any(|candidate| {
            candidate.cause() == cause
                && send_outcome_matches(
                    candidate.kind(),
                    target_shard,
                    target_isolate,
                    target_generation,
                )
        }) {
            return Err(InvariantViolation::new(
                "send_attempts_settle",
                Some(event.id()),
                "send attempt had no accepted/rejected outcome",
            ));
        }
    }
    Ok(())
}

fn send_outcome_matches(
    kind: RuntimeEventKind,
    target_shard: ShardId,
    target_isolate: IsolateId,
    target_generation: AddressGeneration,
) -> bool {
    match kind {
        RuntimeEventKind::SendAccepted {
            target_shard: accepted_shard,
            target_isolate: accepted_isolate,
            target_generation: accepted_generation,
        } => {
            accepted_shard == target_shard
                && accepted_isolate == target_isolate
                && accepted_generation == target_generation
        }
        RuntimeEventKind::SendRejected {
            target_shard: rejected_shard,
            target_isolate: rejected_isolate,
            target_generation: rejected_generation,
            reason: SendRejectedReason::Full | SendRejectedReason::Closed,
        } => {
            rejected_shard == target_shard
                && rejected_isolate == target_isolate
                && rejected_generation == target_generation
        }
        _ => false,
    }
}

/// Checks that every runtime-owned call attempt settles as completed,
/// failed, or completion-rejected.
pub fn call_attempts_settle(events: &[RuntimeEvent]) -> Result<(), InvariantViolation> {
    for event in events {
        let RuntimeEventKind::CallDispatchAttempted { call_id, .. } = event.kind() else {
            continue;
        };
        let cause = Some(CauseId::new(event.id()));
        if !events.iter().any(|candidate| {
            candidate.cause() == cause && call_outcome_matches(candidate.kind(), call_id)
        }) {
            return Err(InvariantViolation::new(
                "call_attempts_settle",
                Some(event.id()),
                "call attempt had no completed/failed/rejected outcome",
            ));
        }
    }
    Ok(())
}

fn call_outcome_matches(kind: RuntimeEventKind, expected: CallId) -> bool {
    matches!(
        kind,
        RuntimeEventKind::CallCompleted {
            call_id,
            ..
        } | RuntimeEventKind::CallFailed {
            call_id,
            ..
        } | RuntimeEventKind::CallCompletionRejected {
            call_id,
            ..
        } | RuntimeEventKind::CallCancelled {
            call_id,
            ..
        } if call_id == expected
    )
}

/// Checks that no handler starts for an isolate identity after that
/// identity has stopped.
pub fn no_handler_after_stop(events: &[RuntimeEvent]) -> Result<(), InvariantViolation> {
    let mut stopped = Vec::new();
    for event in events {
        let identity = (event.shard(), event.isolate());
        if matches!(event.kind(), RuntimeEventKind::HandlerStarted) && stopped.contains(&identity) {
            return Err(InvariantViolation::new(
                "no_handler_after_stop",
                Some(event.id()),
                format!(
                    "isolate {:?} on shard {:?} handled after stop",
                    event.isolate(),
                    event.shard()
                ),
            ));
        }
        if matches!(
            event.kind(),
            RuntimeEventKind::IsolateStopped | RuntimeEventKind::HandlerPanicked
        ) {
            stopped.push(identity);
        }
    }
    Ok(())
}

/// Checks that abandoned messages have visible causal links to earlier
/// events instead of appearing as silent drops.
pub fn no_untraced_abandonment(events: &[RuntimeEvent]) -> Result<(), InvariantViolation> {
    for event in events {
        if matches!(event.kind(), RuntimeEventKind::MessageAbandoned) && event.cause().is_none() {
            return Err(InvariantViolation::new(
                "no_untraced_abandonment",
                Some(event.id()),
                "abandoned message had no cause",
            ));
        }
    }
    Ok(())
}

/// Checks that a durable journal image can be replayed.
pub fn persistence_image_replays(
    image: &DurableImage,
    path: impl AsRef<std::path::Path>,
) -> Result<(), InvariantViolation> {
    let path = path.as_ref();
    let bytes = image.get(path).ok_or_else(|| {
        InvariantViolation::new(
            "persistence_image_replays",
            None,
            format!("durable image has no file at {}", path.display()),
        )
    })?;
    tina_runtime::persistence::replay_journal_bytes(bytes).map_err(|error| {
        InvariantViolation::new(
            "persistence_image_replays",
            None,
            format!(
                "durable journal at {} did not replay: {error:?}",
                path.display()
            ),
        )
    })?;
    Ok(())
}

/// Returns true when the trace contains a visible bounded-pressure
/// rejection.
pub fn contains_visible_pressure(events: &[RuntimeEvent]) -> bool {
    events.iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::SendRejected {
                reason: SendRejectedReason::Full | SendRejectedReason::Closed,
                ..
            } | RuntimeEventKind::CallFailed {
                reason: CallError::TargetFull
                    | CallError::TargetClosed
                    | CallError::StorageFull
                    | CallError::StorageClosed,
                ..
            } | RuntimeEventKind::JournalAppendFailed {
                reason: CallError::StorageFull | CallError::StorageClosed,
                ..
            } | RuntimeEventKind::SnapshotCommitFailed {
                reason: CallError::StorageFull | CallError::StorageClosed,
                ..
            }
        )
    })
}

/// Compares two semantic projections and prints both labels on mismatch.
pub fn assert_projection_eq<T>(left_name: &str, left: &T, right_name: &str, right: &T)
where
    T: PartialEq + Debug,
{
    assert_eq!(
        left, right,
        "semantic projection mismatch between {left_name} and {right_name}"
    );
}
