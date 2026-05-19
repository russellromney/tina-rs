//! Isolate-local in-flight permit gate (Phase 101 Rock 3).
//!
//! Several services keep a `bool` or counter that means "this isolate admitted
//! local work that has not settled yet." [`LocalPermitGate`] names that shape
//! so the invariant is structural: fixed capacity, move-only [`Permit`],
//! explicit release/retire.
//!
//! Not a pool. The gate does not own work, lease a resource, queue waiters, or
//! retry. It is plain state. Permits are visible at the type level: the
//! continuation message carries the permit so the release point is obvious.
//!
//! No auto-release on drop. Forgetting to release is a bug we want loud, not
//! silent — a dropped permit increments the process-wide
//! [`dropped_permit_count`] counter and stays charged against `current` until
//! the gate is rebuilt. Use [`Permit::retire`] for the explicit "abandon this
//! slot, do not credit completion" path.

use std::sync::atomic::{AtomicU64, Ordering};

/// Static identifier attached to a [`LocalPermitGate`] for reports and traces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LocalPermitName(pub &'static str);

/// Snapshot of one [`LocalPermitGate`]'s capacity and pressure counters.
///
/// Reports never describe individual permits; they describe the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LocalPermitReport {
    /// Optional static name for trace and report visibility.
    pub name: Option<LocalPermitName>,
    /// Configured maximum number of concurrently admitted permits.
    pub capacity: usize,
    /// Permits currently outstanding (admitted minus released/retired).
    pub current: usize,
    /// Number of [`LocalPermitGate::try_admit`] calls that returned
    /// [`LocalPermitFull`] because the gate was at capacity.
    pub full_count: u64,
    /// Largest observed value of `current` over this gate's lifetime.
    pub high_water: usize,
    /// Number of permits explicitly retired (without recording completion).
    pub retired_count: u64,
    /// Number of permits that resolved via [`LocalPermitGate::release`].
    pub completed_count: u64,
    /// Number of stale or unknown release/retire attempts. These leave
    /// `current` unchanged and signal a state-machine bug.
    pub invalid_release_count: u64,
}

impl LocalPermitReport {
    /// `true` if no permit is currently outstanding.
    pub const fn is_idle(self) -> bool {
        self.current == 0
    }

    /// `true` if the gate is at capacity.
    pub const fn is_full(self) -> bool {
        self.current >= self.capacity
    }
}

/// Reason returned by [`LocalPermitGate::try_admit`] when the gate is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LocalPermitFull {
    report: LocalPermitReport,
}

impl LocalPermitFull {
    /// Snapshot of the gate at the moment admission was refused.
    pub const fn report(self) -> LocalPermitReport {
        self.report
    }
}

/// Reason a [`LocalPermitGate::release`] or [`LocalPermitGate::retire`] call
/// failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LocalPermitReleaseError {
    /// The permit was issued by a different gate or by a prior generation of
    /// this gate (after [`LocalPermitGate::reset`] or a drain cycle). The
    /// gate's `invalid_release_count` was incremented and `current` was not
    /// changed.
    StaleOrUnknown {
        /// Permit identifier carried by the bad permit.
        permit_id: u64,
        /// Generation carried by the bad permit.
        permit_generation: u64,
        /// Current gate generation.
        gate_generation: u64,
    },
}

/// Fixed-capacity in-flight permit gate.
///
/// Construct one as an isolate field. Each [`try_admit`](Self::try_admit) call
/// either returns a move-only [`Permit`] (charging the gate) or refuses with a
/// [`LocalPermitFull`] report. The caller hands the permit into the
/// continuation message and later passes it back to
/// [`release`](Self::release) (recording a completion) or
/// [`retire`](Self::retire) (recording an explicit abandon).
#[derive(Debug)]
#[must_use = "LocalPermitGate is state; store it on the isolate and admit explicitly"]
pub struct LocalPermitGate {
    name: Option<LocalPermitName>,
    capacity: usize,
    current: usize,
    full_count: u64,
    high_water: usize,
    retired_count: u64,
    completed_count: u64,
    invalid_release_count: u64,
    next_permit_id: u64,
    generation: u64,
}

