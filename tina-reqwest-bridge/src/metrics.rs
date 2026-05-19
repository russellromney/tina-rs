//! Bridge metrics. Plain atomic counters; no time-series, no labels.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Snapshot of bridge counters.
///
/// All counters describe **worker-terminal events** — what the worker
/// observed and what it emitted as its `Effect::Reply`. They do not
/// reflect what the Tina caller saw. The worker has no signal for the
/// caller's `IsolateCall` deadline; if the caller has already given up
/// when the worker fires its Reply, the runtime drops it as
/// `CallReplyRejected` and that event is visible only in the runtime
/// trace, not in these counters.
///
/// Counters are monotonic except for `current_in_flight`, which is the
/// instantaneous in-flight count at snapshot time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReqwestMetrics {
    /// Sends admitted into the worker. Each spawned reqwest task
    /// counts once, including retry attempts.
    pub admitted: u64,
    /// Sends rejected at admission because `max_in_flight` was reached.
    pub full: u64,
    /// Sends rejected at admission because the worker was closed.
    pub closed: u64,
    /// Sends rejected at admission because the body exceeded
    /// `request_body_limit`.
    pub request_too_large: u64,
    /// Sends rejected at admission by URL/method/header validation,
    /// before reqwest saw them.
    pub invalid: u64,
    /// Reqwest tasks the worker terminated with a successful response.
    pub responses: u64,
    /// Reqwest tasks the worker terminated with
    /// [`crate::ReqwestError::Timeout`].
    pub timeout: u64,
    /// Reqwest tasks the worker terminated with
    /// [`crate::ReqwestError::ResponseTooLarge`].
    pub response_too_large: u64,
    /// Reqwest tasks the worker terminated with
    /// [`crate::ReqwestError::Reqwest`] (reqwest-side transport/body
    /// errors).
    pub reqwest_error: u64,
    /// Reqwest tasks the worker terminated with
    /// [`crate::ReqwestError::Internal`].
    pub internal_error: u64,
    /// Retry attempts scheduled (the count of *retries*, not the first
    /// attempt). Always `0` when [`crate::RetryPolicy::None`].
    pub retries: u64,
    /// Current in-flight count (in-flight reqwest tasks plus pending
    /// retries). Set on every admission and terminal Reply.
    pub current_in_flight: u64,
    /// Highest `current_in_flight` ever observed.
    pub in_flight_high_water: u64,
}

#[derive(Debug, Default)]
pub(crate) struct MetricsInner {
    pub(crate) admitted: AtomicU64,
    pub(crate) full: AtomicU64,
    pub(crate) closed: AtomicU64,
    pub(crate) request_too_large: AtomicU64,
    pub(crate) invalid: AtomicU64,
    pub(crate) responses: AtomicU64,
    pub(crate) timeout: AtomicU64,
    pub(crate) response_too_large: AtomicU64,
    pub(crate) reqwest_error: AtomicU64,
    pub(crate) internal_error: AtomicU64,
    pub(crate) retries: AtomicU64,
    pub(crate) current_in_flight: AtomicU64,
    pub(crate) in_flight_high_water: AtomicU64,
}

impl MetricsInner {
    pub(crate) fn snapshot(&self) -> ReqwestMetrics {
        ReqwestMetrics {
            admitted: self.admitted.load(Ordering::Relaxed),
            full: self.full.load(Ordering::Relaxed),
            closed: self.closed.load(Ordering::Relaxed),
            request_too_large: self.request_too_large.load(Ordering::Relaxed),
            invalid: self.invalid.load(Ordering::Relaxed),
            responses: self.responses.load(Ordering::Relaxed),
            timeout: self.timeout.load(Ordering::Relaxed),
            response_too_large: self.response_too_large.load(Ordering::Relaxed),
            reqwest_error: self.reqwest_error.load(Ordering::Relaxed),
            internal_error: self.internal_error.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
            current_in_flight: self.current_in_flight.load(Ordering::Relaxed),
            in_flight_high_water: self.in_flight_high_water.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn set_in_flight(&self, current: u64) {
        self.current_in_flight.store(current, Ordering::Relaxed);
    }

    pub(crate) fn note_in_flight(&self, current: u64) {
        let mut prev = self.in_flight_high_water.load(Ordering::Relaxed);
        while current > prev {
            match self.in_flight_high_water.compare_exchange(
                prev,
                current,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => prev = observed,
            }
        }
    }
}

/// Cloneable handle for inspecting bridge metrics from outside the
/// hosting Tina runtime.
#[derive(Debug, Clone)]
pub struct ReqwestMetricsHandle {
    pub(crate) inner: Arc<MetricsInner>,
}

impl ReqwestMetricsHandle {
    /// Returns a fresh snapshot of counter values.
    pub fn snapshot(&self) -> ReqwestMetrics {
        self.inner.snapshot()
    }
}
