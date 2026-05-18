//! Body-pressure counters for HTTP/1.1 connections.
//!
//! Counters are per shard. `with_body_capacity` exposes request and
//! response body bytes as weighted capacity surfaces sharing one
//! shard-local scope.
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
//! - Heap memory. "Current" is body-byte weight the connection has
//!   admitted into its body buffers. It is not allocator truth.
//! - Cross-shard totals. One [`BodyMetrics`] is one shard. Multi
//!   shard services register one per shard and merge snapshots.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use tina::capacity::{CapacityMode, CapacitySurfaceReport, CapacityWeight};

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
    scope_name: Option<String>,
    local_body_weight_cap: Option<usize>,
    shared_body_weight_cap: Option<usize>,
    shared_body_high_water: AtomicUsize,
    request_body_current: AtomicUsize,
    request_body_high_water: AtomicUsize,
    response_body_current: AtomicUsize,
    response_body_high_water: AtomicUsize,
    body_full_count: AtomicU64,
    request_weight_full_count: AtomicU64,
    response_weight_full_count: AtomicU64,
    shared_weight_full_count: AtomicU64,
    body_timeout_count: AtomicU64,
    body_io_error_count: AtomicU64,
}

/// Why weighted body admission failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyCapacityFull {
    /// Which cap filled: `request_body`, `response_body`, or the
    /// shared scope name.
    pub filled: String,
    /// Weight requested by the attempted admission.
    pub requested_weight: usize,
    /// Current weight before the attempted admission.
    pub current_weight: usize,
    /// Configured cap.
    pub max_weight: usize,
}

impl std::fmt::Display for BodyCapacityFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "HTTP body capacity full: filled={} requested_weight={} current_weight={} max_weight={}",
            self.filled, self.requested_weight, self.current_weight, self.max_weight
        )
    }
}

impl std::error::Error for BodyCapacityFull {}

#[derive(Debug, Clone, Copy)]
struct BodyBytes(usize);

impl CapacityWeight for BodyBytes {
    fn capacity_weight(&self) -> usize {
        self.0
    }
}

impl BodyMetrics {
    /// Builds a fresh metrics instance with all counters at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds metrics with a shard-local weighted scope shared by
    /// request and response body surfaces.
    ///
    /// `local_body_weight_cap` is applied independently to request
    /// and response resident body bytes. `shared_body_weight_cap`
    /// is applied to their aggregate on this one `BodyMetrics`
    /// instance, which callers share only within one shard.
    pub fn with_body_capacity(
        scope_name: impl Into<String>,
        local_body_weight_cap: usize,
        shared_body_weight_cap: usize,
    ) -> Self {
        Self {
            inner: Arc::new(BodyMetricsInner {
                scope_name: Some(scope_name.into()),
                local_body_weight_cap: Some(local_body_weight_cap),
                shared_body_weight_cap: Some(shared_body_weight_cap),
                ..BodyMetricsInner::default()
            }),
        }
    }

    fn shared_current(&self) -> usize {
        self.inner.request_body_current.load(Ordering::Relaxed)
            + self.inner.response_body_current.load(Ordering::Relaxed)
    }

    fn check_admit(
        &self,
        local_name: &'static str,
        local_current: &AtomicUsize,
        local_full_count: &AtomicU64,
        weight: usize,
    ) -> Result<(), BodyCapacityFull> {
        if let Some(max) = self.inner.local_body_weight_cap {
            let current = local_current.load(Ordering::Relaxed);
            if current.saturating_add(weight) > max {
                local_full_count.fetch_add(1, Ordering::Relaxed);
                return Err(BodyCapacityFull {
                    filled: local_name.to_string(),
                    requested_weight: weight,
                    current_weight: current,
                    max_weight: max,
                });
            }
        }
        if let Some(max) = self.inner.shared_body_weight_cap {
            let current = self.shared_current();
            if current.saturating_add(weight) > max {
                self.inner
                    .shared_weight_full_count
                    .fetch_add(1, Ordering::Relaxed);
                return Err(BodyCapacityFull {
                    filled: self
                        .inner
                        .scope_name
                        .clone()
                        .unwrap_or_else(|| "http.bodies".to_string()),
                    requested_weight: weight,
                    current_weight: current,
                    max_weight: max,
                });
            }
        }
        Ok(())
    }

