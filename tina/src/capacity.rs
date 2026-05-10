//! Capacity vocabulary.
//!
//! Pure data types. The runtime collects, the simulator asserts, the
//! bridge specimens emit. None of the *mechanism* lives here.
//!
//! Capacity is not a big number guessed once:
//!
//! ```text
//! unknown -> measured -> fixed
//! ```
//!
//! Count protects scheduler fairness. Weight (PR 2) protects
//! memory-ish payload. Shared weight (PR 2) protects a group of
//! queues. Every `Full` says which cap filled.
//!
//! # First form (PR 1)
//!
//! - [`CapacityMode`]: `Fixed` and `Tuning`. Tuning is `Fixed` with
//!   a loud "this number was chosen for discovery, please report
//!   high water" flag — the cap is still a hard upper bound.
//! - [`CapacityPolicy`]: dev / test / prod gates that decide which
//!   modes are allowed.
//! - [`CapacitySpec`]: the value a user hands a bounded surface
//!   constructor: cap + mode + optional name.
//! - [`CapacitySurfaceReport`]: snapshot a bounded surface produces.
//! - [`CapacityFull`]: typed reason a `Full`-shaped rejection
//!   happened — local message count today, plus weight/shared
//!   variants reserved for PR 2 so callers can match exhaustively
//!   without an API break later.
//!
//! Weight, shared scope, and unbounded modes land in PR 2.

use core::fmt;

/// How a surface's cap was chosen.
///
/// The cap is *always* a hard upper bound. `Tuning` only signals
/// intent so dashboards / discovery formatters can say "read the
/// high water and freeze".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityMode {
    /// A real measured cap. Production-acceptable.
    Fixed,
    /// Cap chosen for discovery. Still a hard upper bound; report
    /// high-water loudly so the user can pick a `Fixed` number.
    Tuning,
}

impl CapacityMode {
    /// Short label for one-line reports.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::Tuning => "tuning",
        }
    }
}

impl fmt::Display for CapacityMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Deployment profile gating which capacity modes are allowed.
///
/// Validation is done at config time via
/// [`CapacityPolicy::validate_mode`]. Wiring this to runtime startup
/// is the caller's job — the policy itself is data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityPolicy {
    /// Local dev. Allows tuning. Unbounded modes (PR 2) allowed
    /// with explicit expiry.
    Development,
    /// CI. Allows tuning. Unbounded with expiry allowed; ugly
    /// no-expiry rejected.
    Test,
    /// Production. Allows fixed and tuning (early production is
    /// where real high-water data first appears). Unbounded modes
    /// rejected.
    Production,
}

/// Why [`CapacityPolicy::validate_mode`] rejected a mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapacityPolicyError {
    /// The mode is not allowed under the configured policy.
    ///
    /// `surface` names the offending surface so the rejection
    /// message can point at it; `mode` and `policy` describe the
    /// disagreement.
    ModeNotAllowed {
        /// Surface whose mode failed validation.
        surface: String,
        /// The mode the surface was configured with.
        mode: CapacityMode,
        /// The policy in effect.
        policy: CapacityPolicy,
    },
}

impl fmt::Display for CapacityPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ModeNotAllowed {
                surface,
                mode,
                policy,
            } => write!(
                f,
                "capacity mode {mode} not allowed for surface {surface:?} under {policy:?}"
            ),
        }
    }
}

impl std::error::Error for CapacityPolicyError {}

impl CapacityPolicy {
    /// Reject a [`CapacityMode`] that is not allowed under this
    /// policy. PR 1 only sees `Fixed` and `Tuning`; both are
    /// always allowed today, but production keeps the hook so PR 2
    /// can plug unbounded rejection here without an API break.
    pub fn validate_mode(
        self,
        surface: &str,
        mode: CapacityMode,
    ) -> Result<(), CapacityPolicyError> {
        // Fixed and Tuning are allowed everywhere in PR 1. The
        // signature stays so PR 2 can add `UnboundedForNow` /
        // `UnboundedWithoutExpiry` rejections without changing
        // call sites.
        let _ = (surface, mode);
        match (self, mode) {
            (_, CapacityMode::Fixed) | (_, CapacityMode::Tuning) => Ok(()),
        }
    }
}

/// Why a `Full`-shaped rejection happened.
///
/// PR 1 only ever produces `LocalMessages`; the other variants are
/// declared today so user code that wants exhaustive matching does
/// not need to be edited when PR 2 adds weight and shared scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityFull {
    /// The local count cap on this surface filled.
    LocalMessages,
    /// The local weight cap on this surface filled. (PR 2.)
    LocalWeight,
    /// A shared-weight scope this surface participates in filled.
    /// (PR 2; the scope id type also lands then.)
    SharedWeight,
}

impl CapacityFull {
    /// Short label for one-line reports.
    pub const fn label(self) -> &'static str {
        match self {
            Self::LocalMessages => "local_messages",
            Self::LocalWeight => "local_weight",
            Self::SharedWeight => "shared_weight",
        }
    }
}

impl fmt::Display for CapacityFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// What the user hands a bounded-surface constructor.
///
/// ```ignore
/// CapacitySpec::fixed(128)
/// CapacitySpec::tuning(256).named("orders.mailbox")
/// ```
///
/// The cap is the hard upper bound. `mode` says how it was chosen.
/// `name` overrides the surface's default-generated name; pin it
/// for CI/DST assertions, omit it for ad-hoc use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacitySpec {
    /// Hard upper bound on count. Always honored.
    pub max: usize,
    /// Whether the cap was measured (`Fixed`) or chosen for
    /// discovery (`Tuning`).
    pub mode: CapacityMode,
    /// Optional explicit name. `None` means the surface uses its
    /// default-generated name.
    pub name: Option<String>,
}

