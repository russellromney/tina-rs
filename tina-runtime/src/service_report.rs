//! Service product surface: builder-driven [`ServiceReport`].
//!
//! A service is more than one handler. Real services have routes, bridges,
//! pools, pending work, capacity surfaces, lifecycle, shutdown, and replay
//! facts. The pieces exist across [`crate::lifecycle`],
//! [`crate::service_pressure`], [`crate::capacity`], [`crate::shared_scope`],
//! and the various pool / bridge / body surface reports. This module is the
//! one boring shape that threads them together for one service.
//!
//! Builder-only: [`ServiceReport`] has private fields. The only way to build
//! one is through [`ServiceReportBuilder`]. Building a half-shaped report by
//! struct literal is a coding mistake; the runtime data inside the report
//! still tells the truth when surfaces are full, missing, or unavailable.
//!
//! Service-local: nothing in this module registers anything globally. There
//! is no hidden registry; the service owns its builder and threads the
//! resulting report into its own response surface.
//!
//! ```ignore
//! use tina_runtime::service_pressure::ServicePressureBuilder;
//! use tina_runtime::service_report::{
//!     ServiceReplayStatus, ServiceReportBuilder,
//! };
//! use tina_runtime::lifecycle::{
//!     Health, Lifecycle, Readiness, ServiceTopology,
//! };
//!
//! let pressure = ServicePressureBuilder::new("billing")?
//!     .unavailable("billing.bridge", "bridge", "stub")?
//!     .finish()?;
//!
//! let report = ServiceReportBuilder::new("billing")?
//!     .lifecycle(Lifecycle::Ready)
//!     .readiness(Readiness::ready())
//!     .health(Health::new("billing", Lifecycle::Ready))
//!     .topology(ServiceTopology::new("billing", Lifecycle::Ready))
//!     .pressure(pressure)
//!     .replay(ServiceReplayStatus::not_captured("never enabled"))
//!     .finish()?;
//!
//! assert!(report.summary_line().contains("service=billing"));
//! # Ok::<(), tina_runtime::service_report::ServiceReportBuildError>(())
//! ```

use std::fmt;

use crate::capacity::{CapacityNameError, CapacitySummary, name_is_valid};
use crate::lifecycle::{
    Health, Lifecycle, Readiness, ResourceCloseReport, ServiceShutdownReport, ServiceTopology,
    ShutdownChoreography,
};
use crate::service_pressure::ServicePressureReport;

/// Reasons a builder rejected its input.
///
/// One typed error for the whole surface: callers can match `Missing*` to
/// know which builder method was skipped and `Bad*`/`Duplicate*` to know
/// which surface was rejected at insertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceReportBuildError {
    /// Service name was empty or contained whitespace/control characters.
    BadServiceName,
    /// Surface name was empty or contained whitespace/control characters.
    BadSurfaceName {
        /// The rejected name.
        name: String,
    },
    /// A surface name was already in the report.
    DuplicateSurface {
        /// The duplicated name.
        name: String,
    },
    /// [`ServiceReportBuilder::finish`] called without
    /// [`ServiceReportBuilder::lifecycle`].
    MissingLifecycle,
    /// [`ServiceReportBuilder::finish`] called without
    /// [`ServiceReportBuilder::readiness`].
    MissingReadiness,
    /// [`ServiceReportBuilder::finish`] called without
    /// [`ServiceReportBuilder::health`].
    MissingHealth,
    /// [`ServiceReportBuilder::finish`] called without
    /// [`ServiceReportBuilder::pressure`].
    MissingPressure,
    /// [`ServiceReportBuilder::finish`] called without
    /// [`ServiceReportBuilder::topology`].
    MissingTopology,
}

