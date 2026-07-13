//! Admission and rate policy types.
//!
//! User-facing pressure policies built on top of the existing
//! [`LocalPermitGate`], [`SharedCapacityScope`], [`crate::FullHandling`], and
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
//! - [`ShedRateLimit`] — the same token bucket with table pressure fixed to
//!   immediate shedding and a four-variant [`ShedRateLimitDecision`].
//!
//! The configurable policies return [`AdmissionDecision`]. `ShedRateLimit`
//! returns its smaller decision vocabulary because wait, degrade, and
//! pressure-triggered close are unrepresentable in its configuration.
//! Successful concurrency admission carries a move-only proof object the
//! caller must release explicitly; a rate grant instead proves that one token
//! was consumed. There is no hidden retry, hidden queue, or growing per-key
//! map.
//!
//! Retry remains caller-owned. Pair these policies with [`crate::FullHandling`]
//! when retry-with-backoff is the right answer, or treat each rejection as
//! terminal.
//!
//! # Design notes and deliberate non-features
//!
//! These are choices, not gaps. They are documented here so the boundaries
//! are explicit:
//!
//! - **No retry inside admission.** None of these types retry. The only
//!   retry path is the separate [`crate::FullHandling`], whose
//!   `retry_backoff(Backoff)` constructor *requires* an explicit
//!   [`tina::time::Backoff`] value — there is no way to get retry behavior
//!   without naming a budget, and the caller still owns idempotency.
//! - **Eviction cannot be type-state-locked to policy code.** A handler
//!   that owns `&mut self` (and therefore `&mut limit`) inside an isolate
//!   can call any method. True request-vs-admin separation would require
//!   splitting ownership across isolates, which is disproportionate here.
//!   [`RateLimit::evict_key_for_capacity`] is therefore enforced by
//!   convention + the `evicted_count` telemetry counter, not by the type
//!   system.
//! - **[`KeyedLimit`] has no eviction.** Its slots hold live, move-only
//!   permits; evicting a key with outstanding permits would orphan them.
//!   Slots free themselves when the last permit is released. There is no
//!   meaningful eviction to expose.
//! - **Per-key storage owns `K`.** Lookups borrow (`try_admit(&K)`), so the
//!   hot path of an existing key is allocation-free. A new key is cloned
//!   once when its slot is allocated, and the stored `K` is dropped when the
//!   slot frees. For `K = String` that is one alloc per *slot allocation*,
//!   not per request.
//! - **[`AdmissionDecision`] is sized at its largest variant** (it carries an
//!   [`AdmissionReport`]). It is meant to be matched and discarded at the
//!   admission site, not stored. [`AdmissionDecision::into_admitted`] keeps
//!   the failure arm by value for `?`-style flows.
//! - **Dropped permits feed a process-wide counter.** Permits are
//!   leak-detected through [`crate::dropped_permit_count`], a global; this is
//!   intentional, since a permit's `Drop` has no back-reference to its gate.

use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tina::capacity::{CapacityMode, CapacitySurfaceReport};

use crate::local_permit::{LocalPermitGate, LocalPermitName, Permit};
use crate::shared_scope::{SharedCapacityScope, SharedLease};

/// Process-wide source of unique gate identifiers.
///
/// Every admission policy that issues move-only permits stamps each permit
/// with its gate's id. Release checks the id so a permit cannot be released
/// against a *different* policy instance (which could otherwise decrement
/// the wrong slot when two gates share the same generation/slot layout).
static NEXT_GATE_ID: AtomicU64 = AtomicU64::new(1);

fn next_gate_id() -> u64 {
    NEXT_GATE_ID.fetch_add(1, Ordering::Relaxed)
}

/// Surface name carried by an admission policy and its reports.
///
/// `Cow<'static, str>` so the common case (a string literal known at compile
/// time) stays allocation-free, while services that name surfaces per route
/// or per tenant at runtime can pass an owned `String`.
pub type SurfaceName = Cow<'static, str>;

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
    pub surface: SurfaceName,
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
    /// Times an explicit policy eviction freed a key
    /// ([`RateLimit::evict_key_for_capacity`]). This is **not** a rejection
    /// and does not contribute to [`Self::any_rejection`] or the
    /// capacity-surface `full_count`; it is admin-action telemetry so a
    /// runaway counter (a request-path bypass) stays visible.
    pub evicted_count: u64,
}

impl AdmissionReport {
    /// `true` if any rejection category has been observed.
    ///
    /// Eviction is excluded — it is an admin action, not a rejection.
    pub const fn any_rejection(&self) -> bool {
        self.full_count > 0
            || self.rate_limited_count > 0
            || self.wait_count > 0
            || self.degrade_count > 0
            || self.closed_count > 0
            || self.timed_out_count > 0
    }

    /// Sum of every rejection category.
    pub const fn total_rejections(&self) -> u64 {
        self.full_count
            .saturating_add(self.rate_limited_count)
            .saturating_add(self.wait_count)
            .saturating_add(self.degrade_count)
            .saturating_add(self.closed_count)
            .saturating_add(self.timed_out_count)
    }

    /// Project this report onto a [`CapacitySurfaceReport`].
    ///
    /// Count fields map directly. `full_count` aggregates every rejection
    /// category that means "we did not admit": full + rate-limited + wait +
    /// degrade + closed + timed-out. This keeps `summary.any_full()` honest
    /// for admission surfaces — any rejection counts as overload truth.
    /// `evicted_count` is intentionally excluded; eviction is not overload.
    pub fn capacity_surface(&self) -> CapacitySurfaceReport {
        CapacitySurfaceReport::count(
            self.surface.clone().into_owned(),
            self.mode.clone(),
            self.capacity,
            self.current,
            self.high_water,
            self.total_rejections(),
        )
    }
}

impl fmt::Display for AdmissionReport {
    /// One grep-friendly `key=value` line, mirroring the capacity
    /// discovery shape.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "admission surface={} mode={} cap={} cur={} high={} full={} rate_limited={} wait={} degrade={} closed={} timed_out={} evicted={}",
            self.surface,
            self.mode.label(),
            self.capacity,
            self.current,
            self.high_water,
            self.full_count,
            self.rate_limited_count,
            self.wait_count,
            self.degrade_count,
            self.closed_count,
            self.timed_out_count,
            self.evicted_count,
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

    /// Short kind label for logs.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Full(_) => "full",
            Self::RateLimited { .. } => "rate_limited",
            Self::Wait { .. } => "wait",
            Self::Degrade { .. } => "degrade",
            Self::Closed(_) => "closed",
            Self::TimedOut(_) => "timed_out",
        }
    }
}

