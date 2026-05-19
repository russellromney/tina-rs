//! Service pressure summary.
//!
//! A `ServicePressureReport` is a copyable, grep-friendly aggregation
//! of every bounded surface a service exposes: mailbox, pending
//! replies/calls, pool waiters/leases, bridge in-flight/full/late,
//! body bytes, protocol sessions. Surfaces a service does not
//! measure are reported as [`ServiceSurfaceState::Unavailable`] with a
//! reason, never silently omitted.
//!
//! The shape is convention, not a framework: each service decides
//! which surfaces it can report. Reports plug into the discovery
//! line format used by [`crate::capacity::format_discovery_line`]
//! and into [`crate::capacity::CapacitySummary`] for assertions.
//!
//! ```ignore
//! use tina_runtime::{
//!     ServicePressureReport, ServicePressureSurface, ServiceSurfaceState,
//! };
//! use tina_runtime::capacity::CapacitySummary;
//! use tina::capacity::{CapacityMode, CapacitySurfaceReport};
//!
//! let mut report = ServicePressureReport::new("billing");
//! report.add_surface(ServicePressureSurface::measured(
//!     "billing.mailbox",
//!     "mailbox",
//!     CapacitySurfaceReport::count("billing.mailbox", CapacityMode::Fixed, 32, 0, 6, 0),
//! ));
//! report.add_surface(ServicePressureSurface::unavailable(
//!     "billing.bridge",
//!     "bridge",
//!     "external bridge not registered",
//! ));
//!
//! let line = report.summary_line();
//! assert!(line.contains("service=billing"));
//! assert!(line.contains("billing.bridge=unavailable"));
//!
//! let summary: CapacitySummary = report.capacity_summary().unwrap();
//! assert_eq!(summary.surface("billing.mailbox").report().unwrap().max_messages, Some(32));
//! ```

use tina::capacity::CapacitySurfaceReport;

use crate::capacity::{CapacityNameError, CapacitySummary, format_discovery_line};

/// Kind of bounded surface a service may report. Kinds are open
/// strings rather than an enum so user code can add surfaces this
/// crate has not seen yet without forking.
///
/// Suggested kinds: `mailbox`, `pending_replies`, `pending_calls`,
/// `pool_waiters`, `pool_leases`, `bridge`, `body`, `protocol`,
/// `events`, `scope`.
pub type SurfaceKind = &'static str;

/// One measured surface, or one explicit `Unavailable` reason.
///
/// `Measured` is boxed because `CapacitySurfaceReport` is large
/// (every variant of an enum has the size of its largest one). The
/// state is constructed once per surface, not in hot paths, so the
/// indirection is invisible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceSurfaceState {
    /// Surface is live and reports a [`CapacitySurfaceReport`].
    Measured(Box<CapacitySurfaceReport>),
    /// Surface is not measured by this service. The reason is shown
    /// in the discovery line.
    Unavailable {
        /// Why the surface is not measured. Keep it searchable.
        reason: String,
    },
}

/// One surface in a [`ServicePressureReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServicePressureSurface {
    /// Stable surface name, e.g. `billing.mailbox`. Use dotted form.
    pub name: String,
    /// What kind of surface this is. Free-text for forward
    /// compatibility, see [`SurfaceKind`] suggestions.
    pub kind: SurfaceKind,
    /// State: measured or unavailable.
    pub state: ServiceSurfaceState,
}

impl ServicePressureSurface {
    /// Build a measured surface from a `CapacitySurfaceReport`.
    pub fn measured(
        name: impl Into<String>,
        kind: SurfaceKind,
        report: CapacitySurfaceReport,
    ) -> Self {
        Self {
            name: name.into(),
            kind,
            state: ServiceSurfaceState::Measured(Box::new(report)),
        }
    }

    /// Build an unavailable surface with a reason.
    pub fn unavailable(
        name: impl Into<String>,
        kind: SurfaceKind,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            kind,
            state: ServiceSurfaceState::Unavailable {
                reason: reason.into(),
            },
        }
    }

    /// Discovery line for one surface: capacity line for measured,
    /// `unavailable=reason` line for unavailable.
    ///
    /// User-supplied `name`, `kind`, and `reason` strings are quoted
    /// when they contain whitespace or control characters so the
    /// line stays a parseable `key=value` sequence even for sloppy
    /// inputs.
    pub fn discovery_line(&self) -> String {
        match &self.state {
            ServiceSurfaceState::Measured(report) => format_discovery_line(report.as_ref()),
            ServiceSurfaceState::Unavailable { reason } => {
                format!(
                    "capacity surface={name} kind={kind} state=unavailable reason={reason:?}",
                    name = crate::capacity::discovery_value(&self.name),
                    kind = crate::capacity::discovery_value(self.kind),
                    reason = reason,
                )
            }
        }
    }
}