    /// Attempts to charge inbound body bytes against local and
    /// shared weighted caps.
    pub fn try_charge_request(&self, n: usize) -> Result<(), BodyCapacityFull> {
        let weight = BodyBytes(n).capacity_weight();
        self.check_admit(
            "request_body",
            &self.inner.request_body_current,
            &self.inner.request_weight_full_count,
            weight,
        )?;
        self.charge_request(n);
        Ok(())
    }

    /// Charges `n` bytes of inbound body. High-water is bumped if
    /// the resulting current exceeds it.
    pub fn charge_request(&self, n: usize) {
        let prev = self
            .inner
            .request_body_current
            .fetch_add(n, Ordering::Relaxed);
        bump_high_water(&self.inner.request_body_high_water, prev + n);
        bump_high_water(&self.inner.shared_body_high_water, self.shared_current());
    }

    /// Releases `n` bytes of inbound body. Saturates at zero so a
    /// double-release does not wrap. In practice each [`BodyMetrics`]
    /// is owned by one shard's isolates, which run single-threaded;
    /// the CAS loop here is for correctness, not contention.
    pub fn release_request(&self, n: usize) {
        saturating_sub(&self.inner.request_body_current, n);
    }

    /// Attempts to charge outbound body bytes against local and
    /// shared weighted caps.
    pub fn try_charge_response(&self, n: usize) -> Result<(), BodyCapacityFull> {
        let weight = BodyBytes(n).capacity_weight();
        self.check_admit(
            "response_body",
            &self.inner.response_body_current,
            &self.inner.response_weight_full_count,
            weight,
        )?;
        self.charge_response(n);
        Ok(())
    }

    /// Charges `n` bytes of outbound body.
    pub fn charge_response(&self, n: usize) {
        let prev = self
            .inner
            .response_body_current
            .fetch_add(n, Ordering::Relaxed);
        bump_high_water(&self.inner.response_body_high_water, prev + n);
        bump_high_water(&self.inner.shared_body_high_water, self.shared_current());
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
            scope_name: self.inner.scope_name.clone(),
            local_body_weight_cap: self.inner.local_body_weight_cap,
            shared_body_weight_cap: self.inner.shared_body_weight_cap,
            shared_body_high_water: self.inner.shared_body_high_water.load(Ordering::Relaxed),
            request_body_current: self.inner.request_body_current.load(Ordering::Relaxed),
            request_body_high_water: self.inner.request_body_high_water.load(Ordering::Relaxed),
            response_body_current: self.inner.response_body_current.load(Ordering::Relaxed),
            response_body_high_water: self.inner.response_body_high_water.load(Ordering::Relaxed),
            body_full_count: self.inner.body_full_count.load(Ordering::Relaxed),
            request_weight_full_count: self.inner.request_weight_full_count.load(Ordering::Relaxed),
            response_weight_full_count: self
                .inner
                .response_weight_full_count
                .load(Ordering::Relaxed),
            shared_weight_full_count: self.inner.shared_weight_full_count.load(Ordering::Relaxed),
            body_timeout_count: self.inner.body_timeout_count.load(Ordering::Relaxed),
            body_io_error_count: self.inner.body_io_error_count.load(Ordering::Relaxed),
        }
    }
}

/// Snapshot of body-pressure counters at a point in time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyPressureReport {
    /// Shard-local shared scope name for request and response body
    /// surfaces.
    pub scope_name: Option<String>,
    /// Per-surface body weight cap.
    pub local_body_weight_cap: Option<usize>,
    /// Shared aggregate body weight cap.
    pub shared_body_weight_cap: Option<usize>,
    /// Shared aggregate body high water.
    pub shared_body_high_water: usize,
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
    /// Number of inbound weighted admissions rejected by the local cap.
    pub request_weight_full_count: u64,
    /// Number of outbound weighted admissions rejected by the local cap.
    pub response_weight_full_count: u64,
    /// Number of weighted admissions rejected by the shared scope.
    pub shared_weight_full_count: u64,
    /// Number of body chunk read/write timeouts.
    pub body_timeout_count: u64,
    /// Number of non-timeout body IO errors (truncations).
    pub body_io_error_count: u64,
}

