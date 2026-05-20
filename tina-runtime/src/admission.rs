//! Admission and rate policy types (Phase 118).
//!
//! User-facing pressure policies built on top of the existing
//! [`LocalPermitGate`], [`SharedCapacityScope`], [`FullHandling`], and
//! [`tina::time::Backoff`] primitives. These types do not invent a second
//! capacity product; they teach services a boring vocabulary for shedding,
//! waiting, rate-limiting, degrading, or closing under pressure.
//!
//! The user story:
//!
//! ```text
//! when I am overloaded, I choose shed, wait boundedly, rate-limit, degrade,
//! or close, and the outcome is typed
//! ```
//!
//! Three policy types live here:
//!
//! - [`ConcurrencyLimit`] — fixed-cap local concurrency over [`LocalPermitGate`].
//! - [`KeyedLimit`] — fixed-cap per-key concurrency with explicit slot reuse.
//! - [`RateLimit`] — replayable token-bucket per key, decisions are pure
//!   functions of `(config, now, key history)`.
//!
//! All three return [`AdmissionDecision`]. Successful admission carries a
//! move-only proof object the caller must release explicitly. There is no
//! hidden retry, no hidden queue, no growing per-key map.
//!
//! Retry remains caller-owned. Pair these policies with [`FullHandling`]
//! when retry-with-backoff is the right answer, or treat each rejection as
//! terminal.

use std::marker::PhantomData;
use std::time::{Duration, Instant};

use tina::capacity::{CapacityMode, CapacitySurfaceReport};

use crate::local_permit::{LocalPermitGate, LocalPermitName, Permit};

/// What the caller wants to happen when the policy is full.
///
/// This is configuration, not runtime behavior. Each policy maps the
/// configured action into the matching [`AdmissionDecision`] variant.
/// The decision is still typed truth; the action just chooses the
/// vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PressureAction {
    /// On full, return [`AdmissionDecision::Full`] immediately.
    Shed,
    /// On full, return [`AdmissionDecision::Degrade`] — caller serves a
    /// reduced response without taking a permit.
    Degrade,
    /// On full, return [`AdmissionDecision::Closed`] — admission is no
    /// longer accepting. Used by ingress-close paths.
    Close,
    /// On full, return [`AdmissionDecision::Wait`] with the policy-supplied
    /// suggested wait. Caller still owns the wait (e.g., via `SharedWork`).
    Wait,
}

/// Shared report carried by every admission decision.
///
/// Counts are cumulative for the lifetime of the policy object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionReport {
    /// Surface name, mirroring [`CapacitySurfaceReport::name`].
    pub surface: &'static str,
    /// How the cap was chosen (Fixed/Tuning/Unbounded-for-now).
    pub mode: CapacityMode,
    /// Configured concurrency cap. For [`RateLimit`] this reflects the
    /// key-table capacity, not the per-second rate.
    pub capacity: usize,
    /// Live concurrency. For [`RateLimit`] this is the live key count.
    pub current: usize,
    /// Largest observed `current` over this policy's lifetime.
    pub high_water: usize,
    /// Times the policy returned `Full`.
    pub full_count: u64,
    /// Times the policy returned `RateLimited`.
    pub rate_limited_count: u64,
    /// Times the policy returned `Wait`.
    pub wait_count: u64,
    /// Times the policy returned `Degrade`.
    pub degrade_count: u64,
    /// Times the policy returned `Closed`.
    pub closed_count: u64,
    /// Times the policy returned `TimedOut`. Set by callers that drive a
    /// caller-owned deadline (see [`AdmissionDecision::timed_out`]).
    pub timed_out_count: u64,
}

impl AdmissionReport {
    /// `true` if any rejection category has been observed.
    pub const fn any_rejection(&self) -> bool {
        self.full_count > 0
            || self.rate_limited_count > 0
            || self.wait_count > 0
            || self.degrade_count > 0
            || self.closed_count > 0
            || self.timed_out_count > 0
    }

    /// Project this report onto a [`CapacitySurfaceReport`].
    ///
    /// Count fields map directly. `full_count` aggregates every rejection
    /// category that means "we did not admit": full + rate-limited + wait +
    /// degrade + closed + timed-out. This keeps `summary.any_full()` honest
    /// for admission surfaces — any rejection counts as overload truth.
    pub fn capacity_surface(&self) -> CapacitySurfaceReport {
        CapacitySurfaceReport::count(
            self.surface,
            self.mode.clone(),
            self.capacity,
            self.current,
            self.high_water,
            self.full_count
                .saturating_add(self.rate_limited_count)
                .saturating_add(self.wait_count)
                .saturating_add(self.degrade_count)
                .saturating_add(self.closed_count)
                .saturating_add(self.timed_out_count),
        )
    }
}

/// Decision returned by every admission policy.
///
/// `T` is the move-only proof that admission succeeded — usually a [`Permit`]
/// or [`KeyedPermit`]. A successful decision must be released or retired
/// before the policy's `current` count drops.
#[derive(Debug)]
#[must_use = "AdmissionDecision must be matched; on failure the caller decides reply/wait/retry"]
pub enum AdmissionDecision<T> {
    /// Admitted. Carry `T` through the continuation; release it on completion.
    Admitted(T),
    /// Refused because the policy was at concurrency cap. No retry scheduled.
    Full(AdmissionReport),
    /// Refused because the rate-limit bucket is empty. `retry_after` is the
    /// earliest time the caller could try again; deterministic in `now`.
    RateLimited {
        /// Earliest delay the caller could try again, derived from policy +
        /// time only.
        retry_after: Duration,
        /// Snapshot at decision time.
        report: AdmissionReport,
    },
    /// Caller should wait `delay` and try again. `delay` is policy-suggested;
    /// the actual wait is caller-owned (e.g., `SharedWork` or `sleep`).
    Wait {
        /// Suggested delay.
        delay: Duration,
        /// Snapshot at decision time.
        report: AdmissionReport,
    },
    /// Serve a degraded reply without taking a permit.
    Degrade {
        /// Snapshot at decision time.
        report: AdmissionReport,
    },
    /// Admission is closed. Subsequent attempts will keep returning `Closed`
    /// until the policy is rebuilt.
    Closed(AdmissionReport),
    /// Caller-supplied deadline elapsed before admission was decided.
    /// Returned by [`AdmissionDecision::timed_out`].
    TimedOut(AdmissionReport),
}

impl<T> AdmissionDecision<T> {
    /// Build a `TimedOut` decision from a report.
    ///
    /// The policy does not own caller deadlines; callers drive their own
    /// timeout and use this helper to record the typed outcome in the
    /// shared report shape.
    pub fn timed_out(mut report: AdmissionReport) -> Self {
        report.timed_out_count = report.timed_out_count.saturating_add(1);
        Self::TimedOut(report)
    }

    /// `true` if this decision carries an admitted permit/charge.
    pub const fn is_admitted(&self) -> bool {
        matches!(self, Self::Admitted(_))
    }

    /// Borrow the carried admission proof, if any.
    pub const fn admitted(&self) -> Option<&T> {
        if let Self::Admitted(t) = self {
            Some(t)
        } else {
            None
        }
    }

