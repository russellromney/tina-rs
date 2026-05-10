//! Body-pressure counters for HTTP/1.1 connections.
//!
//! Counters are per shard. A shared capacity-report shape lives in
//! a separate slice; when it lands these fields fold into it.
//!
//! # What is counted
//!
//! - `request_body_current` / `request_body_high_water` — inbound
//!   body bytes resident in connection isolates right now, and the
//!   peak ever seen. Resident means "read from the socket, not yet
//!   handed to the service". Buffered requests hold the whole
//!   declared length between read-complete and dispatch; streaming
//!   requests hold whatever is in the chunk buffer between socket
//!   read and service pull.
//! - `response_body_current` / `response_body_high_water` — same
//!   for outbound bytes. Charged when a chunk is queued for the
//!   wire write, released as the runtime drains it.
//! - `body_full_count` — bodies rejected for exceeding
//!   [`crate::HttpLimits::max_body_bytes`]. Charge at the
//!   parser, before any service dispatch.
//! - `body_timeout_count` — body chunk read/write timeouts and
//!   timed-out source pulls.
//! - `body_io_error_count` — non-timeout body IO errors. A
//!   positive value means at least one client saw a short body.
//!
//! # What is not counted
//!
//! - Heap memory. "Current" is bytes the connection has admitted
//!   into its own buffers, plus whatever sits inside the runtime
//!   call payload. Real heap footprint is at least this much.
//! - Cross-shard totals. One [`BodyMetrics`] is one shard. Multi
//!   shard services register one per shard and merge snapshots.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

// CAS loop that monotonically pushes `target` up to `new`. Bounded
// by contention: each retry observes a fresh value, so under N
// threads the worst case is N retries.
fn bump_high_water(target: &AtomicUsize, new: usize) {
    let mut hw = target.load(Ordering::Relaxed);
    while new > hw {
        match target.compare_exchange_weak(hw, new, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(observed) => hw = observed,
        }
    }
}

// Atomic saturating sub. CAS loop because `fetch_sub` would wrap
// on underflow. Bounded by contention.
fn saturating_sub(target: &AtomicUsize, n: usize) {
    let mut cur = target.load(Ordering::Relaxed);
    loop {
        let next = cur.saturating_sub(n);
        match target.compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(observed) => cur = observed,
        }
    }
}

/// Shard-local body-pressure counters. Cheap to clone (`Arc`-backed).
///
/// One instance is shared between an [`crate::HttpListener`] (or
/// [`crate::HttpsListener`]) and every [`crate::HttpConnection`] it
/// spawns. Connections charge bytes on admission and release on
/// drain/drop; the listener exposes the aggregate via [`Self::snapshot`].
#[derive(Debug, Clone, Default)]
pub struct BodyMetrics {
    inner: Arc<BodyMetricsInner>,
}

#[derive(Debug, Default)]
struct BodyMetricsInner {
    request_body_current: AtomicUsize,
    request_body_high_water: AtomicUsize,
    response_body_current: AtomicUsize,
    response_body_high_water: AtomicUsize,
    body_full_count: AtomicU64,
    body_timeout_count: AtomicU64,
    body_io_error_count: AtomicU64,
}

impl BodyMetrics {
    /// Builds a fresh metrics instance with all counters at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Charges `n` bytes of inbound body. High-water is bumped if
    /// the resulting current exceeds it.
    pub fn charge_request(&self, n: usize) {
        let prev = self
            .inner
            .request_body_current
            .fetch_add(n, Ordering::Relaxed);
        bump_high_water(&self.inner.request_body_high_water, prev + n);
    }

    /// Releases `n` bytes of inbound body. Saturates at zero so a
    /// double-release does not wrap. In practice each [`BodyMetrics`]
    /// is owned by one shard's isolates, which run single-threaded;
    /// the CAS loop here is for correctness, not contention.
    pub fn release_request(&self, n: usize) {
        saturating_sub(&self.inner.request_body_current, n);
    }

    /// Charges `n` bytes of outbound body.
    pub fn charge_response(&self, n: usize) {
        let prev = self
            .inner
            .response_body_current
            .fetch_add(n, Ordering::Relaxed);
        bump_high_water(&self.inner.response_body_high_water, prev + n);
    }

    /// Releases `n` bytes of outbound body. Saturates at zero.
    pub fn release_response(&self, n: usize) {
        saturating_sub(&self.inner.response_body_current, n);
    }