impl BodyPressureReport {
    /// Convert inbound body bytes into a weighted capacity surface.
    pub fn request_capacity_report(
        &self,
        name: impl Into<String>,
        mode: CapacityMode,
    ) -> Option<CapacitySurfaceReport> {
        self.local_body_weight_cap.map(|max| {
            self.with_shared(CapacitySurfaceReport::weighted(
                name,
                mode,
                max,
                self.request_body_current,
                self.request_body_high_water,
                self.request_weight_full_count,
                "bytes",
            ))
        })
    }

    /// Convert outbound body bytes into a weighted capacity surface.
    pub fn response_capacity_report(
        &self,
        name: impl Into<String>,
        mode: CapacityMode,
    ) -> Option<CapacitySurfaceReport> {
        self.local_body_weight_cap.map(|max| {
            self.with_shared(CapacitySurfaceReport::weighted(
                name,
                mode,
                max,
                self.response_body_current,
                self.response_body_high_water,
                self.response_weight_full_count,
                "bytes",
            ))
        })
    }

    fn with_shared(&self, report: CapacitySurfaceReport) -> CapacitySurfaceReport {
        match (&self.scope_name, self.shared_body_weight_cap) {
            (Some(scope), Some(max)) => report.with_shared_scope(
                scope.clone(),
                max,
                self.request_body_current + self.response_body_current,
                self.shared_body_high_water,
                self.shared_weight_full_count,
            ),
            _ => report,
        }
    }

    /// True iff every counter is zero. Useful as a "no resource leak"
    /// terminal assertion: after shutdown all `current` values must
    /// be zero.
    pub fn is_empty(&self) -> bool {
        self.request_body_current == 0
            && self.request_body_high_water == 0
            && self.response_body_current == 0
            && self.response_body_high_water == 0
            && self.body_full_count == 0
            && self.request_weight_full_count == 0
            && self.response_weight_full_count == 0
            && self.shared_weight_full_count == 0
            && self.body_timeout_count == 0
            && self.body_io_error_count == 0
    }

    /// True iff every "current" counter is zero. Use this after
    /// graceful shutdown to assert no body resource leaked.
    pub fn drained(&self) -> bool {
        self.request_body_current == 0 && self.response_body_current == 0
    }

    /// Convert both request and response body pressure into a vector of
    /// [`CapacitySurfaceReport`]s for direct feeding into
    /// [`tina_runtime::CapacitySummary`].
    ///
    /// Names are `"<prefix>.body.request"` and `"<prefix>.body.response"`.
    /// When the underlying [`BodyMetrics`] was built without a body weight
    /// cap the vector is empty: there are no weighted bytes to surface.
    /// Use [`Self::service_surfaces`] for the
    /// [`tina_runtime::ServicePressureBuilder`] path that names missing
    /// surfaces explicitly.
    pub fn capacity_surfaces(
        &self,
        prefix: &str,
        mode: CapacityMode,
    ) -> Vec<CapacitySurfaceReport> {
        let mut out = Vec::with_capacity(2);
        if let Some(req) =
            self.request_capacity_report(format!("{prefix}.body.request"), mode.clone())
        {
            out.push(req);
        }
        if let Some(resp) = self.response_capacity_report(format!("{prefix}.body.response"), mode) {
            out.push(resp);
        }
        out
    }