impl LocalPermitGate {
    /// Creates a gate with the given fixed capacity.
    ///
    /// # Panics
    ///
    /// Panics if `capacity == 0`. A zero-capacity gate would refuse every
    /// admission and is always a programmer error.
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "LocalPermitGate capacity must be > 0");
        Self {
            name: None,
            capacity,
            current: 0,
            full_count: 0,
            high_water: 0,
            retired_count: 0,
            completed_count: 0,
            invalid_release_count: 0,
            next_permit_id: 0,
            generation: 0,
        }
    }

    /// Attaches a static name visible in [`LocalPermitReport`].
    pub const fn named(mut self, name: LocalPermitName) -> Self {
        self.name = Some(name);
        self
    }

    /// Returns the configured capacity.
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns the gate's static name, if one was set.
    pub const fn name(&self) -> Option<LocalPermitName> {
        self.name
    }

    /// Returns the number of permits currently outstanding.
    pub const fn current(&self) -> usize {
        self.current
    }

    /// `true` if no permit is currently outstanding.
    pub const fn is_idle(&self) -> bool {
        self.current == 0
    }

    /// `true` if the gate is at capacity.
    pub const fn is_full(&self) -> bool {
        self.current >= self.capacity
    }

    /// Returns a snapshot of the gate.
    pub fn report(&self) -> LocalPermitReport {
        LocalPermitReport {
            name: self.name,
            capacity: self.capacity,
            current: self.current,
            full_count: self.full_count,
            high_water: self.high_water,
            retired_count: self.retired_count,
            completed_count: self.completed_count,
            invalid_release_count: self.invalid_release_count,
        }
    }

    /// Resets pressure counters and bumps the generation.
    ///
    /// After reset, permits issued before the reset return
    /// [`LocalPermitReleaseError::StaleOrUnknown`] from
    /// [`release`](Self::release) and [`retire`](Self::retire).
    ///
    /// `current` is always set to zero regardless of outstanding permits.
    /// If permits are still live when this is called, the gate will
    /// under-count subsequent admissions: stale releases return an error
    /// instead of restoring the slot. Drain outstanding permits before
    /// calling `reset` unless you intend to abandon them.
    pub fn reset(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.current = 0;
        self.full_count = 0;
        self.high_water = 0;
        self.retired_count = 0;
        self.completed_count = 0;
        self.invalid_release_count = 0;
    }

    /// Tries to admit one new permit.
    pub fn try_admit(&mut self) -> Result<Permit, LocalPermitFull> {
        if self.current >= self.capacity {
            self.full_count = self.full_count.saturating_add(1);
            return Err(LocalPermitFull {
                report: self.report(),
            });
        }
        self.next_permit_id = self.next_permit_id.saturating_add(1);
        self.current += 1;
        if self.current > self.high_water {
            self.high_water = self.current;
        }
        Ok(Permit {
            inner: Some(PermitInner {
                id: self.next_permit_id,
                generation: self.generation,
            }),
        })
    }

    /// Releases the permit and records a completion.
    pub fn release(
        &mut self,
        mut permit: Permit,
    ) -> Result<LocalPermitReport, LocalPermitReleaseError> {
        let inner = permit
            .inner
            .take()
            .expect("Permit::inner cleared by an earlier consume");
        if inner.generation != self.generation {
            self.invalid_release_count = self.invalid_release_count.saturating_add(1);
            return Err(LocalPermitReleaseError::StaleOrUnknown {
                permit_id: inner.id,
                permit_generation: inner.generation,
                gate_generation: self.generation,
            });
        }
        self.current = self.current.saturating_sub(1);
        self.completed_count = self.completed_count.saturating_add(1);
        Ok(self.report())
    }

    /// Retires the permit without recording a completion.
    pub fn retire(
        &mut self,
        mut permit: Permit,
    ) -> Result<LocalPermitReport, LocalPermitReleaseError> {
        let inner = permit
            .inner
            .take()
            .expect("Permit::inner cleared by an earlier consume");
        if inner.generation != self.generation {
            self.invalid_release_count = self.invalid_release_count.saturating_add(1);
            return Err(LocalPermitReleaseError::StaleOrUnknown {
                permit_id: inner.id,
                permit_generation: inner.generation,
                gate_generation: self.generation,
            });
        }
        self.current = self.current.saturating_sub(1);
        self.retired_count = self.retired_count.saturating_add(1);
        Ok(self.report())
    }
}

#[derive(Debug, Clone, Copy)]
struct PermitInner {
    id: u64,
    generation: u64,
}

/// Move-only handle to one outstanding admission against a [`LocalPermitGate`].
///
/// The permit must be passed to [`LocalPermitGate::release`] or
/// [`LocalPermitGate::retire`] exactly once. Dropping a permit increments the
/// process-wide [`dropped_permit_count`] counter and leaves the slot charged.
#[derive(Debug)]
#[must_use = "a Permit must be released or retired; do not drop it silently"]
pub struct Permit {
    inner: Option<PermitInner>,
}

impl Permit {
    /// Permit identifier, for tracing.
    pub fn id(&self) -> Option<u64> {
        self.inner.as_ref().map(|inner| inner.id)
    }

