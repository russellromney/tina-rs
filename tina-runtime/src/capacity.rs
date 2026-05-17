//! Runtime-side capacity collection and assertions.
//!
//! Vocabulary lives in [`tina::capacity`]. This module collects
//! reports into a [`CapacitySummary`], offers `Result` assertions,
//! and prints one-line discovery output.
//!
//! Discovery line shape matches [`format_pressure_line`]'s
//! `key=value` convention:
//!
//! ```text
//! capacity surface=pool.1.waiters mode=fixed  max=4  cur=0 high=4  full=2 suggest="saw Full — raise cap or shed earlier"
//! capacity surface=orders.mailbox mode=tuning max=64 cur=0 high=11 full=0 suggest="tuning cap is loose; freeze near 2x high water"
//! ```
//!
//! [`format_pressure_line`]: crate::format_pressure_line

use std::fmt;

use tina::capacity::{CapacityMode, CapacitySurfaceReport};

/// Why a [`CapacitySummary`] rejected a [`CapacitySurfaceReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapacityNameError {
    /// Two surfaces tried to register under the same name.
    Duplicate(String),
    /// Name is empty.
    Empty,
    /// Name contains whitespace or non-printable characters that
    /// would break the `key=value` shape of the discovery line.
    /// Use a dotted token form (e.g. `pool.orders.waiters`).
    InvalidName(String),
}

impl fmt::Display for CapacityNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duplicate(name) => {
                write!(f, "duplicate capacity surface name {name:?}")
            }
            Self::Empty => f.write_str("capacity surface name is empty"),
            Self::InvalidName(name) => write!(
                f,
                "capacity surface name {name:?} contains whitespace or \
                 control characters; use a dotted token form like \
                 \"pool.orders.waiters\""
            ),
        }
    }
}

impl std::error::Error for CapacityNameError {}

/// True if `name` is a valid surface name: non-empty and free of
/// whitespace/control characters. Surface names appear unquoted
/// after `surface=` in the discovery line, so anything that breaks
/// `key=value` parsing is rejected at the boundary.
fn name_is_valid(name: &str) -> bool {
    !name.is_empty() && !name.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Why a [`SurfaceAssertion`] failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapacityAssertError {
    /// No surface was registered under the given name.
    UnknownSurface(String),
    /// `high_water_at_most(limit)` saw a higher value.
    HighWaterAbove {
        /// Surface name.
        surface: String,
        /// Configured assertion limit.
        limit: usize,
        /// Observed high water.
        observed: usize,
    },
    /// `no_full()` saw a non-zero full counter.
    Full {
        /// Surface name.
        surface: String,
        /// What filled: `count`, `weight`, or `shared_weight`.
        filled: &'static str,
        /// Observed full counter.
        observed: u64,
    },
    /// `full_count_eq(expected)` saw a different value.
    FullCountMismatch {
        /// Surface name.
        surface: String,
        /// Expected count.
        expected: u64,
        /// Observed count.
        observed: u64,
    },
    /// A weighted assertion targeted a surface with no weight data.
    MissingWeight {
        /// Surface name.
        surface: String,
    },
}

impl fmt::Display for CapacityAssertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSurface(name) => {
                write!(f, "no capacity surface named {name:?}")
            }
            Self::HighWaterAbove {
                surface,
                limit,
                observed,
            } => write!(
                f,
                "surface {surface:?} high_water {observed} exceeds limit {limit}"
            ),
            Self::Full {
                surface,
                filled,
                observed,
            } => write!(
                f,
                "surface {surface:?} hit Full {observed} times on {filled} (expected 0)"
            ),
            Self::FullCountMismatch {
                surface,
                expected,
                observed,
            } => write!(
                f,
                "surface {surface:?} full_count {observed} != expected {expected}"
            ),
            Self::MissingWeight { surface } => {
                write!(f, "surface {surface:?} has no weight fields")
            }
        }
    }
}

impl std::error::Error for CapacityAssertError {}

