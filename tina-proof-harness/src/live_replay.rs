//! Capture a live runtime trace and reduce it to a comparable shape.
//!
//! Tina already has the saved-case workflow: [`tina_sim::dst::ReplayCase`],
//! [`tina_sim::dst::observe_replay_case`], [`tina_sim::dst::assert_replay_case`],
//! [`tina_sim::dst::delete_shrink`], [`tina_sim::dst::TraceShape`]. The
//! missing rung is the live side: a specimen that runs against a real
//! `ThreadedRuntime` needs an easy way to snapshot the live trace shape
//! and compare it back to a sim-side case.
//!
//! [`LiveTrace`] is that rung. It is a thin `TraceObserver` that
//! collects [`RuntimeEvent`]s as the runtime emits them. After the
//! workload, the caller calls [`LiveTrace::snapshot_complete`] to get a
//! live [`TraceShape`] (event_count, `stable_trace_hash`) after proving no
//! upstream buffered observer dropped events. A live runtime trace is not
//! the same thing as a simulator trace. To make a live finding replayable,
//! materialize the important facts into a
//! [`tina_sim::dst::LiveReplayCapture`] and check it with
//! [`tina_sim::dst::check_captured_replay`].
//!
//! Two rules the rung enforces:
//!
//! 1. The observer never blocks. It pushes into a `Mutex<Vec<_>>` and
//!    returns; if the user wants async drain, they pull events out.
//! 2. The complete snapshot refuses to hash a lossy buffered capture.
//!    If you install `BufferedTraceObserver`, first call its `flush()` or
//!    `shutdown()` drain barrier and pass the resulting token through
//!    [`LiveTraceDrain::from_buffered`] to [`LiveTrace::snapshot_complete`] or
//!    [`LiveTrace::compare_live_shape_complete`]. Plain [`LiveTrace::snapshot`]
//!    remains available for diagnostics, not proof.
//! 3. The snapshot computes the trace hash using the same
//!    [`tina_runtime::stable_trace_hash`] the sim side uses. The hash is
//!    a fingerprint of this live trace, not a claim that the live event
//!    stream is byte-identical to a simulator run.

use std::sync::{Arc, Mutex};

use std::collections::BTreeSet;
use std::path::Path;
use tina_runtime::{
    BufferedTraceDrain, PressureSummary, RuntimeEvent, TraceObserver, stable_trace_hash,
};

use tina_sim::dst::{
    CaptureSource, LiveReplayCapture, LiveReplayCaptureBuildError, LiveReplayCaptureBuilder,
    LiveReplayFact, ReplayCase, ReplayConfig, SavedReplayCaseError, TraceProjection,
    TraceProjectionError, TraceShape, UnsupportedLiveFact,
};

/// Live trace collector.
///
/// Build with `LiveTrace::new()`. Pass the `Arc<dyn TraceObserver>` view
/// to `ThreadedRuntime::set_trace_observer` (via
/// `with_config_and_trace_observer` for `ThreadedRuntime`, or
/// `LocalSystemBuilder::trace_observer` for the local system).
///
/// `LiveTrace` is `Send + Sync` and cheap to `clone`: clones share the
/// same underlying event buffer.
#[derive(Clone, Default)]
pub struct LiveTrace {
    events: Arc<Mutex<Vec<RuntimeEvent>>>,
}

impl std::fmt::Debug for LiveTrace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let len = self.events.lock().expect("live trace lock poisoned").len();
        f.debug_struct("LiveTrace").field("len", &len).finish()
    }
}

impl LiveTrace {
    pub fn new() -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Build a typed observer handle suitable for
    /// `ThreadedRuntime::set_trace_observer`.
    pub fn observer(&self) -> Arc<dyn TraceObserver> {
        Arc::new(LiveTraceObserver {
            events: Arc::clone(&self.events),
        })
    }

    /// Returns a typed handle bundling the live trace and its observer.
    /// Convenience for callers that want one value to hold and one to
    /// pass to the runtime constructor.
    pub fn install(&self) -> LiveTraceHandle {
        LiveTraceHandle {
            trace: self.clone(),
            observer: self.observer(),
        }
    }

