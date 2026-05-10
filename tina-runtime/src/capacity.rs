//! Runtime-side capacity collection and assertions.
//!
//! Vocabulary lives in [`tina::capacity`]. This module collects
//! [`CapacitySurfaceReport`]s into a [`CapacitySummary`] and offers
//! tiny `Result`-flavored assertions plus a one-line discovery
//! formatter so users can move from "I do not know the cap" to
//! "I picked a fixed cap" without reading runtime internals.
//!
//! ```text
//! pool.1.waiters       fixed   max=4    high=4   full=2   suggest=raise cap or shed
//! orders.mailbox       tuning  max=64   high=11  full=0   suggest=cap looks loose; freeze around 16
//! ```

use std::fmt;

use tina::capacity::{CapacityMode, CapacitySurfaceReport};

/// Why a [`CapacitySummary`] rejected a [`CapacitySurfaceReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapacityNameError {
    /// Two surfaces tried to register under the same name.
    Duplicate(String),
}

impl fmt::Display for CapacityNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duplicate(name) => {
                write!(f, "duplicate capacity surface name {name:?}")
            }
        }
    }
}

impl std::error::Error for CapacityNameError {}

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
    /// `no_full()` saw a non-zero `full_count`.
    Full {
        /// Surface name.
        surface: String,
        /// Observed `full_count`.
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
            Self::Full { surface, observed } => write!(
                f,
                "surface {surface:?} hit Full {observed} times (expected 0)"
            ),
            Self::FullCountMismatch {
                surface,
                expected,
                observed,
            } => write!(
                f,
                "surface {surface:?} full_count {observed} != expected {expected}"
            ),
        }
    }
}

impl std::error::Error for CapacityAssertError {}

/// Collected snapshot of every bounded surface a system reports.
///
/// Names must be unique within one summary. Iterate via
/// [`Self::reports`] for human / dashboard output; reach for one
/// surface by name via [`Self::surface`].
#[derive(Debug, Default, Clone)]
pub struct CapacitySummary {
    reports: Vec<CapacitySurfaceReport>,
}

impl CapacitySummary {
    /// Empty summary.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a report. Rejects duplicate names so a CI assertion
    /// never silently picks the wrong surface.
    pub fn push(&mut self, report: CapacitySurfaceReport) -> Result<(), CapacityNameError> {
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

    /// Iterator over every registered report.
    pub fn reports(&self) -> impl Iterator<Item = &CapacitySurfaceReport> {
        self.reports.iter()
    }

    /// Look up a single surface by name. Returns an assertion handle
    /// so test code can chain `.no_full()` / `.high_water_at_most(N)`.
    pub fn surface<'a>(&'a self, name: &str) -> SurfaceAssertion<'a> {
        SurfaceAssertion {
            name: name.to_string(),
            report: self.reports.iter().find(|r| r.name == name),
        }
    }

    /// True if any surface ever reported a `Full`-shaped rejection.
    pub fn any_full(&self) -> bool {
        self.reports.iter().any(|r| r.ever_full())
    }
}

/// Result-flavored assertions for one surface. Keeps test code free
/// of failure-string parsing.
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

    /// `Ok(())` iff `full_count == 0`.
    pub fn no_full(&self) -> Result<(), CapacityAssertError> {
        let r = self.report()?;
        if r.full_count == 0 {
            Ok(())
        } else {
            Err(CapacityAssertError::Full {
                surface: r.name.clone(),
                observed: r.full_count,
            })
        }
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

    /// Assert wrapper: panics on failure. Use sparingly — the
    /// `Result`-returning forms compose better in test pipelines.
    #[track_caller]
    pub fn assert_no_full(&self) {
        if let Err(e) = self.no_full() {
            panic!("{e}");
        }
    }

    /// Assert wrapper for [`Self::high_water_at_most`].
    #[track_caller]
    pub fn assert_high_water_at_most(&self, limit: usize) {
        if let Err(e) = self.high_water_at_most(limit) {
            panic!("{e}");
        }
    }
}

/// Human-readable next-action hint for the discovery formatter.
fn suggest_next(report: &CapacitySurfaceReport) -> &'static str {
    let max = match report.max_messages {
        Some(m) => m,
        None => return "unbounded — pick a fixed cap (PR 2 escape hatch)",
    };
    if report.full_count > 0 {
        return "saw Full — raise cap or shed earlier";
    }
    let high = report.high_water_messages;
    if max == 0 {
        return "cap is zero — increase or remove this surface";
    }
    let bp = (high.saturating_mul(10_000)) / max;
    match (report.mode, bp) {
        (CapacityMode::Tuning, b) if b < 2_500 => "tuning cap is loose; freeze near 2x high water",
        (CapacityMode::Tuning, b) if b < 7_500 => "tuning cap fits; freeze near 1.5x high water",
        (CapacityMode::Tuning, _) => "tuning cap is tight; raise then re-measure",
        (CapacityMode::Fixed, b) if b < 2_500 => "fixed cap is loose; consider shrinking",
        (CapacityMode::Fixed, b) if b < 8_500 => "fixed cap fits",
        (CapacityMode::Fixed, _) => "fixed cap is tight; consider raising",
    }
}

/// One-line discovery line per surface.
///
/// ```text
/// pool.1.waiters       fixed   max=4    cur=0   high=4   full=2   suggest=saw Full — raise cap or shed earlier
/// ```
pub fn format_discovery_line(report: &CapacitySurfaceReport) -> String {
    let max = match report.max_messages {
        Some(m) => m.to_string(),
        None => "-".to_string(),
    };
    format!(
        "{name:<24} {mode:<7} max={max:<6} cur={cur:<5} high={high:<5} full={full:<4} suggest={hint}",
        name = report.name,
        mode = report.mode.label(),
        max = max,
        cur = report.current_messages,
        high = report.high_water_messages,
        full = report.full_count,
        hint = suggest_next(report),
    )
}

/// Render a full discovery report (one line per surface).
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
            CapacityAssertError::Full { observed, .. } => assert_eq!(observed, 3),
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
    fn discovery_line_emits_dotted_form() {
        let r = report("pool.1.waiters", 4, 0, 4, 2);
        let line = format_discovery_line(&r);
        assert!(line.contains("pool.1.waiters"), "{line}");
        assert!(line.contains("fixed"), "{line}");
        assert!(line.contains("max=4"), "{line}");
        assert!(line.contains("high=4"), "{line}");
        assert!(line.contains("full=2"), "{line}");
        assert!(line.contains("raise cap"), "{line}");
    }

    #[test]
    fn discovery_report_one_line_per_surface() {
        let mut s = CapacitySummary::new();
        s.push(report("a", 4, 0, 0, 0)).unwrap();
        s.push(report("b", 4, 0, 0, 1)).unwrap();
        let report = format_discovery_report(&s);
        let lines: Vec<&str> = report.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("a"), "{lines:?}");
        assert!(lines[1].starts_with("b"), "{lines:?}");
    }

    #[test]
    fn suggest_next_recognizes_tuning_loose() {
        let r = CapacitySurfaceReport::count("a", CapacityMode::Tuning, 100, 0, 5, 0);
        assert!(suggest_next(&r).contains("loose"));
    }

    #[test]
    fn suggest_next_recognizes_full_pressure() {
        let r = CapacitySurfaceReport::count("a", CapacityMode::Fixed, 4, 0, 4, 1);
        assert!(suggest_next(&r).contains("Full"));
    }
}
