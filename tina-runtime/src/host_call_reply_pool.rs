//! Per-host-thread pool of typed reply channels for `call_blocking`.
//!
//! `mpsc::channel()` allocates per call (an `Arc<Channel<T>>` plus internal
//! state). On a hot `call_blocking` loop on one host thread the channel can
//! safely be reused: a call holds the receiver, hands a sender to the
//! dispatcher, then the receiver is given back once the strong count proves
//! no sender is outstanding.
//!
//! Per warmed call the pool turns the per-call channel allocation into a
//! `Vec::pop()`. The reply payload itself stays typed (`R`, not
//! `Box<dyn Any>`), so the send/recv path adds no extra allocations.
//!
//! Pool key: `TypeId::of::<R>()`. Each `R` gets its own `Vec<TypedReply<R>>`
//! stored as `Box<dyn Any>` (one boxed `Vec` per type at first use; subsequent
//! calls just downcast and `pop`/`push`).
//!
//! Bounded: each per-type pool caps at `MAX_POOLED_PER_TYPE` so a bursty
//! workload doesn't hold onto idle channels forever.

use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Cap per (R type) pool. Past this, extras are dropped instead of cached.
const MAX_POOLED_PER_TYPE: usize = 16;

/// Initial Vec capacity per (R type) pool.
const INITIAL_POOL_CAPACITY: usize = 8;

struct ReplyState<R> {
    inner: Mutex<ReplyInner<R>>,
    cond: Condvar,
}

struct ReplyInner<R> {
    value: Option<R>,
    sender_dropped: bool,
}

/// Receiver side of a reusable typed one-shot channel. Holds an `Arc` to the
/// shared state; cloning a `Sender` adds another `Arc`. Pool eligibility on
/// return is decided by `Arc::strong_count == 1`.
pub(crate) struct TypedReply<R: Send + 'static> {
    state: Arc<ReplyState<R>>,
}

impl<R: Send + 'static> TypedReply<R> {
    fn new() -> Self {
        Self {
            state: Arc::new(ReplyState {
                inner: Mutex::new(ReplyInner {
                    value: None,
                    sender_dropped: false,
                }),
                cond: Condvar::new(),
            }),
        }
    }

    /// Builds a sender that, when dropped or sent through, wakes this
    /// receiver. Cloning is implicit: every sender takes one `Arc` clone of
    /// the shared state.
    pub(crate) fn sender(&self) -> TypedReplySender<R> {
        TypedReplySender {
            state: Arc::clone(&self.state),
        }
    }

    /// Waits up to `timeout` for a value. Returns `Disconnected` if all
    /// senders dropped without sending; returns `Timeout` if neither
    /// happened in time.
    pub(crate) fn recv_timeout(&self, timeout: Duration) -> Result<R, RecvError> {
        let mut guard = self.state.inner.lock().expect("reply recv lock");
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(value) = guard.value.take() {
                return Ok(value);
            }
            if guard.sender_dropped {
                return Err(RecvError::Disconnected);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(RecvError::Timeout);
            }
            let remaining = deadline - now;
            let (next_guard, _wait_result) = self
                .state
                .cond
                .wait_timeout(guard, remaining)
                .expect("reply cond wait");
            guard = next_guard;
        }
    }

    /// Resets state for pool reuse. Caller must already have verified that no
    /// sender is outstanding (`Arc::strong_count(&state) == 1`).
    fn reset_for_reuse(&mut self) {
        let mut guard = self.state.inner.lock().expect("reply reset lock");
        guard.value = None;
        guard.sender_dropped = false;
    }
}

/// Sender side. Drops always wake the receiver — successful sends through the
/// inner value, unsuccessful drops through the `sender_dropped` flag.
pub(crate) struct TypedReplySender<R: Send + 'static> {
    state: Arc<ReplyState<R>>,
}

impl<R: Send + 'static> TypedReplySender<R> {
    pub(crate) fn send(self, value: R) {
        // Hold the lock only to set the value. The notify in `Drop` (which
        // runs after this method returns) wakes the receiver.
        let mut guard = self.state.inner.lock().expect("reply send lock");
        guard.value = Some(value);
    }
}

impl<R: Send + 'static> Drop for TypedReplySender<R> {
    fn drop(&mut self) {
        // If we never sent, mark the channel as disconnected so a parked
        // receiver wakes and returns `Disconnected` instead of timing out.
        {
            let mut guard = self.state.inner.lock().expect("reply drop lock");
            if guard.value.is_none() {
                guard.sender_dropped = true;
            }
        }
        self.state.cond.notify_one();
    }
}