/// A copied, grep-friendly snapshot of every bounded surface a
/// service exposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServicePressureReport {
    /// Free-text service label, e.g. `billing`, `gateway`, `mini_saas`.
    pub service: String,
    /// Surfaces in registration order.
    pub surfaces: Vec<ServicePressureSurface>,
}

impl ServicePressureReport {
    /// Build an empty report for the named service.
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            surfaces: Vec::new(),
        }
    }

    /// Append one surface. No de-duplication: registration order is
    /// what the discovery output prints. Push the same name twice and
    /// `capacity_summary()` will reject the duplicate at conversion.
    pub fn add_surface(&mut self, surface: ServicePressureSurface) -> &mut Self {
        self.surfaces.push(surface);
        self
    }

    /// Append a measured surface from a report. Convenience over
    /// `add_surface(ServicePressureSurface::measured(...))`.
    pub fn add_measured(&mut self, kind: SurfaceKind, report: CapacitySurfaceReport) -> &mut Self {
        let name = report.name.clone();
        self.surfaces
            .push(ServicePressureSurface::measured(name, kind, report));
        self
    }

    /// Append an unavailable surface with a reason.
    pub fn add_unavailable(
        &mut self,
        name: impl Into<String>,
        kind: SurfaceKind,
        reason: impl Into<String>,
    ) -> &mut Self {
        self.surfaces
            .push(ServicePressureSurface::unavailable(name, kind, reason));
        self
    }

    /// Number of registered surfaces.
    pub fn len(&self) -> usize {
        self.surfaces.len()
    }

    /// True iff no surfaces registered.
    pub fn is_empty(&self) -> bool {
        self.surfaces.is_empty()
    }

    /// Compact summary line: `service=<name> surfaces=N measured=M unavailable=U full=F`.
    ///
    /// `full=` aggregates every `*_full_count` across measured
    /// surfaces. Useful as a one-line CI canary.
    pub fn summary_line(&self) -> String {
        let mut measured = 0usize;
        let mut unavailable = 0usize;
        let mut full: u64 = 0;
        let mut unavail_names: Vec<&str> = Vec::new();
        for s in &self.surfaces {
            match &s.state {
                ServiceSurfaceState::Measured(report) => {
                    measured += 1;
                    full = full
                        .saturating_add(report.full_count)
                        .saturating_add(report.weight_full_count)
                        .saturating_add(report.shared_weight_full_count);
                }
                ServiceSurfaceState::Unavailable { .. } => {
                    unavailable += 1;
                    unavail_names.push(s.name.as_str());
                }
            }
        }
        let mut line = format!(
            "service={service} surfaces={surfaces} measured={measured} unavailable={unavailable} full={full}",
            service = crate::capacity::discovery_value(&self.service),
            surfaces = self.surfaces.len(),
        );
        if !unavail_names.is_empty() {
            // Surface names with whitespace/control chars would
            // break the `key=value` shape if joined naked. The list
            // form (comma-separated) goes through `discovery_value`
            // per name, and the trailing per-surface tokens use the
            // same helper.
            line.push_str(" unavailable_surfaces=");
            let joined = unavail_names
                .iter()
                .map(|n| crate::capacity::discovery_value(n))
                .collect::<Vec<_>>()
                .join(",");
            line.push_str(&joined);
        }
        for s in &self.surfaces {
            if let ServiceSurfaceState::Unavailable { .. } = s.state {
                line.push(' ');
                line.push_str(&crate::capacity::discovery_value(&s.name));
                line.push_str("=unavailable");
            }
        }
        line
    }

    /// One discovery line per surface, joined with newlines.
    pub fn discovery_report(&self) -> String {
        let mut out = String::new();
        for s in &self.surfaces {
            out.push_str(&s.discovery_line());
            out.push('\n');
        }
        out
    }

    /// Builds a `CapacitySummary` from every *measured* surface.
    /// Unavailable surfaces are skipped silently — they have no
    /// numbers to assert against. Duplicate names are rejected by the
    /// underlying [`CapacitySummary::push`].
    pub fn capacity_summary(&self) -> Result<CapacitySummary, CapacityNameError> {
        let mut summary = CapacitySummary::new();
        for s in &self.surfaces {
            if let ServiceSurfaceState::Measured(report) = &s.state {
                summary.push((**report).clone())?;
            }
        }
        Ok(summary)
    }

    /// Iterator over the named unavailable surfaces and their
    /// reasons. Useful for service health pages.
    pub fn unavailable_surfaces(&self) -> impl Iterator<Item = (&str, &str, &str)> {
        self.surfaces.iter().filter_map(|s| match &s.state {
            ServiceSurfaceState::Unavailable { reason } => {
                Some((s.name.as_str(), s.kind, reason.as_str()))
            }
            _ => None,
        })
    }

    /// True iff any measured surface ever hit `Full`. Cheap CI check.
    pub fn any_full(&self) -> bool {
        self.surfaces.iter().any(|s| match &s.state {
            ServiceSurfaceState::Measured(r) => r.ever_full(),
            _ => false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tina::capacity::{CapacityMode, CapacitySurfaceReport};

    fn count(name: &str, max: usize, cur: usize, high: usize, full: u64) -> CapacitySurfaceReport {
        CapacitySurfaceReport::count(name, CapacityMode::Fixed, max, cur, high, full)
    }

    #[test]
    fn summary_line_counts_state() {
        let mut r = ServicePressureReport::new("billing");
        r.add_measured("mailbox", count("billing.mailbox", 32, 0, 5, 0));
        r.add_measured("pool_waiters", count("billing.pool.waiters", 4, 0, 4, 2));
        r.add_unavailable("billing.bridge", "bridge", "not registered yet");
        let line = r.summary_line();
        assert!(line.contains("service=billing"), "{line}");
        assert!(line.contains("surfaces=3"), "{line}");
        assert!(line.contains("measured=2"), "{line}");
        assert!(line.contains("unavailable=1"), "{line}");
        assert!(line.contains("full=2"), "{line}");
        assert!(line.contains("billing.bridge=unavailable"), "{line}");
    }

    #[test]
    fn unavailable_surfaces_are_listed_explicitly() {
        // Critical: Rock 1 says missing surfaces are *explicit*
        // `Unavailable`, never silently omitted.
        let mut r = ServicePressureReport::new("billing");
        r.add_unavailable("billing.bridge", "bridge", "stub");
        let line = r.summary_line();
        assert!(line.contains("billing.bridge=unavailable"));
        let report = r.discovery_report();
        let lines: Vec<&str> = report.lines().collect();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("state=unavailable"));
        assert!(lines[0].contains("reason=\"stub\""));
    }

    #[test]
    fn capacity_summary_includes_only_measured() {
        let mut r = ServicePressureReport::new("billing");
        r.add_measured("mailbox", count("billing.mailbox", 32, 0, 5, 0));
        r.add_unavailable("billing.bridge", "bridge", "stub");
        let summary = r.capacity_summary().unwrap();
        assert_eq!(summary.len(), 1);
        summary.surface("billing.mailbox").no_full().unwrap();
    }

    #[test]
    fn any_full_detects_one_filled_surface() {
        let mut r = ServicePressureReport::new("billing");
        r.add_measured("a", count("a", 4, 0, 4, 0));
        assert!(!r.any_full());
        r.add_measured("b", count("b", 4, 0, 4, 1));
        assert!(r.any_full());
    }

    #[test]
    fn unavailable_surfaces_iter_yields_reason() {
        let mut r = ServicePressureReport::new("svc");
        r.add_unavailable("svc.bridge", "bridge", "stub");
        let found: Vec<_> = r.unavailable_surfaces().collect();
        assert_eq!(found, vec![("svc.bridge", "bridge", "stub")]);
    }

    #[test]
    fn discovery_line_for_unavailable_quotes_unsafe_name_and_kind() {
        // A surface name with whitespace or an empty kind would
        // otherwise produce `surface=name with space kind= state=…`
        // which breaks key=value parsers. The fixed shape double
        // quotes such values.
        let surface = ServicePressureSurface::unavailable("a name", "an kind", "with a reason");
        let line = surface.discovery_line();
        assert!(line.contains("surface=\"a name\""), "{line}");
        assert!(line.contains("kind=\"an kind\""), "{line}");
        assert!(line.contains("state=unavailable"), "{line}");
        assert!(line.contains("reason=\"with a reason\""), "{line}");
    }

    #[test]
    fn summary_line_quotes_unsafe_service_and_surface_names() {
        let mut r = ServicePressureReport::new("svc with space");
        r.add_unavailable("a name", "bridge", "stub");
        let line = r.summary_line();
        assert!(line.contains("service=\"svc with space\""), "{line}");
        assert!(line.contains("unavailable_surfaces=\"a name\""), "{line}");
        assert!(line.contains("\"a name\"=unavailable"), "{line}");
    }
}
