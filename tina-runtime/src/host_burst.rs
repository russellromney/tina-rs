//! Host-side burst send accumulator.
//!
//! Wraps the per-send observer ceremony of
//! [`crate::ThreadedRuntime::try_send_and_observe_with`] in a tiny
//! shared counter. The host issues `try_send_outcome(addr, msg, &outcomes)`
//! in a tight loop and then `outcomes.wait_complete(deadline)?` once.
//! Every truth-typed outcome stays distinct in the snapshot:
//! `admitted`, `mailbox_full`, `mailbox_closed`, `ingress_full`,
//! `worker_stopped`.
//!
//! This is *not* a synchronous host-side mailbox check. The mailbox is
//! owned by the worker shard; per-send precision still rides on the
//! observer running on that thread. What this primitive removes is the
//! per-send closure, the Arc-cloned counters, and the manual observed
//! barrier the caller used to spell out by hand.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::errors::{ThreadedSendObservedError, ThreadedTrySendError};

/// Shared counter the host burst writes into. Cheap to clone; the
/// observer side holds a clone so the runtime can update counts after
/// the call returns.
#[derive(Clone, Default, Debug)]
pub struct HostBurstOutcomes {
    pub(crate) inner: Arc<HostBurstInner>,
}

#[derive(Default, Debug)]
pub(crate) struct HostBurstInner {
    pub(crate) submitted: AtomicU32,
    pub(crate) observed: AtomicU32,
    pub(crate) admitted: AtomicU32,
    pub(crate) mailbox_full: AtomicU32,
    pub(crate) mailbox_closed: AtomicU32,
    pub(crate) ingress_full: AtomicU32,
    pub(crate) worker_stopped: AtomicU32,
}

/// Snapshot returned by [`HostBurstOutcomes::snapshot`].
///
/// `submitted - observed` is the number of observers still pending. A
/// finished burst has `submitted == observed` and
/// `admitted + mailbox_full + mailbox_closed + ingress_full +
/// worker_stopped == submitted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostBurstSnapshot {
    /// Total `try_send_outcome` calls issued.
    pub submitted: u32,
    /// Per-send outcomes that have already resolved (observer fired or
    /// host-side error returned).
    pub observed: u32,
    /// Mailbox accepted the message.
    pub admitted: u32,
    /// Mailbox was full when the worker tried admission.
    pub mailbox_full: u32,
    /// Mailbox was closed/stale when the worker tried admission.
    pub mailbox_closed: u32,
    /// Worker ingress queue was full; observer never fired.
    pub ingress_full: u32,
    /// Worker thread was already stopped; observer never fired.
    pub worker_stopped: u32,
}

/// `wait_complete` outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostBurstWaitError {
    /// Deadline elapsed before every observer fired.
    Timeout,
}

impl std::fmt::Display for HostBurstWaitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => f.write_str("timed out before every host-burst observer fired"),
        }
    }
}

impl std::error::Error for HostBurstWaitError {}

impl HostBurstOutcomes {
    /// Fresh counter set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Counts as of this instant. Atomic loads are independent: a
    /// snapshot taken mid-burst may briefly show `submitted >
    /// observed`. After [`Self::wait_complete`] returns `Ok`, the
    /// snapshot is consistent.
    pub fn snapshot(&self) -> HostBurstSnapshot {
        HostBurstSnapshot {
            submitted: self.inner.submitted.load(Ordering::Acquire),
            observed: self.inner.observed.load(Ordering::Acquire),
            admitted: self.inner.admitted.load(Ordering::Acquire),
            mailbox_full: self.inner.mailbox_full.load(Ordering::Acquire),
            mailbox_closed: self.inner.mailbox_closed.load(Ordering::Acquire),
            ingress_full: self.inner.ingress_full.load(Ordering::Acquire),
            worker_stopped: self.inner.worker_stopped.load(Ordering::Acquire),
        }
    }

