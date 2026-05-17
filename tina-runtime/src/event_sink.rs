//! Bounded event/log sink for runtime/service facts.
//!
//! A `BoundedEventSink` is a `VecDeque` with a cap, a drop policy,
//! and counters. Cheap to clone (`Arc`-backed). Senders never block:
//! when the sink is full, a drop policy chooses which item to lose
//! and a counter is bumped.
//!
//! This is the only event/log primitive offered by `tina-runtime`.
//! There is no "observability queue" elsewhere. If a system needs
//! more, it builds it from this.
//!
//! ```ignore
//! use tina_runtime::{BoundedEventSink, DropPolicy};
//!
//! let sink = BoundedEventSink::<String>::new("svc.events", 2, DropPolicy::DropOldest);
//! sink.push("a".into());
//! sink.push("b".into());
//! sink.push("c".into()); // drops "a"
//! let drained = sink.drain_snapshot();
//! assert_eq!(drained.events, vec!["b".to_string(), "c".to_string()]);
//! assert_eq!(drained.report.dropped, 1);
//! ```

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tina::capacity::{CapacityMode, CapacitySurfaceReport};

/// What to do with one event when the sink is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropPolicy {
    /// Drop the oldest queued event to make room. Use when *recent*
    /// matters more than *complete*.
    DropOldest,
    /// Drop the new event. Use when *first signs* matter more.
    DropNewest,
}

impl DropPolicy {
    /// Short label for one-line reports.
    pub fn label(self) -> &'static str {
        match self {
            Self::DropOldest => "drop_oldest",
            Self::DropNewest => "drop_newest",
        }
    }
}

/// Snapshot of a [`BoundedEventSink`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventSinkReport {
    /// Stable name, used in discovery lines.
    pub name: String,
    /// Configured cap.
    pub cap: usize,
    /// Drop policy.
    pub policy: DropPolicy,
    /// Live event count.
    pub len: usize,
    /// Highest `len` ever observed.
    pub high_water: usize,
    /// Cumulative dropped events (any policy).
    pub dropped: u64,
    /// Cumulative dropped events because the sink was full and policy was [`DropPolicy::DropOldest`].
    pub dropped_oldest: u64,
    /// Cumulative dropped events because the sink was full and policy was [`DropPolicy::DropNewest`].
    pub dropped_newest: u64,
    /// Cumulative accepted events (including dropped-old replacements).
    pub accepted: u64,
}

impl EventSinkReport {
    /// True iff the sink never dropped an event.
    pub fn no_drops(&self) -> bool {
        self.dropped == 0
    }
}

/// Result of [`BoundedEventSink::drain_snapshot`].
#[derive(Debug, Clone)]
pub struct EventSinkDrain<T> {
    /// Events drained in FIFO order (oldest first).
    pub events: Vec<T>,
    /// Snapshot at drain time.
    pub report: EventSinkReport,
}

/// Bounded event/log sink. Shared by clone.
#[derive(Debug)]
pub struct BoundedEventSink<T> {
    inner: Arc<EventSinkInner<T>>,
}

impl<T> Clone for BoundedEventSink<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[derive(Debug)]
struct EventSinkInner<T> {
    name: String,
    cap: usize,
    policy: DropPolicy,
    queue: Mutex<VecDeque<T>>,
    high_water: AtomicUsize,
    dropped: AtomicU64,
    dropped_oldest: AtomicU64,
    dropped_newest: AtomicU64,
    accepted: AtomicU64,
}

impl<T> BoundedEventSink<T> {
    /// Build a sink with `cap` slots and the given drop policy.
    ///
    /// The name is used in discovery lines and the surface report.
    pub fn new(name: impl Into<String>, cap: usize, policy: DropPolicy) -> Self {
        Self {
            inner: Arc::new(EventSinkInner {
                name: name.into(),
                cap,
                policy,
                queue: Mutex::new(VecDeque::with_capacity(cap)),
                high_water: AtomicUsize::new(0),
                dropped: AtomicU64::new(0),
                dropped_oldest: AtomicU64::new(0),
                dropped_newest: AtomicU64::new(0),
                accepted: AtomicU64::new(0),
            }),
        }
    }

    /// Sink name.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Cap.
    pub fn cap(&self) -> usize {
        self.inner.cap
    }

    /// Drop policy.
    pub fn policy(&self) -> DropPolicy {
        self.inner.policy
    }

