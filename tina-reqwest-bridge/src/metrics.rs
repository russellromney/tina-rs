//! Bridge metrics. Plain atomic counters; no time-series, no labels.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Snapshot of bridge counters.
///
/// All counters are monotonic and reset only when a new
/// [`crate::ReqwestWorker`] is built.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReqwestMetrics {
    /// Send requests admitted into the worker (handler reached
    /// in-flight bookkeeping).
    pub admitted: u64,
    /// Send requests rejected because `max_in_flight` was reached.
    pub full: u64,
    /// Send requests rejected because the worker was closed.
    pub closed: u64,
    /// Requests rejected before reqwest because the body exceeded
    /// `request_body_limit`.
    pub request_too_large: u64,
    /// Requests rejected by URL/method validation before reqwest saw
    /// them.
    pub invalid: u64,
    /// Reqwest tasks that completed with a successful response.
    pub responses: u64,
    /// Reqwest tasks that produced [`crate::ReqwestError::Timeout`].
    pub timeout: u64,
    /// Reqwest tasks that produced
    /// [`crate::ReqwestError::ResponseTooLarge`].
    pub response_too_large: u64,
    /// Reqwest tasks that produced [`crate::ReqwestError::Reqwest`]
    /// (any other reqwest-side error).
    pub reqwest_error: u64,
    /// Reqwest results that were discarded because the call had
    /// already exceeded its per-request timeout when the result
    /// finally arrived. Counts late results — see the dropped-caller
    /// rule in the crate docs.
    pub late_results: u64,
    /// Retry attempts taken (counts the *retries*, not the first
    /// attempt). Always `0` when [`crate::RetryPolicy::None`].
    pub retries: u64,
    /// In-flight reqwest tasks at the most recent admission or
    /// completion.
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
    pub(crate) late_results: AtomicU64,
    pub(crate) retries: AtomicU64,
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
            late_results: self.late_results.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
            in_flight_high_water: self.in_flight_high_water.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn note_in_flight(&self, current: u64) {
        // Monotonic max via fetch_max would race-free; spell it out so
        // we do not spuriously bump the high-water mark when current
        // shrinks.
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