/// One snapshot of every bounded surface a system reports.
///
/// Names must be unique within one summary. Iterate via
/// [`Self::reports`] for dashboards; look up by name via
/// [`Self::surface`].
#[derive(Debug, Default, Clone)]
pub struct CapacitySummary {
    reports: Vec<CapacitySurfaceReport>,
}

impl CapacitySummary {
    /// Empty summary.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a report. Rejects empty names, names with whitespace or
    /// control characters, and duplicate names. Validation runs at
    /// the summary boundary so the discovery line stays
    /// grep-friendly and CI tests cannot silently pick the wrong
    /// surface.
    pub fn push(&mut self, report: CapacitySurfaceReport) -> Result<(), CapacityNameError> {
        if report.name.is_empty() {
            return Err(CapacityNameError::Empty);
        }
        if !name_is_valid(&report.name) {
            return Err(CapacityNameError::InvalidName(report.name));
        }
        if self.reports.iter().any(|r| r.name == report.name) {
            return Err(CapacityNameError::Duplicate(report.name));
        }
        self.reports.push(report);
        Ok(())
    }

    /// Number of registered surfaces.
    pub fn len(&self) -> usize {
        self.reports.len()
    }

    /// True if no surfaces are registered.
    pub fn is_empty(&self) -> bool {
        self.reports.is_empty()
    }

    /// Iterator over every report.
    pub fn reports(&self) -> impl Iterator<Item = &CapacitySurfaceReport> {
        self.reports.iter()
    }