    /// Number of events captured so far.
    pub fn len(&self) -> usize {
        self.events.lock().expect("live trace lock").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Take a copy of the captured events.
    pub fn events(&self) -> Vec<RuntimeEvent> {
        self.events.lock().expect("live trace lock").clone()
    }

    /// Snapshot the current shape: (event_count, stable_trace_hash).
    ///
    /// Events are ordered by `(shard, event_id)` before hashing. Event ids
    /// are per-shard-local and deterministic, so for a **single-shard**
    /// trace the hash is stable across runs of the same workload.
    ///
    /// This is a diagnostic snapshot, not proof. For a multishard trace the
    /// hash is **not** stable: a free-running multishard runtime interleaves
    /// each shard's inbound cross-shard deliveries with its local work by
    /// wall-clock timing, so the per-shard event sequence differs between
    /// runs. For proof use [`Self::snapshot_complete`], which fails closed
    /// on a multishard or lossy trace.
    pub fn snapshot(&self) -> TraceShape {
        let guard = self.events.lock().expect("live trace lock");
        let mut events = guard.clone();
        // Order by `(shard, event_id)`. Event ids are per-shard-local, so a
        // global id sort would collide across shards and depend on which
        // worker thread won the capture race. The per-shard key is the same
        // logical order on every run, so the hash is stable across runs.
        events.sort_by_key(|event| (event.shard(), event.id()));
        TraceShape {
            event_count: events.len(),
            trace_hash: stable_trace_hash(events.iter()),
        }
    }

    /// Distinct shard ids in the captured trace, ascending.
    pub fn shards(&self) -> Vec<tina::ShardId> {
        let guard = self.events.lock().expect("live trace lock");
        let mut shards: Vec<_> = guard.iter().map(|event| event.shard()).collect();
        shards.sort_unstable();
        shards.dedup();
        shards
    }

    /// Snapshot a live trace as proof material.
    ///
    /// Fails closed in two cases the plain [`Self::snapshot`] hides:
    ///
    /// 1. The upstream capture path dropped events (pass
    ///    `LiveTraceDrain::from_buffered(buffered.flush()?)`; pass
    ///    `LiveTraceDrain::direct()` when `LiveTrace::observer()` is installed
    ///    directly).
    /// 2. The trace spans more than one shard. A free-running multishard
    ///    runtime interleaves each shard's inbound cross-shard deliveries
    ///    with its local handler work by real wall-clock timing, so the
    ///    per-shard event *sequence* — not just the id assignment — differs
    ///    between runs. The trace hash is therefore not a stable proof for
    ///    live multishard. Capture per shard, or use the deterministic
    ///    simulator for a multishard replay proof.
    pub fn snapshot_complete(
        &self,
        drain: LiveTraceDrain,
    ) -> Result<TraceShape, LiveTraceProofError> {
        if drain.upstream_dropped_events != 0 {
            return Err(LiveTraceProofError::Lossy(LiveTraceLoss {
                dropped_events: drain.upstream_dropped_events,
            }));
        }
        let shards = self.shards();
        if shards.len() > 1 {
            return Err(LiveTraceProofError::Multishard {
                shards: shards.into_iter().map(|shard| shard.get()).collect(),
            });
        }
        Ok(self.snapshot())
    }

    /// Pressure summary for the captured trace.
    ///
    /// Counts `SendRejected`, `CallCompletionRejected`, and
    /// `CallReplyRejected` events by reason — the same vocabulary the
    /// runtime uses, so the live-side pressure surface is the same
    /// one a specimen reads after `runtime.shutdown()`. Use this to
    /// fail a live test closed when pressure escaped the typed
    /// surface (e.g., `summary.any_full()` should be `false` on a
    /// clean soak).
    pub fn pressure_summary(&self) -> PressureSummary {
        let guard = self.events.lock().expect("live trace lock");
        PressureSummary::from_events(guard.iter())
    }

    /// Compare the current live trace shape to a previously saved live
    /// shape. This is useful for same-runtime regression checks.
    ///
    /// Do not use this for live-to-simulator replay. Live replay needs a
    /// materialized capture (`LiveReplayCapture`) because simulator traces
    /// and live runtime traces are not expected to have the same full event
    /// stream.
    pub fn compare_live_shape(
        &self,
        expected: TraceShape,
        label: &'static str,
    ) -> Option<LiveReplayMismatch> {
        let actual = self.snapshot();
        if actual == expected {
            return None;
        }
        Some(LiveReplayMismatch {
            label,
            expected_event_count: expected.event_count,
            actual_event_count: actual.event_count,
            expected_trace_hash: expected.trace_hash,
            actual_trace_hash: actual.trace_hash,
        })
    }

    /// Compare a saved live shape as proof material.
    ///
    /// Fails closed for a lossy capture or a multishard trace, matching
    /// [`Self::snapshot_complete`]: a live multishard trace hash is not a
    /// stable regression baseline.
    pub fn compare_live_shape_complete(
        &self,
        expected: TraceShape,
        label: &'static str,
        drain: LiveTraceDrain,
    ) -> Result<Option<LiveReplayMismatch>, LiveTraceProofError> {
        let actual = self.snapshot_complete(drain)?;
        if actual == expected {
            return Ok(None);
        }
        Ok(Some(LiveReplayMismatch {
            label,
            expected_event_count: expected.event_count,
            actual_event_count: actual.event_count,
            expected_trace_hash: expected.trace_hash,
            actual_trace_hash: actual.trace_hash,
        }))
    }
}

/// Bundled "trace + observer" handle returned by
/// [`LiveTrace::install`]. Hold this in the test; pass `observer.clone()`
/// (or `&observer`) to the runtime builder.
pub struct LiveTraceHandle {
    pub trace: LiveTrace,
    pub observer: Arc<dyn TraceObserver>,
}

/// Proof-grade evidence that the live trace capture path has been drained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveTraceDrain {
    upstream_dropped_events: u64,
}