impl fmt::Display for ServiceReportBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadServiceName => {
                f.write_str("service name is empty or contains whitespace/control characters")
            }
            Self::BadSurfaceName { name } => write!(
                f,
                "surface name {name:?} is empty or contains whitespace/control characters"
            ),
            Self::DuplicateSurface { name } => {
                write!(f, "duplicate surface name {name:?}")
            }
            Self::MissingLifecycle => f.write_str("missing required field: lifecycle"),
            Self::MissingReadiness => f.write_str("missing required field: readiness"),
            Self::MissingHealth => f.write_str("missing required field: health"),
            Self::MissingPressure => f.write_str("missing required field: pressure"),
            Self::MissingTopology => f.write_str("missing required field: topology"),
        }
    }
}

impl std::error::Error for ServiceReportBuildError {}

/// Status of replay-facts capture for a service.
///
/// Replay support is explicit. A service either has captured facts ready to
/// hand off, declares that facts are not captured for a typed reason, or
/// observed unsupported facts during capture. The variant carries the
/// typed evidence so the report says exactly what is and is not replayable.
///
/// This is a plain enum, not a free-text label. Plain-string status would
/// let callers smuggle in `"available"` / `"unavailable"` without naming the
/// case or the reason; the typed variants force the question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceReplayStatus {
    /// Replay facts were captured and a deterministic case is available.
    Available {
        /// Case name a replayer can load. Typically the workload binary or
        /// recorded scenario name.
        case_name: String,
        /// Number of typed events the capture projects into the replayable
        /// case. Useful as a sanity check before handing off.
        projected_events: u64,
    },
    /// Capture saw facts the simulator does not yet replay. The names go in
    /// the report so a future facts phase can target them.
    Unsupported {
        /// Names of unsupported facts in the order observed.
        facts: Vec<String>,
    },
    /// Replay was not captured this run. The reason is part of the report.
    NotCaptured {
        /// Why facts were not captured. Examples: "never enabled",
        /// "capture disabled by config", "ring overflowed".
        reason: String,
    },
}

impl ServiceReplayStatus {
    /// Shorthand for the [`Self::Available`] variant.
    pub fn available(case_name: impl Into<String>, projected_events: u64) -> Self {
        Self::Available {
            case_name: case_name.into(),
            projected_events,
        }
    }

    /// Shorthand for the [`Self::Unsupported`] variant.
    pub fn unsupported<I, S>(facts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Unsupported {
            facts: facts.into_iter().map(Into::into).collect(),
        }
    }

    /// Shorthand for the [`Self::NotCaptured`] variant.
    pub fn not_captured(reason: impl Into<String>) -> Self {
        Self::NotCaptured {
            reason: reason.into(),
        }
    }

    /// Greppable line.
    pub fn summary_line(&self) -> String {
        match self {
            Self::Available {
                case_name,
                projected_events,
            } => format!(
                "replay state=available case={case} projected_events={n}",
                case = crate::capacity::discovery_value(case_name),
                n = projected_events,
            ),
            Self::Unsupported { facts } => {
                let joined = facts
                    .iter()
                    .map(|f| crate::capacity::discovery_value(f))
                    .collect::<Vec<_>>()
                    .join(",");
                format!(
                    "replay state=unsupported facts={count} facts_list={joined}",
                    count = facts.len(),
                )
            }
            Self::NotCaptured { reason } => format!(
                "replay state=not_captured reason={reason}",
                reason = crate::capacity::discovery_value(reason),
            ),
        }
    }
}

/// Validate a service name against the same `key=value` shape rules that
/// pressure surface names use.
pub(crate) fn validate_service_name(name: &str) -> Result<(), ServiceReportBuildError> {
    if !name_is_valid(name) {
        return Err(ServiceReportBuildError::BadServiceName);
    }
    Ok(())
}

/// Validate a surface name and reject duplicates against `existing`.
pub(crate) fn validate_surface_name(
    name: &str,
    existing: &[String],
) -> Result<(), ServiceReportBuildError> {
    if !name_is_valid(name) {
        return Err(ServiceReportBuildError::BadSurfaceName {
            name: name.to_owned(),
        });
    }
    if existing.iter().any(|n| n == name) {
        return Err(ServiceReportBuildError::DuplicateSurface {
            name: name.to_owned(),
        });
    }
    Ok(())
}