    /// Push one event. Never blocks. Returns whether the event was
    /// accepted (`true`) or dropped (`false`).
    ///
    /// With a cap of zero, every push drops.
    pub fn push(&self, event: T) -> bool {
        if self.inner.cap == 0 {
            // Drop `event` *before* bumping counters so a slow Drop
            // (or one that re-enters logging) does not appear to
            // happen "inside" the sink's accounting.
            drop(event);
            self.inner.dropped.fetch_add(1, Ordering::Relaxed);
            match self.inner.policy {
                DropPolicy::DropOldest => {
                    self.inner.dropped_oldest.fetch_add(1, Ordering::Relaxed);
                }
                DropPolicy::DropNewest => {
                    self.inner.dropped_newest.fetch_add(1, Ordering::Relaxed);
                }
            }
            return false;
        }
        // Carrier for any T that must be dropped *outside* the lock.
        // A slow `T::drop` (or one that re-enters logging / pushes
        // back into a sink) must not hold the queue mutex, or the
        // "never blocks" observability promise becomes a deadlock
        // hazard for log handlers.
        let to_drop: Option<T>;
        let new_len: usize;
        let accepted;
        let mut dropped_oldest = false;
        let mut dropped_newest = false;
        {
            let mut guard = self.inner.queue.lock().expect("event sink poisoned");
            let len = guard.len();
            if len < self.inner.cap {
                guard.push_back(event);
                to_drop = None;
                accepted = true;
            } else {
                match self.inner.policy {
                    DropPolicy::DropOldest => {
                        // pop_front returns the displaced element;
                        // hand it to `to_drop` so it lives past the
                        // lock release.
                        to_drop = guard.pop_front();
                        guard.push_back(event);
                        dropped_oldest = true;
                        accepted = true;
                    }
                    DropPolicy::DropNewest => {
                        to_drop = Some(event);
                        dropped_newest = true;
                        accepted = false;
                    }
                }
            }
            new_len = guard.len();
        }
        // Lock released. Now drop the displaced/refused event and
        // bump atomic counters. None of this re-enters the sink.
        drop(to_drop);
        bump_high_water(&self.inner.high_water, new_len);
        if dropped_oldest {
            self.inner.dropped.fetch_add(1, Ordering::Relaxed);
            self.inner.dropped_oldest.fetch_add(1, Ordering::Relaxed);
        }
        if dropped_newest {
            self.inner.dropped.fetch_add(1, Ordering::Relaxed);
            self.inner.dropped_newest.fetch_add(1, Ordering::Relaxed);
        }
        if accepted {
            self.inner.accepted.fetch_add(1, Ordering::Relaxed);
        }
        accepted
    }

    /// Current live count.
    pub fn len(&self) -> usize {
        self.inner.queue.lock().expect("event sink poisoned").len()
    }

    /// True iff no events are queued.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Coherent snapshot of counters.
    pub fn snapshot(&self) -> EventSinkReport {
        let len = self.len();
        EventSinkReport {
            name: self.inner.name.clone(),
            cap: self.inner.cap,
            policy: self.inner.policy,
            len,
            high_water: self.inner.high_water.load(Ordering::Relaxed),
            dropped: self.inner.dropped.load(Ordering::Relaxed),
            dropped_oldest: self.inner.dropped_oldest.load(Ordering::Relaxed),
            dropped_newest: self.inner.dropped_newest.load(Ordering::Relaxed),
            accepted: self.inner.accepted.load(Ordering::Relaxed),
        }
    }

    /// Pull everything out FIFO and report.
    pub fn drain_snapshot(&self) -> EventSinkDrain<T> {
        let mut guard = self.inner.queue.lock().expect("event sink poisoned");
        let events: Vec<T> = guard.drain(..).collect();
        drop(guard);
        EventSinkDrain {
            events,
            report: self.snapshot(),
        }
    }

    /// One-line discovery for grep, mirroring the capacity discovery
    /// shape.
    ///
    /// ```text
    /// events sink=name cap=N policy=drop_oldest len=N high=N dropped=N accepted=N
    /// ```
    pub fn discovery_line(&self) -> String {
        let s = self.snapshot();
        format!(
            "events sink={name} cap={cap} policy={policy} len={len} high={high} dropped={dropped} dropped_oldest={dropped_oldest} dropped_newest={dropped_newest} accepted={accepted}",
            name = crate::capacity::discovery_value(&s.name),
            cap = s.cap,
            policy = s.policy.label(),
            len = s.len,
            high = s.high_water,
            dropped = s.dropped,
            dropped_oldest = s.dropped_oldest,
            dropped_newest = s.dropped_newest,
            accepted = s.accepted,
        )
    }

    /// Adapt the sink to a [`CapacitySurfaceReport`] so it plugs into
    /// `CapacitySummary` and shows up in the capacity discovery
    /// report. The `full_count` field tracks dropped events.
    pub fn surface_report(&self, mode: CapacityMode) -> CapacitySurfaceReport {
        let s = self.snapshot();
        CapacitySurfaceReport::count(s.name, mode, s.cap, s.len, s.high_water, s.dropped)
    }
}

impl<T: Clone> BoundedEventSink<T> {
    /// Returns the queued events without draining.
    pub fn peek_all(&self) -> Vec<T> {
        self.inner
            .queue
            .lock()
            .expect("event sink poisoned")
            .iter()
            .cloned()
            .collect()
    }
}

/// `BoundedEventSink<T>` view that erases the event type. Useful when
/// a host wants to surface counts without owning the event payload.
#[derive(Debug, Clone)]
pub struct EventSinkSurface {
    snapshot: EventSinkReport,
}

impl EventSinkSurface {
    /// Build from a snapshot.
    pub fn from_snapshot(snapshot: EventSinkReport) -> Self {
        Self { snapshot }
    }

