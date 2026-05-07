//! Live trace observer hook.
//!
//! One callback. Fires once per [`RuntimeEvent`] before retention
//! trims. Synchronous, on the recording thread. No internal queue.
//!
//! Rules:
//!
//! - No handle to the runtime. Reentry is impossible by construction.
//! - Hot path. Bound your work; clone the event and forward to a
//!   bounded queue if you need a different thread.
//! - Panics are not caught. A panicking observer kills the recording
//!   thread.
//! - `TraceRetention::Off` does not silence the observer. Stream-only
//!   mode is `retention = Off` + observer set.
//!
//! Per-shard order only. Global order across shards is not promised;
//! sort by [`crate::EventId`] if you need it.

use std::sync::Arc;

use crate::trace::RuntimeEvent;

/// One synchronous callback per recorded event.
///
/// `Send + Sync + 'static` because shards run on their own threads
/// and share the observer via `Arc`.
pub trait TraceObserver: Send + Sync + 'static {
    /// Called once per event, before retention.
    fn on_event(&self, event: &RuntimeEvent);
}

pub(crate) type StoredObserver = Option<Arc<dyn TraceObserver>>;

/// Closures with the right shape are observers too.
impl<F> TraceObserver for F
where
    F: Fn(&RuntimeEvent) + Send + Sync + 'static,
{
    fn on_event(&self, event: &RuntimeEvent) {
        self(event)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tina::{IsolateId, ShardId};

    use super::*;
    use crate::trace::{EventId, RuntimeEventKind};

    #[test]
    fn closure_is_an_observer() {
        let count = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&count);
        let observer: Arc<dyn TraceObserver> = Arc::new(move |_event: &RuntimeEvent| {
            counter.fetch_add(1, Ordering::Relaxed);
        });
        let event = RuntimeEvent::new(
            EventId::new(1),
            None,
            ShardId::new(0),
            IsolateId::new(1),
            RuntimeEventKind::HandlerStarted,
        );
        observer.on_event(&event);
        observer.on_event(&event);
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn struct_observer_collects_events() {
        struct Collector(Mutex<Vec<RuntimeEvent>>);
        impl TraceObserver for Collector {
            fn on_event(&self, event: &RuntimeEvent) {
                self.0.lock().expect("collector lock").push(*event);
            }
        }

        let collector = Arc::new(Collector(Mutex::new(Vec::new())));
        let observer: Arc<dyn TraceObserver> = collector.clone();
        let event = RuntimeEvent::new(
            EventId::new(7),
            None,
            ShardId::new(0),
            IsolateId::new(2),
            RuntimeEventKind::IsolateStopped,
        );
        observer.on_event(&event);
        assert_eq!(collector.0.lock().unwrap().len(), 1);
    }
}
