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
//! Count caps protect scheduler fairness. Weight caps protect
//! user-declared payload cost. Shared scopes protect a group of
//! surfaces on one shard.
//!
//! Today's vocabulary:
//!
//! - [`CapacityMode`]: `Fixed` (measured), `Tuning` (discovery),
//!   or explicit unbounded escape hatches.
//! - [`CapacityPolicy`]: dev / test / prod validation for modes.
//! - [`CapacitySurfaceReport`]: one snapshot per bounded surface.

use core::fmt;
use std::time::{Duration, SystemTime};

/// User-declared payload cost for weighted capacity.
///
/// This is deliberately not heap measurement. A type that wants
/// weighted admission states the cost it wants capacity accounting
/// to use: bytes, rows, jobs, handles, or another local unit.
pub trait CapacityWeight {
    /// Cost charged against a weighted capacity surface.
    fn capacity_weight(&self) -> usize;
}

/// How the cap was chosen. Cap is always a hard upper bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapacityMode {
    /// Measured cap. Use in production.
    Fixed,
    /// Discovery cap. Still hard. Reports surface high water so
    /// the user can pick a `Fixed` number.
    Tuning,
    /// Temporarily unbounded. Loud, named, and expires under live
    /// wall-clock time. Rejected by production policy.
    UnboundedForNow {
        /// Human reason. Keep it searchable.
        reason: String,
        /// Live expiry time. Validation rejects once this is in
        /// the past.
        expires_at: SystemTime,
    },
    /// Deliberately ugly no-expiry escape hatch. Development only
    /// by default.
    UnboundedWithoutExpiryIKnowThisIsBad {
        /// Human reason. Keep it searchable.
        reason: String,
    },
}

impl CapacityMode {
    /// Standard live expiry for [`Self::UnboundedForNow`].
    pub const UNBOUNDED_FOR_NOW_LIVE_EXPIRY: Duration = Duration::from_secs(60 * 60);

    /// Build an `UnboundedForNow` mode expiring one hour from now.
    pub fn unbounded_for_now(reason: impl Into<String>) -> Self {
        Self::unbounded_for_now_until(
            reason,
            SystemTime::now() + Self::UNBOUNDED_FOR_NOW_LIVE_EXPIRY,
        )
    }

    /// Build an `UnboundedForNow` mode with an explicit expiry.
    /// This exists so tests can use tiny expiries.
    pub fn unbounded_for_now_until(reason: impl Into<String>, expires_at: SystemTime) -> Self {
        Self::UnboundedForNow {
            reason: reason.into(),
            expires_at,
        }
    }

    /// Build the intentionally ugly no-expiry escape hatch.
    pub fn unbounded_without_expiry_i_know_this_is_bad(reason: impl Into<String>) -> Self {
        Self::UnboundedWithoutExpiryIKnowThisIsBad {
            reason: reason.into(),
        }
    }

    /// Short label for one-line reports.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::Tuning => "tuning",
            Self::UnboundedForNow { .. } => "unbounded_for_now",
            Self::UnboundedWithoutExpiryIKnowThisIsBad { .. } => {
                "unbounded_without_expiry_i_know_this_is_bad"
            }
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
    /// `UnboundedForNow` expired under live wall-clock time.
    UnboundedExpired {
        /// Surface whose mode failed validation.
        surface: String,
        /// Reason attached to the unbounded mode.
        reason: String,
        /// The expiry time that has passed.
        expires_at: SystemTime,
    },
    /// Unbounded modes must carry a searchable reason.
    EmptyUnboundedReason {
        /// Surface whose mode failed validation.
        surface: String,
        /// The mode label whose reason was empty.
        mode: &'static str,
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
            Self::UnboundedExpired {
                surface,
                reason,
                expires_at,
            } => write!(
                f,
                "capacity mode unbounded_for_now expired for surface {surface:?} \
                 at {expires_at:?}; reason={reason:?}"
            ),
            Self::EmptyUnboundedReason { surface, mode } => {
                write!(
                    f,
                    "capacity mode {mode} for surface {surface:?} needs a non-empty reason"
                )
            }
        }
    }
}

impl std::error::Error for CapacityPolicyError {}

impl CapacityPolicy {
    /// Validate `mode` for `surface` under this policy.
    pub fn validate_mode(
        &self,
        surface: &str,
        mode: &CapacityMode,
    ) -> Result<(), CapacityPolicyError> {
        match mode {
            CapacityMode::Fixed | CapacityMode::Tuning => Ok(()),
            CapacityMode::UnboundedForNow { reason, expires_at } => {
                if reason.trim().is_empty() {
                    return Err(CapacityPolicyError::EmptyUnboundedReason {
                        surface: surface.to_string(),
                        mode: mode.label(),
                    });
                }
                if SystemTime::now() >= *expires_at {
                    return Err(CapacityPolicyError::UnboundedExpired {
                        surface: surface.to_string(),
                        reason: reason.clone(),
                        expires_at: *expires_at,
                    });
                }
                match self {
                    CapacityPolicy::Development | CapacityPolicy::Test => Ok(()),
                    CapacityPolicy::Production => Err(CapacityPolicyError::ModeNotAllowed {
                        surface: surface.to_string(),
                        mode: mode.clone(),
                        policy: *self,
                    }),
                }
            }
            CapacityMode::UnboundedWithoutExpiryIKnowThisIsBad { reason } => {
                if reason.trim().is_empty() {
                    return Err(CapacityPolicyError::EmptyUnboundedReason {
                        surface: surface.to_string(),
                        mode: mode.label(),
                    });
                }
                match self {
                    CapacityPolicy::Development => Ok(()),
                    CapacityPolicy::Test | CapacityPolicy::Production => {
                        Err(CapacityPolicyError::ModeNotAllowed {
                            surface: surface.to_string(),
                            mode: mode.clone(),
                            policy: *self,
                        })
                    }
                }
            }
        }
    }
}

