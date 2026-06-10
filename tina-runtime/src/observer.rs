//! Live trace observer hook.
//!
//! One callback per [`RuntimeEvent`], synchronous by default, on the
//! recording thread, before retention trims.
//!
//! Rules:
//! - No runtime handle. Reentry impossible by construction.
//! - Hot path. Bound your work. Use [`BufferedTraceObserver`] when a
//!   subscriber may block.
//! - Panics are not caught — they kill the recording thread.
//! - `TraceRetention::Off` does not silence the observer. Stream-only
//!   = `Off` + observer.
//! - Per-shard order only. No global cross-shard order; sort by
//!   [`crate::EventId`] if you need one.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, TrySendError};
use std::thread::{self, JoinHandle};

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

/// Bounded non-blocking observer adapter.
///
/// The shard thread only copies the [`RuntimeEvent`] into a bounded queue with
/// `try_send`. A dedicated drain thread invokes the wrapped observer. If the
/// queue is full, the event is dropped and [`dropped_count`](Self::dropped_count)
/// records the loss.
///
/// This adapter is useful for production-style observation where blocking the
/// shard is worse than losing an event. Do not feed a proof/replay hash from a
/// buffered observer unless the proof path first completes `flush()` or
/// `shutdown()` and uses the returned drain token.
pub struct BufferedTraceObserver {
    sender: std::sync::Mutex<Option<mpsc::SyncSender<ObserverWork>>>,
    drain_thread: std::sync::Mutex<Option<JoinHandle<()>>>,
    dropped: AtomicU64,
}

enum ObserverWork {
    Event(RuntimeEvent),
    Flush(mpsc::SyncSender<()>),
}

/// Proof-grade result of draining a [`BufferedTraceObserver`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferedTraceDrain {
    dropped_events: u64,
}

impl BufferedTraceDrain {
    /// Number of events dropped before the drain barrier completed.
    pub const fn dropped_events(&self) -> u64 {
        self.dropped_events
    }
}

/// A buffered trace observer could not complete a drain barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferedTraceDrainError {
    /// The drain thread is already gone.
    Disconnected,
}

impl std::fmt::Display for BufferedTraceDrainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disconnected => f.write_str("buffered trace observer drain thread disconnected"),
        }
    }
}

impl std::error::Error for BufferedTraceDrainError {}

impl std::fmt::Debug for BufferedTraceObserver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BufferedTraceObserver")
            .field("dropped", &self.dropped_count())
            .finish_non_exhaustive()
    }
}

impl BufferedTraceObserver {
    /// Creates a bounded observer that drains into `observer` on a dedicated
    /// thread.
    ///
    /// `capacity` is clamped to at least one event so construction cannot
    /// accidentally produce an always-dropping observer.
    pub fn new(capacity: usize, observer: Arc<dyn TraceObserver>) -> Self {
        let (sender, receiver) = mpsc::sync_channel(capacity.max(1));
        let drain_thread = thread::Builder::new()
            .name("tina-trace-observer".to_string())
            .spawn(move || {
                while let Ok(work) = receiver.recv() {
                    match work {
                        ObserverWork::Event(event) => observer.on_event(&event),
                        ObserverWork::Flush(ack) => {
                            let _ = ack.send(());
                        }
                    }
                }
            })
            .expect("failed to spawn tina trace observer thread");
        Self {
            sender: std::sync::Mutex::new(Some(sender)),
            drain_thread: std::sync::Mutex::new(Some(drain_thread)),
            dropped: AtomicU64::new(0),
        }
    }

    /// Number of events dropped because the buffer was full or closed.
    ///
    /// This is a diagnostic counter only. Proof snapshots need a drain barrier:
    /// call [`flush`](Self::flush) or [`shutdown`](Self::shutdown) and pass the
    /// returned [`BufferedTraceDrain`] to the proof layer.
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Blocks until every event accepted before this call has reached the
    /// wrapped observer, then returns a proof-grade dropped-event count.
    pub fn flush(&self) -> Result<BufferedTraceDrain, BufferedTraceDrainError> {
        let (ack_tx, ack_rx) = mpsc::sync_channel(0);
        let sender = self.sender.lock().expect("buffered observer sender lock");
        let Some(sender) = sender.as_ref() else {
            return Err(BufferedTraceDrainError::Disconnected);
        };
        sender
            .send(ObserverWork::Flush(ack_tx))
            .map_err(|_| BufferedTraceDrainError::Disconnected)?;
        ack_rx
            .recv()
            .map_err(|_| BufferedTraceDrainError::Disconnected)?;
        Ok(BufferedTraceDrain {
            dropped_events: self.dropped_count(),
        })
    }