impl LiveTraceDrain {
    /// The trace observer is synchronous and has no upstream buffer to drain.
    pub const fn direct() -> Self {
        Self {
            upstream_dropped_events: 0,
        }
    }

    /// Builds proof evidence from a completed buffered-observer drain barrier.
    pub const fn from_buffered(drain: BufferedTraceDrain) -> Self {
        Self {
            upstream_dropped_events: drain.dropped_events(),
        }
    }

    #[cfg(test)]
    const fn lossy_for_test(dropped_events: u64) -> Self {
        Self {
            upstream_dropped_events: dropped_events,
        }
    }
}

/// The live trace capture path lost events before the proof snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveTraceLoss {
    pub dropped_events: u64,
}

impl std::fmt::Display for LiveTraceLoss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "live trace capture dropped {} event(s); snapshot is diagnostic only",
            self.dropped_events
        )
    }
}

impl std::error::Error for LiveTraceLoss {}

/// Why a proof-grade snapshot refused the captured trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveTraceProofError {
    /// The upstream capture path dropped events.
    Lossy(LiveTraceLoss),
    /// The trace spans more than one shard. A live multishard trace hash is
    /// not a stable proof (the per-shard event sequence is timing-ordered).
    Multishard { shards: Vec<u32> },
}

impl std::fmt::Display for LiveTraceProofError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lossy(loss) => write!(f, "{loss}"),
            Self::Multishard { shards } => write!(
                f,
                "live trace spans {} shards {:?}; a multishard live trace hash is not a stable \
                 proof — capture per shard or use the simulator for a multishard replay proof",
                shards.len(),
                shards,
            ),
        }
    }
}

impl std::error::Error for LiveTraceProofError {}

impl From<LiveTraceLoss> for LiveTraceProofError {
    fn from(loss: LiveTraceLoss) -> Self {
        Self::Lossy(loss)
    }
}

struct LiveTraceObserver {
    events: Arc<Mutex<Vec<RuntimeEvent>>>,
}