/// Service-shaped product report.
///
/// Fields are private. Build through [`ServiceReportBuilder`]; inspect
/// through the accessor methods. Cloning is cheap-shaped: the report holds
/// owned strings, an owned topology, an owned pressure report, and small
/// enums.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceReport {
    service: String,
    lifecycle: Lifecycle,
    readiness: Readiness,
    health: Health,
    topology: ServiceTopology,
    pressure: ServicePressureReport,
    shutdown: Option<ServiceShutdownReport>,
    replay: ServiceReplayStatus,
}

impl ServiceReport {
    /// Service label.
    pub fn service(&self) -> &str {
        &self.service
    }

    /// Lifecycle state at the time the report was built.
    pub const fn lifecycle(&self) -> Lifecycle {
        self.lifecycle
    }

    /// Readiness verdict.
    pub fn readiness(&self) -> &Readiness {
        &self.readiness
    }

    /// Health report (pairs typed lifecycle with current pressure).
    pub fn health(&self) -> &Health {
        &self.health
    }

    /// Topology snapshot: components and the pressure name vocabulary.
    pub fn topology(&self) -> &ServiceTopology {
        &self.topology
    }

    /// Pressure surfaces.
    pub fn pressure(&self) -> &ServicePressureReport {
        &self.pressure
    }

    /// Shutdown report, when the service has finished shutting down.
    pub fn shutdown(&self) -> Option<&ServiceShutdownReport> {
        self.shutdown.as_ref()
    }

    /// Replay-fact status.
    pub fn replay(&self) -> &ServiceReplayStatus {
        &self.replay
    }

    /// Compact one-line head: `service=<svc> lifecycle=<state>
    /// ready=<bool> health=<state> pressure=<surfaces=N> [shutdown=<clean>]
    /// replay=<state>`.
    pub fn summary_line(&self) -> String {
        let mut out = format!(
            "service={service} lifecycle={lifecycle} ready={ready} health={health} \
             pressure={surfaces} surfaces_full={full} unavailable={unavailable}",
            service = crate::capacity::discovery_value(&self.service),
            lifecycle = self.lifecycle,
            ready = self.readiness.ready,
            health = self.health.state,
            surfaces = self.pressure.len(),
            full = self.pressure.any_full(),
            unavailable = self.pressure.unavailable_surfaces().count(),
        );
        if let Some(shutdown) = &self.shutdown {
            out.push_str(&format!(" shutdown=clean:{clean}", clean = shutdown.clean));
        }
        match &self.replay {
            ServiceReplayStatus::Available { .. } => out.push_str(" replay=available"),
            ServiceReplayStatus::Unsupported { .. } => out.push_str(" replay=unsupported"),
            ServiceReplayStatus::NotCaptured { .. } => out.push_str(" replay=not_captured"),
        }
        out
    }

    /// Greppable one-line shutdown summary, or a typed "no shutdown" line
    /// when the report has not folded a shutdown in yet.
    pub fn shutdown_summary_line(&self) -> String {
        match &self.shutdown {
            Some(report) => report.summary_line(),
            None => format!(
                "shutdown service={service} state=not_recorded",
                service = crate::capacity::discovery_value(&self.service),
            ),
        }
    }

    /// All grep-friendly lines, in source order:
    /// 1. summary
    /// 2. readiness wire body (`legacy_body`, trimmed)
    /// 3. health summary
    /// 4. topology summary
    /// 5. topology discovery lines (one per component, then pressure)
    /// 6. pressure summary
    /// 7. pressure discovery lines (one per surface)
    /// 8. replay summary
    /// 9. shutdown summary (or `state=not_recorded`)
    /// 10. shutdown discovery lines, when present
    pub fn discovery_lines(&self) -> Vec<String> {
        let mut out = vec![
            self.summary_line(),
            self.readiness.legacy_body().trim_end().to_owned(),
            self.health.summary_line(),
            self.topology.summary_line(),
        ];
        out.extend(self.topology.discovery_lines());
        out.push(self.pressure.summary_line());
        for s in &self.pressure.surfaces {
            out.push(s.discovery_line());
        }
        out.push(self.replay.summary_line());
        out.push(self.shutdown_summary_line());
        if let Some(shutdown) = &self.shutdown {
            out.extend(shutdown.discovery_lines());
        }
        out
    }

