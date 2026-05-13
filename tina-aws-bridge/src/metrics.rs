//! Bridge metrics. Plain atomic counters; no time-series, no labels.
//!
//! Counters are worker-terminal unless their docs say "rejected at
//! admission." They do not describe what a Tina caller saw after an
//! outer `CallOutcome::Timeout`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Snapshot of bridge counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct S3Metrics {
    /// Admitted into SDK work.
    pub admitted: u64,
    /// Rejected at admission because `max_in_flight` saturated.
    pub full: u64,
    /// Rejected at admission because the worker is closed.
    pub closed: u64,
    /// Rejected at admission due to request validation.
    pub invalid: u64,
    /// Rejected at admission because the request body was too large.
    pub request_too_large: u64,
    /// Bridge per-operation timeouts.
    pub timeouts: u64,
    /// SDK futures that completed successfully.
    pub responses: u64,
    /// SDK futures that ended with an SDK error.
    pub sdk_errors: u64,
    /// SDK futures whose response stream failed while reading.
    pub body_errors: u64,
    /// SDK get-object responses rejected by response body cap.
    pub response_too_large: u64,
    /// SDK future terminal after the bridge already returned timeout.
    pub late_results: u64,
    /// Current in-flight SDK futures.
    pub in_flight_current: u64,
    /// Highest `in_flight_current` observed.
    pub in_flight_high_water: u64,
    /// Configured SDK attempts per admitted operation.
    ///
    /// `0` means unknown because the bridge was built around a
    /// caller-supplied S3 client. In that path, SDK retry settings
    /// live entirely on the supplied client.
    pub sdk_max_attempts: u64,
}

#[derive(Debug, Default)]
pub(crate) struct MetricsInner {
    pub(crate) admitted: AtomicU64,
    pub(crate) full: AtomicU64,
    pub(crate) closed: AtomicU64,
    pub(crate) invalid: AtomicU64,
    pub(crate) request_too_large: AtomicU64,
    pub(crate) timeouts: AtomicU64,
    pub(crate) responses: AtomicU64,
    pub(crate) sdk_errors: AtomicU64,
    pub(crate) body_errors: AtomicU64,
    pub(crate) response_too_large: AtomicU64,
    pub(crate) late_results: AtomicU64,
    pub(crate) in_flight_current: AtomicU64,
    pub(crate) in_flight_high_water: AtomicU64,
    pub(crate) sdk_max_attempts: AtomicU64,
    in_flight_by_kind: Mutex<HashMap<&'static str, u64>>,
}

impl MetricsInner {
    pub(crate) fn snapshot(&self) -> S3Metrics {
        S3Metrics {
            admitted: self.admitted.load(Ordering::Relaxed),
            full: self.full.load(Ordering::Relaxed),
            closed: self.closed.load(Ordering::Relaxed),
            invalid: self.invalid.load(Ordering::Relaxed),
            request_too_large: self.request_too_large.load(Ordering::Relaxed),
            timeouts: self.timeouts.load(Ordering::Relaxed),
            responses: self.responses.load(Ordering::Relaxed),
            sdk_errors: self.sdk_errors.load(Ordering::Relaxed),
            body_errors: self.body_errors.load(Ordering::Relaxed),
            response_too_large: self.response_too_large.load(Ordering::Relaxed),
            late_results: self.late_results.load(Ordering::Relaxed),
            in_flight_current: self.in_flight_current.load(Ordering::Relaxed),
            in_flight_high_water: self.in_flight_high_water.load(Ordering::Relaxed),
            sdk_max_attempts: self.sdk_max_attempts.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn set_in_flight(&self, current: u64) {
        self.in_flight_current.store(current, Ordering::Relaxed);
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

    pub(crate) fn note_admit_kind(&self, kind: &'static str) {
        let mut kinds = self.in_flight_by_kind.lock().expect("s3 metrics lock");
        *kinds.entry(kind).or_insert(0) += 1;
    }

    pub(crate) fn note_terminal_kind(&self, kind: &'static str) {
        let mut kinds = self.in_flight_by_kind.lock().expect("s3 metrics lock");
        if let Some(count) = kinds.get_mut(kind) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                kinds.remove(kind);
            }
        }
    }

    pub(crate) fn in_flight_kinds(&self) -> Vec<(&'static str, u64)> {
        let mut kinds: Vec<_> = self
            .in_flight_by_kind
            .lock()
            .expect("s3 metrics lock")
            .iter()
            .map(|(kind, count)| (*kind, *count))
            .collect();
        kinds.sort_by_key(|(kind, _)| *kind);
        kinds
    }
}

/// Pressure report for the bridge's `max_in_flight` capacity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct S3PressureReport {
    /// Configured capacity.
    pub capacity: usize,
    /// Currently available slots.
    pub available: usize,
    /// Currently leased slots.
    pub leased: usize,
    /// Waiters accepted by the bridge. Always `0`; callers are
    /// rejected with `S3Error::Full` instead of queued behind a slot.
    pub waiters: usize,
    /// Max waiters. Always `0`.
    pub max_waiters: usize,
    /// Cumulative full rejections.
    pub full_count: u64,
    /// Cumulative closed rejections.
    pub closed_count: u64,
    /// Cumulative bridge timeouts.
    pub timeout_count: u64,
    /// Cumulative late results.
    pub late_result_count: u64,
    /// Highest leased count observed.
    pub high_water: u64,
}

/// Cloneable metrics handle.
#[derive(Debug, Clone)]
pub struct S3MetricsHandle {
    pub(crate) inner: Arc<MetricsInner>,
    pub(crate) capacity: usize,
}

impl S3MetricsHandle {
    /// Returns a fresh snapshot.
    pub fn snapshot(&self) -> S3Metrics {
        self.inner.snapshot()
    }

    /// Returns pressure mapped to bounded-pool vocabulary.
    pub fn pressure_report(&self) -> S3PressureReport {
        let m = self.inner.snapshot();
        let leased = m.in_flight_current.min(self.capacity as u64) as usize;
        S3PressureReport {
            capacity: self.capacity,
            available: self.capacity.saturating_sub(leased),
            leased,
            waiters: 0,
            max_waiters: 0,
            full_count: m.full,
            closed_count: m.closed,
            timeout_count: m.timeouts,
            late_result_count: m.late_results,
            high_water: m.in_flight_high_water,
        }
    }
}