    /// Look up one surface by name. Returns a handle so test code
    /// can chain `.no_full()` / `.high_water_at_most(N)`.
    pub fn surface<'a>(&'a self, name: &str) -> SurfaceAssertion<'a> {
        SurfaceAssertion {
            name: name.to_string(),
            report: self.reports.iter().find(|r| r.name == name),
        }
    }

    /// True if any surface ever hit `Full`.
    pub fn any_full(&self) -> bool {
        self.reports.iter().any(|r| r.ever_full())
    }

    /// Aggregate "no Full anywhere" assertion. Returns every surface
    /// that filled, with one error per surface and counter. Use this
    /// in CI to fail with copyable, grep-friendly output covering
    /// every offender at once.
    pub fn assert_no_full(&self) -> Result<(), Vec<CapacityAssertError>> {
        let mut errors = Vec::new();
        for r in &self.reports {
            if r.full_count > 0 {
                errors.push(CapacityAssertError::Full {
                    surface: r.name.clone(),
                    filled: "count",
                    observed: r.full_count,
                });
            }
            if r.weight_full_count > 0 {
                errors.push(CapacityAssertError::Full {
                    surface: r.name.clone(),
                    filled: "weight",
                    observed: r.weight_full_count,
                });
            }
            if r.shared_weight_full_count > 0 {
                errors.push(CapacityAssertError::Full {
                    surface: r.name.clone(),
                    filled: "shared_weight",
                    observed: r.shared_weight_full_count,
                });
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// One-line "what cap to tune" hint per failed surface. Use this to
/// format `assert_no_full()` errors into a copyable CI report.
///
/// ```text
/// FAIL surface=pool.1.waiters filled=count observed=2 — see capacity discovery for cap and high water
/// ```
pub fn format_assertion_failure(error: &CapacityAssertError) -> String {
    match error {
        CapacityAssertError::Full {
            surface,
            filled,
            observed,
        } => format!(
            "FAIL surface={surface} filled={filled} observed={observed} — see capacity discovery for cap and high water",
        ),
        CapacityAssertError::HighWaterAbove {
            surface,
            limit,
            observed,
        } => format!(
            "FAIL surface={surface} high_water={observed} limit={limit} — raise expected limit or shrink the system's true high water",
        ),
        CapacityAssertError::FullCountMismatch {
            surface,
            expected,
            observed,
        } => format!("FAIL surface={surface} full_count={observed} expected={expected}",),
        CapacityAssertError::UnknownSurface(name) => format!(
            "FAIL surface={name} unknown — the assertion targets a name not registered in this summary",
        ),
        CapacityAssertError::MissingWeight { surface } => format!(
            "FAIL surface={surface} missing_weight — assertion needs weight fields but the surface is count-only",
        ),
    }
}

/// `Result` assertions for one surface. Keeps test code free of
/// string parsing.
pub struct SurfaceAssertion<'a> {
    name: String,
    report: Option<&'a CapacitySurfaceReport>,
}

impl<'a> SurfaceAssertion<'a> {
    /// The underlying report, or an `UnknownSurface` error.
    pub fn report(&self) -> Result<&'a CapacitySurfaceReport, CapacityAssertError> {
        self.report
            .ok_or_else(|| CapacityAssertError::UnknownSurface(self.name.clone()))
    }

    /// `Ok(())` iff every full counter is zero.
    pub fn no_full(&self) -> Result<(), CapacityAssertError> {
        let r = self.report()?;
        if r.full_count > 0 {
            return Err(CapacityAssertError::Full {
                surface: r.name.clone(),
                filled: "count",
                observed: r.full_count,
            });
        }
        if r.weight_full_count > 0 {
            return Err(CapacityAssertError::Full {
                surface: r.name.clone(),
                filled: "weight",
                observed: r.weight_full_count,
            });
        }
        if r.shared_weight_full_count > 0 {
            return Err(CapacityAssertError::Full {
                surface: r.name.clone(),
                filled: "shared_weight",
                observed: r.shared_weight_full_count,
            });
        }
        Ok(())
    }

    /// `Ok(())` iff `high_water_messages <= limit`.
    pub fn high_water_at_most(&self, limit: usize) -> Result<(), CapacityAssertError> {
        let r = self.report()?;
        if r.high_water_messages <= limit {
            Ok(())
        } else {
            Err(CapacityAssertError::HighWaterAbove {
                surface: r.name.clone(),
                limit,
                observed: r.high_water_messages,
            })
        }
    }

    /// `Ok(())` iff `full_count == expected`.
    pub fn full_count_eq(&self, expected: u64) -> Result<(), CapacityAssertError> {
        let r = self.report()?;
        if r.full_count == expected {
            Ok(())
        } else {
            Err(CapacityAssertError::FullCountMismatch {
                surface: r.name.clone(),
                expected,
                observed: r.full_count,
            })
        }
    }

    /// `Ok(())` iff weighted high-water is present and `<= limit`.
    pub fn high_water_weight_at_most(&self, limit: usize) -> Result<(), CapacityAssertError> {
        let r = self.report()?;
        let observed = r
            .high_water_weight
            .ok_or_else(|| CapacityAssertError::MissingWeight {
                surface: r.name.clone(),
            })?;
        if observed <= limit {
            Ok(())
        } else {
            Err(CapacityAssertError::HighWaterAbove {
                surface: r.name.clone(),
                limit,
                observed,
            })
        }
    }

    /// `Ok(())` iff `weight_full_count == expected`.
    pub fn weight_full_count_eq(&self, expected: u64) -> Result<(), CapacityAssertError> {
        let r = self.report()?;
        if r.max_weight.is_none() && r.high_water_weight.is_none() {
            return Err(CapacityAssertError::MissingWeight {
                surface: r.name.clone(),
            });
        }
        if r.weight_full_count == expected {
            Ok(())
        } else {
            Err(CapacityAssertError::FullCountMismatch {
                surface: r.name.clone(),
                expected,
                observed: r.weight_full_count,
            })
        }
    }

    /// `Ok(())` iff shared-scope full count equals `expected`.
    pub fn shared_weight_full_count_eq(&self, expected: u64) -> Result<(), CapacityAssertError> {
        let r = self.report()?;
        if r.shared_weight_full_count == expected {
            Ok(())
        } else {
            Err(CapacityAssertError::FullCountMismatch {
                surface: r.name.clone(),
                expected,
                observed: r.shared_weight_full_count,
            })
        }
    }
}

/// Next-action hint for the discovery formatter.
///
/// Thresholds are basis points of `high_water / max`:
///
/// - `Fixed` is supposed to be tight. Below 25% is loose; above
///   85% is tight.
/// - `Tuning` expects more headroom. Below 25% is very loose;
///   above 75% is tight enough to re-measure.
fn suggest_next(report: &CapacitySurfaceReport) -> &'static str {
    let fulls = report.full_count + report.weight_full_count + report.shared_weight_full_count;
    let max = match report.max_messages {
        Some(m) => m,
        // No reachable producer of `None` today. Future unbounded
        // modes will replace this hint.
        None if fulls > 0 => return "saw Full — raise cap or shed earlier",
        None => {
            if let Some(max_weight) = report.max_weight {
                if max_weight == 0 {
                    return "weighted cap is zero — increase or remove this surface";
                }
                let high = report.high_water_weight.unwrap_or(0);
                let bp = (high.saturating_mul(10_000)) / max_weight;
                return match (&report.mode, bp) {
                    (CapacityMode::Tuning, b) if b < 2_500 => {
                        "weighted tuning cap is loose; freeze near 2x high water"
                    }
                    (CapacityMode::Tuning, b) if b < 7_500 => {
                        "weighted tuning cap fits; freeze near 1.5x high water"
                    }
                    (CapacityMode::Tuning, _) => {
                        "weighted tuning cap is tight; raise then re-measure"
                    }
                    (CapacityMode::Fixed, b) if b < 2_500 => {
                        "weighted fixed cap is loose; consider shrinking"
                    }
                    (CapacityMode::Fixed, b) if b < 8_500 => "weighted fixed cap fits",
                    (CapacityMode::Fixed, _) => "weighted fixed cap is tight; consider raising",
                    (CapacityMode::UnboundedForNow { .. }, _) => {
                        "unbounded-for-now is live; pick a fixed weighted cap"
                    }
                    (CapacityMode::UnboundedWithoutExpiryIKnowThisIsBad { .. }, _) => {
                        "no-expiry unbounded escape is live; replace with a fixed weighted cap"
                    }
                };
            }
            return "no cap configured — pick a fixed cap";
        }
    };
    if fulls > 0 {
        return "saw Full — raise cap or shed earlier";
    }
    if max == 0 {
        return "cap is zero — increase or remove this surface";
    }
    let bp = (report.high_water_messages.saturating_mul(10_000)) / max;
    match (&report.mode, bp) {
        (CapacityMode::Tuning, b) if b < 2_500 => "tuning cap is loose; freeze near 2x high water",
        (CapacityMode::Tuning, b) if b < 7_500 => "tuning cap fits; freeze near 1.5x high water",
        (CapacityMode::Tuning, _) => "tuning cap is tight; raise then re-measure",
        (CapacityMode::Fixed, b) if b < 2_500 => "fixed cap is loose; consider shrinking",
        (CapacityMode::Fixed, b) if b < 8_500 => "fixed cap fits",
        (CapacityMode::Fixed, _) => "fixed cap is tight; consider raising",
        (CapacityMode::UnboundedForNow { .. }, _) => "unbounded-for-now is live; pick a fixed cap",
        (CapacityMode::UnboundedWithoutExpiryIKnowThisIsBad { .. }, _) => {
            "no-expiry unbounded escape is live; replace with a fixed cap"
        }
    }
}