    /// Builds a [`CapacitySummary`] from every measured pressure surface.
    /// Unavailable surfaces are skipped — they have no numbers to assert
    /// against. Duplicate surface names are rejected at builder insertion,
    /// so this conversion succeeds whenever the builder did.
    pub fn capacity_summary(&self) -> Result<CapacitySummary, CapacityNameError> {
        self.pressure.capacity_summary()
    }
}

/// Builder for [`ServiceReport`].
///
/// Required fields: `lifecycle`, `readiness`, `health`, `topology`,
/// `pressure`. Optional fields: `shutdown`, `replay` (defaults to
/// `NotCaptured { reason: "not configured" }` so missing replay support is
/// explicit, not omitted).
///
/// All setters consume and return `self` so calls chain. Validation runs at
/// [`Self::new`] for the service name and at [`Self::finish`] for required
/// fields.
#[derive(Debug, Clone)]
pub struct ServiceReportBuilder {
    service: String,
    lifecycle: Option<Lifecycle>,
    readiness: Option<Readiness>,
    health: Option<Health>,
    topology: Option<ServiceTopology>,
    pressure: Option<ServicePressureReport>,
    shutdown: Option<ServiceShutdownReport>,
    replay: Option<ServiceReplayStatus>,
}

impl ServiceReportBuilder {
    /// Begin a report for `service`. Rejects empty service names or names
    /// with whitespace/control characters.
    pub fn new(service: impl Into<String>) -> Result<Self, ServiceReportBuildError> {
        let service = service.into();
        validate_service_name(&service)?;
        Ok(Self {
            service,
            lifecycle: None,
            readiness: None,
            health: None,
            topology: None,
            pressure: None,
            shutdown: None,
            replay: None,
        })
    }

    /// Set the lifecycle state.
    pub fn lifecycle(mut self, lifecycle: Lifecycle) -> Self {
        self.lifecycle = Some(lifecycle);
        self
    }

    /// Set the readiness verdict.
    pub fn readiness(mut self, readiness: Readiness) -> Self {
        self.readiness = Some(readiness);
        self
    }

    /// Set the health report.
    pub fn health(mut self, health: Health) -> Self {
        self.health = Some(health);
        self
    }

    /// Set the topology snapshot.
    pub fn topology(mut self, topology: ServiceTopology) -> Self {
        self.topology = Some(topology);
        self
    }

    /// Set the pressure report (typically produced by
    /// [`crate::service_pressure::ServicePressureBuilder::finish`]).
    pub fn pressure(mut self, pressure: ServicePressureReport) -> Self {
        self.pressure = Some(pressure);
        self
    }

    /// Fold a finished shutdown report into the service report.
    pub fn shutdown(mut self, shutdown: ServiceShutdownReport) -> Self {
        self.shutdown = Some(shutdown);
        self
    }

    /// Fold a shutdown choreography into the service report by finishing it
    /// here. Convenience over `.shutdown(choreo.finish())` for the common
    /// case where the builder is the next call after the last step is
    /// recorded.
    pub fn shutdown_choreography(self, choreography: ShutdownChoreography) -> Self {
        self.shutdown(choreography.finish())
    }