/// Snapshot of one bounded surface.
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
    /// Configured weight cap.
    pub max_weight: Option<usize>,
    /// Live weight right now.
    pub current_weight: Option<usize>,
    /// High-water weight since construction.
    pub high_water_weight: Option<usize>,
    /// Cumulative weight-cap `Full` rejections.
    pub weight_full_count: u64,
    /// Unit for weight fields, e.g. `bytes` or `rows`.
    pub weight_unit: Option<String>,
    /// Shard-local shared weight scope this surface charges.
    pub shared_scope: Option<String>,
    /// Configured shared-scope weight cap.
    pub shared_max_weight: Option<usize>,
    /// Current shared-scope weight.
    pub shared_current_weight: Option<usize>,
    /// Shared-scope high-water weight.
    pub shared_high_water_weight: Option<usize>,
    /// Cumulative shared-scope full rejections.
    pub shared_weight_full_count: u64,
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
            weight_unit: None,
            shared_scope: None,
            shared_max_weight: None,
            shared_current_weight: None,
            shared_high_water_weight: None,
            shared_weight_full_count: 0,
        }
    }

    /// Build a weighted report with no message count cap.
    pub fn weighted(
        name: impl Into<String>,
        mode: CapacityMode,
        max_weight: usize,
        current_weight: usize,
        high_water_weight: usize,
        weight_full_count: u64,
        weight_unit: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            mode,
            max_messages: None,
            current_messages: 0,
            high_water_messages: 0,
            full_count: 0,
            max_weight: Some(max_weight),
            current_weight: Some(current_weight),
            high_water_weight: Some(high_water_weight),
            weight_full_count,
            weight_unit: Some(weight_unit.into()),
            shared_scope: None,
            shared_max_weight: None,
            shared_current_weight: None,
            shared_high_water_weight: None,
            shared_weight_full_count: 0,
        }
    }

    /// Attach shard-local shared-scope weight fields to a report.
    pub fn with_shared_scope(
        mut self,
        scope: impl Into<String>,
        max_weight: usize,
        current_weight: usize,
        high_water_weight: usize,
        weight_full_count: u64,
    ) -> Self {
        self.shared_scope = Some(scope.into());
        self.shared_max_weight = Some(max_weight);
        self.shared_current_weight = Some(current_weight);
        self.shared_high_water_weight = Some(high_water_weight);
        self.shared_weight_full_count = weight_full_count;
        self
    }

    /// True if this surface ever hit `Full` (count or weight).
    pub fn ever_full(&self) -> bool {
        self.full_count > 0 || self.weight_full_count > 0 || self.shared_weight_full_count > 0
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
                    .validate_mode("any", &mode)
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
    fn weighted_constructor_populates_weight_fields() {
        let r = CapacitySurfaceReport::weighted(
            "http.response",
            CapacityMode::Fixed,
            100,
            40,
            70,
            2,
            "bytes",
        )
        .with_shared_scope("http.bodies", 150, 90, 120, 1);
        assert_eq!(r.max_messages, None);
        assert_eq!(r.max_weight, Some(100));
        assert_eq!(r.current_weight, Some(40));
        assert_eq!(r.high_water_weight, Some(70));
        assert_eq!(r.weight_unit.as_deref(), Some("bytes"));
        assert_eq!(r.shared_scope.as_deref(), Some("http.bodies"));
        assert_eq!(r.shared_max_weight, Some(150));
        assert!(r.ever_full());
    }

    #[test]
    fn unbounded_for_now_expires_under_live_time() {
        let mode = CapacityMode::unbounded_for_now_until(
            "temporary import",
            SystemTime::now() - Duration::from_millis(1),
        );
        let err = CapacityPolicy::Test
            .validate_mode("import.queue", &mode)
            .unwrap_err();
        assert!(matches!(err, CapacityPolicyError::UnboundedExpired { .. }));
    }

    #[test]
    fn production_rejects_unbounded_for_now() {
        let mode = CapacityMode::unbounded_for_now("temporary import");
        let err = CapacityPolicy::Production
            .validate_mode("import.queue", &mode)
            .unwrap_err();
        assert!(matches!(err, CapacityPolicyError::ModeNotAllowed { .. }));
    }

    #[test]
    fn test_and_production_reject_no_expiry_escape_by_default() {
        let mode = CapacityMode::unbounded_without_expiry_i_know_this_is_bad("scratch");
        assert!(
            CapacityPolicy::Development
                .validate_mode("scratch", &mode)
                .is_ok()
        );
        assert!(
            CapacityPolicy::Test
                .validate_mode("scratch", &mode)
                .is_err()
        );
        assert!(
            CapacityPolicy::Production
                .validate_mode("scratch", &mode)
                .is_err()
        );
    }

    #[test]
    fn unbounded_modes_reject_empty_reasons() {
        let expiring = CapacityMode::unbounded_for_now(" ");
        assert!(matches!(
            CapacityPolicy::Development.validate_mode("scratch", &expiring),
            Err(CapacityPolicyError::EmptyUnboundedReason { .. })
        ));
        let no_expiry = CapacityMode::unbounded_without_expiry_i_know_this_is_bad("");
        assert!(matches!(
            CapacityPolicy::Development.validate_mode("scratch", &no_expiry),
            Err(CapacityPolicyError::EmptyUnboundedReason { .. })
        ));
    }

    #[test]
    fn capacity_mode_label_is_distinct() {
        assert_ne!(CapacityMode::Fixed.label(), CapacityMode::Tuning.label());
    }
}