/// Format a value for inclusion after a `key=` token. Bare when safe
/// (no whitespace/control), otherwise double-quoted with debug
/// formatting so parsers do not split the line at the unsafe char.
pub(crate) fn discovery_value(value: &str) -> String {
    if name_is_valid(value) {
        value.to_string()
    } else {
        format!("{value:?}")
    }
}

/// Utilization in basis points (1/10000ths). `None` when there is
/// no count cap *and* no weight cap to divide against. Saturates at
/// 10000 (100%) for high-water peaks above cap.
///
/// `max == 0` is reported as 10000 *only* when the surface actually
/// took traffic (`full_count > 0` or some high water observed); a
/// zero-cap surface with no use returns 0, not "infinitely full,
/// despite never being touched."
fn utilization_bp(report: &CapacitySurfaceReport) -> Option<u64> {
    if let Some(max) = report.max_messages {
        let high = report.high_water_messages as u64;
        if max == 0 {
            return Some(if high > 0 || report.full_count > 0 {
                10_000
            } else {
                0
            });
        }
        return Some((high.saturating_mul(10_000)) / (max as u64));
    }
    if let Some(max) = report.max_weight {
        let high = report.high_water_weight.unwrap_or(0) as u64;
        if max == 0 {
            return Some(if high > 0 || report.weight_full_count > 0 {
                10_000
            } else {
                0
            });
        }
        return Some((high.saturating_mul(10_000)) / (max as u64));
    }
    None
}

