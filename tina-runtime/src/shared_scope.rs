//! Shard-local shared weighted capacity scopes.
//!
//! A `SharedCapacityScope` is one cap shared by many surfaces on one
//! shard. Each admission charges a user-declared weight (count by
//! default; callers can charge bytes, rows, jobs, anything). Charges
//! are released when the returned [`SharedLease`] is dropped, or
//! when an owner stops and drops every lease it held.
//!
//! Not magic: heap is not measured, threads do not coordinate. The
//! scope is one shard's promise about how much weight it will admit
//! at once.
//!
//! Reports plug into [`CapacitySummary`](crate::capacity::CapacitySummary)
//! via [`SharedCapacityScope::surface_report`] or
//! [`SharedCapacityScope::decorate`], which decorates an existing
//! weighted surface report.
//!
//! The scope itself can be reported by name as its own count-style
//! surface (`weight=` shows up under `weight_unit=...`).
//!
//! ```ignore
//! use tina_runtime::{SharedCapacityScope, CapacitySummary, format_discovery_line};
//! use tina::capacity::CapacityMode;
//!
//! let scope = SharedCapacityScope::new("api.in_flight", "requests", 4);
//! let lease = scope.try_admit(1).expect("cap=4, was 0");
//! assert_eq!(scope.snapshot().current, 1);
//! drop(lease);
//! assert_eq!(scope.snapshot().current, 0);
//!
//! let mut summary = CapacitySummary::new();
//! summary
//!     .push(scope.surface_report(CapacityMode::Fixed))
//!     .unwrap();
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use tina::capacity::{CapacityMode, CapacitySurfaceReport};

/// Why [`SharedCapacityScope::try_admit`] rejected an admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedScopeFull {
    /// Scope name, e.g. `gateway.in_flight`.
    pub scope: String,
    /// Weight the caller asked to charge.
    pub requested: usize,
    /// Current weight at admission time.
    pub current: usize,
    /// Configured cap.
    pub max: usize,
}

impl std::fmt::Display for SharedScopeFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "shared capacity scope {:?} full: requested={} current={} max={}",
            self.scope, self.requested, self.current, self.max
        )
    }
}

impl std::error::Error for SharedScopeFull {}

/// Snapshot of a [`SharedCapacityScope`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedScopeReport {
    /// Scope name.
    pub scope: String,
    /// Weight unit, e.g. `requests`, `bytes`, `rows`.
    pub unit: String,
    /// Configured cap.
    pub max: usize,
    /// Live charged weight.
    pub current: usize,
    /// Highest `current` ever observed.
    pub high_water: usize,
    /// Cumulative admissions that returned [`SharedScopeFull`].
    pub full_count: u64,
    /// Cumulative weight released (sum of every lease's weight after drop).
    pub released: u64,
    /// Cumulative weight admitted.
    pub admitted: u64,
}

impl SharedScopeReport {
    /// True iff this scope is empty *and* never filled.
    pub fn drained_clean(&self) -> bool {
        self.current == 0 && self.full_count == 0
    }
}

/// Shard-local shared weighted capacity scope.
///
/// Cheap to clone. Every clone shares one cap.
#[derive(Debug, Clone)]
pub struct SharedCapacityScope {
    inner: Arc<SharedScopeInner>,
}

#[derive(Debug)]
struct SharedScopeInner {
    scope: String,
    unit: String,
    max: usize,
    current: AtomicUsize,
    high_water: AtomicUsize,
    full_count: AtomicU64,
    released: AtomicU64,
    admitted: AtomicU64,
}

impl SharedCapacityScope {
    /// Builds a scope with `max` cap and `unit` label.
    ///
    /// `name` is the discovery name (`gateway.in_flight`,
    /// `gateway.upstream_bytes`, etc). `unit` is what one weight unit
    /// counts (`requests`, `bytes`, `rows`).
    pub fn new(name: impl Into<String>, unit: impl Into<String>, max: usize) -> Self {
        Self {
            inner: Arc::new(SharedScopeInner {
                scope: name.into(),
                unit: unit.into(),
                max,
                current: AtomicUsize::new(0),
                high_water: AtomicUsize::new(0),
                full_count: AtomicU64::new(0),
                released: AtomicU64::new(0),
                admitted: AtomicU64::new(0),
            }),
        }
    }