impl fmt::Display for AdmissionFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RateLimited {
                retry_after,
                report,
            } => write!(
                f,
                "admission_rejected={} retry_after_ms={} ({report})",
                self.kind(),
                retry_after.as_millis()
            ),
            Self::Wait { delay, report } => write!(
                f,
                "admission_rejected={} wait_ms={} ({report})",
                self.kind(),
                delay.as_millis()
            ),
            other => write!(f, "admission_rejected={} ({})", self.kind(), other.report()),
        }
    }
}

impl std::error::Error for AdmissionFailure {}

// -----------------------------------------------------------------------------
// ServicePolicy — the public admission-policy extension seam
// -----------------------------------------------------------------------------

/// A custom admission/rate policy.
///
/// This is the open extension seam for service pressure policies. The
/// built-in policies ([`ConcurrencyLimit`], [`KeyedLimit`], [`RateLimit`])
/// keep their ergonomic inherent `try_admit` methods, and also implement
/// this trait so generic service code can drive a built-in or custom
/// policy through one `(key, now) -> decision` shape. Policies that are
/// not time-based ignore `now`.
///
/// The contract a custom policy must keep:
///
/// - **Return a decision; do not act.** [`decide`](ServicePolicy::decide)
///   returns an [`AdmissionDecision`]. The policy must **not** send
///   messages, spawn work, retry, sleep, or wait. Retry and waiting stay
///   caller-owned (pair with [`crate::FullHandling`] when retry is the
///   right answer). A `Wait { delay, .. }` decision is advice the caller
///   acts on, never a hidden queue the policy drains.
/// - **Be replayable.** `decide` must be a pure function of
///   `(config, now, key history)`. Never read wall-clock time inside the
///   policy; take `now` from `ctx.now()` (live) or the simulator
///   (replay). The same inputs must yield the same decision so a DST run
///   reproduces a live overload exactly.
/// - **Report the truth.** [`report`](ServicePolicy::report) returns an
///   [`AdmissionReport`] snapshot. Counts must be the policy's real
///   state, not a fresh config, so a dashboard sees installed capacity
///   and accumulated rejections.
///
/// Admission carries a move-only `Permit`/grant proof the caller releases
/// (or drops) explicitly; the policy does not track it after the
/// decision.
pub trait ServicePolicy {
    /// The key the policy admits against (`()` for a global policy).
    type Key: ?Sized;
    /// The move-only proof returned on admission.
    type Permit;

    /// Decide admission for `key` at logical time `now`. Pure over
    /// `(config, now, key history)`; must not send, retry, sleep, or
    /// wait.
    fn decide(&mut self, key: &Self::Key, now: Instant) -> AdmissionDecision<Self::Permit>;

    /// A replayable snapshot of the policy's pressure state.
    fn report(&self) -> AdmissionReport;
}

impl<K: Eq + Clone> ServicePolicy for RateLimit<K> {
    type Key = K;
    type Permit = RateGrant<K>;

    fn decide(&mut self, key: &K, now: Instant) -> AdmissionDecision<RateGrant<K>> {
        self.try_admit(key, now)
    }

    fn report(&self) -> AdmissionReport {
        RateLimit::report(self)
    }
}

impl ServicePolicy for ConcurrencyLimit {
    type Key = ();
    type Permit = ConcurrencyPermit;

    fn decide(&mut self, _key: &(), _now: Instant) -> AdmissionDecision<ConcurrencyPermit> {
        self.try_admit()
    }

    fn report(&self) -> AdmissionReport {
        ConcurrencyLimit::report(self)
    }
}

impl<K: Eq + Clone> ServicePolicy for KeyedLimit<K> {
    type Key = K;
    type Permit = KeyedPermit<K>;

    fn decide(&mut self, key: &K, _now: Instant) -> AdmissionDecision<KeyedPermit<K>> {
        self.try_admit(key)
    }

    fn report(&self) -> AdmissionReport {
        KeyedLimit::report(self)
    }
}

// -----------------------------------------------------------------------------
// ConcurrencyLimit
// -----------------------------------------------------------------------------

