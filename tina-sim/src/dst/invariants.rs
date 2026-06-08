//! Trace-invariant vocabulary for DST.
//!
//! Owns `InvariantViolation`, the `InvariantSuite` bundle, the
//! per-invariant check functions, the `contains_visible_pressure`
//! helper, and the `assert_projection_eq` projection-equality helper.

use std::fmt::Debug;

use tina::{AddressGeneration, IsolateId, ShardId};
use tina_runtime::{
    CallError, CallId, CauseId, EventId, RuntimeEvent, RuntimeEventKind, SendRejectedReason,
};

use crate::DurableImage;

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

/// Checks that event ids strictly increase within each shard's id stream.
///
/// This is the honest universal form. Live multishard ids are per-shard
/// local (each shard counts from 1), and sim multishard ids come from one
/// global counter interleaved across shards — so a *merged* trace is never
/// globally `+1` contiguous and is not even per-shard `+1` contiguous in
/// the sim case. What holds in every case is that each shard emits its own
/// events in strictly increasing id order. A non-increasing id within one
/// shard means the trace was reordered or an event was duplicated.
pub fn events_are_monotonic(events: &[RuntimeEvent]) -> Result<(), InvariantViolation> {
    use std::collections::BTreeMap;
    let mut previous: BTreeMap<ShardId, u64> = BTreeMap::new();
    for event in events {
        if let Some(&prev) = previous.get(&event.shard()) {
            if event.id().get() <= prev {
                return Err(InvariantViolation::new(
                    "events_are_monotonic",
                    Some(event.id()),
                    format!(
                        "event id {} did not increase past {} on shard {}",
                        event.id().get(),
                        prev,
                        event.shard().get()
                    ),
                ));
            }
        }
        previous.insert(event.shard(), event.id().get());
    }
    Ok(())
}

/// Checks that every cause points at an earlier event in the trace.
///
/// A same-shard cause must point backward in that shard's local id stream.
/// A cross-shard cause carries the *originating* shard's per-shard id, which
/// can be numerically >= the caused event's local id and lives in another
/// shard's stream; it is sound as long as it names an event present in the
/// merged trace.
pub fn causes_point_backward(events: &[RuntimeEvent]) -> Result<(), InvariantViolation> {
    for event in events {
        let Some(cause) = event.cause() else {
            continue;
        };
        let same_shard_cause = events.iter().find(|candidate| {
            candidate.shard() == event.shard() && candidate.id() == cause.event()
        });
        if let Some(same_shard_cause) = same_shard_cause {
            // Same shard: the cause must be strictly earlier locally.
            if same_shard_cause.id() >= event.id() {
                return Err(InvariantViolation::new(
                    "causes_point_backward",
                    Some(event.id()),
                    format!("same-shard cause {:?} does not point backward", cause),
                ));
            }
            continue;
        }
        // No same-shard match: accept a cross-shard cause that exists
        // anywhere in the merged trace; reject a cause that names nothing.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn event(shard: u32, id: u64, cause: Option<u64>) -> RuntimeEvent {
        RuntimeEvent::new(
            EventId::new(id),
            cause.map(|c| CauseId::new(EventId::new(c))),
            ShardId::new(shard),
            IsolateId::new(1),
            RuntimeEventKind::HandlerStarted,
        )
    }

    #[test]
    fn monotonic_holds_per_shard_on_multishard_trace() {
        // Live multishard ids are per-shard-local: each shard counts from 1,
        // so a merged trace is not globally contiguous. The invariant must
        // check monotonicity per shard, not false-fail on the cross-shard
        // id reset.
        let trace = [
            event(11, 1, None),
            event(11, 2, None),
            event(22, 1, None),
            event(22, 2, None),
        ];
        events_are_monotonic(&trace).expect("per-shard monotonic must pass");
    }

    #[test]
    fn monotonic_holds_on_non_contiguous_per_shard_ids() {
        // Sim multishard ids come from a global counter interleaved across
        // shards, so one shard's stream is increasing but not `+1`. Strictly
        // increasing must still pass.
        let trace = [
            event(11, 1, None),
            event(22, 2, None),
            event(11, 3, None),
            event(22, 4, None),
        ];
        events_are_monotonic(&trace).expect("non-contiguous increasing ids must pass");
    }

    #[test]
    fn monotonic_rejects_non_increasing_id_within_shard() {
        let trace = [event(11, 3, None), event(11, 2, None)];
        let violation = events_are_monotonic(&trace)
            .expect_err("a non-increasing id inside one shard's stream must fail");
        assert_eq!(violation.invariant(), "events_are_monotonic");
    }

    #[test]
    fn causes_point_backward_accepts_cross_shard_cause() {
        // A cross-shard cause carries the originating shard's per-shard id,
        // which can be numerically >= the caused event's id and lives in a
        // different shard's id stream. It must be accepted as long as it
        // exists in the merged trace.
        let trace = [
            // shard 11 emits the cause (id 5).
            event(11, 5, None),
            // shard 22's caused event has a smaller local id (1) but cites
            // the cross-shard cause id 5.
            event(22, 1, Some(5)),
        ];
        causes_point_backward(&trace).expect("cross-shard cause must be accepted");
    }

    #[test]
    fn causes_point_backward_rejects_same_shard_forward_cause() {
        // Within one shard a cause must still point at an earlier local id.
        let trace = [event(11, 1, Some(2)), event(11, 2, None)];
        let violation = causes_point_backward(&trace)
            .expect_err("a same-shard forward cause must fail");
        assert_eq!(violation.invariant(), "causes_point_backward");
    }

    #[test]
    fn causes_point_backward_rejects_missing_cause() {
        let trace = [event(11, 1, Some(99))];
        let violation = causes_point_backward(&trace)
            .expect_err("a cause pointing at no event must fail");
        assert_eq!(violation.invariant(), "causes_point_backward");
    }
}
