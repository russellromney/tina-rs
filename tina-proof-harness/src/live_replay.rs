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
//! workload, the caller calls [`LiveTrace::snapshot`] to get a
//! [`TraceShape`] (event_count, `stable_trace_hash`) and can compare it
//! against a saved case via [`LiveTrace::compare_to`].
//!
//! Two rules the rung enforces:
//!
//! 1. The observer never blocks. It pushes into a `Mutex<Vec<_>>` and
//!    returns; if the user wants async drain, they pull events out.
//! 2. The snapshot computes the trace hash using the same
//!    [`tina_runtime::stable_trace_hash`] the sim side uses, so a
//!    sim-vs-live trace shape comparison is a byte-for-byte equal of
//!    the same hash function.

use std::sync::{Arc, Mutex};

use tina_runtime::{RuntimeEvent, TraceObserver, stable_trace_hash};
use tina_sim::dst::TraceShape;

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
        let len = self.events.lock().map(|v| v.len()).unwrap_or(0);
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
    pub fn snapshot(&self) -> TraceShape {
        let guard = self.events.lock().expect("live trace lock");
        TraceShape {
            event_count: guard.len(),
            trace_hash: stable_trace_hash(guard.iter()),
        }
    }

    /// Compare the live shape to a saved [`tina_sim::dst::ReplayCase`]
    /// without running the simulator side again. Returns `None` when
    /// the saved expected shape matches the live shape, otherwise
    /// returns a [`LiveReplayMismatch`] naming the diverging field(s).
    pub fn compare_to<Op>(
        &self,
        case: &tina_sim::dst::ReplayCase<Op>,
    ) -> Option<LiveReplayMismatch> {
        let shape = self.snapshot();
        if shape.event_count == case.expected_event_count
            && shape.trace_hash == case.expected_trace_hash
        {
            return None;
        }
        Some(LiveReplayMismatch {
            case_name: case.name,
            expected_event_count: case.expected_event_count,
            actual_event_count: shape.event_count,
            expected_trace_hash: case.expected_trace_hash,
            actual_trace_hash: shape.trace_hash,
        })
    }
}

/// Bundled "trace + observer" handle returned by
/// [`LiveTrace::install`]. Hold this in the test; pass `observer.clone()`
/// (or `&observer`) to the runtime builder.
pub struct LiveTraceHandle {
    pub trace: LiveTrace,
    pub observer: Arc<dyn TraceObserver>,
}

struct LiveTraceObserver {
    events: Arc<Mutex<Vec<RuntimeEvent>>>,
}

impl TraceObserver for LiveTraceObserver {
    fn on_event(&self, event: &RuntimeEvent) {
        if let Ok(mut guard) = self.events.lock() {
            guard.push(*event);
        }
    }
}

/// Why a live shape did not match a saved case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveReplayMismatch {
    pub case_name: &'static str,
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
            self.case_name,
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

#[cfg(test)]
mod tests {
    use super::*;
    use tina::{IsolateId, ShardId};
    use tina_runtime::{EventId, RuntimeEventKind};

    fn sample_event(id: u64) -> RuntimeEvent {
        RuntimeEvent::new(
            EventId::new(id),
            None,
            ShardId::new(0),
            IsolateId::new(1),
            RuntimeEventKind::HandlerStarted,
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
    fn compare_to_returns_none_on_match_and_mismatch_struct_on_drift() {
        use tina_sim::dst::ReplayCase;
        let trace = LiveTrace::new();
        let observer = trace.observer();
        observer.on_event(&sample_event(1));
        observer.on_event(&sample_event(2));
        let shape = trace.snapshot();
        let case = ReplayCase::<()>::new(
            "stub",
            7,
            tina_sim::dst::ReplayConfig::default(),
            "stub scenario",
            Vec::new(),
            "stub invariant",
        )
        .expecting(shape.event_count, shape.trace_hash);
        assert!(trace.compare_to(&case).is_none());

        let drifted = ReplayCase::<()>::new(
            "stub",
            7,
            tina_sim::dst::ReplayConfig::default(),
            "stub scenario",
            Vec::new(),
            "stub invariant",
        )
        .expecting(shape.event_count + 1, shape.trace_hash);
        let mismatch = trace.compare_to(&drifted).expect("mismatch");
        assert!(mismatch.count_diverged());
        assert!(!mismatch.hash_diverged());
    }
}