    /// Waits up to `timeout` for every issued send to resolve (the
    /// observer fired on the worker thread, or the host-side return
    /// path bumped the counters).
    ///
    /// The "target" is captured at entry: this is "wait for everything
    /// submitted *so far*", not a moving fence. Don't issue more
    /// `try_send_outcome` calls between calling `wait_complete` and
    /// reading the snapshot.
    ///
    /// The wait does not pin a CPU. The fast path (observers already
    /// fired by the time the host calls in) returns on the first
    /// atomic load. Otherwise the loop sleeps briefly between checks
    /// so a stalled or slow worker doesn't burn the host's core for
    /// the entire `timeout`.
    pub fn wait_complete(&self, timeout: Duration) -> Result<(), HostBurstWaitError> {
        let target = self.inner.submitted.load(Ordering::Acquire);
        let deadline = tina::Deadline::from_instant(Instant::now(), timeout).instant();
        // Sub-millisecond polling: typical bursts resolve in a handful
        // of iterations on a healthy worker, while a stalled worker
        // costs at most ~10k wakeups/s instead of pinning a core.
        let poll_interval = Duration::from_micros(100);
        loop {
            if self.inner.observed.load(Ordering::Acquire) >= target {
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(HostBurstWaitError::Timeout);
            }
            // Don't oversleep past the deadline.
            let remaining = deadline.saturating_duration_since(now);
            thread::sleep(poll_interval.min(remaining));
        }
    }

    /// Internal: bump submitted, run the per-send branch.
    pub(crate) fn note_submitted(&self) {
        self.inner.submitted.fetch_add(1, Ordering::Release);
    }

    /// Internal: caller-side error path; observer never fires for these.
    pub(crate) fn note_host_side_error(&self, err: ThreadedTrySendError) {
        match err {
            ThreadedTrySendError::IngressFull => {
                self.inner.ingress_full.fetch_add(1, Ordering::Release);
            }
            ThreadedTrySendError::WorkerStopped => {
                self.inner.worker_stopped.fetch_add(1, Ordering::Release);
            }
        }
        self.inner.observed.fetch_add(1, Ordering::Release);
    }

    /// Internal: build an observer that records the worker-side outcome.
    pub(crate) fn observer(
        &self,
    ) -> impl FnOnce(Result<(), ThreadedSendObservedError>) + Send + 'static {
        let inner = Arc::clone(&self.inner);
        move |outcome| {
            match outcome {
                Ok(()) => {
                    inner.admitted.fetch_add(1, Ordering::Release);
                }
                Err(ThreadedSendObservedError::MailboxFull) => {
                    inner.mailbox_full.fetch_add(1, Ordering::Release);
                }
                Err(ThreadedSendObservedError::MailboxClosed) => {
                    inner.mailbox_closed.fetch_add(1, Ordering::Release);
                }
                Err(ThreadedSendObservedError::IngressFull) => {
                    inner.ingress_full.fetch_add(1, Ordering::Release);
                }
                Err(ThreadedSendObservedError::WorkerStopped) => {
                    inner.worker_stopped.fetch_add(1, Ordering::Release);
                }
            }
            inner.observed.fetch_add(1, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_complete_times_out_when_no_observer_fires() {
        let outcomes = HostBurstOutcomes::new();
        outcomes.note_submitted();
        outcomes.note_submitted();
        let result = outcomes.wait_complete(Duration::from_millis(20));
        assert_eq!(result, Err(HostBurstWaitError::Timeout));

        let snap = outcomes.snapshot();
        assert_eq!(snap.submitted, 2);
        assert_eq!(snap.observed, 0);
    }

    #[test]
    fn wait_complete_returns_ok_when_no_sends_were_issued() {
        let outcomes = HostBurstOutcomes::new();
        let result = outcomes.wait_complete(Duration::from_millis(5));
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn host_side_error_path_bumps_observed_and_buckets() {
        let outcomes = HostBurstOutcomes::new();
        outcomes.note_submitted();
        outcomes.note_host_side_error(ThreadedTrySendError::IngressFull);
        outcomes.note_submitted();
        outcomes.note_host_side_error(ThreadedTrySendError::WorkerStopped);

        outcomes
            .wait_complete(Duration::from_millis(50))
            .expect("observed already caught up");

        let snap = outcomes.snapshot();
        assert_eq!(snap.submitted, 2);
        assert_eq!(snap.observed, 2);
        assert_eq!(snap.ingress_full, 1);
        assert_eq!(snap.worker_stopped, 1);
        assert_eq!(snap.admitted, 0);
    }
}