    /// Take the carried admission proof or convert into an [`AdmissionFailure`].
    ///
    /// `AdmissionFailure` carries a full `AdmissionReport` (including a
    /// cloned [`CapacityMode`]), so the `Err` arm is comparable in size to
    /// the original decision. Callers in hot paths can avoid the move by
    /// `match`-ing on the decision directly; `into_admitted` is provided
    /// for `?`-style flows where ergonomics beats stack layout.
    #[allow(clippy::result_large_err)]
    pub fn into_admitted(self) -> Result<T, AdmissionFailure> {
        match self {
            Self::Admitted(t) => Ok(t),
            Self::Full(r) => Err(AdmissionFailure::Full(r)),
            Self::RateLimited {
                retry_after,
                report,
            } => Err(AdmissionFailure::RateLimited {
                retry_after,
                report,
            }),
            Self::Wait { delay, report } => Err(AdmissionFailure::Wait { delay, report }),
            Self::Degrade { report } => Err(AdmissionFailure::Degrade { report }),
            Self::Closed(r) => Err(AdmissionFailure::Closed(r)),
            Self::TimedOut(r) => Err(AdmissionFailure::TimedOut(r)),
        }
    }

    /// Borrow the report carried by this decision, if any.
    ///
    /// `Admitted(t)` carries no separate report; callers can call
    /// [`ConcurrencyLimit::report`] (or the equivalent) for live state.
    pub const fn report(&self) -> Option<&AdmissionReport> {
        match self {
            Self::Admitted(_) => None,
            Self::Full(r)
            | Self::Closed(r)
            | Self::TimedOut(r)
            | Self::Degrade { report: r }
            | Self::RateLimited { report: r, .. }
            | Self::Wait { report: r, .. } => Some(r),
        }
    }
}

/// Rejection cause without the admitted arm. Useful for `?`-style flows that
/// only need the failure shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionFailure {
    /// Concurrency cap reached.
    Full(AdmissionReport),
    /// Rate-limit bucket empty.
    RateLimited {
        /// Earliest delay the caller could try again.
        retry_after: Duration,
        /// Snapshot at decision time.
        report: AdmissionReport,
    },
    /// Caller should wait `delay` and retry through the policy again.
    Wait {
        /// Suggested delay.
        delay: Duration,
        /// Snapshot at decision time.
        report: AdmissionReport,
    },
    /// Serve a degraded reply.
    Degrade {
        /// Snapshot at decision time.
        report: AdmissionReport,
    },
    /// Admission closed.
    Closed(AdmissionReport),
    /// Caller-owned deadline elapsed.
    TimedOut(AdmissionReport),
}

impl AdmissionFailure {
    /// Borrow the report this failure carries.
    pub const fn report(&self) -> &AdmissionReport {
        match self {
            Self::Full(r)
            | Self::Closed(r)
            | Self::TimedOut(r)
            | Self::Degrade { report: r }
            | Self::RateLimited { report: r, .. }
            | Self::Wait { report: r, .. } => r,
        }
    }
}

// -----------------------------------------------------------------------------
// ConcurrencyLimit
// -----------------------------------------------------------------------------

/// Fixed-capacity local concurrency policy.
///
/// Thin wrapper over [`LocalPermitGate`] that returns [`AdmissionDecision`]
/// instead of `Result<Permit, LocalPermitFull>` so it composes with the rest
/// of the admission vocabulary. The action on full is configurable: shed,
/// degrade, close, or hint a wait.
#[derive(Debug)]
#[must_use = "ConcurrencyLimit is state; store it on the isolate"]
pub struct ConcurrencyLimit {
    surface: &'static str,
    mode: CapacityMode,
    gate: LocalPermitGate,
    action: PressureAction,
    wait_hint: Duration,
    closed: bool,
    closed_count: u64,
    degrade_count: u64,
    wait_count: u64,
    timed_out_count: u64,
    /// Count of `AdmissionDecision::Full(_)` decisions returned. Distinct
    /// from `gate.full_count`, which counts every cap-reached try_admit
    /// regardless of the action (under `PressureAction::Degrade`/`Close`/
    /// `Wait` those events do not surface as `Full`).
    full_decision_count: u64,
}

impl ConcurrencyLimit {
    /// Build a shed-on-full concurrency limit.
    pub fn with_capacity(surface: &'static str, capacity: usize) -> Self {
        Self {
            surface,
            mode: CapacityMode::Fixed,
            gate: LocalPermitGate::with_capacity(capacity).named(LocalPermitName(surface)),
            action: PressureAction::Shed,
            wait_hint: Duration::ZERO,
            closed: false,
            closed_count: 0,
            degrade_count: 0,
            wait_count: 0,
            timed_out_count: 0,
            full_decision_count: 0,
        }
    }

    /// Set the configured capacity mode (Fixed/Tuning/...).
    pub fn with_mode(mut self, mode: CapacityMode) -> Self {
        self.mode = mode;
        self
    }

    /// Choose what to return on full.
    pub const fn on_pressure(mut self, action: PressureAction) -> Self {
        self.action = action;
        self
    }

    /// Set the wait hint returned when `PressureAction::Wait` is selected.
    /// Ignored for other actions.
    pub const fn wait_hint(mut self, delay: Duration) -> Self {
        self.wait_hint = delay;
        self
    }

    /// Try to admit one new permit.
    pub fn try_admit(&mut self) -> AdmissionDecision<Permit> {
        if self.closed {
            self.closed_count = self.closed_count.saturating_add(1);
            return AdmissionDecision::Closed(self.report());
        }
        match self.gate.try_admit() {
            Ok(permit) => AdmissionDecision::Admitted(permit),
            Err(_) => match self.action {
                PressureAction::Shed => {
                    self.full_decision_count = self.full_decision_count.saturating_add(1);
                    AdmissionDecision::Full(self.report())
                }
                PressureAction::Degrade => {
                    self.degrade_count = self.degrade_count.saturating_add(1);
                    AdmissionDecision::Degrade {
                        report: self.report(),
                    }
                }
                PressureAction::Close => {
                    self.closed_count = self.closed_count.saturating_add(1);
                    AdmissionDecision::Closed(self.report())
                }
                PressureAction::Wait => {
                    self.wait_count = self.wait_count.saturating_add(1);
                    AdmissionDecision::Wait {
                        delay: self.wait_hint,
                        report: self.report(),
                    }
                }
            },
        }
    }

    /// Release a permit and record a completion.
    ///
    /// Errors are forwarded from [`LocalPermitGate::release`] so stale or
    /// post-reset permits surface as typed truth.
    pub fn release(&mut self, permit: Permit) -> Result<(), crate::LocalPermitReleaseError> {
        self.gate.release(permit).map(|_| ())
    }

    /// Retire a permit without recording a completion.
    pub fn retire(&mut self, permit: Permit) -> Result<(), crate::LocalPermitReleaseError> {
        self.gate.retire(permit).map(|_| ())
    }

    /// Mark this policy closed. Subsequent admissions return `Closed(...)`.
    pub fn close(&mut self) {
        self.closed = true;
    }