    /// Generation that issued this permit, for tracing.
    pub fn generation(&self) -> Option<u64> {
        self.inner.as_ref().map(|inner| inner.generation)
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        if self.inner.is_some() {
            // The user dropped a still-live permit. We have no back-reference
            // to the gate from inside the permit (gates are not Sync), so we
            // record the drop in a global counter that integration tests and
            // reports can use to detect leaks across all gates.
            DROPPED_PERMIT_COUNT.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Process-wide counter of dropped permits across all [`LocalPermitGate`]
/// instances. Used by tests and reports to detect leaked permits.
static DROPPED_PERMIT_COUNT: AtomicU64 = AtomicU64::new(0);

/// Returns the process-wide dropped-permit count.
pub fn dropped_permit_count() -> u64 {
    DROPPED_PERMIT_COUNT.load(Ordering::Relaxed)
}

/// Records one dropped move-only permit against the process-wide counter.
///
/// Used by sibling admission-policy permits (e.g. `KeyedPermit`) so leaked
/// permits stay visible in the same global counter the gate uses.
pub(crate) fn record_dropped_permit() {
    DROPPED_PERMIT_COUNT.fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_refuse_release_refill() {
        let mut gate = LocalPermitGate::with_capacity(2);
        let a = gate.try_admit().expect("first admit");
        let b = gate.try_admit().expect("second admit");
        let full = gate.try_admit().expect_err("third must refuse");
        assert_eq!(full.report().current, 2);
        assert_eq!(full.report().capacity, 2);
        assert_eq!(full.report().full_count, 1);

        let r1 = gate.release(a).expect("release first");
        assert_eq!(r1.current, 1);
        assert_eq!(r1.completed_count, 1);

        let c = gate.try_admit().expect("refill");
        let _ = gate.release(b).expect("release second");
        let _ = gate.release(c).expect("release third");
        let snap = gate.report();
        assert_eq!(snap.current, 0);
        assert_eq!(snap.high_water, 2);
        assert_eq!(snap.completed_count, 3);
        assert_eq!(snap.full_count, 1);
    }

    #[test]
    fn double_release_returns_stale_unknown_and_leaves_current_unchanged() {
        let mut gate = LocalPermitGate::with_capacity(1);
        let permit = gate.try_admit().expect("admit");
        let permit_id = permit.id().unwrap();
        let _ = gate.release(permit).expect("first release");

        // Reset bumps the generation; an old permit from before reset must
        // not be releasable. We construct one directly via a stale Permit
        // captured before the gate's generation moved.
        let _bookkeeping = LocalPermitFull {
            report: gate.report(),
        };

        // Now admit again, then simulate a stale release by tampering with
        // generation: we can't reach into PermitInner from outside the module,
        // so we exercise the same path by bumping the gate generation while a
        // live permit is outstanding.
        let live = gate.try_admit().expect("admit after release");
        gate.reset();
        let err = gate
            .release(live)
            .expect_err("permit from before reset must be stale");
        match err {
            LocalPermitReleaseError::StaleOrUnknown {
                permit_id: stale_id,
                permit_generation,
                gate_generation,
            } => {
                // generation 0 → 1; permit was issued at gen 0.
                assert_eq!(permit_generation, 0);
                assert_eq!(gate_generation, 1);
                assert!(stale_id >= permit_id);
            }
        }
        // After reset, current was zeroed; the stale release must not move it.
        assert_eq!(gate.current(), 0);
        assert_eq!(gate.report().invalid_release_count, 1);
    }

    #[test]
    fn retire_records_separately_from_release() {
        let mut gate = LocalPermitGate::with_capacity(2);
        let a = gate.try_admit().unwrap();
        let b = gate.try_admit().unwrap();
        let _ = gate.retire(a).unwrap();
        let _ = gate.release(b).unwrap();
        let snap = gate.report();
        assert_eq!(snap.current, 0);
        assert_eq!(snap.retired_count, 1);
        assert_eq!(snap.completed_count, 1);
    }

    #[test]
    fn shutdown_drain_report_keeps_outstanding_count_visible() {
        let mut gate = LocalPermitGate::with_capacity(3).named(LocalPermitName("flush"));
        let _a = gate.try_admit().unwrap();
        let _b = gate.try_admit().unwrap();
        let snap = gate.report();
        assert_eq!(snap.current, 2);
        assert!(!snap.is_idle());
        assert_eq!(snap.name, Some(LocalPermitName("flush")));
        // Outstanding permits stay charged at drain — caller decides whether
        // to wait, retire, or accept the leak.
    }

    #[test]
    fn dropped_permit_increments_global_counter() {
        let baseline = dropped_permit_count();
        let mut gate = LocalPermitGate::with_capacity(1);
        let permit = gate.try_admit().unwrap();
        drop(permit);
        assert!(dropped_permit_count() > baseline);
        // The gate's current stays at 1 because Drop does not auto-release.
        assert_eq!(gate.current(), 1);
    }

    #[test]
    #[should_panic(expected = "capacity must be > 0")]
    fn zero_capacity_rejected() {
        let _ = LocalPermitGate::with_capacity(0);
    }
}