/// One `key=value` line per surface. Matches
/// [`format_pressure_line`](crate::format_pressure_line) so the
/// same greppers work. Token-like values are printed bare when they
/// are safe; values with whitespace or control characters are
/// double-quoted.
///
/// `util_bp` is high-water utilization in basis points (out of 10000).
/// `100` = 1%, `8500` = 85%, `10000` = at or above cap. Use this for
/// grep-friendly utilization assertions:
/// `grep -E 'util_bp=([0-9]{4,5}|9[0-9]{3})' …`.
///
/// ```text
/// capacity surface=pool.1.waiters mode=fixed max=4 cur=0 high=4 full=2 util_bp=10000 suggest="saw Full — raise cap or shed earlier"
/// ```
pub fn format_discovery_line(report: &CapacitySurfaceReport) -> String {
    let max = match report.max_messages {
        Some(m) => m.to_string(),
        None => "-".to_string(),
    };
    let util = match utilization_bp(report) {
        Some(bp) => bp.to_string(),
        None => "-".to_string(),
    };
    let mut line = format!(
        "capacity surface={name} mode={mode} max={max} cur={cur} high={high} full={full} util_bp={util} suggest={hint:?}",
        name = discovery_value(&report.name),
        mode = report.mode.label(),
        max = max,
        cur = report.current_messages,
        high = report.high_water_messages,
        full = report.full_count,
        util = util,
        hint = suggest_next(report),
    );
    if let Some(max_weight) = report.max_weight {
        let unit = report.weight_unit.as_deref().unwrap_or("weight");
        line.push_str(&format!(
            " weight_unit={unit} max_weight={max_weight} cur_weight={cur} high_weight={high} weight_full={full}",
            unit = discovery_value(unit),
            cur = report.current_weight.unwrap_or(0),
            high = report.high_water_weight.unwrap_or(0),
            full = report.weight_full_count,
        ));
    }
    if let Some(scope) = &report.shared_scope {
        line.push_str(&format!(
            " shared_scope={scope} shared_max_weight={max} shared_cur_weight={cur} shared_high_weight={high} shared_weight_full={full}",
            scope = discovery_value(scope),
            max = report.shared_max_weight.unwrap_or(0),
            cur = report.shared_current_weight.unwrap_or(0),
            high = report.shared_high_water_weight.unwrap_or(0),
            full = report.shared_weight_full_count,
        ));
    }
    line
}

