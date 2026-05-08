//! Live trace observer hook.
//!
//! One callback per [`RuntimeEvent`], synchronous, on the recording
//! thread, before retention trims. No queue.
//!
//! Rules:
//! - No runtime handle. Reentry impossible by construction.
//! - Hot path. Bound your work. Clone and forward if you need
//!   another thread.
//! - Panics are not caught — they kill the recording thread.
//! - `TraceRetention::Off` does not silence the observer. Stream-only
//!   = `Off` + observer.
//! - Per-shard order only. No global cross-shard order; sort by
//!   [`crate::EventId`] if you need one.

use std::sync::Arc;

use crate::trace::RuntimeEvent;

/// Sync callback fired once per recorded event.
///
/// `Send + Sync + 'static` so shards on their own threads can share
/// it via `Arc`.
pub trait TraceObserver: Send + Sync + 'static {
    /// Fires once per event, before retention.
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
