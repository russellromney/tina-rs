//! Typed, bounded host observation handles for runtime facts.
//!
//! Phase 047 Rock 4: replace `Arc<Mutex<Option<SocketAddr>>>` and
//! `Arc<AtomicBool>` side channels in user code with typed handles that the
//! runtime owns and notifies. The first slice ships only the
//! [`BoundAddressWaiter`] — the remaining handles (isolate complete,
//! operation done, child restarted) follow once the base shape is boring.
//!
//! Each handle is one bounded slot. Handles do not create secret unbounded
//! queues. Trace remains the source of audit truth: handles only surface
//! facts the runtime already records into the trace.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::time::Duration;

use crate::call::CallError;

/// Outcome of a [`BoundAddressWaiter::wait`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitError {
    /// The waiter timed out before the runtime produced an outcome.
    Timeout,
    /// The runtime stopped (or was dropped) before the awaited fact happened.
    RuntimeStopped,
    /// The awaited runtime call failed; the trace recorded the failure.
    CallFailed(CallError),
}

/// Internal per-slot outcome value carried over the bounded one-slot channel.
#[derive(Debug, Clone, Copy)]
pub(crate) enum BoundAddressOutcome {
    Bound(SocketAddr),
    Failed(CallError),
}

/// Typed, bounded handle that resolves once the runtime completes the *next*
/// `tcp_bind` call and records its bound `SocketAddr`.
///
/// Replaces the `Arc<Mutex<Option<SocketAddr>>>` side channel in Eiffel
/// examples. Each waiter is one bounded slot — there is no hidden queue. If
/// the runtime stops before a bind completes, [`wait`](Self::wait) returns
/// [`WaitError::RuntimeStopped`].
///
/// The waiter is consumed by `wait`. Drop it before `wait` to cancel the
/// observation; the runtime then skips this slot when the next bind
/// completes (the slot is removed from the registry on next dispatch).
#[derive(Debug)]
pub struct BoundAddressWaiter {
    rx: mpsc::Receiver<BoundAddressOutcome>,
}

impl BoundAddressWaiter {
    pub(crate) fn from_pair() -> (Self, SyncSender<BoundAddressOutcome>) {
        let (tx, rx) = mpsc::sync_channel(1);
        (Self { rx }, tx)
    }

    /// Waits up to `timeout` for the next `tcp_bind` to complete.
    ///
    /// Returns the bound `SocketAddr` on success, or one of the typed errors
    /// in [`WaitError`]. Consumes the waiter.
    pub fn wait(self, timeout: Duration) -> Result<SocketAddr, WaitError> {
        match self.rx.recv_timeout(timeout) {
            Ok(BoundAddressOutcome::Bound(addr)) => Ok(addr),
            Ok(BoundAddressOutcome::Failed(error)) => Err(WaitError::CallFailed(error)),
            Err(RecvTimeoutError::Timeout) => Err(WaitError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(WaitError::RuntimeStopped),
        }
    }
}

/// Internal observation state owned by a [`crate::Runtime`].
///
/// Holds typed bounded one-slot senders for each pending observer. The
/// runtime drains the front of each queue when the matching fact happens;
/// observers register in the order they want to be served.
pub(crate) struct ObservationRegistry {
    pending_bound: VecDeque<SyncSender<BoundAddressOutcome>>,
}

impl std::fmt::Debug for ObservationRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ObservationRegistry")
            .field("pending_bound", &self.pending_bound.len())
            .finish()
    }
}

/// Constructs a waiter whose receiver is already disconnected, so the next
/// `wait` resolves immediately to [`WaitError::RuntimeStopped`].
///
/// Used by the threaded runtime when the worker has already exited and the
/// host requests an observer: the host should still get a typed waiter so
/// it can choose its own timeout/error policy without separate plumbing.
pub(crate) fn stopped_bound_waiter() -> BoundAddressWaiter {
    let (tx, rx) = mpsc::sync_channel(1);
    drop(tx);
    BoundAddressWaiter { rx }
}

impl ObservationRegistry {
    pub(crate) const fn new() -> Self {
        Self {
            pending_bound: VecDeque::new(),
        }
    }

    /// Registers a one-slot bound-address observer; returns the matching
    /// host-side waiter.
    pub(crate) fn register_bound(&mut self) -> BoundAddressWaiter {
        let (waiter, sender) = BoundAddressWaiter::from_pair();
        self.pending_bound.push_back(sender);
        waiter
    }

    /// Notifies the front bound-address observer (if any). The send is
    /// non-blocking: senders are bounded one-slot channels, so this never
    /// blocks the runtime worker. If the receiver was already dropped the
    /// outcome is silently discarded and the slot moves on.
    pub(crate) fn notify_bound(&mut self, outcome: BoundAddressOutcome) {
        while let Some(sender) = self.pending_bound.pop_front() {
            // try_send rather than send: the channel is one-slot, this only
            // ever has capacity 1, and we hold a fresh one-slot channel per
            // observer. If the receiver is gone the outcome is dropped and
            // we move on to the next observer.
            match sender.try_send(outcome) {
                Ok(()) => return,
                Err(mpsc::TrySendError::Full(_)) => {
                    // Should not happen for a one-slot channel that we
                    // populate exactly once, but treat defensively as a
                    // dropped observer rather than panicking the worker.
                    continue;
                }
                Err(mpsc::TrySendError::Disconnected(_)) => continue,
            }
        }
    }
}