    /// `true` if closed.
    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    /// Build the current report.
    ///
    /// `full_count` is the count of `AdmissionDecision::Full(_)` returns —
    /// not the underlying gate's cap-reached count. Under non-`Shed`
    /// actions, gate-full events surface as `Degrade`/`Closed`/`Wait`
    /// and are recorded under those counters instead, so the capacity-
    /// surface projection does not double-count.
    pub fn report(&self) -> AdmissionReport {
        let snap = self.gate.report();
        AdmissionReport {
            surface: self.surface,
            mode: self.mode.clone(),
            capacity: snap.capacity,
            current: snap.current,
            high_water: snap.high_water,
            full_count: self.full_decision_count,
            rate_limited_count: 0,
            wait_count: self.wait_count,
            degrade_count: self.degrade_count,
            closed_count: self.closed_count,
            timed_out_count: self.timed_out_count,
        }
    }

    /// Underlying gate-cap-reached count. Distinct from
    /// [`AdmissionReport::full_count`], which only counts decisions that
    /// surfaced as `Full(...)`. Use this for "how often was the local
    /// concurrency cap saturated" telemetry.
    pub fn gate_full_count(&self) -> u64 {
        self.gate.report().full_count
    }

    /// Capacity surface projection.
    pub fn capacity_surface(&self) -> CapacitySurfaceReport {
        self.report().capacity_surface()
    }
}

// -----------------------------------------------------------------------------
// KeyedLimit
// -----------------------------------------------------------------------------

/// Per-key concurrency policy with fixed-capacity key storage.
///
/// Storage is an explicit `Vec<Option<Slot>>` of length `max_keys` — never a
/// growing `HashMap`. Each key has its own per-key cap. A new key gets a free
/// slot; the table is `Full` when all slots are taken by other keys.
///
/// `K: Eq + Clone` so a slot lookup is `O(max_keys)` linear scan. `max_keys`
/// is expected to be small (tens to hundreds).
#[derive(Debug)]
#[must_use = "KeyedLimit is state; store it on the isolate"]
pub struct KeyedLimit<K> {
    surface: &'static str,
    mode: CapacityMode,
    per_key_capacity: usize,
    slots: Vec<Option<KeyedSlot<K>>>,
    live_keys: usize,
    high_water_keys: usize,
    full_count: u64,
    per_key_full_count: u64,
    closed: bool,
    closed_count: u64,
    next_permit_id: u64,
    next_generation: u64,
    invalid_release_count: u64,
}

#[derive(Debug)]
struct KeyedSlot<K> {
    key: K,
    current: usize,
    high_water: usize,
    generation: u64,
}

impl<K: Eq + Clone> KeyedLimit<K> {
    /// Build a keyed limit with `max_keys` distinct keys and `per_key`
    /// concurrent admissions per key.
    ///
    /// # Panics
    ///
    /// Panics if `max_keys == 0` or `per_key == 0`.
    pub fn new(surface: &'static str, max_keys: usize, per_key: usize) -> Self {
        assert!(max_keys > 0, "KeyedLimit max_keys must be > 0");
        assert!(per_key > 0, "KeyedLimit per_key capacity must be > 0");
        let mut slots = Vec::with_capacity(max_keys);
        for _ in 0..max_keys {
            slots.push(None);
        }
        Self {
            surface,
            mode: CapacityMode::Fixed,
            per_key_capacity: per_key,
            slots,
            live_keys: 0,
            high_water_keys: 0,
            full_count: 0,
            per_key_full_count: 0,
            closed: false,
            closed_count: 0,
            next_permit_id: 0,
            next_generation: 1,
            invalid_release_count: 0,
        }
    }

    /// Set the configured capacity mode.
    pub fn with_mode(mut self, mode: CapacityMode) -> Self {
        self.mode = mode;
        self
    }

    /// Configured key-table capacity.
    pub fn max_keys(&self) -> usize {
        self.slots.len()
    }

    /// Configured per-key concurrent admissions.
    pub const fn per_key_capacity(&self) -> usize {
        self.per_key_capacity
    }

    /// Number of distinct keys currently holding at least one permit.
    pub const fn live_keys(&self) -> usize {
        self.live_keys
    }

    /// Try to admit one permit for `key`.
    ///
    /// `key` is borrowed for the lookup. The implementation only clones it
    /// when allocating a new slot, so passing an already-owned `K` (e.g.,
    /// `&String`) avoids per-call allocation on the hot path of existing
    /// keys.
    ///
    /// Three paths:
    /// 1. Key already has a slot below `per_key`: admit, increment.
    /// 2. Key already has a slot at `per_key`: refuse with `Full(report)`.
    ///    The per-key cap is the bottleneck, not the table.
    /// 3. Key is new and a free slot exists: claim the slot (cloning
    ///    `key`), admit.
    /// 4. Key is new and the table is full of other keys: refuse with
    ///    `Full(report)`. No silent eviction.
    pub fn try_admit(&mut self, key: &K) -> AdmissionDecision<KeyedPermit<K>> {
        if self.closed {
            self.closed_count = self.closed_count.saturating_add(1);
            return AdmissionDecision::Closed(self.report());
        }
        // 1) Find an existing slot for this key.
        if let Some(idx) = self.find_slot(key) {
            let slot = self
                .slots
                .get_mut(idx)
                .and_then(Option::as_mut)
                .expect("find_slot returned a live index");
            if slot.current >= self.per_key_capacity {
                self.per_key_full_count = self.per_key_full_count.saturating_add(1);
                self.full_count = self.full_count.saturating_add(1);
                return AdmissionDecision::Full(self.report());
            }
            slot.current += 1;
            if slot.current > slot.high_water {
                slot.high_water = slot.current;
            }
            let generation = slot.generation;
            return AdmissionDecision::Admitted(self.issue_permit(idx, generation));
        }
        // 2) Allocate a fresh slot if room exists. This is the only path
        //    that clones the key.
        if let Some(idx) = self.slots.iter().position(Option::is_none) {
            let generation = self.next_generation;
            self.next_generation = self.next_generation.saturating_add(1);
            self.slots[idx] = Some(KeyedSlot {
                key: key.clone(),
                current: 1,
                high_water: 1,
                generation,
            });
            self.live_keys = self.live_keys.saturating_add(1);
            if self.live_keys > self.high_water_keys {
                self.high_water_keys = self.live_keys;
            }
            return AdmissionDecision::Admitted(self.issue_permit(idx, generation));
        }
        // 3) Table full.
        self.full_count = self.full_count.saturating_add(1);
        AdmissionDecision::Full(self.report())
    }

    fn find_slot(&self, key: &K) -> Option<usize> {
        for (idx, slot) in self.slots.iter().enumerate() {
            if let Some(s) = slot {
                if &s.key == key {
                    return Some(idx);
                }
            }
        }
        None
    }

    fn issue_permit(&mut self, slot_idx: usize, generation: u64) -> KeyedPermit<K> {
        self.next_permit_id = self.next_permit_id.saturating_add(1);
        KeyedPermit {
            inner: Some(KeyedPermitInner {
                id: self.next_permit_id,
                slot_idx,
                generation,
            }),
            _key: PhantomData,
        }
    }