    /// Increments the cap-full counter. Use when the parser rejects
    /// a request for declared `Content-Length` greater than
    /// [`crate::HttpLimits::max_body_bytes`]. The cap is checked once
    /// at parse time; this counter does not fire later in the body
    /// stream.
    pub fn record_body_full(&self) {
        self.inner.body_full_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Increments the timeout counter. Use when a body chunk
    /// read/write surfaces `CallError::Timeout`, or when the
    /// service's outer call into the chunk source times out.
    pub fn record_body_timeout(&self) {
        self.inner
            .body_timeout_count
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Increments the IO-error counter. Use when a body chunk
    /// read/write surfaces a non-timeout `CallError` (truncation).
    pub fn record_body_io_error(&self) {
        self.inner
            .body_io_error_count
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Reads a coherent snapshot. "Coherent" here is best-effort:
    /// each counter is read atomically but the snapshot is not
    /// taken under a lock, so a concurrent charge/release may make
    /// `current + recent release` differ slightly from `high_water`.
    /// For testing assertions in single-threaded scenarios the
    /// snapshot is exact.
    pub fn snapshot(&self) -> BodyPressureReport {
        BodyPressureReport {
            request_body_current: self.inner.request_body_current.load(Ordering::Relaxed),
            request_body_high_water: self.inner.request_body_high_water.load(Ordering::Relaxed),
            response_body_current: self.inner.response_body_current.load(Ordering::Relaxed),
            response_body_high_water: self.inner.response_body_high_water.load(Ordering::Relaxed),
            body_full_count: self.inner.body_full_count.load(Ordering::Relaxed),
            body_timeout_count: self.inner.body_timeout_count.load(Ordering::Relaxed),
            body_io_error_count: self.inner.body_io_error_count.load(Ordering::Relaxed),
        }
    }
}

/// Snapshot of body-pressure counters at a point in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyPressureReport {
    /// Inbound body bytes resident in connection isolates right now.
    pub request_body_current: usize,
    /// Maximum `request_body_current` ever observed since startup.
    pub request_body_high_water: usize,
    /// Outbound body bytes pending write right now.
    pub response_body_current: usize,
    /// Maximum `response_body_current` ever observed since startup.
    pub response_body_high_water: usize,
    /// Number of bodies rejected for exceeding `max_body_bytes`.
    pub body_full_count: u64,
    /// Number of body chunk read/write timeouts.
    pub body_timeout_count: u64,
    /// Number of non-timeout body IO errors (truncations).
    pub body_io_error_count: u64,
}

impl BodyPressureReport {
    /// True iff every counter is zero. Useful as a "no resource leak"
    /// terminal assertion: after shutdown all `current` values must
    /// be zero.
    pub fn is_empty(&self) -> bool {
        self.request_body_current == 0
            && self.request_body_high_water == 0
            && self.response_body_current == 0
            && self.response_body_high_water == 0
            && self.body_full_count == 0
            && self.body_timeout_count == 0
            && self.body_io_error_count == 0
    }

    /// True iff every "current" counter is zero. Use this after
    /// graceful shutdown to assert no body resource leaked.
    pub fn drained(&self) -> bool {
        self.request_body_current == 0 && self.response_body_current == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charge_release_round_trip_zeros_current_and_keeps_high_water() {
        let metrics = BodyMetrics::new();
        metrics.charge_request(64);
        metrics.charge_request(96);
        let after_charges = metrics.snapshot();
        assert_eq!(after_charges.request_body_current, 160);
        assert_eq!(after_charges.request_body_high_water, 160);

        metrics.release_request(64);
        metrics.release_request(96);
        let after_releases = metrics.snapshot();
        assert_eq!(after_releases.request_body_current, 0);
        assert_eq!(
            after_releases.request_body_high_water, 160,
            "high water is monotonic"
        );
    }

    #[test]
    fn high_water_tracks_the_largest_intermediate_total() {
        let metrics = BodyMetrics::new();
        metrics.charge_request(100);
        metrics.charge_request(50); // current=150
        metrics.release_request(100); // current=50
        metrics.charge_request(40); // current=90, but never exceeds 150
        let snap = metrics.snapshot();
        assert_eq!(snap.request_body_current, 90);
        assert_eq!(snap.request_body_high_water, 150);
    }

    #[test]
    fn release_does_not_underflow() {
        let metrics = BodyMetrics::new();
        metrics.release_request(usize::MAX);
        let snap = metrics.snapshot();
        assert_eq!(
            snap.request_body_current, 0,
            "saturating release must clamp at zero, not wrap"
        );
    }

    #[test]
    fn full_timeout_io_counters_increment_independently() {
        let metrics = BodyMetrics::new();
        metrics.record_body_full();
        metrics.record_body_full();
        metrics.record_body_timeout();
        metrics.record_body_io_error();
        let snap = metrics.snapshot();
        assert_eq!(snap.body_full_count, 2);
        assert_eq!(snap.body_timeout_count, 1);
        assert_eq!(snap.body_io_error_count, 1);
    }

    #[test]
    fn empty_and_drained_helpers() {
        let metrics = BodyMetrics::new();
        assert!(metrics.snapshot().is_empty());

        metrics.charge_request(10);
        let mid = metrics.snapshot();
        assert!(!mid.is_empty());
        assert!(!mid.drained());

        metrics.release_request(10);
        let after = metrics.snapshot();
        assert!(
            after.drained(),
            "drained ignores high-water and counts; only currents must be zero"
        );
        assert!(
            !after.is_empty(),
            "is_empty includes high-water; drained does not"
        );
    }
}