/// One discovery line per surface, joined with newlines.
pub fn format_discovery_report(summary: &CapacitySummary) -> String {
    let mut out = String::new();
    for r in summary.reports() {
        out.push_str(&format_discovery_line(r));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tina::capacity::CapacityMode;

    fn report(name: &str, max: usize, cur: usize, hi: usize, full: u64) -> CapacitySurfaceReport {
        CapacitySurfaceReport::count(name, CapacityMode::Fixed, max, cur, hi, full)
    }

    #[test]
    fn summary_push_rejects_duplicates() {
        let mut s = CapacitySummary::new();
        s.push(report("a", 4, 0, 0, 0)).unwrap();
        let err = s.push(report("a", 4, 0, 0, 0)).unwrap_err();
        assert_eq!(err, CapacityNameError::Duplicate("a".into()));
    }

    #[test]
    fn summary_push_rejects_empty_name() {
        let mut s = CapacitySummary::new();
        let err = s.push(report("", 4, 0, 0, 0)).unwrap_err();
        assert_eq!(err, CapacityNameError::Empty);
    }

    #[test]
    fn summary_push_rejects_whitespace_in_name() {
        // A space in the name would break the surface=name field of
        // the discovery line into "surface=foo bar=...".
        let mut s = CapacitySummary::new();
        let err = s.push(report("foo bar", 4, 0, 0, 0)).unwrap_err();
        assert_eq!(err, CapacityNameError::InvalidName("foo bar".into()));
    }

    #[test]
    fn summary_push_rejects_tab_and_newline() {
        let mut s = CapacitySummary::new();
        assert!(matches!(
            s.push(report("foo\tbar", 4, 0, 0, 0)),
            Err(CapacityNameError::InvalidName(_))
        ));
        assert!(matches!(
            s.push(report("foo\nbar", 4, 0, 0, 0)),
            Err(CapacityNameError::InvalidName(_))
        ));
    }

    #[test]
    fn summary_push_accepts_dotted_form() {
        let mut s = CapacitySummary::new();
        s.push(report("pool.orders.waiters", 4, 0, 0, 0)).unwrap();
        s.push(report("frontend.pending", 4, 0, 0, 0)).unwrap();
    }

    #[test]
    fn surface_lookup_finds_registered() {
        let mut s = CapacitySummary::new();
        s.push(report("orders.mailbox", 64, 0, 0, 0)).unwrap();
        let r = s.surface("orders.mailbox").report().unwrap();
        assert_eq!(r.name, "orders.mailbox");
    }

    #[test]
    fn surface_lookup_missing_gives_unknown_surface() {
        let s = CapacitySummary::new();
        let err = s.surface("nope").no_full().unwrap_err();
        assert_eq!(err, CapacityAssertError::UnknownSurface("nope".into()));
    }

    #[test]
    fn no_full_passes_when_zero_and_fails_otherwise() {
        let mut s = CapacitySummary::new();
        s.push(report("a", 4, 0, 4, 0)).unwrap();
        s.push(report("b", 4, 0, 4, 3)).unwrap();
        s.surface("a").no_full().unwrap();
        let err = s.surface("b").no_full().unwrap_err();
        match err {
            CapacityAssertError::Full {
                filled, observed, ..
            } => {
                assert_eq!(filled, "count");
                assert_eq!(observed, 3);
            }
            _ => panic!("wrong error: {err:?}"),
        }
    }

    #[test]
    fn high_water_at_most_compares() {
        let mut s = CapacitySummary::new();
        s.push(report("a", 16, 0, 5, 0)).unwrap();
        s.surface("a").high_water_at_most(5).unwrap();
        s.surface("a").high_water_at_most(8).unwrap();
        s.surface("a").high_water_at_most(4).unwrap_err();
    }

    #[test]
    fn full_count_eq_compares() {
        let mut s = CapacitySummary::new();
        s.push(report("a", 4, 0, 4, 2)).unwrap();
        s.surface("a").full_count_eq(2).unwrap();
        s.surface("a").full_count_eq(0).unwrap_err();
    }

    #[test]
    fn weight_assertions_compare_weight_fields() {
        let mut s = CapacitySummary::new();
        s.push(CapacitySurfaceReport::weighted(
            "http.response",
            CapacityMode::Fixed,
            100,
            0,
            75,
            2,
            "bytes",
        ))
        .unwrap();
        s.surface("http.response")
            .high_water_weight_at_most(75)
            .unwrap();
        s.surface("http.response")
            .high_water_weight_at_most(74)
            .unwrap_err();
        s.surface("http.response").weight_full_count_eq(2).unwrap();
        let err = s.surface("http.response").no_full().unwrap_err();
        match err {
            CapacityAssertError::Full {
                filled, observed, ..
            } => {
                assert_eq!(filled, "weight");
                assert_eq!(observed, 2);
            }
            _ => panic!("wrong error: {err:?}"),
        }
    }

    #[test]
    fn weight_assertions_reject_count_only_surfaces() {
        let mut s = CapacitySummary::new();
        s.push(report("pool.waiters", 4, 0, 0, 0)).unwrap();
        let err = s
            .surface("pool.waiters")
            .high_water_weight_at_most(1)
            .unwrap_err();
        assert_eq!(
            err,
            CapacityAssertError::MissingWeight {
                surface: "pool.waiters".to_string()
            }
        );
        assert!(matches!(
            s.surface("pool.waiters").weight_full_count_eq(0),
            Err(CapacityAssertError::MissingWeight { .. })
        ));
    }

    #[test]
    fn shared_weight_assertion_names_what_filled() {
        let mut s = CapacitySummary::new();
        s.push(
            CapacitySurfaceReport::weighted(
                "http.request",
                CapacityMode::Fixed,
                100,
                0,
                10,
                0,
                "bytes",
            )
            .with_shared_scope("http.bodies", 150, 0, 150, 1),
        )
        .unwrap();
        s.surface("http.request")
            .shared_weight_full_count_eq(1)
            .unwrap();
        let err = s.surface("http.request").no_full().unwrap_err();
        match err {
            CapacityAssertError::Full {
                filled, observed, ..
            } => {
                assert_eq!(filled, "shared_weight");
                assert_eq!(observed, 1);
            }
            _ => panic!("wrong error: {err:?}"),
        }
    }

    #[test]
    fn discovery_line_uses_key_value_form() {
        let r = report("pool.1.waiters", 4, 0, 4, 2);
        let line = format_discovery_line(&r);
        // surface name and key=value pairs all appear, no padding.
        assert!(line.starts_with("capacity "), "{line}");
        assert!(line.contains("surface=pool.1.waiters"), "{line}");
        assert!(line.contains("mode=fixed"), "{line}");
        assert!(line.contains("max=4"), "{line}");
        assert!(line.contains("cur=0"), "{line}");
        assert!(line.contains("high=4"), "{line}");
        assert!(line.contains("full=2"), "{line}");
        assert!(line.contains("util_bp=10000"), "{line}");
        assert!(line.contains("raise cap"), "{line}");
    }

    #[test]
    fn discovery_line_includes_util_bp_for_partial_fill() {
        // high=1 / max=4 = 2500 bp
        let r = report("p", 4, 0, 1, 0);
        let line = format_discovery_line(&r);
        assert!(line.contains("util_bp=2500"), "{line}");
    }

    #[test]
    fn assert_no_full_aggregates_every_offender() {
        let mut s = CapacitySummary::new();
        s.push(report("a", 4, 0, 4, 0)).unwrap();
        s.push(report("b", 4, 0, 4, 2)).unwrap();
        s.push(
            CapacitySurfaceReport::weighted("c", CapacityMode::Fixed, 100, 0, 100, 3, "bytes")
                .with_shared_scope("scope", 1000, 0, 1000, 1),
        )
        .unwrap();
        let errors = s.assert_no_full().unwrap_err();
        // b contributes count, c contributes weight + shared_weight.
        assert_eq!(errors.len(), 3);
    }

    #[test]
    fn format_assertion_failure_starts_with_fail() {
        let err = CapacityAssertError::Full {
            surface: "pool.1.waiters".to_string(),
            filled: "count",
            observed: 2,
        };
        let line = format_assertion_failure(&err);
        assert!(line.starts_with("FAIL "), "{line}");
        assert!(line.contains("surface=pool.1.waiters"), "{line}");
        assert!(line.contains("filled=count"), "{line}");
        assert!(line.contains("observed=2"), "{line}");
    }

    #[test]
    fn format_assertion_failure_covers_every_variant() {
        for err in [
            CapacityAssertError::HighWaterAbove {
                surface: "p".into(),
                limit: 4,
                observed: 9,
            },
            CapacityAssertError::FullCountMismatch {
                surface: "p".into(),
                expected: 0,
                observed: 3,
            },
            CapacityAssertError::UnknownSurface("nope".into()),
            CapacityAssertError::MissingWeight {
                surface: "p".into(),
            },
        ] {
            let line = format_assertion_failure(&err);
            assert!(line.starts_with("FAIL "), "{line}");
            assert!(line.contains("surface="), "{line}");
        }
    }

    #[test]
    fn util_bp_zero_when_zero_cap_never_touched() {
        let r = report("p", 0, 0, 0, 0);
        let line = format_discovery_line(&r);
        assert!(line.contains("util_bp=0"), "{line}");
    }

    #[test]
    fn util_bp_saturates_at_10000_when_zero_cap_was_filled() {
        let r = report("p", 0, 0, 0, 3);
        let line = format_discovery_line(&r);
        assert!(line.contains("util_bp=10000"), "{line}");
    }

    #[test]
    fn discovery_line_quotes_suggest_string() {
        // suggest= value contains spaces; pure k=v parsers need it
        // quoted so they don't split the hint into separate fields.
        let r = report("a", 4, 0, 4, 0);
        let line = format_discovery_line(&r);
        let suggest_idx = line.find("suggest=").expect("suggest=");
        assert!(
            line[suggest_idx..].starts_with("suggest=\""),
            "expected double-quoted suggest=, got {line}"
        );
    }

    #[test]
    fn discovery_line_includes_weight_and_shared_scope() {
        let r = CapacitySurfaceReport::weighted(
            "http.response",
            CapacityMode::Fixed,
            4096,
            0,
            1024,
            1,
            "bytes",
        )
        .with_shared_scope("http.bodies", 8192, 0, 4096, 2);
        let line = format_discovery_line(&r);
        assert!(line.contains("weight_unit=bytes"), "{line}");
        assert!(line.contains("max_weight=4096"), "{line}");
        assert!(line.contains("weight_full=1"), "{line}");
        assert!(line.contains("shared_scope=http.bodies"), "{line}");
        assert!(line.contains("shared_weight_full=2"), "{line}");
        assert!(line.contains("saw Full"), "{line}");
    }

    #[test]
    fn discovery_line_quotes_unsafe_token_fields() {
        let r = CapacitySurfaceReport::weighted(
            "http response",
            CapacityMode::Fixed,
            4096,
            0,
            1024,
            1,
            "body bytes",
        )
        .with_shared_scope("http bodies", 8192, 0, 4096, 2);
        let line = format_discovery_line(&r);
        assert!(line.contains("surface=\"http response\""), "{line}");
        assert!(line.contains("weight_unit=\"body bytes\""), "{line}");
        assert!(line.contains("shared_scope=\"http bodies\""), "{line}");
    }

    #[test]
    fn weighted_discovery_hint_uses_weight_utilization() {
        let loose = CapacitySurfaceReport::weighted(
            "http.response",
            CapacityMode::Fixed,
            10_000,
            0,
            100,
            0,
            "bytes",
        );
        assert!(suggest_next(&loose).contains("loose"));

        let tight = CapacitySurfaceReport::weighted(
            "http.response",
            CapacityMode::Fixed,
            100,
            0,
            95,
            0,
            "bytes",
        );
        assert!(suggest_next(&tight).contains("tight"));
    }

    #[test]
    fn discovery_report_one_line_per_surface() {
        let mut s = CapacitySummary::new();
        s.push(report("a", 4, 0, 0, 0)).unwrap();
        s.push(report("b", 4, 0, 0, 1)).unwrap();
        let out = format_discovery_report(&s);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("surface=a"), "{lines:?}");
        assert!(lines[1].contains("surface=b"), "{lines:?}");
    }

    #[test]
    fn suggest_next_recognizes_tuning_loose() {
        let r = CapacitySurfaceReport::count("a", CapacityMode::Tuning, 100, 0, 5, 0);
        assert!(suggest_next(&r).contains("loose"));
    }

    #[test]
    fn suggest_next_recognizes_full_pressure_over_loose_advice() {
        // Even when high water is low, a non-zero full_count must
        // win — load that *did* shed should not be suggested as
        // "loose, shrink the cap".
        let r = CapacitySurfaceReport::count("a", CapacityMode::Fixed, 100, 0, 1, 5);
        assert!(suggest_next(&r).contains("Full"));
        assert!(!suggest_next(&r).contains("loose"));
    }
}