impl TraceObserver for LiveTraceObserver {
    fn on_event(&self, event: &RuntimeEvent) {
        self.events
            .lock()
            .expect("live trace lock poisoned")
            .push(*event);
    }
}

/// Why a live shape did not match a saved case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveReplayMismatch {
    pub label: &'static str,
    pub expected_event_count: usize,
    pub actual_event_count: usize,
    pub expected_trace_hash: u64,
    pub actual_trace_hash: u64,
}

impl LiveReplayMismatch {
    pub const fn count_diverged(&self) -> bool {
        self.expected_event_count != self.actual_event_count
    }

    pub const fn hash_diverged(&self) -> bool {
        self.expected_trace_hash != self.actual_trace_hash
    }
}

impl std::fmt::Display for LiveReplayMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "live trace shape for `{}` diverged from saved case: \
             events expected {} got {}{}; hash expected 0x{:016x} got 0x{:016x}{}",
            self.label,
            self.expected_event_count,
            self.actual_event_count,
            if self.count_diverged() {
                " (diverged)"
            } else {
                ""
            },
            self.expected_trace_hash,
            self.actual_trace_hash,
            if self.hash_diverged() {
                " (diverged)"
            } else {
                ""
            },
        )
    }
}

/// User-facing "capture this run" handle.
///
/// Create this before constructing the runtime, pass [`observer`](Self::observer)
/// into the runtime builder, run the workload, then call [`finish`](Self::finish)
/// with explicit replay material. The helper only wires the live observer and
/// fills capture metadata from observed events; it does not guess history,
/// mailbox roles, invariants, or replay facts.
#[derive(Debug, Clone)]
pub struct RunCapture {
    name: &'static str,
    trace: LiveTrace,
}

impl RunCapture {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            trace: LiveTrace::new(),
        }
    }

    pub fn observer(&self) -> Arc<dyn TraceObserver> {
        self.trace.observer()
    }

    pub fn trace(&self) -> &LiveTrace {
        &self.trace
    }

    pub fn finish<Op>(
        self,
        inputs: RunCaptureInputs<Op>,
    ) -> Result<LiveReplayCapture<Op>, RunCaptureFinishError> {
        let expected_roles: BTreeSet<_> = inputs.topology_roles.iter().copied().collect();
        let config_roles: BTreeSet<_> = inputs.config.mailboxes.keys().copied().collect();
        if expected_roles != config_roles {
            return Err(RunCaptureFinishError::TopologyRolesMismatch {
                expected: expected_roles.into_iter().collect(),
                config: config_roles.into_iter().collect(),
            });
        }
        let observed = self
            .trace
            .snapshot_complete(inputs.trace_drain)
            .map_err(RunCaptureFinishError::TraceNotProvable)?;
        if observed != inputs.expected {
            return Err(RunCaptureFinishError::ExpectedShapeMismatch {
                expected: inputs.expected,
                observed,
            });
        }

        let mut builder = LiveReplayCaptureBuilder::new(self.name)
            .with_seed(inputs.seed)
            .with_config(inputs.config)
            .with_scenario(inputs.scenario)
            .with_history(inputs.history)
            .with_invariant(inputs.invariant)
            .with_source(inputs.source)
            .with_source_metadata(inputs.source_metadata)
            .with_projection(inputs.projection)
            .with_trace(&self.trace.events());
        for fact in inputs.live_facts {
            builder = builder.with_fact(fact);
        }
        for fact in inputs.unsupported_facts {
            builder = builder.with_unsupported_fact(fact.fact, fact.reason);
        }
        builder.finish().map_err(RunCaptureFinishError::Build)
    }
}

/// Explicit replay material required to finish a [`RunCapture`].
#[derive(Debug, Clone)]
pub struct RunCaptureInputs<Op> {
    pub seed: u64,
    pub config: ReplayConfig,
    pub topology_roles: Vec<&'static str>,
    pub scenario: &'static str,
    pub history: Vec<Op>,
    pub invariant: &'static str,
    pub expected: TraceShape,
    pub source: &'static str,
    pub source_metadata: CaptureSource,
    pub projection: TraceProjection,
    pub trace_drain: LiveTraceDrain,
    pub live_facts: Vec<LiveReplayFact>,
    pub unsupported_facts: Vec<UnsupportedLiveFact>,
}