impl CapacitySpec {
    /// A measured cap.
    pub fn fixed(max: usize) -> Self {
        Self {
            max,
            mode: CapacityMode::Fixed,
            name: None,
        }
    }

    /// A discovery cap. Still a hard upper bound; just report
    /// high-water loudly so the user can promote it to `Fixed`.
    pub fn tuning(max: usize) -> Self {
        Self {
            max,
            mode: CapacityMode::Tuning,
            name: None,
        }
    }

    /// Override the surface's default name. Pin for CI/DST tests.
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
}

/// Snapshot of one bounded count surface.
///
/// Weight fields are reserved for PR 2. They are present today as
/// `Option`s so first-form callers do not need to change the report
/// shape when weight surfaces start filling them in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacitySurfaceReport {
    /// Stable name. Defaults to a generated `<kind>.<n>.<role>` form;
    /// explicit `.named(...)` is the blessed shape for CI/DST.
    pub name: String,
    /// How the cap was chosen.
    pub mode: CapacityMode,
    /// Configured count cap. `None` is reserved for unbounded modes
    /// (PR 2); first-form surfaces always set it.
    pub max_messages: Option<usize>,
    /// Live count right now.
    pub current_messages: usize,
    /// Highest live count observed since construction.
    pub high_water_messages: usize,
    /// Cumulative `Full`-shaped rejections caused by this count cap.
    pub full_count: u64,
    /// Configured weight cap. PR 2.
    pub max_weight: Option<usize>,
    /// Live weight right now. PR 2.
    pub current_weight: Option<usize>,
    /// High-water weight since construction. PR 2.
    pub high_water_weight: Option<usize>,
    /// Cumulative weight-related `Full` rejections. PR 2.
    pub weight_full_count: u64,
}

impl CapacitySurfaceReport {
    /// Build a count-only report. Weight fields land empty.
    pub fn count(
        name: impl Into<String>,
        mode: CapacityMode,
        max: usize,
        current: usize,
        high_water: usize,
        full_count: u64,
    ) -> Self {
        Self {
            name: name.into(),
            mode,
            max_messages: Some(max),
            current_messages: current,
            high_water_messages: high_water,
            full_count,
            max_weight: None,
            current_weight: None,
            high_water_weight: None,
            weight_full_count: 0,
        }
    }

    /// True iff this report ever observed a `Full`-shaped rejection
    /// (count or weight).
    pub fn ever_full(&self) -> bool {
        self.full_count > 0 || self.weight_full_count > 0
    }

    /// High-water as a fraction of cap, in basis points (0..=10_000).
    /// Useful for "we touched 90% of cap" dashboards. Returns
    /// `None` if `max_messages` is `None`.
    pub fn fill_basis_points(&self) -> Option<u32> {
        let max = self.max_messages?;
        if max == 0 {
            return Some(10_000);
        }
        let bp = (self.high_water_messages.saturating_mul(10_000)) / max;
        Some(bp.min(10_000) as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_spec_fixed_default_name_is_none() {
        let spec = CapacitySpec::fixed(8);
        assert_eq!(spec.max, 8);
        assert_eq!(spec.mode, CapacityMode::Fixed);
        assert!(spec.name.is_none());
    }

    #[test]
    fn capacity_spec_named_overrides() {
        let spec = CapacitySpec::tuning(16).named("orders.mailbox");
        assert_eq!(spec.mode, CapacityMode::Tuning);
        assert_eq!(spec.name.as_deref(), Some("orders.mailbox"));
    }

    #[test]
    fn capacity_policy_allows_fixed_everywhere() {
        for policy in [
            CapacityPolicy::Development,
            CapacityPolicy::Test,
            CapacityPolicy::Production,
        ] {
            policy
                .validate_mode("any", CapacityMode::Fixed)
                .expect("fixed allowed");
        }
    }

    #[test]
    fn capacity_policy_allows_tuning_everywhere_in_pr1() {
        // PR 1 keeps tuning allowed in production. Promotion to
        // warn/reject lives in PR 2 with the shared-budget pieces.
        for policy in [
            CapacityPolicy::Development,
            CapacityPolicy::Test,
            CapacityPolicy::Production,
        ] {
            policy
                .validate_mode("any", CapacityMode::Tuning)
                .expect("tuning allowed in PR 1");
        }
    }

    #[test]
    fn surface_report_count_round_trips() {
        let r = CapacitySurfaceReport::count("p.waiters", CapacityMode::Fixed, 4, 3, 4, 1);
        assert_eq!(r.max_messages, Some(4));
        assert_eq!(r.current_messages, 3);
        assert_eq!(r.high_water_messages, 4);
        assert_eq!(r.full_count, 1);
        assert!(r.ever_full());
        assert_eq!(r.fill_basis_points(), Some(10_000));
    }

    #[test]
    fn surface_report_basis_points_partial() {
        let r = CapacitySurfaceReport::count("p.waiters", CapacityMode::Tuning, 100, 0, 23, 0);
        assert_eq!(r.fill_basis_points(), Some(2_300));
        assert!(!r.ever_full());
    }

    #[test]
    fn capacity_full_labels_are_distinct() {
        assert_ne!(
            CapacityFull::LocalMessages.label(),
            CapacityFull::LocalWeight.label()
        );
        assert_ne!(
            CapacityFull::LocalWeight.label(),
            CapacityFull::SharedWeight.label()
        );
    }
}