    /// Add a single resource close report to the shutdown summary.
    ///
    /// If no shutdown has been folded in yet, this opens a fresh
    /// [`ShutdownChoreography`] for the service and records the close. The
    /// resulting report keeps the same step/close vocabulary as a manually
    /// driven choreography so a service can build a terminal shutdown
    /// without inventing its own resource-close glue.
    pub fn resource_close_report(
        mut self,
        label: &'static str,
        close: ResourceCloseReport,
    ) -> Self {
        let existing = self.shutdown.take();
        let mut choreo = ShutdownChoreography::new(self.service.clone());
        if let Some(prev) = &existing {
            for step in &prev.steps {
                choreo.record(step.step, step.label, step.elapsed, step.outcome.clone());
            }
            for prev_close in &prev.closes {
                if !std::ptr::eq(prev_close, &close) {
                    choreo.record_close(prev_close, "carry_forward");
                }
            }
        }
        choreo.record_close(&close, label);
        self.shutdown = Some(choreo.finish());
        self
    }

    /// Set the replay status.
    pub fn replay(mut self, replay: ServiceReplayStatus) -> Self {
        self.replay = Some(replay);
        self
    }

    /// Validate required fields and build the report.
    ///
    /// Returns a typed `Missing*` error when one of the required setters
    /// (`lifecycle`, `readiness`, `health`, `topology`, `pressure`) was not
    /// called.
    pub fn finish(self) -> Result<ServiceReport, ServiceReportBuildError> {
        let lifecycle = self
            .lifecycle
            .ok_or(ServiceReportBuildError::MissingLifecycle)?;
        let readiness = self
            .readiness
            .ok_or(ServiceReportBuildError::MissingReadiness)?;
        let health = self.health.ok_or(ServiceReportBuildError::MissingHealth)?;
        let topology = self
            .topology
            .ok_or(ServiceReportBuildError::MissingTopology)?;
        let pressure = self
            .pressure
            .ok_or(ServiceReportBuildError::MissingPressure)?;
        let replay = self
            .replay
            .unwrap_or_else(|| ServiceReplayStatus::NotCaptured {
                reason: "not configured".to_owned(),
            });
        Ok(ServiceReport {
            service: self.service,
            lifecycle,
            readiness,
            health,
            topology,
            pressure,
            shutdown: self.shutdown,
            replay,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tina::capacity::{CapacityMode, CapacitySurfaceReport};

    use super::*;
    use crate::lifecycle::{
        CloseAdmission, ComponentKind, ResourceKind, ServiceShutdownReport, ShutdownChoreography,
        ShutdownStep, StepOutcome, TopologyComponent,
    };
    use crate::service_pressure::ServicePressureBuilder;

    fn ready_topology() -> ServiceTopology {
        let mut t = ServiceTopology::new("billing", Lifecycle::Ready);
        t.push_component(TopologyComponent::new(
            "billing.main",
            "isolate" as ComponentKind,
            "shard:0",
        ));
        t
    }

    fn ready_pressure() -> ServicePressureReport {
        ServicePressureBuilder::new("billing")
            .unwrap()
            .surface(
                "mailbox",
                CapacitySurfaceReport::count("billing.mailbox", CapacityMode::Fixed, 32, 0, 1, 0),
            )
            .unwrap()
            .finish()
            .unwrap()
    }

    #[test]
    fn finish_requires_all_mandatory_fields() {
        let err = ServiceReportBuilder::new("billing")
            .unwrap()
            .finish()
            .unwrap_err();
        assert_eq!(err, ServiceReportBuildError::MissingLifecycle);

        let err = ServiceReportBuilder::new("billing")
            .unwrap()
            .lifecycle(Lifecycle::Ready)
            .finish()
            .unwrap_err();
        assert_eq!(err, ServiceReportBuildError::MissingReadiness);

        let err = ServiceReportBuilder::new("billing")
            .unwrap()
            .lifecycle(Lifecycle::Ready)
            .readiness(Readiness::ready())
            .finish()
            .unwrap_err();
        assert_eq!(err, ServiceReportBuildError::MissingHealth);

        let err = ServiceReportBuilder::new("billing")
            .unwrap()
            .lifecycle(Lifecycle::Ready)
            .readiness(Readiness::ready())
            .health(Health::new("billing", Lifecycle::Ready))
            .finish()
            .unwrap_err();
        assert_eq!(err, ServiceReportBuildError::MissingTopology);

        let err = ServiceReportBuilder::new("billing")
            .unwrap()
            .lifecycle(Lifecycle::Ready)
            .readiness(Readiness::ready())
            .health(Health::new("billing", Lifecycle::Ready))
            .topology(ready_topology())
            .finish()
            .unwrap_err();
        assert_eq!(err, ServiceReportBuildError::MissingPressure);
    }

    #[test]
    fn bad_service_name_rejected_at_new() {
        let err = ServiceReportBuilder::new("bad name").unwrap_err();
        assert_eq!(err, ServiceReportBuildError::BadServiceName);
        let err = ServiceReportBuilder::new("").unwrap_err();
        assert_eq!(err, ServiceReportBuildError::BadServiceName);
    }

    #[test]
    fn report_names_every_inserted_component() {
        let report = ServiceReportBuilder::new("billing")
            .unwrap()
            .lifecycle(Lifecycle::Ready)
            .readiness(Readiness::ready())
            .health(Health::new("billing", Lifecycle::Ready))
            .topology(ready_topology())
            .pressure(ready_pressure())
            .finish()
            .unwrap();

        let lines = report.discovery_lines();
        assert!(
            lines.iter().any(|l| l.contains("billing.main")),
            "topology component missing: {lines:#?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("billing.mailbox")),
            "pressure surface missing: {lines:#?}"
        );
        assert!(lines.iter().any(|l| l.contains("service=billing")));
    }

    #[test]
    fn readiness_preserves_service_pressure_policy_full() {
        // Builder preserves whatever readiness the service decided, even
        // when its own pressure policy reads "Full" on a surface and the
        // service has chosen to degrade. The builder does not re-derive
        // readiness from pressure.
        let pressure = ServicePressureBuilder::new("billing")
            .unwrap()
            .surface(
                "mailbox",
                CapacitySurfaceReport::count("billing.mailbox", CapacityMode::Fixed, 4, 4, 4, 1),
            )
            .unwrap()
            .finish()
            .unwrap();
        let readiness = Readiness::not_ready(
            Lifecycle::Degraded,
            vec![crate::lifecycle::ReadinessReason::DependencyFull("mailbox")],
        );

        let report = ServiceReportBuilder::new("billing")
            .unwrap()
            .lifecycle(Lifecycle::Degraded)
            .readiness(readiness.clone())
            .health(Health::new("billing", Lifecycle::Degraded))
            .topology(ready_topology())
            .pressure(pressure)
            .finish()
            .unwrap();

        assert!(!report.readiness().ready);
        assert_eq!(report.readiness().state, Lifecycle::Degraded);
    }

    #[test]
    fn readiness_unchanged_when_history_only() {
        // Surface with historical full counts but no current pressure;
        // service still says ready. The builder must not poison that.
        let pressure = ServicePressureBuilder::new("billing")
            .unwrap()
            .surface(
                "mailbox",
                CapacitySurfaceReport::count("billing.mailbox", CapacityMode::Fixed, 4, 0, 4, 1),
            )
            .unwrap()
            .finish()
            .unwrap();

        let report = ServiceReportBuilder::new("billing")
            .unwrap()
            .lifecycle(Lifecycle::Ready)
            .readiness(Readiness::ready())
            .health(Health::new("billing", Lifecycle::Ready))
            .topology(ready_topology())
            .pressure(pressure)
            .finish()
            .unwrap();

        assert!(report.readiness().ready);
    }

    #[test]
    fn shutdown_report_survives_finish() {
        let mut choreo = ShutdownChoreography::new("billing");
        choreo.record(
            ShutdownStep::StopIngress,
            "stop_ingress",
            Duration::from_micros(1),
            StepOutcome::Clean,
        );
        let report = ServiceReportBuilder::new("billing")
            .unwrap()
            .lifecycle(Lifecycle::Stopped)
            .readiness(Readiness::not_ready(
                Lifecycle::Stopped,
                vec![crate::lifecycle::ReadinessReason::IngressStopped],
            ))
            .health(Health::new("billing", Lifecycle::Stopped))
            .topology(ready_topology())
            .pressure(ready_pressure())
            .shutdown_choreography(choreo)
            .finish()
            .unwrap();

        let summary = report.shutdown_summary_line();
        assert!(summary.contains("shutdown service=billing"));
        assert!(summary.contains("steps=1"));
    }

    #[test]
    fn missing_bridge_pressure_shows_unavailable_in_report() {
        let pressure = ServicePressureBuilder::new("billing")
            .unwrap()
            .unavailable("billing.bridge", "bridge", "not registered yet")
            .unwrap()
            .finish()
            .unwrap();

        let report = ServiceReportBuilder::new("billing")
            .unwrap()
            .lifecycle(Lifecycle::Ready)
            .readiness(Readiness::ready())
            .health(Health::new("billing", Lifecycle::Ready))
            .topology(ready_topology())
            .pressure(pressure)
            .finish()
            .unwrap();

        let summary = report.summary_line();
        assert!(summary.contains("unavailable=1"), "{summary}");
        let lines = report.discovery_lines();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("billing.bridge") && l.contains("state=unavailable")),
            "missing bridge unavailable line: {lines:#?}"
        );
    }

    #[test]
    fn shutdown_summary_line_when_not_recorded() {
        let report = ServiceReportBuilder::new("billing")
            .unwrap()
            .lifecycle(Lifecycle::Ready)
            .readiness(Readiness::ready())
            .health(Health::new("billing", Lifecycle::Ready))
            .topology(ready_topology())
            .pressure(ready_pressure())
            .finish()
            .unwrap();
        assert!(
            report
                .shutdown_summary_line()
                .contains("state=not_recorded")
        );
    }

    #[test]
    fn replay_status_renders_each_variant() {
        for status in [
            ServiceReplayStatus::available("case.basic", 42),
            ServiceReplayStatus::unsupported(["websocket.peer_reset", "tls.session_resume"]),
            ServiceReplayStatus::not_captured("never enabled"),
        ] {
            let line = status.summary_line();
            assert!(line.starts_with("replay state="), "{line}");
        }
    }

    #[test]
    fn missing_replay_facts_does_not_fail_health() {
        let report = ServiceReportBuilder::new("billing")
            .unwrap()
            .lifecycle(Lifecycle::Ready)
            .readiness(Readiness::ready())
            .health(Health::new("billing", Lifecycle::Ready))
            .topology(ready_topology())
            .pressure(ready_pressure())
            .replay(ServiceReplayStatus::not_captured("never enabled"))
            .finish()
            .unwrap();
        assert_eq!(report.health().state, Lifecycle::Ready);
        assert!(report.readiness().ready);
    }

    #[test]
    fn resource_close_report_folds_in_when_no_prior_shutdown() {
        let close = ResourceCloseReport::clean(
            "outbound.pool",
            ResourceKind::Pool,
            CloseAdmission::Drain,
            Duration::from_micros(1),
        );
        let report = ServiceReportBuilder::new("billing")
            .unwrap()
            .lifecycle(Lifecycle::Stopped)
            .readiness(Readiness::not_ready(
                Lifecycle::Stopped,
                vec![crate::lifecycle::ReadinessReason::IngressStopped],
            ))
            .health(Health::new("billing", Lifecycle::Stopped))
            .topology(ready_topology())
            .pressure(ready_pressure())
            .resource_close_report("close_outbound_pool", close)
            .finish()
            .unwrap();
        let shutdown: &ServiceShutdownReport =
            report.shutdown().expect("shutdown should be present");
        assert_eq!(shutdown.closes.len(), 1);
        assert_eq!(shutdown.closes[0].name, "outbound.pool");
    }
}