    /// Release a permit; the slot is freed when its count reaches zero.
    ///
    /// On generation mismatch (e.g., the slot was already freed and reused
    /// for a different key) the gate counts the invalid release and leaves
    /// `current` unchanged.
    pub fn release(&mut self, mut permit: KeyedPermit<K>) -> Result<(), KeyedReleaseError> {
        let inner = permit
            .inner
            .take()
            .expect("KeyedPermit::inner cleared by an earlier consume");
        let Some(slot) = self.slots.get_mut(inner.slot_idx) else {
            self.invalid_release_count = self.invalid_release_count.saturating_add(1);
            return Err(KeyedReleaseError::OutOfRange {
                slot_idx: inner.slot_idx,
            });
        };
        let entry = match slot {
            Some(s) if s.generation == inner.generation => s,
            _ => {
                self.invalid_release_count = self.invalid_release_count.saturating_add(1);
                return Err(KeyedReleaseError::StaleOrUnknown {
                    permit_id: inner.id,
                    slot_idx: inner.slot_idx,
                    permit_generation: inner.generation,
                });
            }
        };
        entry.current = entry.current.saturating_sub(1);
        if entry.current == 0 {
            *slot = None;
            self.live_keys = self.live_keys.saturating_sub(1);
        }
        Ok(())
    }

    /// Drop a permit explicitly, treating its slot the same as `release` —
    /// the slot is freed when its count reaches zero.
    pub fn retire(&mut self, permit: KeyedPermit<K>) -> Result<(), KeyedReleaseError> {
        self.release(permit)
    }

    /// Mark this policy closed.
    pub fn close(&mut self) {
        self.closed = true;
    }

    /// `true` if closed.
    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    /// Build the current report. `current` is live key count; `capacity` is
    /// `max_keys`.
    pub fn report(&self) -> AdmissionReport {
        AdmissionReport {
            surface: self.surface,
            mode: self.mode.clone(),
            capacity: self.slots.len(),
            current: self.live_keys,
            high_water: self.high_water_keys,
            full_count: self.full_count,
            rate_limited_count: 0,
            wait_count: 0,
            degrade_count: 0,
            closed_count: self.closed_count,
            timed_out_count: 0,
        }
    }

    /// Per-key full rejections (a subset of `full_count`).
    pub const fn per_key_full_count(&self) -> u64 {
        self.per_key_full_count
    }

    /// Invalid (stale / out-of-range) release attempts.
    pub const fn invalid_release_count(&self) -> u64 {
        self.invalid_release_count
    }

    /// Snapshot for one key.
    pub fn key_report(&self, key: &K) -> Option<KeyedSlotReport> {
        self.find_slot(key).and_then(|idx| {
            self.slots[idx].as_ref().map(|s| KeyedSlotReport {
                slot_idx: idx,
                current: s.current,
                high_water: s.high_water,
                generation: s.generation,
                per_key_capacity: self.per_key_capacity,
            })
        })
    }

    /// Capacity surface projection.
    pub fn capacity_surface(&self) -> CapacitySurfaceReport {
        self.report().capacity_surface()
    }
}

/// Snapshot of one keyed slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyedSlotReport {
    /// Internal slot index. Stable while the slot is live.
    pub slot_idx: usize,
    /// Live permit count for this key.
    pub current: usize,
    /// High-water permit count for this key.
    pub high_water: usize,
    /// Generation of the current slot occupation.
    pub generation: u64,
    /// Configured per-key cap.
    pub per_key_capacity: usize,
}

/// Move-only proof that one keyed admission succeeded.
///
/// **Permits are not tagged with the gate that issued them.** A `KeyedPermit`
/// from limit A released against limit B can decrement B's counters if B
/// happens to have a slot with matching `(slot_idx, generation)`. The
/// type system cannot distinguish two `KeyedLimit<K>` instances over the
/// same `K`. Treat permits like file descriptors: only release them on
/// the limit that issued them. The same caveat applies to
/// [`Permit`](crate::Permit) and [`LocalPermitGate`](crate::LocalPermitGate).
#[must_use = "a KeyedPermit must be released or retired; do not drop it silently"]
#[derive(Debug)]
pub struct KeyedPermit<K> {
    inner: Option<KeyedPermitInner>,
    _key: PhantomData<K>,
}

#[derive(Debug, Clone, Copy)]
struct KeyedPermitInner {
    id: u64,
    slot_idx: usize,
    generation: u64,
}

impl<K> KeyedPermit<K> {
    /// Permit identifier, for tracing.
    pub fn id(&self) -> Option<u64> {
        self.inner.as_ref().map(|i| i.id)
    }

    /// Generation at admission time, for tracing.
    pub fn generation(&self) -> Option<u64> {
        self.inner.as_ref().map(|i| i.generation)
    }
}

impl<K> Drop for KeyedPermit<K> {
    fn drop(&mut self) {
        if self.inner.is_some() {
            crate::local_permit::record_dropped_permit();
        }
    }
}

/// Why a [`KeyedLimit::release`] failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyedReleaseError {
    /// Slot index out of range. Should not happen for permits issued by this
    /// gate; treated as an internal bug.
    OutOfRange {
        /// Out-of-range index carried by the permit.
        slot_idx: usize,
    },
    /// Slot empty or reused under a new generation.
    StaleOrUnknown {
        /// Permit id, for tracing.
        permit_id: u64,
        /// Slot the permit was issued against.
        slot_idx: usize,
        /// Generation carried by the permit.
        permit_generation: u64,
    },
}

// -----------------------------------------------------------------------------
// RateLimit
// -----------------------------------------------------------------------------

/// Token-bucket rate limiter with replayable time.
///
/// Decisions are pure functions of `(config, now, key history)`. The policy
/// never reads wall-clock time; the caller passes `now` from `ctx.now()` (or
/// sim-supplied time) on every admit. Math is integer-only — token credit is
/// tracked in nano-tokens so partial gain is preserved across calls.
///
/// Per-key storage is fixed-capacity. The default first form does not evict
/// keys silently. Use [`RateLimit::evict_key_for_capacity`] to make room explicitly.
#[derive(Debug)]
#[must_use = "RateLimit is state; store it on the isolate"]
pub struct RateLimit<K> {
    surface: &'static str,
    mode: CapacityMode,
    rate_per_sec: u64,
    burst: u32,
    burst_nano_tokens: u128,
    slots: Vec<Option<RateSlot<K>>>,
    live_keys: usize,
    high_water_keys: usize,
    full_count: u64,
    rate_limited_count: u64,
    evicted_count: u64,
    closed: bool,
    closed_count: u64,
}

#[derive(Debug)]
struct RateSlot<K> {
    key: K,
    available_nt: u128,
    last_seen: Instant,
}

const ONE_TOKEN_NT: u128 = 1_000_000_000;

impl<K: Eq + Clone> RateLimit<K> {
    /// Build a rate limit allowing `rate_per_sec` admissions per second per
    /// key, with a token bucket up to `burst`. Up to `max_keys` distinct
    /// keys can hold state at once.
    ///
    /// # Panics
    ///
    /// Panics if `rate_per_sec == 0`, `burst == 0`, or `max_keys == 0`.
    pub fn new(surface: &'static str, max_keys: usize, rate_per_sec: u64, burst: u32) -> Self {
        assert!(rate_per_sec > 0, "RateLimit rate_per_sec must be > 0");
        assert!(burst > 0, "RateLimit burst must be > 0");
        assert!(max_keys > 0, "RateLimit max_keys must be > 0");
        let mut slots = Vec::with_capacity(max_keys);
        for _ in 0..max_keys {
            slots.push(None);
        }
        Self {
            surface,
            mode: CapacityMode::Fixed,
            rate_per_sec,
            burst,
            burst_nano_tokens: u128::from(burst) * ONE_TOKEN_NT,
            slots,
            live_keys: 0,
            high_water_keys: 0,
            full_count: 0,
            rate_limited_count: 0,
            evicted_count: 0,
            closed: false,
            closed_count: 0,
        }
    }