    /// Scope name.
    pub fn name(&self) -> &str {
        &self.inner.scope
    }

    /// Unit label.
    pub fn unit(&self) -> &str {
        &self.inner.unit
    }

    /// Configured cap.
    pub fn max(&self) -> usize {
        self.inner.max
    }

    /// Try to charge `weight`. Returns a lease that releases the
    /// charge on drop, or [`SharedScopeFull`] if the cap would be
    /// exceeded.
    ///
    /// `weight == 0` is a no-op admission: the lease still releases
    /// zero on drop, and `admitted` does not increment.
    pub fn try_admit(&self, weight: usize) -> Result<SharedLease, SharedScopeFull> {
        if weight == 0 {
            return Ok(SharedLease {
                scope: self.clone(),
                weight: 0,
            });
        }
        // Two-phase admission. Phase 1 CAS-walks toward a successful
        // admission. If a CAS fails with `cur + weight > max`, we
        // *re-load* with Acquire ordering and only then declare Full;
        // a concurrent release may have freed room since the stale
        // observation, in which case we keep retrying. This keeps
        // `full_count` honest under contention: every counted Full
        // corresponds to a moment when the scope truly had no room.
        let mut cur = self.inner.current.load(Ordering::Relaxed);
        loop {
            let next = cur.saturating_add(weight);
            if next > self.inner.max {
                // Re-observe with Acquire so a concurrent release is
                // visible. If room appeared, retry without counting.
                let fresh = self.inner.current.load(Ordering::Acquire);
                if fresh.saturating_add(weight) <= self.inner.max {
                    cur = fresh;
                    continue;
                }
                self.inner.full_count.fetch_add(1, Ordering::Relaxed);
                return Err(SharedScopeFull {
                    scope: self.inner.scope.clone(),
                    requested: weight,
                    current: fresh,
                    max: self.inner.max,
                });
            }
            match self.inner.current.compare_exchange_weak(
                cur,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.inner
                        .admitted
                        .fetch_add(weight as u64, Ordering::Relaxed);
                    bump_high_water(&self.inner.high_water, next);
                    return Ok(SharedLease {
                        scope: self.clone(),
                        weight,
                    });
                }
                Err(observed) => cur = observed,
            }
        }
    }

    fn release(&self, weight: usize) {
        if weight == 0 {
            return;
        }
        // Release ordering so the Acquire load in `try_admit`'s Full
        // re-check observes this decrement and avoids phantom Full.
        saturating_sub_release(&self.inner.current, weight);
        self.inner
            .released
            .fetch_add(weight as u64, Ordering::Relaxed);
    }

    /// Reads a coherent snapshot. Counters are individually atomic,
    /// the snapshot is not locked.
    pub fn snapshot(&self) -> SharedScopeReport {
        SharedScopeReport {
            scope: self.inner.scope.clone(),
            unit: self.inner.unit.clone(),
            max: self.inner.max,
            current: self.inner.current.load(Ordering::Relaxed),
            high_water: self.inner.high_water.load(Ordering::Relaxed),
            full_count: self.inner.full_count.load(Ordering::Relaxed),
            released: self.inner.released.load(Ordering::Relaxed),
            admitted: self.inner.admitted.load(Ordering::Relaxed),
        }
    }

    /// Report the scope itself as a capacity surface. Use this when
    /// the scope is the only surface to report. For surfaces that
    /// charge the scope, decorate their own report with
    /// [`Self::decorate`].
    pub fn surface_report(&self, mode: CapacityMode) -> CapacitySurfaceReport {
        let snap = self.snapshot();
        CapacitySurfaceReport::weighted(
            snap.scope.clone(),
            mode,
            snap.max,
            snap.current,
            snap.high_water,
            snap.full_count,
            snap.unit.clone(),
        )
    }

    /// Attach this scope's fields to an existing weighted surface
    /// report so the surface shows both its local and shared columns.
    pub fn decorate(&self, report: CapacitySurfaceReport) -> CapacitySurfaceReport {
        let snap = self.snapshot();
        report.with_shared_scope(
            snap.scope,
            snap.max,
            snap.current,
            snap.high_water,
            snap.full_count,
        )
    }

    /// One-line discovery summary for grep, mirroring the capacity
    /// discovery shape.
    ///
    /// ```text
    /// scope name=gateway.in_flight unit=requests max=4 cur=2 high=4 full=1 admitted=37 released=35
    /// ```
    pub fn discovery_line(&self) -> String {
        let snap = self.snapshot();
        format!(
            "scope name={name} unit={unit} max={max} cur={cur} high={high} full={full} admitted={admitted} released={released}",
            name = crate::capacity::discovery_value(&snap.scope),
            unit = crate::capacity::discovery_value(&snap.unit),
            max = snap.max,
            cur = snap.current,
            high = snap.high_water,
            full = snap.full_count,
            admitted = snap.admitted,
            released = snap.released,
        )
    }
}