    /// Closes the queue, joins the drain thread, and returns the final
    /// dropped-event count.
    pub fn shutdown(&self) -> Result<BufferedTraceDrain, BufferedTraceDrainError> {
        let sender = self.sender.lock().expect("buffered observer sender lock").take();
        drop(sender);

        if let Some(handle) = self
            .drain_thread
            .lock()
            .expect("buffered observer thread lock")
            .take()
        {
            handle
                .join()
                .map_err(|_| BufferedTraceDrainError::Disconnected)?;
        }

        Ok(BufferedTraceDrain {
            dropped_events: self.dropped_count(),
        })
    }
}

impl TraceObserver for BufferedTraceObserver {
    fn on_event(&self, event: &RuntimeEvent) {
        let Some(sender) = self
            .sender
            .lock()
            .expect("buffered observer sender lock")
            .as_ref()
            .cloned()
        else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        match sender.try_send(ObserverWork::Event(*event)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

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
    use std::sync::mpsc;
    use std::time::Duration;

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

    #[test]
    fn buffered_observer_counts_drops_when_drain_is_full() {
        struct BlockingObserver {
            started: mpsc::SyncSender<()>,
            release: Mutex<mpsc::Receiver<()>>,
        }

        impl TraceObserver for BlockingObserver {
            fn on_event(&self, _event: &RuntimeEvent) {
                let _ = self.started.try_send(());
                let _ = self
                    .release
                    .lock()
                    .expect("release lock")
                    .recv_timeout(Duration::from_secs(5));
            }
        }

        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let downstream: Arc<dyn TraceObserver> = Arc::new(BlockingObserver {
            started: started_tx,
            release: Mutex::new(release_rx),
        });
        let buffered = BufferedTraceObserver::new(1, downstream);
        let event = RuntimeEvent::new(
            EventId::new(9),
            None,
            ShardId::new(0),
            IsolateId::new(3),
            RuntimeEventKind::HandlerStarted,
        );

        buffered.on_event(&event);
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("drain thread entered downstream observer");
        for _ in 0..16 {
            buffered.on_event(&event);
        }

        assert!(
            buffered.dropped_count() > 0,
            "full buffer should drop visibly instead of blocking the shard"
        );
        let _ = release_tx.send(());
    }

    #[test]
    fn buffered_observer_flush_delivers_accepted_events_before_snapshot() {
        struct Collector(Mutex<Vec<RuntimeEvent>>);

        impl TraceObserver for Collector {
            fn on_event(&self, event: &RuntimeEvent) {
                self.0.lock().expect("collector lock").push(*event);
            }
        }

        let collector = Arc::new(Collector(Mutex::new(Vec::new())));
        let downstream: Arc<dyn TraceObserver> = collector.clone();
        let buffered = BufferedTraceObserver::new(8, downstream);

        let events: Vec<_> = (1..=4)
            .map(|id| {
                RuntimeEvent::new(
                    EventId::new(id),
                    None,
                    ShardId::new(0),
                    IsolateId::new(3),
                    RuntimeEventKind::HandlerStarted,
                )
            })
            .collect();
        for event in &events {
            buffered.on_event(event);
        }

        let drain = buffered.flush().expect("flush should drain accepted events");
        assert_eq!(drain.dropped_events(), 0);
        assert_eq!(*collector.0.lock().expect("collector lock"), events);

        let final_drain = buffered.shutdown().expect("shutdown joins drain thread");
        assert_eq!(final_drain.dropped_events(), 0);
        buffered.on_event(&RuntimeEvent::new(
            EventId::new(99),
            None,
            ShardId::new(0),
            IsolateId::new(3),
            RuntimeEventKind::HandlerStarted,
        ));
        assert_eq!(buffered.dropped_count(), 1);
    }
}