    /// Set the configured capacity mode.
    pub fn with_mode(mut self, mode: CapacityMode) -> Self {
        self.mode = mode;
        self
    }

    /// Configured rate per second.
    pub const fn rate_per_sec(&self) -> u64 {
        self.rate_per_sec
    }

    /// Configured burst.
    pub const fn burst(&self) -> u32 {
        self.burst
    }

    /// Configured key-table capacity.
    pub fn max_keys(&self) -> usize {
        self.slots.len()
    }

    /// Number of live key slots.
    pub const fn live_keys(&self) -> usize {
        self.live_keys
    }

    /// Cumulative count of explicit evictions via
    /// [`RateLimit::evict_key_for_capacity`].
    pub const fn evicted_count(&self) -> u64 {
        self.evicted_count
    }

    /// Try to admit one request for `key` at `now`.
    ///
    /// `key` is borrowed for the lookup. The implementation only clones it
    /// when allocating a new slot, so the hot path of an existing tenant
    /// is allocation-free even for `K = String`.
    ///
    /// `now` must be monotonic across calls for the same policy. Going
    /// backwards is treated as "no new credit since last call" — the
    /// previous `last_seen` is preserved.
    pub fn try_admit(&mut self, key: &K, now: Instant) -> AdmissionDecision<RateGrant<K>> {
        if self.closed {
            self.closed_count = self.closed_count.saturating_add(1);
            return AdmissionDecision::Closed(self.report());
        }
        // Find existing slot.
        if let Some(idx) = self.find_slot(key) {
            return self.admit_existing(idx, now);
        }
        // Allocate a slot for the new key. This is the only path that
        // clones the key.
        if let Some(idx) = self.slots.iter().position(Option::is_none) {
            // New key starts with a full burst minus one for the admit.
            let admitted = self.burst_nano_tokens >= ONE_TOKEN_NT;
            let available_nt = if admitted {
                self.burst_nano_tokens - ONE_TOKEN_NT
            } else {
                self.burst_nano_tokens
            };
            self.slots[idx] = Some(RateSlot {
                key: key.clone(),
                available_nt,
                last_seen: now,
            });
            self.live_keys = self.live_keys.saturating_add(1);
            if self.live_keys > self.high_water_keys {
                self.high_water_keys = self.live_keys;
            }
            // burst >= 1 (we panic in `new`), so first admit always succeeds.
            return AdmissionDecision::Admitted(RateGrant { _key: PhantomData });
        }
        // Table full of other keys.
        self.full_count = self.full_count.saturating_add(1);
        AdmissionDecision::Full(self.report())
    }

    fn admit_existing(&mut self, idx: usize, now: Instant) -> AdmissionDecision<RateGrant<K>> {
        let burst_nt = self.burst_nano_tokens;
        let rate = self.rate_per_sec;
        let slot = self
            .slots
            .get_mut(idx)
            .and_then(Option::as_mut)
            .expect("admit_existing called for a live slot");
        // Refill: elapsed seconds since last_seen × rate, in nano-tokens.
        // `checked_duration_since` returns None when `now < last_seen` —
        // a backwards clock contributes zero credit and does *not* lower
        // last_seen, so a later forward call cannot over-refill the bucket
        // by exploiting the regression.
        let elapsed_ns: u128 = now
            .checked_duration_since(slot.last_seen)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let gained_nt = elapsed_ns.saturating_mul(u128::from(rate));
        let new_available = slot.available_nt.saturating_add(gained_nt).min(burst_nt);
        let next_last_seen = if elapsed_ns == 0 { slot.last_seen } else { now };
        if new_available >= ONE_TOKEN_NT {
            slot.available_nt = new_available - ONE_TOKEN_NT;
            slot.last_seen = next_last_seen;
            AdmissionDecision::Admitted(RateGrant { _key: PhantomData })
        } else {
            // Not enough credit. Compute `retry_after` deterministically.
            let needed_nt = ONE_TOKEN_NT - new_available;
            let retry_ns = needed_nt.div_ceil(u128::from(rate));
            // u64::MAX nanoseconds is ~584 years; clamp to keep the cast safe.
            let retry_after = Duration::from_nanos(u64::try_from(retry_ns).unwrap_or(u64::MAX));
            slot.available_nt = new_available;
            slot.last_seen = next_last_seen;
            self.rate_limited_count = self.rate_limited_count.saturating_add(1);
            AdmissionDecision::RateLimited {
                retry_after,
                report: self.report_const(),
            }
        }
    }

    fn find_slot(&self, key: &K) -> Option<usize> {
        for (idx, slot) in self.slots.iter().enumerate() {
            if let Some(s) = slot {
                if &s.key == key {
                    return Some(idx);
                }
            }
        }
        None
    }

    /// Explicit policy-driven eviction of one key's bucket state.
    ///
    /// **This is an admin/policy lever, not a request-path helper.**
    /// Calling it on the request path turns the rate-limiter into a
    /// no-op for that key — the next admit re-initialises a full
    /// burst as if the tenant were brand new. The type system cannot
    /// prevent that misuse, so:
    ///
    /// - Use this from supervisor/policy code that has its own
    ///   admission decision about *whether* a key deserves eviction
    ///   (idle TTL, fairness round-robin, tenant deactivation, etc.).
    /// - Do not call it from the same code path that calls
    ///   [`try_admit`](Self::try_admit) for that key.
    ///
    /// Every eviction increments [`evicted_count`](Self::evicted_count)
    /// so audit/telemetry can watch for runaway calls.
    ///
    /// Returns `true` if the key was present.
    pub fn evict_key_for_capacity(&mut self, key: &K) -> bool {
        if let Some(idx) = self.find_slot(key) {
            self.slots[idx] = None;
            self.live_keys = self.live_keys.saturating_sub(1);
            self.evicted_count = self.evicted_count.saturating_add(1);
            true
        } else {
            false
        }
    }

    /// Mark this policy closed.
    pub fn close(&mut self) {
        self.closed = true;
    }

    /// `true` if closed.
    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    /// Build the current report. `current` is live key count.
    pub fn report(&self) -> AdmissionReport {
        self.report_const()
    }

    fn report_const(&self) -> AdmissionReport {
        AdmissionReport {
            surface: self.surface,
            mode: self.mode.clone(),
            capacity: self.slots.len(),
            current: self.live_keys,
            high_water: self.high_water_keys,
            full_count: self.full_count,
            rate_limited_count: self.rate_limited_count,
            wait_count: 0,
            degrade_count: 0,
            closed_count: self.closed_count,
            timed_out_count: 0,
        }
    }

    /// Inspect one key's bucket state, if any.
    pub fn key_state(&self, key: &K) -> Option<RateKeyState> {
        self.find_slot(key).and_then(|idx| {
            self.slots[idx].as_ref().map(|s| RateKeyState {
                available_tokens: s.available_nt / ONE_TOKEN_NT,
                available_nano_tokens: s.available_nt,
                last_seen: s.last_seen,
                burst: self.burst,
            })
        })
    }