/// One outstanding charge against a [`SharedCapacityScope`].
///
/// Drop releases the charge.
#[must_use = "dropping the lease releases the charge"]
#[derive(Debug)]
pub struct SharedLease {
    scope: SharedCapacityScope,
    weight: usize,
}

impl SharedLease {
    /// Weight this lease charges.
    pub fn weight(&self) -> usize {
        self.weight
    }

    /// Scope this lease was admitted against.
    pub fn scope(&self) -> &SharedCapacityScope {
        &self.scope
    }

    /// Releases early. Equivalent to `drop`.
    pub fn release(self) {
        // drop runs
    }
}

impl Drop for SharedLease {
    fn drop(&mut self) {
        self.scope.release(self.weight);
    }
}

fn bump_high_water(target: &AtomicUsize, new: usize) {
    let mut hw = target.load(Ordering::Relaxed);
    while new > hw {
        match target.compare_exchange_weak(hw, new, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(observed) => hw = observed,
        }
    }
}

fn saturating_sub_release(target: &AtomicUsize, n: usize) {
    let mut cur = target.load(Ordering::Relaxed);
    loop {
        let next = cur.saturating_sub(n);
        // Release on success so subsequent Acquire loads observe the
        // decrement before the admitter declares Full.
        match target.compare_exchange_weak(cur, next, Ordering::Release, Ordering::Relaxed) {
            Ok(_) => return,
            Err(observed) => cur = observed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::thread;

    #[test]
    fn admission_fills_and_release_refills() {
        let scope = SharedCapacityScope::new("gw.in_flight", "requests", 2);
        let a = scope.try_admit(1).unwrap();
        let b = scope.try_admit(1).unwrap();
        let err = scope.try_admit(1).unwrap_err();
        assert_eq!(err.max, 2);
        assert_eq!(err.current, 2);

        drop(a);
        let _c = scope.try_admit(1).unwrap();
        drop(b);

        let snap = scope.snapshot();
        assert_eq!(snap.current, 1);
        assert_eq!(snap.high_water, 2);
        assert_eq!(snap.full_count, 1);
    }

    #[test]
    fn weighted_admission_respects_weight() {
        let scope = SharedCapacityScope::new("gw.bytes", "bytes", 100);
        let big = scope.try_admit(60).unwrap();
        let err = scope.try_admit(50).unwrap_err();
        assert_eq!(err.requested, 50);
        assert_eq!(err.current, 60);
        drop(big);
        let _ok = scope.try_admit(50).unwrap();
    }

    #[test]
    fn owner_stop_releases_held_charges() {
        // "Owner stop" is simulated by dropping every lease the owner
        // held.
        let scope = SharedCapacityScope::new("gw.in_flight", "requests", 4);
        let leases: Vec<_> = (0..4).map(|_| scope.try_admit(1).unwrap()).collect();
        assert_eq!(scope.snapshot().current, 4);
        drop(leases);
        let snap = scope.snapshot();
        assert_eq!(snap.current, 0);
        assert_eq!(snap.high_water, 4);
        assert_eq!(snap.released, 4);
    }

    #[test]
    fn zero_weight_is_admitted_without_charge() {
        let scope = SharedCapacityScope::new("gw.in_flight", "requests", 0);
        let lease = scope.try_admit(0).unwrap();
        assert_eq!(scope.snapshot().current, 0);
        drop(lease);
        assert_eq!(scope.snapshot().admitted, 0);
    }

    #[test]
    fn surface_report_pushes_into_capacity_summary() {
        use crate::CapacitySummary;
        let scope = SharedCapacityScope::new("gw.in_flight", "requests", 4);
        let _l = scope.try_admit(2).unwrap();
        let mut summary = CapacitySummary::new();
        summary
            .push(scope.surface_report(CapacityMode::Fixed))
            .unwrap();
        let report = summary.surface("gw.in_flight").report().unwrap();
        assert_eq!(report.max_weight, Some(4));
        assert_eq!(report.current_weight, Some(2));
        assert_eq!(report.weight_unit.as_deref(), Some("requests"));
    }

    #[test]
    fn decorate_attaches_shared_columns() {
        let scope = SharedCapacityScope::new("gw.bytes", "bytes", 1000);
        let _l = scope.try_admit(300).unwrap();
        let local = CapacitySurfaceReport::weighted(
            "route.upload",
            CapacityMode::Fixed,
            500,
            300,
            300,
            0,
            "bytes",
        );
        let decorated = scope.decorate(local);
        assert_eq!(decorated.shared_scope.as_deref(), Some("gw.bytes"));
        assert_eq!(decorated.shared_current_weight, Some(300));
        assert_eq!(decorated.shared_max_weight, Some(1000));
    }

    #[test]
    fn discovery_line_is_grep_friendly() {
        let scope = SharedCapacityScope::new("gw.in_flight", "requests", 4);
        let _l = scope.try_admit(3).unwrap();
        let line = scope.discovery_line();
        assert!(line.starts_with("scope "), "{line}");
        assert!(line.contains("name=gw.in_flight"), "{line}");
        assert!(line.contains("unit=requests"), "{line}");
        assert!(line.contains("max=4"), "{line}");
        assert!(line.contains("cur=3"), "{line}");
        assert!(line.contains("admitted=3"), "{line}");
    }

    #[test]
    fn discovery_line_quotes_unsafe_names() {
        let scope = SharedCapacityScope::new("gw bad name", "weird unit", 4);
        let line = scope.discovery_line();
        assert!(line.contains("name=\"gw bad name\""), "{line}");
        assert!(line.contains("unit=\"weird unit\""), "{line}");
    }

    #[test]
    fn full_count_is_honest_under_admit_release_contention() {
        // Eight threads each do `try_admit(1)` then immediately drop.
        // Cap is 4 so brief moments of contention may make the
        // observed `cur` look full when it actually is not. The fix:
        // re-load on the Full branch. Run enough iterations that a
        // race would be very likely to spuriously inflate full_count
        // without the re-load.
        const THREADS: usize = 8;
        const ITERS: usize = 5_000;
        let scope = SharedCapacityScope::new("gw.race", "requests", 4);
        let gate = std::sync::Arc::new(Barrier::new(THREADS));
        let mut handles = Vec::with_capacity(THREADS);
        for _ in 0..THREADS {
            let s = scope.clone();
            let b = std::sync::Arc::clone(&gate);
            handles.push(thread::spawn(move || {
                b.wait();
                let mut local_fulls = 0u64;
                for _ in 0..ITERS {
                    match s.try_admit(1) {
                        Ok(lease) => drop(lease),
                        Err(_) => local_fulls += 1,
                    }
                }
                local_fulls
            }));
        }
        let observed_fulls: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
        let snap = scope.snapshot();
        // Every Full observed by a thread must correspond to exactly
        // one counted Full. The bug-before-fix would count *more*.
        assert_eq!(
            snap.full_count, observed_fulls,
            "full_count drifted from observed Full results: snap={snap:?}",
        );
        // After all threads finish, the scope must drain cleanly.
        assert_eq!(snap.current, 0);
        assert_eq!(snap.admitted, snap.released);
    }
}