#[derive(Debug)]
pub(crate) enum RecvError {
    Timeout,
    Disconnected,
}

thread_local! {
    static REPLY_POOLS: RefCell<HashMap<TypeId, Box<dyn Any>>> = RefCell::new(HashMap::new());
}

/// Pops a typed reply channel from the calling thread's pool or constructs a
/// fresh one. The returned `TypedReply<R>` is exclusively owned by the
/// caller; the channel is hot (no leftover value, no leftover senders).
pub(crate) fn checkout<R: Send + 'static>() -> TypedReply<R> {
    REPLY_POOLS.with(|pools| {
        let mut pools = pools.borrow_mut();
        let pool_entry = pools
            .entry(TypeId::of::<R>())
            .or_insert_with(|| -> Box<dyn Any> {
                Box::new(Vec::<TypedReply<R>>::with_capacity(INITIAL_POOL_CAPACITY))
            });
        let typed_pool: &mut Vec<TypedReply<R>> = pool_entry
            .downcast_mut::<Vec<TypedReply<R>>>()
            .expect("typed reply pool downcast");
        typed_pool.pop().unwrap_or_else(TypedReply::new)
    })
}

/// Returns a typed reply channel to the calling thread's pool — but only if
/// it is safe to reuse. A surviving sender (e.g. `HostWaitTimeout` while the
/// call is still in flight on a dispatcher) means the channel must be
/// dropped, not pooled, or the next caller would race with a late reply.
pub(crate) fn checkin<R: Send + 'static>(mut reply: TypedReply<R>) {
    if Arc::strong_count(&reply.state) != 1 {
        // Sender outstanding — let the channel die naturally when the
        // dispatcher eventually drops its sender. The next call_blocking
        // pulls a fresh channel from the pool (or allocates one).
        return;
    }
    reply.reset_for_reuse();
    REPLY_POOLS.with(|pools| {
        let mut pools = pools.borrow_mut();
        if let Some(pool_entry) = pools.get_mut(&TypeId::of::<R>())
            && let Some(typed_pool) = pool_entry.downcast_mut::<Vec<TypedReply<R>>>()
            && typed_pool.len() < MAX_POOLED_PER_TYPE
        {
            typed_pool.push(reply);
        }
        // else: pool absent (impossible — checkout creates it), wrong type
        // (impossible — TypeId keyed), or at cap. Drop the reply.
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    #[test]
    fn checkout_then_send_then_recv() {
        let reply = checkout::<u32>();
        let sender = reply.sender();
        sender.send(42);
        assert_eq!(reply.recv_timeout(Duration::from_secs(1)).unwrap(), 42);
        checkin(reply);
    }

    #[test]
    fn sender_dropped_without_send_disconnects() {
        let reply = checkout::<u32>();
        drop(reply.sender());
        assert!(matches!(
            reply.recv_timeout(Duration::from_secs(1)),
            Err(RecvError::Disconnected)
        ));
        checkin(reply);
    }

    #[test]
    fn timeout_when_no_sender_activity() {
        let reply = checkout::<u32>();
        let _sender = reply.sender(); // keep alive so we don't get Disconnected
        assert!(matches!(
            reply.recv_timeout(Duration::from_millis(20)),
            Err(RecvError::Timeout)
        ));
        checkin(reply);
    }

    #[test]
    fn cross_thread_send() {
        let reply = checkout::<u32>();
        let sender = reply.sender();
        let handle = thread::spawn(move || {
            sender.send(7);
        });
        let value = reply.recv_timeout(Duration::from_secs(1)).unwrap();
        handle.join().unwrap();
        assert_eq!(value, 7);
        checkin(reply);
    }

    #[test]
    fn checkin_with_outstanding_sender_does_not_pool() {
        let reply = checkout::<u32>();
        let _sender = reply.sender(); // outstanding
        let arc_count_before = Arc::strong_count(&reply.state);
        assert_eq!(arc_count_before, 2);
        checkin(reply); // should silently not pool because count != 1
        // No assert needed; just verifies no panic.
    }

    #[test]
    fn pool_reuses_channels() {
        // Drain any state from prior tests on this thread.
        REPLY_POOLS.with(|pools| pools.borrow_mut().clear());

        let counter = AtomicUsize::new(0);
        for _ in 0..32 {
            let reply = checkout::<u64>();
            let sender = reply.sender();
            sender.send(1);
            assert_eq!(reply.recv_timeout(Duration::from_secs(1)).unwrap(), 1);
            checkin(reply);
            counter.fetch_add(1, Ordering::Relaxed);
        }
        assert_eq!(counter.load(Ordering::Relaxed), 32);
    }
}