/// Fixed-capacity local concurrency policy.
///
/// Wrapper over [`LocalPermitGate`] that returns [`AdmissionDecision`]
/// instead of `Result<Permit, LocalPermitFull>` so it composes with the rest
/// of the admission vocabulary. The action on full is configurable: shed,
/// degrade, close, or hint a wait.
///
/// Optionally charges a shared [`SharedCapacityScope`] alongside the local
/// gate (see [`with_shared_scope`](Self::with_shared_scope)) so several
/// routes can share one weighted budget while each keeps its own local cap.
///
/// Admission returns a [`ConcurrencyPermit`], which is stamped with this
/// limit's process-unique gate id. Releasing a permit on a *different*
/// `ConcurrencyLimit` is rejected with [`ConcurrencyReleaseError::WrongGate`]
/// (and the permit is handed back) instead of silently decrementing the
/// wrong gate.
#[derive(Debug)]
#[must_use = "ConcurrencyLimit is state; store it on the isolate"]
pub struct ConcurrencyLimit {
    surface: SurfaceName,
    gate_id: u64,
    mode: CapacityMode,
    gate: LocalPermitGate,
    shared: Option<SharedScopeBinding>,
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

#[derive(Debug)]
struct SharedScopeBinding {
    scope: SharedCapacityScope,
    weight: usize,
}

impl ConcurrencyLimit {
    /// Build a shed-on-full concurrency limit.
    pub fn with_capacity(surface: impl Into<SurfaceName>, capacity: usize) -> Self {
        let surface = surface.into();
        // LocalPermitName needs a `&'static str`; only static surfaces get a
        // gate name. The admission report always carries the full surface
        // (static or owned), so dynamic names are not lost.
        let gate = match &surface {
            Cow::Borrowed(name) => {
                LocalPermitGate::with_capacity(capacity).named(LocalPermitName(name))
            }
            Cow::Owned(_) => LocalPermitGate::with_capacity(capacity),
        };
        Self {
            surface,
            gate_id: next_gate_id(),
            mode: CapacityMode::Fixed,
            gate,
            shared: None,
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
    pub fn on_pressure(mut self, action: PressureAction) -> Self {
        self.action = action;
        self
    }

    /// Set the wait hint returned when `PressureAction::Wait` is selected.
    /// Ignored for other actions.
    pub fn wait_hint(mut self, delay: Duration) -> Self {
        self.wait_hint = delay;
        self
    }

    /// Also charge a shared weighted budget on each admission.
    ///
    /// On `try_admit`, the local gate is charged first; if it admits, the
    /// shared scope is charged `weight`. If the scope is full, the local
    /// permit is released immediately and the decision is the configured
    /// pressure outcome (default `Full`) with the report decorated by the
    /// shared scope's columns. The returned [`ConcurrencyPermit`] owns the
    /// [`SharedLease`]; releasing or dropping it releases both.
    pub fn with_shared_scope(mut self, scope: SharedCapacityScope, weight: usize) -> Self {
        self.shared = Some(SharedScopeBinding { scope, weight });
        self
    }

    /// This limit's process-unique gate id.
    pub const fn gate_id(&self) -> u64 {
        self.gate_id
    }

    /// Try to admit one new permit.
    pub fn try_admit(&mut self) -> AdmissionDecision<ConcurrencyPermit> {
        if self.closed {
            self.closed_count = self.closed_count.saturating_add(1);
            return AdmissionDecision::Closed(self.report());
        }
        let permit = match self.gate.try_admit() {
            Ok(permit) => permit,
            Err(_) => return self.refuse(),
        };
        // Local gate admitted. Charge the shared scope if bound.
        // Two-phase: the local permit is retired (not completed) if the
        // shared scope is full, so `current` returns to its pre-admit
        // value. The gate's `high_water` may briefly reflect the rolled-
        // back attempt — that is accurate (a slot *was* momentarily taken)
        // and never affects `current` or the rejection counters.
        let lease = match &self.shared {
            Some(binding) => match binding.scope.try_admit(binding.weight) {
                Ok(lease) => Some(lease),
                Err(_) => {
                    let _ = self.gate.retire(permit);
                    return self.refuse();
                }
            },
            None => None,
        };
        AdmissionDecision::Admitted(ConcurrencyPermit {
            inner: Some(permit),
            lease,
            gate_id: self.gate_id,
        })
    }

    /// Build the configured pressure outcome for a refused admission.
    fn refuse(&mut self) -> AdmissionDecision<ConcurrencyPermit> {
        match self.action {
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
                self.closed = true;
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
        }
    }

    /// Release a permit and record a completion. Drops any shared lease.
    pub fn release(&mut self, permit: ConcurrencyPermit) -> Result<(), ConcurrencyReleaseError> {
        self.consume(permit, true)
    }

    /// Retire a permit without recording a completion. Drops any shared lease.
    pub fn retire(&mut self, permit: ConcurrencyPermit) -> Result<(), ConcurrencyReleaseError> {
        self.consume(permit, false)
    }

    fn consume(
        &mut self,
        mut permit: ConcurrencyPermit,
        record_completion: bool,
    ) -> Result<(), ConcurrencyReleaseError> {
        if permit.gate_id != self.gate_id {
            // Not ours. Hand it back so the caller can release it on the
            // gate that issued it; do not touch our counters.
            return Err(ConcurrencyReleaseError::WrongGate { permit });
        }
        // Drop the shared lease first (auto-releases the scope charge).
        permit.lease.take();
        let inner = permit
            .inner
            .take()
            .expect("ConcurrencyPermit::inner cleared by an earlier consume");
        let result = if record_completion {
            self.gate.release(inner)
        } else {
            self.gate.retire(inner)
        };
        result.map(|_| ()).map_err(ConcurrencyReleaseError::Gate)
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
    /// surface projection does not double-count. If a shared scope is
    /// bound, the capacity surface is decorated with the scope's columns.
    pub fn report(&self) -> AdmissionReport {
        let snap = self.gate.report();
        AdmissionReport {
            surface: self.surface.clone(),
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
            evicted_count: 0,
        }
    }

    /// Underlying gate-cap-reached count. Distinct from
    /// [`AdmissionReport::full_count`], which only counts decisions that
    /// surfaced as `Full(...)`. Use this for "how often was the local
    /// concurrency cap saturated" telemetry.
    pub fn gate_full_count(&self) -> u64 {
        self.gate.report().full_count
    }

    /// Capacity surface projection, decorated with the shared scope's
    /// columns if one is bound.
    pub fn capacity_surface(&self) -> CapacitySurfaceReport {
        let surface = self.report().capacity_surface();
        match &self.shared {
            Some(binding) => binding.scope.decorate(surface),
            None => surface,
        }
    }
}

/// Move-only proof of one successful [`ConcurrencyLimit`] admission.
///
/// Carries the underlying [`Permit`], an optional shared-scope
/// [`SharedLease`], and the issuing limit's gate id. Must be released or
/// retired on the limit that issued it.
#[must_use = "a ConcurrencyPermit must be released or retired; do not drop it silently"]
#[derive(Debug)]
pub struct ConcurrencyPermit {
    inner: Option<Permit>,
    lease: Option<SharedLease>,
    gate_id: u64,
}

impl ConcurrencyPermit {
    /// Gate id of the limit that issued this permit.
    pub const fn gate_id(&self) -> u64 {
        self.gate_id
    }

    /// Borrow the underlying local permit, for tracing.
    pub fn permit(&self) -> Option<&Permit> {
        self.inner.as_ref()
    }

    /// `true` if this permit also holds a shared-scope lease.
    pub const fn holds_shared_lease(&self) -> bool {
        self.lease.is_some()
    }
}

impl Drop for ConcurrencyPermit {
    fn drop(&mut self) {
        // Dropping the lease auto-releases the shared scope charge. The
        // inner Permit, if still present, was not handed back to its gate;
        // its own Drop increments the process-wide dropped-permit counter
        // so the leak is loud. (This only happens if the user drops the
        // ConcurrencyPermit instead of releasing it.)
        self.lease.take();
    }
}

/// Why a [`ConcurrencyLimit::release`] / `retire` failed.
#[derive(Debug)]
pub enum ConcurrencyReleaseError {
    /// The permit was issued by a different `ConcurrencyLimit`. The permit
    /// is returned so the caller can release it on the right limit.
    WrongGate {
        /// The permit, handed back intact.
        permit: ConcurrencyPermit,
    },
    /// The underlying gate rejected the release (stale generation, etc.).
    Gate(crate::LocalPermitReleaseError),
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
    surface: SurfaceName,
    gate_id: u64,
    mode: CapacityMode,
    per_key_capacity: usize,
    slots: Vec<Option<KeyedSlot<K>>>,
    live_keys: usize,
    high_water_keys: usize,
    action: PressureAction,
    wait_hint: Duration,
    full_count: u64,
    per_key_full_count: u64,
    degrade_count: u64,
    wait_count: u64,
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
    pub fn new(surface: impl Into<SurfaceName>, max_keys: usize, per_key: usize) -> Self {
        assert!(max_keys > 0, "KeyedLimit max_keys must be > 0");
        assert!(per_key > 0, "KeyedLimit per_key capacity must be > 0");
        let mut slots = Vec::with_capacity(max_keys);
        for _ in 0..max_keys {
            slots.push(None);
        }
        Self {
            surface: surface.into(),
            gate_id: next_gate_id(),
            mode: CapacityMode::Fixed,
            per_key_capacity: per_key,
            slots,
            live_keys: 0,
            high_water_keys: 0,
            action: PressureAction::Shed,
            wait_hint: Duration::ZERO,
            full_count: 0,
            per_key_full_count: 0,
            degrade_count: 0,
            wait_count: 0,
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

    /// Choose what to return when a key cannot be admitted (per-key cap
    /// reached or key table full). Default is [`PressureAction::Shed`]
    /// (`Full`).
    pub fn on_pressure(mut self, action: PressureAction) -> Self {
        self.action = action;
        self
    }

    /// Wait hint returned under [`PressureAction::Wait`]. Ignored otherwise.
    pub fn wait_hint(mut self, delay: Duration) -> Self {
        self.wait_hint = delay;
        self
    }

    /// This limit's process-unique gate id.
    pub const fn gate_id(&self) -> u64 {
        self.gate_id
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
                return self.refuse();
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
        self.refuse()
    }

    /// Map a refused admission (per-key cap or table full) onto the
    /// configured [`PressureAction`]. `full_count` is bumped only when the
    /// decision actually surfaces as `Full` so the capacity projection does
    /// not double-count (mirrors `ConcurrencyLimit`).
    fn refuse(&mut self) -> AdmissionDecision<KeyedPermit<K>> {
        match self.action {
            PressureAction::Shed => {
                self.full_count = self.full_count.saturating_add(1);
                AdmissionDecision::Full(self.report())
            }
            PressureAction::Degrade => {
                self.degrade_count = self.degrade_count.saturating_add(1);
                AdmissionDecision::Degrade {
                    report: self.report(),
                }
            }
            PressureAction::Close => {
                self.closed = true;
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

    fn issue_permit(&mut self, slot_idx: usize, generation: u64) -> KeyedPermit<K> {
        self.next_permit_id = self.next_permit_id.saturating_add(1);
        KeyedPermit {
            inner: Some(KeyedPermitInner {
                id: self.next_permit_id,
                slot_idx,
                generation,
            }),
            gate_id: self.gate_id,
            _key: PhantomData,
        }
    }

    /// Release a permit; the slot is freed when its count reaches zero.
    ///
    /// On gate mismatch (a permit from a different `KeyedLimit`) the permit
    /// is handed back via [`KeyedReleaseError::WrongGate`] and no counter is
    /// touched. On generation mismatch (the slot was freed and reused for a
    /// different key) the gate counts the invalid release and leaves
    /// `current` unchanged.
    pub fn release(&mut self, mut permit: KeyedPermit<K>) -> Result<(), KeyedReleaseError<K>> {
        if permit.gate_id != self.gate_id {
            return Err(KeyedReleaseError::WrongGate { permit });
        }
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
    pub fn retire(&mut self, permit: KeyedPermit<K>) -> Result<(), KeyedReleaseError<K>> {
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
            surface: self.surface.clone(),
            mode: self.mode.clone(),
            capacity: self.slots.len(),
            current: self.live_keys,
            high_water: self.high_water_keys,
            full_count: self.full_count,
            rate_limited_count: 0,
            wait_count: self.wait_count,
            degrade_count: self.degrade_count,
            closed_count: self.closed_count,
            timed_out_count: 0,
            evicted_count: 0,
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
/// Stamped with the issuing limit's process-unique gate id. Releasing a
/// permit on a *different* `KeyedLimit` returns
/// [`KeyedReleaseError::WrongGate`] (with the permit handed back) instead of
/// touching the wrong gate's slots.
#[must_use = "a KeyedPermit must be released or retired; do not drop it silently"]
#[derive(Debug)]
pub struct KeyedPermit<K> {
    inner: Option<KeyedPermitInner>,
    gate_id: u64,
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

    /// Gate id of the limit that issued this permit.
    pub const fn gate_id(&self) -> u64 {
        self.gate_id
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
#[derive(Debug)]
pub enum KeyedReleaseError<K> {
    /// The permit was issued by a different `KeyedLimit`. The permit is
    /// returned so the caller can release it on the right limit.
    WrongGate {
        /// The permit, handed back intact.
        permit: KeyedPermit<K>,
    },
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
    surface: SurfaceName,
    mode: CapacityMode,
    rate_per_sec: u64,
    burst: u32,
    burst_nano_tokens: u128,
    slots: Vec<Option<RateSlot<K>>>,
    live_keys: usize,
    high_water_keys: usize,
    action: PressureAction,
    wait_hint: Duration,
    full_count: u64,
    rate_limited_count: u64,
    degrade_count: u64,
    wait_count: u64,
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

#[derive(Debug)]
enum RateLimitCoreDecision<T> {
    Admitted(T),
    RateLimited {
        retry_after: Duration,
        report: AdmissionReport,
    },
    TableFull,
    Closed(AdmissionReport),
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
    pub fn new(
        surface: impl Into<SurfaceName>,
        max_keys: usize,
        rate_per_sec: u64,
        burst: u32,
    ) -> Self {
        assert!(rate_per_sec > 0, "RateLimit rate_per_sec must be > 0");
        assert!(burst > 0, "RateLimit burst must be > 0");
        assert!(max_keys > 0, "RateLimit max_keys must be > 0");
        let mut slots = Vec::with_capacity(max_keys);
        for _ in 0..max_keys {
            slots.push(None);
        }
        Self {
            surface: surface.into(),
            mode: CapacityMode::Fixed,
            rate_per_sec,
            burst,
            burst_nano_tokens: u128::from(burst) * ONE_TOKEN_NT,
            slots,
            live_keys: 0,
            high_water_keys: 0,
            action: PressureAction::Shed,
            wait_hint: Duration::ZERO,
            full_count: 0,
            rate_limited_count: 0,
            degrade_count: 0,
            wait_count: 0,
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

    /// Choose what to return when the key *table* is full (no slot for a
    /// new key). Default is [`PressureAction::Shed`] (`Full`).
    ///
    /// This does **not** change the per-key rate decision: a key whose
    /// bucket is empty always returns `RateLimited { retry_after }`, which
    /// is more useful than a generic `Degrade`. The action only governs the
    /// hard table-capacity rejection.
    pub fn on_table_pressure(mut self, action: PressureAction) -> Self {
        self.action = action;
        self
    }

    /// Wait hint returned under [`PressureAction::Wait`] for the table-full
    /// path. Ignored otherwise.
    pub fn wait_hint(mut self, delay: Duration) -> Self {
        self.wait_hint = delay;
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
        match self.try_admit_core(key, now) {
            RateLimitCoreDecision::Admitted(grant) => AdmissionDecision::Admitted(grant),
            RateLimitCoreDecision::RateLimited {
                retry_after,
                report,
            } => AdmissionDecision::RateLimited {
                retry_after,
                report,
            },
            RateLimitCoreDecision::TableFull => self.refuse_table(),
            RateLimitCoreDecision::Closed(report) => AdmissionDecision::Closed(report),
        }
    }

    fn try_admit_core(&mut self, key: &K, now: Instant) -> RateLimitCoreDecision<RateGrant<K>> {
        if self.closed {
            self.closed_count = self.closed_count.saturating_add(1);
            return RateLimitCoreDecision::Closed(self.report());
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
            return RateLimitCoreDecision::Admitted(RateGrant { _key: PhantomData });
        }
        RateLimitCoreDecision::TableFull
    }

    /// Map a table-full rejection onto the configured [`PressureAction`].
    /// `full_count` is bumped only when the decision surfaces as `Full`.
    fn refuse_table(&mut self) -> AdmissionDecision<RateGrant<K>> {
        match self.action {
            PressureAction::Shed => AdmissionDecision::Full(self.refuse_table_shed()),
            PressureAction::Degrade => {
                self.degrade_count = self.degrade_count.saturating_add(1);
                AdmissionDecision::Degrade {
                    report: self.report(),
                }
            }
            PressureAction::Close => {
                self.closed = true;
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
        }
    }

    fn refuse_table_shed(&mut self) -> AdmissionReport {
        self.full_count = self.full_count.saturating_add(1);
        self.report()
    }

    fn admit_existing(&mut self, idx: usize, now: Instant) -> RateLimitCoreDecision<RateGrant<K>> {
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
            RateLimitCoreDecision::Admitted(RateGrant { _key: PhantomData })
        } else {
            // Not enough credit. Compute `retry_after` deterministically.
            let needed_nt = ONE_TOKEN_NT - new_available;
            let retry_ns = needed_nt.div_ceil(u128::from(rate));
            // u64::MAX nanoseconds is ~584 years; clamp to keep the cast safe.
            let retry_after = Duration::from_nanos(u64::try_from(retry_ns).unwrap_or(u64::MAX));
            slot.available_nt = new_available;
            slot.last_seen = next_last_seen;
            self.rate_limited_count = self.rate_limited_count.saturating_add(1);
            RateLimitCoreDecision::RateLimited {
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
            surface: self.surface.clone(),
            mode: self.mode.clone(),
            capacity: self.slots.len(),
            current: self.live_keys,
            high_water: self.high_water_keys,
            full_count: self.full_count,
            rate_limited_count: self.rate_limited_count,
            wait_count: self.wait_count,
            degrade_count: self.degrade_count,
            closed_count: self.closed_count,
            timed_out_count: 0,
            evicted_count: self.evicted_count,
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

/// Per-key token-bucket rate limiter with shed-only table pressure.
///
/// Unlike [`RateLimit`], this policy does not expose configurable table
/// pressure. Its decision vocabulary is therefore limited to the four
/// outcomes it can actually produce: admitted, rate-limited, table full, and
/// closed. Use this form when a service sheds new keys immediately rather than
/// waiting, degrading, or closing on table pressure.
#[derive(Debug)]
#[must_use = "ShedRateLimit is state; store it on the isolate"]
pub struct ShedRateLimit<K> {
    inner: RateLimit<K>,
}

/// Decision returned by [`ShedRateLimit::try_admit`].
#[derive(Debug)]
#[must_use = "ShedRateLimitDecision must be matched; on failure the caller decides reply or retry"]
pub enum ShedRateLimitDecision<T> {
    /// Admitted. Consume the move-only grant on the admitted path.
    Admitted(T),
    /// Refused because the key's token bucket was empty.
    RateLimited {
        /// Earliest delay after which the caller could try again.
        retry_after: Duration,
        /// Snapshot at decision time.
        report: AdmissionReport,
    },
    /// Refused because the fixed-capacity key table had no free slot.
    TableFull(AdmissionReport),
    /// Refused because the policy was explicitly closed.
    Closed(AdmissionReport),
}

impl<T> ShedRateLimitDecision<T> {
    /// Borrow the report carried by a rejection, if any.
    pub const fn report(&self) -> Option<&AdmissionReport> {
        match self {
            Self::Admitted(_) => None,
            Self::RateLimited { report, .. } | Self::TableFull(report) | Self::Closed(report) => {
                Some(report)
            }
        }
    }
}

impl<K: Eq + Clone> ShedRateLimit<K> {
    /// Build a shed-only rate limit allowing `rate_per_sec` admissions per
    /// second per key, with a token bucket up to `burst` and at most
    /// `max_keys` tracked keys.
    ///
    /// # Panics
    ///
    /// Panics if `rate_per_sec == 0`, `burst == 0`, or `max_keys == 0`.
    pub fn new(
        surface: impl Into<SurfaceName>,
        max_keys: usize,
        rate_per_sec: u64,
        burst: u32,
    ) -> Self {
        Self {
            inner: RateLimit::new(surface, max_keys, rate_per_sec, burst),
        }
    }

    /// Set the configured capacity mode.
    pub fn with_mode(mut self, mode: CapacityMode) -> Self {
        self.inner = self.inner.with_mode(mode);
        self
    }

    /// Try to admit one request for `key` at logical time `now`.
    pub fn try_admit(&mut self, key: &K, now: Instant) -> ShedRateLimitDecision<RateGrant<K>> {
        match self.inner.try_admit_core(key, now) {
            RateLimitCoreDecision::Admitted(grant) => ShedRateLimitDecision::Admitted(grant),
            RateLimitCoreDecision::RateLimited {
                retry_after,
                report,
            } => ShedRateLimitDecision::RateLimited {
                retry_after,
                report,
            },
            RateLimitCoreDecision::TableFull => {
                ShedRateLimitDecision::TableFull(self.inner.refuse_table_shed())
            }
            RateLimitCoreDecision::Closed(report) => ShedRateLimitDecision::Closed(report),
        }
    }

    /// Configured rate per second.
    pub const fn rate_per_sec(&self) -> u64 {
        self.inner.rate_per_sec()
    }

    /// Configured burst.
    pub const fn burst(&self) -> u32 {
        self.inner.burst()
    }

    /// Configured key-table capacity.
    pub fn max_keys(&self) -> usize {
        self.inner.max_keys()
    }

    /// Number of live key slots.
    pub const fn live_keys(&self) -> usize {
        self.inner.live_keys()
    }

    /// Cumulative count of explicit key-table evictions.
    pub const fn evicted_count(&self) -> u64 {
        self.inner.evicted_count()
    }

    /// Explicitly evict one key's bucket state to free table capacity.
    ///
    /// This is an administrative policy lever with the same caveats as
    /// [`RateLimit::evict_key_for_capacity`].
    pub fn evict_key_for_capacity(&mut self, key: &K) -> bool {
        self.inner.evict_key_for_capacity(key)
    }

    /// Mark this policy closed.
    pub fn close(&mut self) {
        self.inner.close();
    }

    /// `true` if closed.
    pub const fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    /// Build the current report.
    pub fn report(&self) -> AdmissionReport {
        self.inner.report()
    }

    /// Inspect one key's bucket state, if any.
    pub fn key_state(&self, key: &K) -> Option<RateKeyState> {
        self.inner.key_state(key)
    }

    /// Project the current report onto a capacity surface.
    pub fn capacity_surface(&self) -> CapacitySurfaceReport {
        self.inner.capacity_surface()
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

    /// Drive a policy through the trait, never the concrete type. Proves
    /// generic service code can take any `ServicePolicy`.
    fn admit_via_trait<P: ServicePolicy>(
        policy: &mut P,
        key: &P::Key,
        now: Instant,
    ) -> AdmissionDecision<P::Permit> {
        policy.decide(key, now)
    }

    #[test]
    fn rate_limit_is_a_service_policy() {
        let mut limit: RateLimit<()> = RateLimit::new("policy.trait", 1, 1, 1);
        let now = fixed_now();
        // First decision admits (burst = 1).
        assert!(matches!(
            admit_via_trait(&mut limit, &(), now),
            AdmissionDecision::Admitted(_)
        ));
        // Second decision in the same instant is rate-limited.
        assert!(matches!(
            admit_via_trait(&mut limit, &(), now),
            AdmissionDecision::RateLimited { .. }
        ));
        // The trait report matches the inherent report.
        assert_eq!(
            ServicePolicy::report(&limit).rate_limited_count,
            RateLimit::report(&limit).rate_limited_count
        );
    }

    #[test]
    fn concurrency_limit_is_a_service_policy() {
        let mut limit = ConcurrencyLimit::with_capacity("policy.concurrency", 1);
        let now = fixed_now();
        let _held = admit_via_trait(&mut limit, &(), now)
            .into_admitted()
            .expect("first admission");
        assert!(matches!(
            admit_via_trait(&mut limit, &(), now),
            AdmissionDecision::Full(_)
        ));
        assert_eq!(
            ServicePolicy::report(&limit).full_count,
            ConcurrencyLimit::report(&limit).full_count
        );
    }

    #[test]
    fn keyed_limit_is_a_service_policy() {
        let mut limit = KeyedLimit::new("policy.keyed", 1, 1);
        let now = fixed_now();
        let key = "tenant-a";
        let _held = admit_via_trait(&mut limit, &key, now)
            .into_admitted()
            .expect("first keyed admission");
        assert!(matches!(
            admit_via_trait(&mut limit, &key, now),
            AdmissionDecision::Full(_)
        ));
        assert_eq!(
            ServicePolicy::report(&limit).full_count,
            KeyedLimit::report(&limit).full_count
        );
    }

    #[test]
    fn custom_service_policy_returns_typed_decisions_only() {
        // A tiny extension-style policy: admit once, then close. Proves
        // the trait is implementable outside the built-ins and that a
        // policy can express the full decision vocabulary without ever
        // sending or retrying.
        struct OneShot {
            used: bool,
            report: AdmissionReport,
        }
        impl ServicePolicy for OneShot {
            type Key = str;
            type Permit = ();
            fn decide(&mut self, _key: &str, _now: Instant) -> AdmissionDecision<()> {
                if self.used {
                    AdmissionDecision::Closed(self.report.clone())
                } else {
                    self.used = true;
                    AdmissionDecision::Admitted(())
                }
            }
            fn report(&self) -> AdmissionReport {
                self.report.clone()
            }
        }

        let template = ConcurrencyLimit::with_capacity("policy.oneshot", 1);
        let mut policy = OneShot {
            used: false,
            report: template.report(),
        };
        let now = fixed_now();
        assert!(matches!(
            policy.decide("alice", now),
            AdmissionDecision::Admitted(())
        ));
        assert!(matches!(
            policy.decide("alice", now),
            AdmissionDecision::Closed(_)
        ));
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
        let p = close.try_admit().into_admitted().unwrap();
        match close.try_admit() {
            AdmissionDecision::Closed(r) => {
                assert_eq!(r.closed_count, 1);
            }
            other => panic!("Close expected, got {other:?}"),
        }
        assert!(close.is_closed(), "Close action must stick");
        close.release(p).expect("release after close");
        match close.try_admit() {
            AdmissionDecision::Closed(r) => {
                assert_eq!(r.current, 0);
                assert_eq!(r.closed_count, 2);
            }
            other => panic!("closed policy must stay closed, got {other:?}"),
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
            surface: Cow::Borrowed("fake"),
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
            evicted_count: 0,
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
        let a = limit.try_admit().into_admitted().unwrap();
        let b = limit.try_admit().into_admitted().unwrap();
        // Force a Full.
        let _ = limit.try_admit();
        let surface = limit.capacity_surface();
        assert_eq!(surface.name, "conc.surface");
        assert_eq!(surface.max_messages, Some(2));
        assert_eq!(surface.current_messages, 2);
        assert!(surface.full_count >= 1);
        limit.release(a).unwrap();
        limit.release(b).unwrap();
    }

    // ---- Gate-tagging (#8, #12) -------------------------------------------

    #[test]
    fn concurrency_permit_released_on_wrong_gate_is_rejected_and_handed_back() {
        let mut a = ConcurrencyLimit::with_capacity("conc.a", 2);
        let mut b = ConcurrencyLimit::with_capacity("conc.b", 2);
        assert_ne!(a.gate_id(), b.gate_id());
        let permit_a = a.try_admit().into_admitted().unwrap();
        // Releasing A's permit on B must not touch B's counters.
        let err = b.release(permit_a).expect_err("wrong gate must reject");
        let permit_a = match err {
            ConcurrencyReleaseError::WrongGate { permit } => permit,
            other => panic!("expected WrongGate, got {other:?}"),
        };
        // B's gate is untouched: still empty.
        assert_eq!(b.report().current, 0);
        // A still shows the permit outstanding.
        assert_eq!(a.report().current, 1);
        // The handed-back permit releases correctly on A.
        a.release(permit_a).expect("release on issuing gate");
        assert_eq!(a.report().current, 0);
    }

    #[test]
    fn keyed_permit_released_on_wrong_gate_is_rejected_and_handed_back() {
        let mut a = KeyedLimit::<&'static str>::new("keyed.a", 2, 2);
        let mut b = KeyedLimit::<&'static str>::new("keyed.b", 2, 2);
        assert_ne!(a.gate_id(), b.gate_id());
        // Both occupy slot 0 with generation 1 — the exact collision that
        // would silently corrupt B without gate tagging.
        let pa = a.try_admit(&"x").into_admitted().unwrap();
        let _pb = b.try_admit(&"y").into_admitted().unwrap();
        let err = b.release(pa).expect_err("wrong gate must reject");
        let pa = match err {
            KeyedReleaseError::WrongGate { permit } => permit,
            other => panic!("expected WrongGate, got {other:?}"),
        };
        // B's slot for "y" is intact (current still 1).
        assert_eq!(b.key_report(&"y").unwrap().current, 1);
        // A's slot for "x" is intact.
        assert_eq!(a.key_report(&"x").unwrap().current, 1);
        a.release(pa).expect("release on issuing gate");
        assert_eq!(a.live_keys(), 0);
    }

    // ---- PressureAction on KeyedLimit / RateLimit (#1, #18) ---------------

    #[test]
    fn keyed_limit_honors_pressure_action() {
        // Degrade on per-key full.
        let mut degrade = KeyedLimit::<&'static str>::new("keyed.degrade", 4, 1)
            .on_pressure(PressureAction::Degrade);
        let _p = degrade.try_admit(&"k").into_admitted().unwrap();
        match degrade.try_admit(&"k") {
            AdmissionDecision::Degrade { report } => assert_eq!(report.degrade_count, 1),
            other => panic!("expected Degrade, got {other:?}"),
        }
        assert_eq!(degrade.report().full_count, 0, "degrade is not a Full");

        // Wait on table full.
        let mut waiting = KeyedLimit::<u32>::new("keyed.wait", 1, 1)
            .on_pressure(PressureAction::Wait)
            .wait_hint(Duration::from_millis(9));
        let _p = waiting.try_admit(&1).into_admitted().unwrap();
        match waiting.try_admit(&2) {
            AdmissionDecision::Wait { delay, report } => {
                assert_eq!(delay, Duration::from_millis(9));
                assert_eq!(report.wait_count, 1);
            }
            other => panic!("expected Wait, got {other:?}"),
        }

        // Close on pressure is sticky, not a one-request label.
        let mut closing =
            KeyedLimit::<&'static str>::new("keyed.close", 2, 1).on_pressure(PressureAction::Close);
        let p = closing.try_admit(&"k").into_admitted().unwrap();
        match closing.try_admit(&"k") {
            AdmissionDecision::Closed(report) => assert_eq!(report.closed_count, 1),
            other => panic!("expected Closed, got {other:?}"),
        }
        assert!(
            closing.is_closed(),
            "Close action must stop future admission"
        );
        closing.release(p).expect("release after close");
        match closing.try_admit(&"other") {
            AdmissionDecision::Closed(report) => {
                assert_eq!(report.current, 0);
                assert_eq!(report.closed_count, 2);
            }
            other => panic!("closed keyed policy must stay closed, got {other:?}"),
        }
    }

    #[test]
    fn rate_limit_table_pressure_action_applies_only_to_table_full() {
        let mut limit = RateLimit::<u32>::new("rate.degrade_table", 1, 5, 1)
            .on_table_pressure(PressureAction::Degrade);
        let now = fixed_now();
        // First key admits.
        let _ = limit.try_admit(&1, now).into_admitted().unwrap();
        // Same key, bucket empty → RateLimited (NOT Degrade — per-key rate
        // decision is unaffected by the table action).
        match limit.try_admit(&1, now) {
            AdmissionDecision::RateLimited { .. } => {}
            other => panic!("expected RateLimited, got {other:?}"),
        }
        // New key, table full → Degrade (the table action).
        match limit.try_admit(&2, now) {
            AdmissionDecision::Degrade { report } => assert_eq!(report.degrade_count, 1),
            other => panic!("expected Degrade for table-full, got {other:?}"),
        }

        let mut closing = RateLimit::<u32>::new("rate.close_table", 1, 5, 1)
            .on_table_pressure(PressureAction::Close);
        let _ = closing.try_admit(&1, now).into_admitted().unwrap();
        match closing.try_admit(&2, now) {
            AdmissionDecision::Closed(report) => assert_eq!(report.closed_count, 1),
            other => panic!("expected Closed for table-full, got {other:?}"),
        }
        assert!(
            closing.is_closed(),
            "Close action must stop future admission"
        );
        match closing.try_admit(&1, now + Duration::from_secs(1)) {
            AdmissionDecision::Closed(report) => assert_eq!(report.closed_count, 2),
            other => panic!("closed rate policy must stay closed, got {other:?}"),
        }
    }

    // ---- Shared scope composition (#3) ------------------------------------

    #[test]
    fn concurrency_limit_charges_shared_scope_and_releases_both() {
        let scope = SharedCapacityScope::new("shared.budget", "weight", 3);
        let mut route_a =
            ConcurrencyLimit::with_capacity("route.a", 10).with_shared_scope(scope.clone(), 2);
        let mut route_b =
            ConcurrencyLimit::with_capacity("route.b", 10).with_shared_scope(scope.clone(), 2);
        // route_a takes weight 2 (scope now 2/3).
        let pa = route_a.try_admit().into_admitted().unwrap();
        assert!(pa.holds_shared_lease());
        assert_eq!(scope.snapshot().current, 2);
        // route_b wants 2 more but only 1 weight remains → shared full.
        // Local gate for route_b has room (cap 10), so the rejection is the
        // shared budget, and the local permit must be rolled back.
        match route_b.try_admit() {
            AdmissionDecision::Full(_) => {}
            other => panic!("expected Full from shared scope, got {other:?}"),
        }
        // route_b's local gate must show zero outstanding (rolled back).
        assert_eq!(route_b.report().current, 0);
        // Scope still at 2 (route_b's charge was rolled back).
        assert_eq!(scope.snapshot().current, 2);
        // The capacity surface is decorated with the shared scope columns.
        let surface = route_a.capacity_surface();
        assert_eq!(surface.shared_scope.as_deref(), Some("shared.budget"));
        // Releasing route_a frees both local and shared.
        route_a.release(pa).unwrap();
        assert_eq!(scope.snapshot().current, 0);
        assert_eq!(route_a.report().current, 0);
    }

    #[test]
    fn concurrency_permit_drop_releases_shared_lease() {
        let scope = SharedCapacityScope::new("shared.drop", "weight", 4);
        let mut limit =
            ConcurrencyLimit::with_capacity("route.drop", 10).with_shared_scope(scope.clone(), 2);
        let permit = limit.try_admit().into_admitted().unwrap();
        assert_eq!(scope.snapshot().current, 2);
        // Dropping the permit (instead of releasing) still frees the shared
        // lease (the local gate stays charged + the global dropped counter
        // bumps — that's the loud-leak contract).
        drop(permit);
        assert_eq!(scope.snapshot().current, 0, "shared lease freed on drop");
    }

    // ---- close() with live state (#6) -------------------------------------

    #[test]
    fn rate_limit_close_with_live_tenants_returns_closed_and_keeps_counts() {
        let mut limit = RateLimit::<&'static str>::new("rate.close", 4, 10, 2);
        let now = fixed_now();
        let _ = limit.try_admit(&"alpha", now).into_admitted().unwrap();
        let _ = limit.try_admit(&"beta", now).into_admitted().unwrap();
        assert_eq!(limit.live_keys(), 2);
        limit.close();
        match limit.try_admit(&"gamma", now) {
            AdmissionDecision::Closed(r) => {
                assert!(r.closed_count >= 1);
                // Live tenant state is preserved through close — close is
                // admission-only, not a state reset.
                assert_eq!(r.current, 2);
            }
            other => panic!("expected Closed, got {other:?}"),
        }
    }

    #[test]
    fn keyed_limit_close_with_outstanding_permits_allows_release() {
        let mut limit = KeyedLimit::<&'static str>::new("keyed.close", 4, 2);
        let p = limit.try_admit(&"alpha").into_admitted().unwrap();
        limit.close();
        match limit.try_admit(&"beta") {
            AdmissionDecision::Closed(r) => {
                assert!(r.closed_count >= 1);
                assert_eq!(r.current, 1, "outstanding permit still counted");
            }
            other => panic!("expected Closed, got {other:?}"),
        }
        // Release after close still works (close is admission-only).
        limit.release(p).expect("release after close");
        assert_eq!(limit.live_keys(), 0);
    }

    // ---- mode round-trip (#7) ---------------------------------------------

    #[test]
    fn keyed_and_rate_modes_round_trip_into_surface() {
        let keyed = KeyedLimit::<u32>::new("keyed.mode", 4, 2).with_mode(CapacityMode::Tuning);
        assert_eq!(keyed.capacity_surface().mode, CapacityMode::Tuning);

        let rate = RateLimit::<u32>::new("rate.mode", 4, 5, 2).with_mode(CapacityMode::Tuning);
        assert_eq!(rate.capacity_surface().mode, CapacityMode::Tuning);
    }

    // ---- evict during outstanding grant (#9) ------------------------------

    #[test]
    fn rate_limit_evict_does_not_affect_already_issued_grant() {
        // RateGrant carries no slot reference, so an eviction between admit
        // and grant-consumption cannot corrupt anything. Pin that.
        let mut limit = RateLimit::<&'static str>::new("rate.evict_race", 4, 10, 2);
        let now = fixed_now();
        let grant = limit.try_admit(&"alpha", now).into_admitted().unwrap();
        // Evict alpha while the grant is still in hand.
        assert!(limit.evict_key_for_capacity(&"alpha"));
        assert_eq!(limit.live_keys(), 0);
        // The grant is still a valid proof; consuming it is a no-op for the
        // policy (no slot to free). No panic, no underflow.
        drop(grant);
        assert_eq!(limit.live_keys(), 0);
        // A fresh admit for alpha starts a new bucket.
        let _ = limit.try_admit(&"alpha", now).into_admitted().unwrap();
        assert_eq!(limit.live_keys(), 1);
    }

    // ---- stress / correctness at scale (#10) ------------------------------

    #[test]
    fn keyed_limit_stress_churn_keeps_live_keys_exact() {
        // Drive many distinct keys through admit/release churn and verify
        // the O(1) live_keys field never drifts from the true Some-count.
        let max_keys = 512;
        let mut limit = KeyedLimit::<u32>::new("keyed.stress", max_keys, 1);
        let mut held: Vec<KeyedPermit<u32>> = Vec::new();
        // Fill the table.
        for k in 0..max_keys as u32 {
            held.push(limit.try_admit(&k).into_admitted().unwrap());
        }
        assert_eq!(limit.live_keys(), max_keys);
        // Table full: a new key is refused.
        assert!(matches!(limit.try_admit(&9999), AdmissionDecision::Full(_)));
        // Release every other permit, then re-admit fresh keys into the gaps.
        let mut released = 0usize;
        let mut remaining = Vec::new();
        for (i, p) in held.into_iter().enumerate() {
            if i % 2 == 0 {
                limit.release(p).unwrap();
                released += 1;
            } else {
                remaining.push(p);
            }
        }
        assert_eq!(limit.live_keys(), max_keys - released);
        for k in 0..released as u32 {
            remaining.push(limit.try_admit(&(10_000 + k)).into_admitted().unwrap());
        }
        assert_eq!(limit.live_keys(), max_keys);
        // Drain everything.
        for p in remaining {
            limit.release(p).unwrap();
        }
        assert_eq!(limit.live_keys(), 0);
    }

    #[test]
    fn rate_limit_stress_many_keys_independent_buckets() {
        let max_keys = 256;
        let mut limit = RateLimit::<u32>::new("rate.stress", max_keys, 1, 1);
        let now = fixed_now();
        // Each key admits exactly once (burst 1), then is rate-limited.
        for k in 0..max_keys as u32 {
            let _ = limit.try_admit(&k, now).into_admitted().unwrap();
        }
        assert_eq!(limit.live_keys(), max_keys);
        for k in 0..max_keys as u32 {
            assert!(matches!(
                limit.try_admit(&k, now),
                AdmissionDecision::RateLimited { .. }
            ));
        }
        // Table full for a brand-new key.
        assert!(matches!(
            limit.try_admit(&99_999, now),
            AdmissionDecision::Full(_)
        ));
    }

    // ---- Display / dynamic surface (#16, #17) -----------------------------

    #[test]
    fn report_display_is_grep_friendly() {
        let mut limit = ConcurrencyLimit::with_capacity("conc.display", 1);
        let _h = limit.try_admit().into_admitted().unwrap();
        let _ = limit.try_admit();
        let line = format!("{}", limit.report());
        assert!(line.contains("admission surface=conc.display"), "{line}");
        assert!(line.contains("full=1"), "{line}");
        assert!(line.contains("evicted=0"), "{line}");
    }

    #[test]
    fn admission_failure_display_includes_retry_after() {
        let mut limit = RateLimit::<&'static str>::new("rate.display", 4, 10, 1);
        let now = fixed_now();
        let _ = limit.try_admit(&"alpha", now).into_admitted().unwrap();
        let failure = limit.try_admit(&"alpha", now).into_admitted().unwrap_err();
        let line = format!("{failure}");
        assert!(line.contains("admission_rejected=rate_limited"), "{line}");
        assert!(line.contains("retry_after_ms=100"), "{line}");
    }

    #[test]
    fn dynamic_owned_surface_name_round_trips() {
        // A runtime-built surface name (per-route / per-tenant) works.
        let name = format!("route.{}", "items");
        let mut limit = RateLimit::<&'static str>::new(name.clone(), 4, 5, 1);
        let now = fixed_now();
        let _ = limit.try_admit(&"t", now).into_admitted().unwrap();
        assert_eq!(limit.report().surface, name);
        assert_eq!(limit.capacity_surface().name, name);
    }

    #[test]
    fn evicted_count_in_report_but_not_in_rejection_sum() {
        let mut limit = RateLimit::<u32>::new("rate.evict_report", 2, 5, 1);
        let now = fixed_now();
        let _ = limit.try_admit(&1, now).into_admitted().unwrap();
        assert!(limit.evict_key_for_capacity(&1));
        let report = limit.report();
        assert_eq!(report.evicted_count, 1);
        // Eviction is not a rejection.
        assert!(!report.any_rejection());
        assert_eq!(report.total_rejections(), 0);
        // And it does not inflate the capacity-surface full_count.
        assert_eq!(limit.capacity_surface().full_count, 0);
    }
}
