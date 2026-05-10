//! Capacity vocabulary.
//!
//! Pure data. Runtime collects, simulator asserts, specimens emit.
//! No mechanism lives here.
//!
//! Capacity is not a guess:
//!
//! ```text
//! unknown -> measured -> fixed
//! ```
//!
//! Count caps protect scheduler fairness. Future weight caps will
//! protect memory-ish payload; future shared scopes will protect a
//! group of queues.
//!
//! Today's vocabulary:
//!
//! - [`CapacityMode`]: `Fixed` (measured) or `Tuning` (discovery).
//!   Tuning is still a hard cap; the flag just says "report high
//!   water loudly".
//! - [`CapacityPolicy`]: dev / test / prod stub. Both modes pass
//!   everywhere today. Future unbounded modes plug in here.
//! - [`CapacitySurfaceReport`]: one snapshot per bounded surface.

use core::fmt;

/// How the cap was chosen. Cap is always a hard upper bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityMode {
    /// Measured cap. Use in production.
    Fixed,
    /// Discovery cap. Still hard. Reports surface high water so
    /// the user can pick a `Fixed` number.
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

/// Deployment profile. Decides which modes pass validation.
///
/// Both modes pass everywhere today. The shape exists so future
/// unbounded modes can plug in here without touching call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityPolicy {
    /// Local dev.
    Development,
    /// CI.
    Test,
    /// Production.
    Production,
}

/// Why [`CapacityPolicy::validate_mode`] rejected a mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapacityPolicyError {
    /// Mode not allowed under this policy. The error names the
    /// surface, the mode, and the policy so the message points at
    /// the offender.
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
    /// Validate `mode` for `surface` under this policy.
    ///
    /// Today every `(policy, mode)` pair passes. The signature is
    /// stable so call sites can pre-wire validation before
    /// unbounded modes land.
    pub fn validate_mode(
        self,
        _surface: &str,
        _mode: CapacityMode,
    ) -> Result<(), CapacityPolicyError> {
        Ok(())
    }
}

/// Snapshot of one bounded count surface.
///
/// Weight fields are reserved for future weighted surfaces. They
/// are always `None` / `0` today; the shape carries them so adding
/// weight later does not break readers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacitySurfaceReport {
    /// Stable surface name. Use dotted form, e.g.
    /// `pool.orders.waiters`. Discovery formatter assumes no
    /// whitespace in the name.
    pub name: String,
    /// How the cap was chosen.
    pub mode: CapacityMode,
    /// Configured count cap. `None` is reserved for future
    /// unbounded modes; today's surfaces always set it.
    pub max_messages: Option<usize>,
    /// Live count right now.
    pub current_messages: usize,
    /// Highest live count observed since construction.
    pub high_water_messages: usize,
    /// Cumulative count-cap `Full` rejections.
    pub full_count: u64,
    /// Configured weight cap. Reserved.
    pub max_weight: Option<usize>,
    /// Live weight right now. Reserved.
    pub current_weight: Option<usize>,
    /// High-water weight since construction. Reserved.
    pub high_water_weight: Option<usize>,
    /// Cumulative weight-cap `Full` rejections. Reserved.
    pub weight_full_count: u64,
}

impl CapacitySurfaceReport {
    /// Build a count-only report. Weight fields stay empty.
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

    /// True if this surface ever hit `Full` (count or weight).
    pub fn ever_full(&self) -> bool {
        self.full_count > 0 || self.weight_full_count > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_policy_allows_every_known_mode() {
        for policy in [
            CapacityPolicy::Development,
            CapacityPolicy::Test,
            CapacityPolicy::Production,
        ] {
            for mode in [CapacityMode::Fixed, CapacityMode::Tuning] {
                policy
                    .validate_mode("any", mode)
                    .expect("Fixed and Tuning pass everywhere today");
            }
        }
    }

    #[test]
    fn count_constructor_populates_message_fields() {
        let r = CapacitySurfaceReport::count("p.waiters", CapacityMode::Fixed, 4, 3, 4, 1);
        assert_eq!(r.max_messages, Some(4));
        assert_eq!(r.current_messages, 3);
        assert_eq!(r.high_water_messages, 4);
        assert_eq!(r.full_count, 1);
        assert!(r.ever_full());
    }

    #[test]
    fn ever_full_is_false_when_neither_counter_fired() {
        let r = CapacitySurfaceReport::count("p.waiters", CapacityMode::Tuning, 100, 0, 23, 0);
        assert!(!r.ever_full());
    }

    #[test]
    fn capacity_mode_label_is_distinct() {
        assert_ne!(CapacityMode::Fixed.label(), CapacityMode::Tuning.label());
    }
}