#[derive(Debug)]
pub enum RunCaptureFinishError {
    /// The captured trace cannot back a proof: it lost events or it spans
    /// more than one shard.
    TraceNotProvable(LiveTraceProofError),
    ExpectedShapeMismatch {
        expected: TraceShape,
        observed: TraceShape,
    },
    TopologyRolesMismatch {
        expected: Vec<&'static str>,
        config: Vec<&'static str>,
    },
    Build(LiveReplayCaptureBuildError),
}

impl std::fmt::Display for RunCaptureFinishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TraceNotProvable(err) => write!(f, "{err}"),
            Self::ExpectedShapeMismatch { expected, observed } => write!(
                f,
                "capture expected trace shape events={} hash=0x{:016x}, observed events={} hash=0x{:016x}",
                expected.event_count,
                expected.trace_hash,
                observed.event_count,
                observed.trace_hash
            ),
            Self::TopologyRolesMismatch { expected, config } => write!(
                f,
                "capture topology roles {:?} do not match ReplayConfig mailbox roles {:?}",
                expected, config
            ),
            Self::Build(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for RunCaptureFinishError {}

pub fn capture_run<Op>(name: &'static str) -> LiveReplayCaptureBuilder<Op> {
    tina_sim::dst::capture_live_run(name)
}

pub fn save_bug<Op, F>(
    path: impl AsRef<Path>,
    capture: &LiveReplayCapture<Op>,
    encode_op: F,
) -> Result<(), SavedReplayCaseError>
where
    F: FnMut(&Op) -> String,
{
    tina_sim::dst::write_saved_replay_case(path, capture, encode_op)
}

pub fn replay_bug<Op, Output, Runner>(
    capture: &LiveReplayCapture<Op>,
    candidate: &ReplayCase<Op>,
    runner: Runner,
) -> tina_sim::dst::LiveReplayReport<Output>
where
    Op: Clone + PartialEq + std::fmt::Debug,
    Runner: FnMut(
        &ReplayCase<Op>,
    ) -> Result<tina_sim::dst::LiveReplayReport<Output>, TraceProjectionError>,
{
    tina_sim::dst::assert_captured_replay(capture, candidate, runner)
}

pub fn shrink_bug<Op, Output, Runner, StillFails>(
    capture: &LiveReplayCapture<Op>,
    config: tina_sim::dst::ShrinkConfig,
    reason: impl Into<String>,
    runner: Runner,
    still_fails: StillFails,
) -> Result<tina_sim::dst::ShrinkCapturedReport<Op, Output>, tina_sim::dst::ShrinkCapturedReplayError>
where
    Op: Clone,
    Runner: FnMut(
        &ReplayCase<Op>,
    ) -> Result<tina_sim::dst::LiveReplayReport<Output>, TraceProjectionError>,
    StillFails: FnMut(&tina_sim::dst::LiveReplayReport<Output>) -> bool,
{
    tina_sim::dst::shrink_captured_replay(capture, config, reason, runner, still_fails)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tina::{AddressGeneration, IsolateId, ShardId};
    use tina_runtime::{EventId, RuntimeEventKind, SendRejectedReason};

    fn sample_event(id: u64) -> RuntimeEvent {
        RuntimeEvent::new(
            EventId::new(id),
            None,
            ShardId::new(0),
            IsolateId::new(1),
            RuntimeEventKind::HandlerStarted,
        )
    }

    fn send_rejected_full_event(id: u64) -> RuntimeEvent {
        RuntimeEvent::new(
            EventId::new(id),
            None,
            ShardId::new(0),
            IsolateId::new(1),
            RuntimeEventKind::SendRejected {
                target_shard: ShardId::new(0),
                target_isolate: IsolateId::new(2),
                target_generation: AddressGeneration::new(0),
                reason: SendRejectedReason::Full,
            },
        )
    }

    #[test]
    fn observer_collects_events() {
        let trace = LiveTrace::new();
        let observer = trace.observer();
        observer.on_event(&sample_event(1));
        observer.on_event(&sample_event(2));
        observer.on_event(&sample_event(3));
        let shape = trace.snapshot();
        assert_eq!(shape.event_count, 3);
        assert_ne!(shape.trace_hash, 0);
        assert_eq!(trace.snapshot_complete(LiveTraceDrain::direct()), Ok(shape));
    }

    #[test]
    fn complete_snapshot_rejects_lossy_buffered_capture() {
        let trace = LiveTrace::new();
        let observer = trace.observer();
        observer.on_event(&sample_event(1));

        let err = trace
            .snapshot_complete(LiveTraceDrain::lossy_for_test(1))
            .expect_err("proof snapshot must reject dropped buffered events");
        assert_eq!(
            err,
            LiveTraceProofError::Lossy(LiveTraceLoss { dropped_events: 1 })
        );
    }

    #[test]
    fn complete_snapshot_rejects_multishard_trace() {
        // A live multishard trace hash is not a stable proof; the complete
        // snapshot must fail closed rather than return a flapping hash.
        let trace = LiveTrace::new();
        let observer = trace.observer();
        observer.on_event(&sample_event_on(11, 1));
        observer.on_event(&sample_event_on(22, 1));

        let err = trace
            .snapshot_complete(LiveTraceDrain::direct())
            .expect_err("proof snapshot must reject a multishard trace");
        assert!(
            matches!(err, LiveTraceProofError::Multishard { ref shards } if shards == &[11, 22]),
            "expected multishard rejection, got {err:?}"
        );

        // The plain diagnostic snapshot still works (not proof).
        assert_eq!(trace.snapshot().event_count, 2);
    }

    fn sample_event_on(shard: u32, id: u64) -> RuntimeEvent {
        RuntimeEvent::new(
            EventId::new(id),
            None,
            ShardId::new(shard),
            IsolateId::new(1),
            RuntimeEventKind::HandlerStarted,
        )
    }

    #[test]
    fn snapshot_hash_ignores_capture_arrival_order_single_shard() {
        // Within one shard the event ids are contiguous; the `(shard, id)`
        // sort recovers the same order no matter what order events arrived.
        let ordered = LiveTrace::new();
        let ordered_observer = ordered.observer();
        ordered_observer.on_event(&sample_event(1));
        ordered_observer.on_event(&sample_event(2));
        ordered_observer.on_event(&sample_event(3));

        let arrival_scrambled = LiveTrace::new();
        let scrambled_observer = arrival_scrambled.observer();
        scrambled_observer.on_event(&sample_event(3));
        scrambled_observer.on_event(&sample_event(1));
        scrambled_observer.on_event(&sample_event(2));

        assert_eq!(ordered.snapshot(), arrival_scrambled.snapshot());
    }

    #[test]
    fn snapshot_hash_ignores_cross_shard_arrival_order() {
        // Event ids are per-shard-local, so two shards both use ids 1,2.
        // The `(shard, id)` sort key keeps each shard's stream together and
        // in local order, so the hash does not depend on which shard's
        // worker thread captured first.
        let shard_a_first = LiveTrace::new();
        let a_first = shard_a_first.observer();
        a_first.on_event(&sample_event_on(11, 1));
        a_first.on_event(&sample_event_on(11, 2));
        a_first.on_event(&sample_event_on(22, 1));
        a_first.on_event(&sample_event_on(22, 2));

        let interleaved = LiveTrace::new();
        let mixed = interleaved.observer();
        mixed.on_event(&sample_event_on(22, 1));
        mixed.on_event(&sample_event_on(11, 1));
        mixed.on_event(&sample_event_on(22, 2));
        mixed.on_event(&sample_event_on(11, 2));

        assert_eq!(shard_a_first.snapshot(), interleaved.snapshot());
    }

    #[test]
    fn install_returns_handle_with_live_view() {
        let trace = LiveTrace::new();
        let handle = trace.install();
        handle.observer.on_event(&sample_event(42));
        assert_eq!(handle.trace.len(), 1);
        // The original `trace` shares the buffer.
        assert_eq!(trace.len(), 1);
    }

    #[test]
    fn compare_live_shape_returns_none_on_match_and_mismatch_struct_on_drift() {
        let trace = LiveTrace::new();
        let observer = trace.observer();
        observer.on_event(&sample_event(1));
        observer.on_event(&sample_event(2));
        let shape = trace.snapshot();
        assert!(trace.compare_live_shape(shape, "stub").is_none());

        let drifted = TraceShape {
            event_count: shape.event_count + 1,
            trace_hash: shape.trace_hash,
        };
        let mismatch = trace.compare_live_shape(drifted, "stub").expect("mismatch");
        assert!(mismatch.count_diverged());
        assert!(!mismatch.hash_diverged());

        let err = trace
            .compare_live_shape_complete(shape, "stub", LiveTraceDrain::lossy_for_test(2))
            .expect_err("lossy capture must fail before comparing hashes");
        assert_eq!(
            err,
            LiveTraceProofError::Lossy(LiveTraceLoss { dropped_events: 2 })
        );
        assert_eq!(
            trace.compare_live_shape_complete(shape, "stub", LiveTraceDrain::direct()),
            Ok(None)
        );
    }

    #[test]
    fn pressure_summary_counts_send_rejected_full() {
        let trace = LiveTrace::new();
        let observer = trace.observer();
        observer.on_event(&sample_event(1));
        observer.on_event(&send_rejected_full_event(2));
        observer.on_event(&send_rejected_full_event(3));
        observer.on_event(&sample_event(4));
        let pressure = trace.pressure_summary();
        assert_eq!(pressure.send_rejected_full, 2);
        assert_eq!(pressure.send_rejected_closed, 0);
        assert!(pressure.non_zero());
        assert!(pressure.any_full());
    }

    #[test]
    fn pressure_summary_clean_on_handler_only_trace() {
        let trace = LiveTrace::new();
        let observer = trace.observer();
        observer.on_event(&sample_event(1));
        observer.on_event(&sample_event(2));
        let pressure = trace.pressure_summary();
        assert!(!pressure.non_zero());
        assert!(!pressure.any_full());
    }

    #[test]
    fn run_capture_finishes_only_with_explicit_matching_replay_inputs() {
        let capture = RunCapture::new("copied_path");
        let observer = capture.observer();
        observer.on_event(&sample_event(1));
        observer.on_event(&sample_event(2));
        let expected = capture.trace().snapshot();
        let config = ReplayConfig::new().with_mailbox("svc", 4);

        let finished = capture
            .finish(RunCaptureInputs {
                seed: 7,
                config,
                topology_roles: vec!["svc"],
                scenario: "request reaches service",
                history: vec!["op"],
                invariant: "request completed",
                expected,
                source: "unit test",
                source_metadata: CaptureSource::new("unit test").runtime_kind("local-system"),
                projection: TraceProjection::Exact,
                trace_drain: LiveTraceDrain::direct(),
                live_facts: Vec::new(),
                unsupported_facts: Vec::new(),
            })
            .unwrap();

        assert_eq!(finished.name, "copied_path");
        assert_eq!(finished.summary().history_len, 1);
        assert_eq!(finished.expected, expected);
    }

    #[test]
    fn run_capture_rejects_topology_role_drift_and_lossy_trace() {
        let capture = RunCapture::new("bad_roles");
        capture.observer().on_event(&sample_event(1));
        let expected = capture.trace().snapshot();
        let err = capture
            .finish::<&'static str>(RunCaptureInputs {
                seed: 1,
                config: ReplayConfig::new().with_mailbox("actual", 2),
                topology_roles: vec!["expected"],
                scenario: "drift",
                history: vec!["op"],
                invariant: "same roles",
                expected,
                source: "unit test",
                source_metadata: CaptureSource::new("unit test"),
                projection: TraceProjection::Exact,
                trace_drain: LiveTraceDrain::direct(),
                live_facts: Vec::new(),
                unsupported_facts: Vec::new(),
            })
            .unwrap_err();
        assert!(matches!(
            err,
            RunCaptureFinishError::TopologyRolesMismatch { .. }
        ));

        let lossy = RunCapture::new("lossy");
        lossy.observer().on_event(&sample_event(1));
        let expected = lossy.trace().snapshot();
        let err = lossy
            .finish::<&'static str>(RunCaptureInputs {
                seed: 1,
                config: ReplayConfig::new().with_mailbox("svc", 2),
                topology_roles: vec!["svc"],
                scenario: "lossy",
                history: vec!["op"],
                invariant: "complete trace",
                expected,
                source: "unit test",
                source_metadata: CaptureSource::new("unit test"),
                projection: TraceProjection::Exact,
                trace_drain: LiveTraceDrain::lossy_for_test(1),
                live_facts: Vec::new(),
                unsupported_facts: Vec::new(),
            })
            .unwrap_err();
        assert!(matches!(
            err,
            RunCaptureFinishError::TraceNotProvable(LiveTraceProofError::Lossy(_))
        ));
    }

    #[test]
    fn run_capture_rejects_expected_shape_drift() {
        let capture = RunCapture::new("shape_drift");
        capture.observer().on_event(&sample_event(1));
        let mut expected = capture.trace().snapshot();
        expected.event_count += 1;
        let err = capture
            .finish::<&'static str>(RunCaptureInputs {
                seed: 1,
                config: ReplayConfig::new().with_mailbox("svc", 2),
                topology_roles: vec!["svc"],
                scenario: "shape drift",
                history: vec!["op"],
                invariant: "same shape",
                expected,
                source: "unit test",
                source_metadata: CaptureSource::new("unit test"),
                projection: TraceProjection::Exact,
                trace_drain: LiveTraceDrain::direct(),
                live_facts: Vec::new(),
                unsupported_facts: Vec::new(),
            })
            .unwrap_err();
        assert!(matches!(
            err,
            RunCaptureFinishError::ExpectedShapeMismatch { .. }
        ));
    }

    #[test]
    fn bug_workflow_wrappers_delegate_to_saved_replay_primitives() {
        let capture = RunCapture::new("bug_wrappers");
        let observer = capture.observer();
        observer.on_event(&sample_event(1));
        let events = capture.trace().events();
        let expected = capture.trace().snapshot();
        let finished = capture
            .finish(RunCaptureInputs {
                seed: 11,
                config: ReplayConfig::new().with_mailbox("svc", 1),
                topology_roles: vec!["svc"],
                scenario: "wrapper smoke",
                history: vec!["op"],
                invariant: "replays",
                expected,
                source: "unit test",
                source_metadata: CaptureSource::new("unit test"),
                projection: TraceProjection::Exact,
                trace_drain: LiveTraceDrain::direct(),
                live_facts: Vec::new(),
                unsupported_facts: Vec::new(),
            })
            .unwrap();

        let path = std::env::temp_dir().join(format!(
            "tina-proof-harness-bug-wrapper-{}.case",
            std::process::id()
        ));
        save_bug(&path, &finished, |op| op.to_string()).unwrap();
        assert!(path.exists());
        let _ = std::fs::remove_file(&path);

        let case = finished.to_replay_case();
        let report = replay_bug(&finished, &case, |case| {
            tina_sim::dst::LiveReplayReport::from_case_and_events(
                case,
                &events,
                "ok",
                TraceProjection::Exact,
            )
        });
        assert_eq!(report.replay.event_count, expected.event_count);

        let shrunk = shrink_bug(
            &finished,
            tina_sim::dst::ShrinkConfig { max_attempts: 0 },
            "kept failing",
            |case| {
                tina_sim::dst::LiveReplayReport::from_case_and_events(
                    case,
                    &events,
                    "ok",
                    TraceProjection::Exact,
                )
            },
            |_| true,
        )
        .unwrap();
        assert_eq!(shrunk.capture().history.len(), 1);
    }
}