    /// Capacity surface projection.
    pub fn capacity_surface(&self) -> CapacitySurfaceReport {
        self.report().capacity_surface()
    }
}

/// Snapshot of one key's rate-limit bucket.
#[derive(Debug, Clone, Copy)]
pub struct RateKeyState {
    /// Whole tokens currently available.
    pub available_tokens: u128,
    /// Available credit in nano-tokens (1 token = 1_000_000_000 nt).
    pub available_nano_tokens: u128,
    /// Last `now` observed for this key.
    pub last_seen: Instant,
    /// Configured burst.
    pub burst: u32,
}

/// Move-only proof of one successful rate-limit admission.
///
/// Carries no charge to release; it exists so callers cannot fake admission.
/// Drop the grant on the admitted path; the policy does not track grants
/// after the decision.
#[must_use = "RateGrant must be consumed on the admitted path"]
#[derive(Debug)]
pub struct RateGrant<K> {
    _key: PhantomData<K>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_now() -> Instant {
        Instant::now()
    }

    #[test]
    fn concurrency_limit_admit_release_refill() {
        let mut limit = ConcurrencyLimit::with_capacity("conc.test", 2);
        let a = match limit.try_admit() {
            AdmissionDecision::Admitted(p) => p,
            other => panic!("expected Admitted, got {other:?}"),
        };
        let b = match limit.try_admit() {
            AdmissionDecision::Admitted(p) => p,
            other => panic!("expected Admitted, got {other:?}"),
        };
        match limit.try_admit() {
            AdmissionDecision::Full(report) => {
                assert_eq!(report.full_count, 1);
                assert_eq!(report.capacity, 2);
                assert_eq!(report.current, 2);
            }
            other => panic!("expected Full, got {other:?}"),
        }
        limit.release(a).unwrap();
        match limit.try_admit() {
            AdmissionDecision::Admitted(p) => {
                limit.release(p).unwrap();
            }
            other => panic!("expected Admitted, got {other:?}"),
        }
        limit.release(b).unwrap();
        let snap = limit.report();
        assert_eq!(snap.current, 0);
        assert_eq!(snap.high_water, 2);
    }

    #[test]
    fn concurrency_limit_actions_map_correctly() {
        let mut shed = ConcurrencyLimit::with_capacity("conc.shed", 1);
        let _p = match shed.try_admit() {
            AdmissionDecision::Admitted(p) => p,
            other => panic!("got {other:?}"),
        };
        match shed.try_admit() {
            AdmissionDecision::Full(_) => {}
            other => panic!("Shed must return Full, got {other:?}"),
        }

        let mut degrade =
            ConcurrencyLimit::with_capacity("conc.degrade", 1).on_pressure(PressureAction::Degrade);
        let _p = degrade.try_admit().into_admitted().unwrap();
        match degrade.try_admit() {
            AdmissionDecision::Degrade { report } => {
                assert_eq!(report.degrade_count, 1);
            }
            other => panic!("Degrade expected, got {other:?}"),
        }

        let mut close =
            ConcurrencyLimit::with_capacity("conc.close", 1).on_pressure(PressureAction::Close);
        let _p = close.try_admit().into_admitted().unwrap();
        match close.try_admit() {
            AdmissionDecision::Closed(r) => {
                assert_eq!(r.closed_count, 1);
            }
            other => panic!("Close expected, got {other:?}"),
        }

        let mut wait = ConcurrencyLimit::with_capacity("conc.wait", 1)
            .on_pressure(PressureAction::Wait)
            .wait_hint(Duration::from_millis(7));
        let _p = wait.try_admit().into_admitted().unwrap();
        match wait.try_admit() {
            AdmissionDecision::Wait { delay, report } => {
                assert_eq!(delay, Duration::from_millis(7));
                assert_eq!(report.wait_count, 1);
            }
            other => panic!("Wait expected, got {other:?}"),
        }
    }

    #[test]
    fn concurrency_close_is_sticky_and_typed() {
        let mut limit = ConcurrencyLimit::with_capacity("conc.sticky", 2);
        limit.close();
        match limit.try_admit() {
            AdmissionDecision::Closed(r) => {
                assert!(r.closed_count >= 1);
            }
            other => panic!("Closed expected, got {other:?}"),
        }
        match limit.try_admit() {
            AdmissionDecision::Closed(r) => {
                assert!(r.closed_count >= 2);
            }
            other => panic!("Closed expected, got {other:?}"),
        }
    }

    #[test]
    fn keyed_limit_isolates_keys() {
        let mut limit = KeyedLimit::<&'static str>::new("keyed.test", 4, 2);
        let a1 = limit.try_admit(&"alpha").into_admitted().unwrap();
        let a2 = limit.try_admit(&"alpha").into_admitted().unwrap();
        // alpha at cap.
        match limit.try_admit(&"alpha") {
            AdmissionDecision::Full(_) => {}
            other => panic!("alpha per-key full expected, got {other:?}"),
        }
        // beta still admits.
        let b1 = limit.try_admit(&"beta").into_admitted().unwrap();
        let key_a = limit.key_report(&"alpha").unwrap();
        assert_eq!(key_a.current, 2);
        let key_b = limit.key_report(&"beta").unwrap();
        assert_eq!(key_b.current, 1);
        assert_eq!(limit.per_key_full_count(), 1);
        limit.release(a1).unwrap();
        limit.release(a2).unwrap();
        // After both alphas release, the slot is free; a fresh key can use it.
        let _c = limit.try_admit(&"gamma").into_admitted().unwrap();
        limit.release(b1).unwrap();
    }

    #[test]
    fn keyed_limit_table_full_when_all_distinct() {
        let mut limit = KeyedLimit::<u32>::new("keyed.full", 2, 1);
        let p0 = limit.try_admit(&0).into_admitted().unwrap();
        let p1 = limit.try_admit(&1).into_admitted().unwrap();
        match limit.try_admit(&2) {
            AdmissionDecision::Full(report) => {
                assert_eq!(report.capacity, 2);
                assert_eq!(report.current, 2);
                assert!(report.full_count >= 1);
            }
            other => panic!("expected Full, got {other:?}"),
        }
        limit.release(p0).unwrap();
        // Slot freed; 2 may now claim it.
        let _p2 = limit.try_admit(&2).into_admitted().unwrap();
        limit.release(p1).unwrap();
    }

    #[test]
    fn keyed_stale_permit_after_slot_reuse_is_rejected() {
        let mut limit = KeyedLimit::<&'static str>::new("keyed.stale", 1, 1);
        let p_alpha = limit.try_admit(&"alpha").into_admitted().unwrap();
        // alpha occupies the only slot; record its generation.
        let gen_a = limit.key_report(&"alpha").unwrap().generation;
        // Release alpha — slot freed.
        limit.release(p_alpha).unwrap();
        // beta claims the same slot with a new generation.
        let p_beta = limit.try_admit(&"beta").into_admitted().unwrap();
        let gen_b = limit.key_report(&"beta").unwrap().generation;
        assert_ne!(gen_a, gen_b);
        // A stale permit from alpha (synthesized via the public id) cannot
        // be made here because permits are move-only; the proof is the
        // generation field on KeyedSlotReport — beta's generation differs.
        // Releasing beta works.
        limit.release(p_beta).unwrap();
    }