    /// Same as [`Self::capacity_surfaces`] but wrapped as
    /// [`tina_runtime::ServicePressureSurface`] entries so they slot into a
    /// service pressure report with one builder call.
    ///
    /// When the underlying [`BodyMetrics`] was built without a body weight
    /// cap both surfaces are emitted as
    /// [`tina_runtime::ServiceSurfaceState::Unavailable`] with a typed
    /// reason rather than silently omitted, matching the rule that missing
    /// surfaces must be declared explicitly.
    pub fn service_surfaces(
        &self,
        prefix: &str,
        kind: tina_runtime::service_pressure::SurfaceKind,
        mode: CapacityMode,
    ) -> Vec<tina_runtime::ServicePressureSurface> {
        use tina_runtime::ServicePressureSurface;
        let mut out = Vec::with_capacity(2);
        let req_name = format!("{prefix}.body.request");
        match self.request_capacity_report(req_name.clone(), mode.clone()) {
            Some(report) => out.push(ServicePressureSurface::measured(req_name, kind, report)),
            None => out.push(ServicePressureSurface::unavailable(
                req_name,
                kind,
                "no body weight cap configured",
            )),
        }
        let resp_name = format!("{prefix}.body.response");
        match self.response_capacity_report(resp_name.clone(), mode) {
            Some(report) => out.push(ServicePressureSurface::measured(resp_name, kind, report)),
            None => out.push(ServicePressureSurface::unavailable(
                resp_name,
                kind,
                "no body weight cap configured",
            )),
        }
        out
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

    #[test]
    fn small_weighted_payload_fits_and_reports_weight() {
        let metrics = BodyMetrics::with_body_capacity("http.bodies", 100, 150);
        metrics.try_charge_request(40).unwrap();
        let snap = metrics.snapshot();
        assert_eq!(snap.request_body_current, 40);
        assert_eq!(snap.shared_body_high_water, 40);
        let report = snap
            .request_capacity_report("http.request", CapacityMode::Fixed)
            .expect("weighted report enabled");
        assert_eq!(report.max_weight, Some(100));
        assert_eq!(report.current_weight, Some(40));
        assert_eq!(report.weight_unit.as_deref(), Some("bytes"));
        assert_eq!(report.shared_scope.as_deref(), Some("http.bodies"));
    }

    #[test]
    fn oversized_weighted_payload_rejects_with_weight_reason() {
        let metrics = BodyMetrics::with_body_capacity("http.bodies", 100, 150);
        let err = metrics.try_charge_request(101).unwrap_err();
        assert_eq!(err.filled, "request_body");
        assert_eq!(err.requested_weight, 101);
        assert_eq!(err.current_weight, 0);
        assert_eq!(err.max_weight, 100);
        let snap = metrics.snapshot();
        assert_eq!(snap.request_weight_full_count, 1);
        assert_eq!(snap.request_body_current, 0);
    }

    #[test]
    fn shared_aggregate_fills_while_local_caps_are_okay() {
        let metrics = BodyMetrics::with_body_capacity("http.bodies", 100, 150);
        metrics.try_charge_request(90).unwrap();
        let err = metrics.try_charge_response(70).unwrap_err();
        assert_eq!(err.filled, "http.bodies");
        assert_eq!(err.current_weight, 90);
        assert_eq!(err.max_weight, 150);
        let snap = metrics.snapshot();
        assert_eq!(snap.request_body_current, 90);
        assert_eq!(snap.response_body_current, 0);
        assert_eq!(snap.response_weight_full_count, 0);
        assert_eq!(snap.shared_weight_full_count, 1);
        let response = snap
            .response_capacity_report("http.response", CapacityMode::Fixed)
            .expect("weighted report enabled");
        assert_eq!(response.shared_weight_full_count, 1);
    }

    #[test]
    fn release_after_weighted_charge_lets_new_work_admit() {
        let metrics = BodyMetrics::with_body_capacity("http.bodies", 100, 100);
        metrics.try_charge_request(100).unwrap();
        assert!(metrics.try_charge_response(1).is_err());
        metrics.release_request(100);
        metrics.try_charge_response(100).unwrap();
        let snap = metrics.snapshot();
        assert_eq!(snap.request_body_current, 0);
        assert_eq!(snap.response_body_current, 100);
        assert_eq!(snap.shared_weight_full_count, 1);
    }
}