    /// The snapshot.
    pub fn snapshot(&self) -> &EventSinkReport {
        &self.snapshot
    }
}

fn bump_high_water(target: &AtomicUsize, new: usize) {
    let mut hw = target.load(Ordering::Relaxed);
    while new > hw {
        match target.compare_exchange_weak(hw, new, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(observed) => hw = observed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_cap_keeps_all_events() {
        let sink = BoundedEventSink::<u32>::new("ev", 4, DropPolicy::DropOldest);
        assert!(sink.push(1));
        assert!(sink.push(2));
        assert!(sink.push(3));
        let drain = sink.drain_snapshot();
        assert_eq!(drain.events, vec![1, 2, 3]);
        assert!(drain.report.no_drops());
        assert_eq!(drain.report.high_water, 3);
    }

    #[test]
    fn drop_oldest_evicts_front() {
        let sink = BoundedEventSink::<u32>::new("ev", 2, DropPolicy::DropOldest);
        assert!(sink.push(1));
        assert!(sink.push(2));
        assert!(sink.push(3));
        assert!(sink.push(4));
        let drain = sink.drain_snapshot();
        assert_eq!(drain.events, vec![3, 4]);
        assert_eq!(drain.report.dropped, 2);
        assert_eq!(drain.report.dropped_oldest, 2);
        assert_eq!(drain.report.dropped_newest, 0);
    }

    #[test]
    fn drop_newest_keeps_first() {
        let sink = BoundedEventSink::<u32>::new("ev", 2, DropPolicy::DropNewest);
        assert!(sink.push(1));
        assert!(sink.push(2));
        assert!(!sink.push(3));
        assert!(!sink.push(4));
        let drain = sink.drain_snapshot();
        assert_eq!(drain.events, vec![1, 2]);
        assert_eq!(drain.report.dropped, 2);
        assert_eq!(drain.report.dropped_newest, 2);
    }

    #[test]
    fn cap_zero_drops_everything() {
        let sink = BoundedEventSink::<u32>::new("ev", 0, DropPolicy::DropOldest);
        assert!(!sink.push(1));
        assert!(!sink.push(2));
        assert_eq!(sink.snapshot().dropped, 2);
    }

    #[test]
    fn high_water_tracks_peak_not_current() {
        let sink = BoundedEventSink::<u32>::new("ev", 4, DropPolicy::DropOldest);
        sink.push(1);
        sink.push(2);
        sink.push(3);
        let _ = sink.drain_snapshot();
        sink.push(4);
        assert_eq!(sink.snapshot().high_water, 3);
    }

    #[test]
    fn discovery_line_is_grep_friendly() {
        let sink = BoundedEventSink::<u32>::new("svc.events", 2, DropPolicy::DropOldest);
        sink.push(1);
        sink.push(2);
        sink.push(3);
        let line = sink.discovery_line();
        assert!(line.starts_with("events "), "{line}");
        assert!(line.contains("sink=svc.events"), "{line}");
        assert!(line.contains("cap=2"), "{line}");
        assert!(line.contains("policy=drop_oldest"), "{line}");
        assert!(line.contains("dropped=1"), "{line}");
    }

    #[test]
    fn discovery_line_quotes_unsafe_names() {
        let sink = BoundedEventSink::<u32>::new("ev with space", 2, DropPolicy::DropOldest);
        let line = sink.discovery_line();
        assert!(line.contains("sink=\"ev with space\""), "{line}");
    }

    #[test]
    fn displaced_events_drop_outside_the_lock() {
        // A `T` whose Drop re-enters the sink would deadlock if the
        // sink's mutex were still held when the displaced T was
        // dropped. Build a `T` whose Drop calls `sink.len()` (which
        // takes the lock) and confirm it does not panic/deadlock.
        struct Reentrant {
            sink: BoundedEventSink<u32>,
        }
        impl Drop for Reentrant {
            fn drop(&mut self) {
                // Touching the sink during T::drop must not block.
                let _ = self.sink.len();
            }
        }

        let primary = BoundedEventSink::<u32>::new("primary", 1, DropPolicy::DropOldest);
        let reentry_sink = primary.clone();

        // Each push displaces the previous element. The displaced T's
        // Drop re-locks the same sink. Pre-fix this would deadlock on
        // the second push.
        let observer = BoundedEventSink::<Reentrant>::new("observer", 1, DropPolicy::DropOldest);
        observer.push(Reentrant {
            sink: reentry_sink.clone(),
        });
        observer.push(Reentrant { sink: reentry_sink });
        observer.push(Reentrant {
            sink: primary.clone(),
        });
        assert_eq!(observer.snapshot().dropped, 2);
    }

    #[test]
    fn surface_report_uses_dropped_as_full_count() {
        let sink = BoundedEventSink::<u32>::new("svc.events", 1, DropPolicy::DropNewest);
        sink.push(1);
        sink.push(2);
        sink.push(3);
        let r = sink.surface_report(CapacityMode::Fixed);
        assert_eq!(r.max_messages, Some(1));
        assert_eq!(r.full_count, 2);
    }
}