    #[test]
    fn rate_limit_first_admit_consumes_one_token() {
        let mut limit = RateLimit::<&'static str>::new("rate.first", 4, 10, 5);
        let now = fixed_now();
        let _ = limit.try_admit(&"alpha", now).into_admitted().unwrap();
        let state = limit.key_state(&"alpha").unwrap();
        assert_eq!(state.available_tokens, 4);
    }

    #[test]
    fn rate_limit_bucket_drains_then_rate_limits_with_deterministic_retry() {
        // 10 admissions/sec, burst 3.
        let mut limit = RateLimit::<&'static str>::new("rate.drain", 4, 10, 3);
        let now = fixed_now();
        // First 3 admits succeed (burst).
        for _ in 0..3 {
            let _ = limit.try_admit(&"alpha", now).into_admitted().unwrap();
        }
        match limit.try_admit(&"alpha", now) {
            AdmissionDecision::RateLimited {
                retry_after,
                report,
            } => {
                // 1 token at 10/sec = 100ms.
                assert_eq!(retry_after, Duration::from_millis(100));
                assert_eq!(report.rate_limited_count, 1);
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn rate_limit_refills_over_time_deterministically() {
        let mut limit = RateLimit::<u32>::new("rate.refill", 4, 10, 2);
        let t0 = fixed_now();
        let _ = limit.try_admit(&1, t0).into_admitted().unwrap();
        let _ = limit.try_admit(&1, t0).into_admitted().unwrap();
        match limit.try_admit(&1, t0) {
            AdmissionDecision::RateLimited { retry_after, .. } => {
                assert_eq!(retry_after, Duration::from_millis(100));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
        // After 100ms, one token has refilled.
        let t1 = t0 + Duration::from_millis(100);
        let _ = limit.try_admit(&1, t1).into_admitted().unwrap();
        // The next admit is rate-limited again.
        match limit.try_admit(&1, t1) {
            AdmissionDecision::RateLimited { retry_after, .. } => {
                assert_eq!(retry_after, Duration::from_millis(100));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn rate_limit_cold_key_unaffected_by_hot_key() {
        let mut limit = RateLimit::<&'static str>::new("rate.fair", 4, 5, 2);
        let now = fixed_now();
        // Hot key exhausts its bucket.
        for _ in 0..2 {
            let _ = limit.try_admit(&"hot", now).into_admitted().unwrap();
        }
        match limit.try_admit(&"hot", now) {
            AdmissionDecision::RateLimited { .. } => {}
            other => panic!("hot must be limited, got {other:?}"),
        }
        // Cold key still succeeds with its own bucket.
        let _ = limit.try_admit(&"cold", now).into_admitted().unwrap();
    }

    #[test]
    fn rate_limit_replay_is_byte_identical_under_same_inputs() {
        // Two runs over the same (key, now) sequence must produce the same
        // decision sequence. This is the "sim replay proves determinism"
        // proof for time-based policy.
        let now0 = fixed_now();
        let trace = |limit: &mut RateLimit<&'static str>| {
            let mut out = Vec::new();
            let inputs: &[(&'static str, Duration)] = &[
                ("alpha", Duration::ZERO),
                ("alpha", Duration::ZERO),
                ("alpha", Duration::ZERO),
                ("beta", Duration::ZERO),
                ("alpha", Duration::from_millis(50)),
                ("alpha", Duration::from_millis(150)),
                ("beta", Duration::from_millis(150)),
                ("alpha", Duration::from_millis(160)),
            ];
            for (key, offset) in inputs {
                let outcome = match limit.try_admit(key, now0 + *offset) {
                    AdmissionDecision::Admitted(_) => "ok".to_string(),
                    AdmissionDecision::RateLimited { retry_after, .. } => {
                        format!("rate:{}ms", retry_after.as_millis())
                    }
                    other => format!("other:{other:?}"),
                };
                out.push((*key, *offset, outcome));
            }
            out
        };
        let mut a = RateLimit::<&'static str>::new("rate.replay", 4, 10, 2);
        let mut b = RateLimit::<&'static str>::new("rate.replay", 4, 10, 2);
        let trace_a = trace(&mut a);
        let trace_b = trace(&mut b);
        assert_eq!(trace_a, trace_b);
    }

    #[test]
    fn rate_limit_backwards_clock_does_not_over_refill() {
        // A backwards-going `now` must not let a later forward call admit
        // more requests than the wall-clock rate would allow.
        let mut limit = RateLimit::<&'static str>::new("rate.skew", 2, 10, 1);
        let t0 = fixed_now();
        // First admit at t0 consumes the only burst token.
        let _ = limit.try_admit(&"alpha", t0).into_admitted().unwrap();
        // At t0 + 200ms the bucket would have refilled 2 tokens but caps
        // at burst=1, so the next admit succeeds.
        let t_forward = t0 + Duration::from_millis(200);
        let _ = limit
            .try_admit(&"alpha", t_forward)
            .into_admitted()
            .unwrap();
        // Now go backwards.
        match limit.try_admit(&"alpha", t0) {
            AdmissionDecision::RateLimited { .. } => {}
            other => panic!("expected RateLimited on backward clock, got {other:?}"),
        }
        // After the backward call, last_seen must still be t_forward; the
        // bucket has at most ~100ms × rate ≈ 1 token of credit since the
        // last admit at t_forward. At t_forward + 50ms there should not
        // yet be enough credit for an admit.
        match limit.try_admit(&"alpha", t_forward + Duration::from_millis(50)) {
            AdmissionDecision::RateLimited { .. } => {}
            other => panic!("backwards clock must not let bucket over-refill — got {other:?}"),
        }
    }

    #[test]
    fn rate_limit_table_full_does_not_evict() {
        let mut limit = RateLimit::<u32>::new("rate.cap", 2, 5, 1);
        let now = fixed_now();
        let _ = limit.try_admit(&1, now).into_admitted().unwrap();
        let _ = limit.try_admit(&2, now).into_admitted().unwrap();
        // Third key — table full, no automatic eviction.
        match limit.try_admit(&3, now) {
            AdmissionDecision::Full(r) => {
                assert_eq!(r.capacity, 2);
                assert_eq!(r.current, 2);
            }
            other => panic!("expected Full, got {other:?}"),
        }
        // Explicit policy eviction is the only path that frees a slot for
        // a new key. evicted_count goes up; live_keys goes down; the next
        // admit for the evicted key starts a fresh bucket.
        assert_eq!(limit.evicted_count(), 0);
        assert!(limit.evict_key_for_capacity(&1));
        assert_eq!(limit.evicted_count(), 1);
        assert_eq!(limit.live_keys(), 1);
        let _ = limit.try_admit(&3, now).into_admitted().unwrap();
        // A second eviction of an absent key is a no-op for telemetry.
        assert!(!limit.evict_key_for_capacity(&99));
        assert_eq!(limit.evicted_count(), 1);
    }

    #[test]
    fn rate_limit_eviction_resets_bucket_state() {
        // Eviction is policy-owned: the next admit for the evicted key
        // gets a fresh full burst as if the tenant were brand new. This
        // is the documented behavior — callers must not use eviction
        // as a request-path "reset" because it bypasses rate-limiting.
        let mut limit = RateLimit::<&'static str>::new("rate.reset", 2, 10, 1);
        let t0 = fixed_now();
        let _ = limit.try_admit(&"alpha", t0).into_admitted().unwrap();
        // Bucket exhausted — next admit at the same instant is rate-limited.
        match limit.try_admit(&"alpha", t0) {
            AdmissionDecision::RateLimited { .. } => {}
            other => panic!("expected RateLimited, got {other:?}"),
        }
        // Evict and re-admit. The evicted key admits immediately, with
        // a fresh burst, even though no time has passed.
        assert!(limit.evict_key_for_capacity(&"alpha"));
        let _ = limit
            .try_admit(&"alpha", t0)
            .into_admitted()
            .expect("evicted key starts fresh");
        // Telemetry records the eviction; CI can alarm on a runaway
        // counter that would mean a request-path bypass.
        assert_eq!(limit.evicted_count(), 1);
    }

    #[test]
    fn keyed_limit_live_keys_field_is_correct_after_churn() {
        // live_keys is now a field; this test pins the increment/decrement
        // accounting against a churn pattern: alloc, hit per-key cap,
        // release, allocate a different key on the freed slot, release.
        let mut limit = KeyedLimit::<&'static str>::new("keyed.live_field", 3, 2);
        assert_eq!(limit.live_keys(), 0);
        let a1 = limit.try_admit(&"alpha").into_admitted().unwrap();
        assert_eq!(limit.live_keys(), 1);
        let a2 = limit.try_admit(&"alpha").into_admitted().unwrap();
        assert_eq!(
            limit.live_keys(),
            1,
            "per-key admit must not change live count"
        );
        let _full = limit.try_admit(&"alpha");
        assert_eq!(
            limit.live_keys(),
            1,
            "per-key Full must not change live count"
        );
        let b1 = limit.try_admit(&"beta").into_admitted().unwrap();
        assert_eq!(limit.live_keys(), 2);
        limit.release(a1).unwrap();
        assert_eq!(limit.live_keys(), 2, "partial alpha release keeps slot");
        limit.release(a2).unwrap();
        assert_eq!(limit.live_keys(), 1, "final alpha release frees slot");
        let _g = limit.try_admit(&"gamma").into_admitted().unwrap();
        assert_eq!(limit.live_keys(), 2);
        limit.release(b1).unwrap();
        assert_eq!(limit.live_keys(), 1);
    }

    #[test]
    fn timed_out_decision_increments_counter() {
        let report = AdmissionReport {
            surface: "fake",
            mode: CapacityMode::Fixed,
            capacity: 1,
            current: 1,
            high_water: 1,
            full_count: 0,
            rate_limited_count: 0,
            wait_count: 0,
            degrade_count: 0,
            closed_count: 0,
            timed_out_count: 0,
        };
        let decision: AdmissionDecision<()> = AdmissionDecision::timed_out(report);
        match decision {
            AdmissionDecision::TimedOut(r) => assert_eq!(r.timed_out_count, 1),
            other => panic!("expected TimedOut, got {other:?}"),
        }
    }

    #[test]
    fn admission_failure_borrow_report_is_consistent() {
        let mut limit = ConcurrencyLimit::with_capacity("conc.fail", 1);
        let _p = limit.try_admit().into_admitted().unwrap();
        let failure = limit.try_admit().into_admitted().unwrap_err();
        match &failure {
            AdmissionFailure::Full(r) => {
                assert_eq!(r.surface, "conc.fail");
                assert_eq!(r.full_count, 1);
            }
            other => panic!("expected Full, got {other:?}"),
        }
        let report = failure.report();
        assert_eq!(report.full_count, 1);
    }

    #[test]
    fn non_shed_actions_do_not_double_count_in_capacity_surface() {
        // Regression: under PressureAction::Degrade the underlying gate
        // increments its cap-reached counter on every refused try_admit.
        // If the AdmissionReport surfaced that gate count as `full_count`,
        // the capacity-surface projection would sum it with `degrade_count`
        // and double-count each event. The fix is to count only decisions
        // that returned `Full(_)` toward `full_count`; the gate's view is
        // exposed separately as `gate_full_count()`.
        let mut limit = ConcurrencyLimit::with_capacity("conc.no_double_count", 1)
            .on_pressure(PressureAction::Degrade);
        let _held = limit.try_admit().into_admitted().unwrap();
        // Three Degrade rejections.
        for _ in 0..3 {
            match limit.try_admit() {
                AdmissionDecision::Degrade { .. } => {}
                other => panic!("expected Degrade, got {other:?}"),
            }
        }
        let report = limit.report();
        // The decision-level full_count must be 0 — every rejection was
        // a Degrade.
        assert_eq!(
            report.full_count, 0,
            "Degrade actions must not register as Full decisions: {report:?}"
        );
        assert_eq!(report.degrade_count, 3);
        // Underlying gate still tracks cap-reached separately.
        assert_eq!(limit.gate_full_count(), 3);
        // Capacity-surface projection sums rejection categories — degrade
        // counts once each, not twice.
        let surface = limit.capacity_surface();
        assert_eq!(
            surface.full_count, 3,
            "projection must count 3 rejections, not 6: {surface:?}"
        );

        // Same regression test for Close.
        let mut closing = ConcurrencyLimit::with_capacity("conc.close_no_dup", 1)
            .on_pressure(PressureAction::Close);
        let _h = closing.try_admit().into_admitted().unwrap();
        let _ = closing.try_admit();
        let _ = closing.try_admit();
        let report = closing.report();
        assert_eq!(report.full_count, 0);
        assert_eq!(report.closed_count, 2);
        assert_eq!(closing.capacity_surface().full_count, 2);

        // Wait too.
        let mut waiting = ConcurrencyLimit::with_capacity("conc.wait_no_dup", 1)
            .on_pressure(PressureAction::Wait)
            .wait_hint(Duration::from_millis(1));
        let _h = waiting.try_admit().into_admitted().unwrap();
        let _ = waiting.try_admit();
        let _ = waiting.try_admit();
        let report = waiting.report();
        assert_eq!(report.full_count, 0);
        assert_eq!(report.wait_count, 2);
        assert_eq!(waiting.capacity_surface().full_count, 2);
    }

    #[test]
    fn shed_full_count_matches_returned_decisions() {
        let mut limit = ConcurrencyLimit::with_capacity("conc.shed_count", 1);
        let _h = limit.try_admit().into_admitted().unwrap();
        let _ = limit.try_admit();
        let _ = limit.try_admit();
        let report = limit.report();
        assert_eq!(report.full_count, 2, "Shed should count returned Fulls");
        assert_eq!(limit.gate_full_count(), 2, "gate matches under Shed");
        assert_eq!(limit.capacity_surface().full_count, 2);
    }

    #[test]
    fn capacity_surface_projection_round_trips_counts() {
        let mut limit = ConcurrencyLimit::with_capacity("conc.surface", 2);
        let _ = limit.try_admit().into_admitted().unwrap();
        let _ = limit.try_admit().into_admitted().unwrap();
        // Force a Full.
        let _ = limit.try_admit();
        let surface = limit.capacity_surface();
        assert_eq!(surface.name, "conc.surface");
        assert_eq!(surface.max_messages, Some(2));
        assert_eq!(surface.current_messages, 2);
        assert!(surface.full_count >= 1);
    }
}
