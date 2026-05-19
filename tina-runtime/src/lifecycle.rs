//! Typed service lifecycle vocabulary.
//!
//! Production services need a boring shape: start, become ready, stop
//! admitting, drain, close resources, report what happened, stop. If every
//! service hand-rolls this, every service gets one edge wrong. The types in
//! this module name the shape; the service still owns the choreography.
//!
//! Three families:
//!
//! - [`Lifecycle`] / [`Health`] / [`ReadinessReason`] / [`Readiness`] —
//!   typed words for "what is this service doing right now."
//! - [`ServiceTopology`] / [`TopologyComponent`] / [`ComponentKind`] —
//!   a copyable snapshot of which isolates/pools/bridges/listeners the
//!   service started, with addresses and capacity surfaces.
//! - [`ShutdownChoreography`] / [`ShutdownStep`] / [`ShutdownStepReport`] /
//!   [`ServiceShutdownReport`] / [`ResourceCloseReport`] — an explicit
//!   ordered shutdown recorder. Each step carries kind + label + outcome +
//!   elapsed. Out-of-order steps are recorded with
//!   [`StepOutcome::OrderingViolation`] so a bad sequence is visible, not
//!   hidden.
//!
//! This module is data plus tiny state. It does not run a background task,
//! does not own any resource, and does not register itself anywhere. There
//! is no hidden global registry; each service constructs and threads these
//! reports explicitly. Bridges and pools keep their own typed close
//! reports (`KeepalivePoolShutdownReport`, `BridgeShutdownReport`,
//! `SqliteCloser`); [`ResourceCloseReport`] is the small uniform wrapper so
//! every service can build one terminal report without giving up the
//! resource-specific facts.

use std::fmt;
use std::time::{Duration, Instant};

use crate::service_pressure::ServicePressureReport;

/// Lifecycle state of a service.
///
/// Distinct from process liveness. A process is alive iff it can answer at
/// all. A service is [`Ready`](Lifecycle::Ready) iff it can do useful work
/// for new callers right now.
///
/// Order is intentional: services move forward through this list. Going
/// backward (for example, `Stopped` → `Ready`) is a bug and the
/// [`ShutdownChoreography`] helper rejects it with a typed
/// [`StepOutcome::OrderingViolation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lifecycle {
    /// The service is starting up: not yet accepting useful work.
    Starting,
    /// The service can accept and complete useful work.
    Ready,
    /// The service is up but a named dependency is unhappy. Some routes may
    /// still work; readers should consult [`Readiness::reasons`].
    Degraded,
    /// The service has stopped admitting new work and is settling
    /// already-admitted work.
    Draining,
    /// The service is up but cannot do useful work right now (for example,
    /// a critical dependency closed). Distinct from `Stopped`: traffic may
    /// resume if the dependency recovers.
    NotReady,
    /// The service has emitted its terminal report and stopped.
    Stopped,
}

impl Lifecycle {
    /// Wire-stable lowercase label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Degraded => "degraded",
            Self::Draining => "draining",
            Self::NotReady => "not_ready",
            Self::Stopped => "stopped",
        }
    }

    /// `true` iff the service is willing to accept new work.
    pub const fn admits_new_work(self) -> bool {
        matches!(self, Self::Ready | Self::Degraded)
    }
}

impl fmt::Display for Lifecycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a service is not ready right now.
///
/// Each variant carries a stable wire token via [`Self::as_token`]. The token
/// is what HTTP bodies, RPC replies, and dashboards already use; the typed
/// variants exist so services can pattern-match on the reason instead of
/// comparing strings. Open via [`Self::Custom`] for service-specific reasons
/// the runtime has not seen yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReadinessReason {
    /// The service is still in [`Lifecycle::Starting`].
    Starting,
    /// The service has begun [`Lifecycle::Draining`] and is no longer
    /// accepting new work.
    IngressStopped,
    /// A named dependency is closed.
    DependencyClosed(&'static str),
    /// A named dependency is at capacity.
    DependencyFull(&'static str),
    /// A named dependency timed out on the last probe.
    DependencyTimeout(&'static str),
    /// A named dependency returned an error.
    DependencyError(&'static str),
    /// A service-specific reason. The token is the wire string used in HTTP
    /// bodies / RPC replies. Use only when no typed variant fits.
    Custom(&'static str),
}

impl ReadinessReason {
    /// Stable wire token used in HTTP responses, logs, and dashboards.
    ///
    /// For dependency variants the token is the dependency name plus the
    /// failure word (`db` + `closed` → `db_closed`). Names should be the
    /// short prefix the service uses everywhere (`db`, `outbound`, `cache`).
    pub fn as_token(self) -> ReadinessToken {
        match self {
            Self::Starting => ReadinessToken::Static("starting"),
            Self::IngressStopped => ReadinessToken::Static("ingress_stopped"),
            Self::DependencyClosed(dep) => ReadinessToken::Owned(format!("{dep}_closed")),
            Self::DependencyFull(dep) => ReadinessToken::Owned(format!("{dep}_full")),
            Self::DependencyTimeout(dep) => ReadinessToken::Owned(format!("{dep}_timeout")),
            Self::DependencyError(dep) => ReadinessToken::Owned(format!("{dep}_error")),
            Self::Custom(token) => ReadinessToken::Static(token),
        }
    }
}

/// Owned or static readiness token. Avoids forcing a heap allocation for
/// non-dependency reasons.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ReadinessToken {
    /// A `'static` token used by the typed variants without a name.
    Static(&'static str),
    /// An owned token built from a dependency name plus the failure word.
    Owned(String),
}

impl ReadinessToken {
    /// Borrow the token as a string.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Static(s) => s,
            Self::Owned(s) => s.as_str(),
        }
    }
}

impl fmt::Display for ReadinessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Per-call readiness verdict.
///
/// Holds the typed [`Lifecycle`] the service is in plus the reasons it is
/// not [`Lifecycle::Ready`]. When [`Self::ready`] is `true`, [`Self::reasons`]
/// is empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Readiness {
    /// `true` iff the service can accept and complete useful work now.
    pub ready: bool,
    /// The typed lifecycle state at the time of the probe.
    pub state: Lifecycle,
    /// The reasons the service is not ready, in the order the service
    /// observed them. Empty iff [`Self::ready`] is `true`.
    pub reasons: Vec<ReadinessReason>,
}

/// Fallback wire token used when a caller builds a `not_ready` /
/// `degraded` `Readiness` with an empty reasons list. Keeps the wire body
/// parseable instead of emitting `not_ready reasons=`.
pub const READINESS_UNKNOWN_REASON: ReadinessReason = ReadinessReason::Custom("unknown");

fn ensure_some_reason(mut reasons: Vec<ReadinessReason>) -> Vec<ReadinessReason> {
    if reasons.is_empty() {
        reasons.push(READINESS_UNKNOWN_REASON);
    }
    reasons
}

impl Readiness {
    /// Build a `Ready` verdict for a service in [`Lifecycle::Ready`].
    pub fn ready() -> Self {
        Self {
            ready: true,
            state: Lifecycle::Ready,
            reasons: Vec::new(),
        }
    }

    /// Build a `Ready` verdict but in [`Lifecycle::Degraded`] for a service
    /// that can still admit work despite a dependency being unhappy.
    ///
    /// An empty `reasons` list is folded to a single
    /// [`READINESS_UNKNOWN_REASON`] so the wire body stays parseable. A
    /// service that has no named reason should usually call
    /// [`Self::ready`] instead.
    pub fn degraded(reasons: Vec<ReadinessReason>) -> Self {
        Self {
            ready: true,
            state: Lifecycle::Degraded,
            reasons: ensure_some_reason(reasons),
        }
    }

    /// Build a not-ready verdict with reasons and the lifecycle state that
    /// caused it.
    ///
    /// An empty `reasons` list is folded to a single
    /// [`READINESS_UNKNOWN_REASON`] so the wire body never emits the
    /// ambiguous `not_ready reasons=` shape. Callers should usually
    /// supply at least one typed reason; the fallback exists so a
    /// misuse cannot poison downstream parsers.
    pub fn not_ready(state: Lifecycle, reasons: Vec<ReadinessReason>) -> Self {
        Self {
            ready: false,
            state,
            reasons: ensure_some_reason(reasons),
        }
    }

    /// Comma-joined wire tokens for the reasons. Matches the format used by
    /// existing services (`ingress_stopped,db_closed`).
    pub fn reasons_token_csv(&self) -> String {
        let mut joined = String::new();
        for (idx, reason) in self.reasons.iter().enumerate() {
            if idx > 0 {
                joined.push(',');
            }
            joined.push_str(reason.as_token().as_str());
        }
        joined
    }

    /// Body line for the legacy HTTP `/ready` shape used across Tina
    /// specimens.
    ///
    /// - `self.ready == true`  → `"ready\n"`. Degraded services still
    ///   answer ready on the wire because the HTTP/RPC semantics ask
    ///   "may I send traffic?" — the typed `Lifecycle` and `reasons` on
    ///   this report carry the degradation detail for dashboards.
    /// - `self.ready == false` → `"not_ready reasons=<csv>\n"`. The
    ///   constructors fold an empty reasons list into
    ///   [`READINESS_UNKNOWN_REASON`] so the ambiguous `not_ready
    ///   reasons=` shape is impossible.
    pub fn legacy_body(&self) -> String {
        if self.ready {
            "ready\n".to_owned()
        } else {
            format!("not_ready reasons={}\n", self.reasons_token_csv())
        }
    }
}

/// Compact health report. Pairs the typed lifecycle state with whatever
/// pressure facts the service was holding when the probe ran.
///
/// Liveness is not just "the process answered." It is "the service is in a
/// named state and these are the surfaces that are bounded right now."
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    /// Service label, same one used by [`ServicePressureReport::service`].
    pub service: String,
    /// Current lifecycle state.
    pub state: Lifecycle,
    /// Optional pressure snapshot. `None` means the probe did not sample.
    pub pressure: Option<ServicePressureReport>,
}

impl Health {
    /// Build a health report for a service in `state` with no pressure
    /// snapshot attached.
    pub fn new(service: impl Into<String>, state: Lifecycle) -> Self {
        Self {
            service: service.into(),
            state,
            pressure: None,
        }
    }

    /// Replace the pressure snapshot.
    pub fn with_pressure(mut self, pressure: ServicePressureReport) -> Self {
        self.pressure = Some(pressure);
        self
    }

    /// `health <service> state=<state>` plus a pressure tail when present.
    pub fn summary_line(&self) -> String {
        let head = format!(
            "health service={service} state={state}",
            service = crate::capacity::discovery_value(&self.service),
            state = self.state,
        );
        match &self.pressure {
            Some(p) => format!("{head} | {}", p.summary_line()),
            None => head,
        }
    }
}

// ---------------- topology ----------------

/// Kind of named component in a service topology.
///
/// Open string for forward compatibility — services that add a new component
/// shape do not need to wait on this crate. Suggested values: `listener`,
/// `isolate`, `pool`, `bridge`, `timer`, `child`, `address`.
pub type ComponentKind = &'static str;

/// One named component in a [`ServiceTopology`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopologyComponent {
    /// Dotted name, e.g. `db.bridge`, `outbound.pool`, `http.main_listener`.
    pub name: String,
    /// Kind label, e.g. `listener`, `bridge`, `pool`.
    pub kind: ComponentKind,
    /// Free-text address or short description, e.g. `127.0.0.1:43210` or
    /// `sqlite:///tmp/x.sqlite`. Empty when not applicable.
    pub address: String,
    /// Free-text notes. Stays short and search-friendly.
    pub notes: String,
}

impl TopologyComponent {
    /// Build a topology component with name, kind, and an address string.
    pub fn new(name: impl Into<String>, kind: ComponentKind, address: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind,
            address: address.into(),
            notes: String::new(),
        }
    }

    /// Replace the notes.
    pub fn with_notes(mut self, notes: impl Into<String>) -> Self {
        self.notes = notes.into();
        self
    }

    /// Greppable line: `topology.component name=<name> kind=<kind> address=<address> notes=<notes>`.
    pub fn discovery_line(&self) -> String {
        let mut out = format!(
            "topology.component name={name} kind={kind}",
            name = crate::capacity::discovery_value(&self.name),
            kind = crate::capacity::discovery_value(self.kind),
        );
        if !self.address.is_empty() {
            out.push_str(&format!(
                " address={}",
                crate::capacity::discovery_value(&self.address),
            ));
        }
        if !self.notes.is_empty() {
            out.push_str(&format!(
                " notes={}",
                crate::capacity::discovery_value(&self.notes),
            ));
        }
        out
    }
}

/// Live snapshot of every named resource a service started.
///
/// A topology report names the listeners, isolates, bridges, pools, and
/// addresses the service holds, plus its current [`Lifecycle`] state and the
/// pressure snapshot that backs it. Use this as the "what is running"
/// answer instead of grepping raw traces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceTopology {
    /// Service label.
    pub service: String,
    /// Shard label, when meaningful. Set to `None` for single-shard apps.
    pub shard: Option<String>,
    /// Current lifecycle state at the time the report was built.
    pub state: Lifecycle,
    /// Named components in registration order.
    pub components: Vec<TopologyComponent>,
    /// Bounded surfaces. Reuses [`ServicePressureReport`] so capacity facts
    /// are the same vocabulary in topology, health, and `/debug/capacity`.
    pub pressure: ServicePressureReport,
}

impl ServiceTopology {
    /// Build an empty topology for `service`. Add components and the
    /// pressure snapshot before exposing.
    pub fn new(service: impl Into<String>, state: Lifecycle) -> Self {
        let service = service.into();
        let pressure = ServicePressureReport::new(service.clone());
        Self {
            service,
            shard: None,
            state,
            components: Vec::new(),
            pressure,
        }
    }

    /// Set the shard label.
    pub fn with_shard(mut self, shard: impl Into<String>) -> Self {
        self.shard = Some(shard.into());
        self
    }

    /// Append one component.
    pub fn push_component(&mut self, component: TopologyComponent) -> &mut Self {
        self.components.push(component);
        self
    }

    /// Replace the pressure snapshot.
    pub fn with_pressure(mut self, pressure: ServicePressureReport) -> Self {
        self.pressure = pressure;
        self
    }

    /// Compact one-line head: `topology service=<svc> state=<state>
    /// shard=<shard> components=<n>` followed by the pressure summary line.
    pub fn summary_line(&self) -> String {
        let shard = match &self.shard {
            Some(s) => format!(" shard={}", crate::capacity::discovery_value(s)),
            None => String::new(),
        };
        format!(
            "topology service={service} state={state}{shard} components={n} | {pressure}",
            service = crate::capacity::discovery_value(&self.service),
            state = self.state,
            n = self.components.len(),
            pressure = self.pressure.summary_line(),
        )
    }

    /// One discovery line per component, then per pressure surface.
    pub fn discovery_lines(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.components.len() + self.pressure.surfaces.len());
        for c in &self.components {
            out.push(c.discovery_line());
        }
        for s in &self.pressure.surfaces {
            out.push(s.discovery_line());
        }
        out
    }
}

// ---------------- shutdown choreography ----------------

/// Ordered step kinds in a service shutdown.
///
/// The integer order matters: [`ShutdownChoreography::record`] uses it to
/// flag out-of-order steps with [`StepOutcome::OrderingViolation`]. Services
/// can skip kinds (no batchers? skip `FlushBatchers`); they must not go
/// backward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShutdownStep {
    /// Stop accepting new ingress. New callers see `IngressStopped`.
    StopIngress,
    /// Cancel or close per-session state (subscriptions, WebSocket peers).
    CancelSessions,
    /// Wait for in-flight work to settle or report a typed timeout.
    DrainInFlight,
    /// Flush batchers and any other coalesced work.
    FlushBatchers,
    /// Close a named resource (pool, bridge, listener).
    CloseResource,
    /// Emit the terminal report.
    EmitReport,
    /// Stop the runtime owner.
    StopOwner,
}

impl ShutdownStep {
    /// Wire-stable lowercase label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StopIngress => "stop_ingress",
            Self::CancelSessions => "cancel_sessions",
            Self::DrainInFlight => "drain_in_flight",
            Self::FlushBatchers => "flush_batchers",
            Self::CloseResource => "close_resource",
            Self::EmitReport => "emit_report",
            Self::StopOwner => "stop_owner",
        }
    }

    const fn ordinal(self) -> u8 {
        match self {
            Self::StopIngress => 0,
            Self::CancelSessions => 1,
            Self::DrainInFlight => 2,
            Self::FlushBatchers => 3,
            Self::CloseResource => 4,
            Self::EmitReport => 5,
            Self::StopOwner => 6,
        }
    }
}

impl fmt::Display for ShutdownStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Outcome of one recorded shutdown step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    /// The step completed cleanly.
    Clean,
    /// The step's per-step timeout fired before completion. The waited
    /// duration is recorded for context.
    Timeout {
        /// How long the step waited before the timeout fired.
        waited: Duration,
    },
    /// The step failed with a typed reason supplied by the service.
    Failed {
        /// Short reason; goes straight into the discovery line.
        reason: String,
    },
    /// The step was recorded out of order. Includes the ordinal that was
    /// expected next (the highest ordinal recorded so far) so callers can
    /// see how far ahead this step landed.
    OrderingViolation {
        /// Ordinal of the step recorded just before this one.
        previous_step: ShutdownStep,
    },
    /// The step was recorded but the resource was already in the target
    /// state (already closed, already drained, etc.). Visible but not
    /// failure.
    AlreadyDone,
}

impl StepOutcome {
    /// `true` iff this outcome is a clean / already-done success.
    pub const fn is_clean(&self) -> bool {
        matches!(self, Self::Clean | Self::AlreadyDone)
    }

    /// Short wire label for the variant, without payload.
    pub const fn kind_str(&self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Timeout { .. } => "timeout",
            Self::Failed { .. } => "failed",
            Self::OrderingViolation { .. } => "ordering_violation",
            Self::AlreadyDone => "already_done",
        }
    }
}

/// One recorded shutdown step in a [`ShutdownChoreography`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownStepReport {
    /// Kind of step recorded.
    pub step: ShutdownStep,
    /// Human label, e.g. `"close_sqlite_bridge"`. Short and dotted.
    pub label: &'static str,
    /// How long the step took.
    pub elapsed: Duration,
    /// What happened.
    pub outcome: StepOutcome,
}

impl ShutdownStepReport {
    /// Greppable line for one step.
    pub fn discovery_line(&self) -> String {
        let mut out = format!(
            "shutdown.step step={step} label={label} elapsed_us={us} outcome={kind}",
            step = self.step,
            label = crate::capacity::discovery_value(self.label),
            us = self.elapsed.as_micros(),
            kind = self.outcome.kind_str(),
        );
        match &self.outcome {
            StepOutcome::Timeout { waited } => {
                out.push_str(&format!(" waited_us={}", waited.as_micros()));
            }
            StepOutcome::Failed { reason } => {
                out.push_str(&format!(
                    " reason={}",
                    crate::capacity::discovery_value(reason),
                ));
            }
            StepOutcome::OrderingViolation { previous_step } => {
                out.push_str(&format!(" previous_step={previous_step}"));
            }
            StepOutcome::Clean | StepOutcome::AlreadyDone => {}
        }
        out
    }
}

/// Kind of resource a [`ResourceCloseReport`] wraps.
///
/// Open via [`Self::Custom`] for service-specific resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ResourceKind {
    /// A bounded pool (database, outbound HTTP, etc.).
    Pool,
    /// A bridge worker (SQLite, S3, SQS, reqwest, etc.).
    Bridge,
    /// An accept-side listener (HTTP, gRPC, RPC, Unix).
    Listener,
    /// A single connection or session.
    Connection,
    /// A streaming body (request, response).
    BodyStream,
    /// A spawned child isolate.
    Child,
    /// A periodic timer or recurring tick.
    Timer,
    /// Anything else; use the wire label of [`ResourceCloseReport`].
    Custom(&'static str),
}

impl ResourceKind {
    /// Wire-stable lowercase label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pool => "pool",
            Self::Bridge => "bridge",
            Self::Listener => "listener",
            Self::Connection => "connection",
            Self::BodyStream => "body_stream",
            Self::Child => "child",
            Self::Timer => "timer",
            Self::Custom(label) => label,
        }
    }
}

impl fmt::Display for ResourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Typed close report for one resource. Wraps the resource-specific report
/// (e.g. `KeepalivePoolShutdownReport`, `BridgeShutdownReport`) in a small
/// shared vocabulary so every service can build a terminal report without
/// stringly-typed glue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceCloseReport {
    /// Resource name, e.g. `db.bridge`, `outbound.pool`, `main.listener`.
    pub name: String,
    /// Kind of resource.
    pub kind: ResourceKind,
    /// Whether the close drained outstanding work or forced it.
    pub admission: CloseAdmission,
    /// Outcome of the close.
    pub outcome: CloseOutcome,
    /// How long the close took.
    pub elapsed: Duration,
    /// Free-text resource-specific details. Stays one line.
    pub details: String,
}

impl ResourceCloseReport {
    /// Build a clean close report.
    pub fn clean(
        name: impl Into<String>,
        kind: ResourceKind,
        admission: CloseAdmission,
        elapsed: Duration,
    ) -> Self {
        Self {
            name: name.into(),
            kind,
            admission,
            outcome: CloseOutcome::Clean,
            elapsed,
            details: String::new(),
        }
    }

    /// Replace the details one-liner.
    pub fn with_details(mut self, details: impl Into<String>) -> Self {
        self.details = details.into();
        self
    }

    /// Replace the outcome.
    pub fn with_outcome(mut self, outcome: CloseOutcome) -> Self {
        self.outcome = outcome;
        self
    }

    /// Greppable line.
    pub fn discovery_line(&self) -> String {
        let mut out = format!(
            "resource.close name={name} kind={kind} admission={admission} outcome={outcome} elapsed_us={us}",
            name = crate::capacity::discovery_value(&self.name),
            kind = self.kind,
            admission = self.admission,
            outcome = self.outcome.kind_str(),
            us = self.elapsed.as_micros(),
        );
        if let CloseOutcome::Failed { reason } = &self.outcome {
            out.push_str(&format!(
                " reason={}",
                crate::capacity::discovery_value(reason),
            ));
        }
        if let CloseOutcome::TimedOut { waited } = &self.outcome {
            out.push_str(&format!(" waited_us={}", waited.as_micros()));
        }
        if !self.details.is_empty() {
            out.push_str(&format!(
                " details={}",
                crate::capacity::discovery_value(&self.details),
            ));
        }
        out
    }
}

/// How a resource was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CloseAdmission {
    /// In-flight work was drained before close completed.
    Drain,
    /// In-flight work was forced (callers see typed `Closed` outcomes).
    Force,
    /// The resource was never open or admission is not meaningful.
    NotApplicable,
}

impl CloseAdmission {
    /// Wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Drain => "drain",
            Self::Force => "force",
            Self::NotApplicable => "n_a",
        }
    }
}

impl fmt::Display for CloseAdmission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Outcome of a [`ResourceCloseReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseOutcome {
    /// Closed cleanly; no remaining in-flight work.
    Clean,
    /// Close timed out while waiting for drain; some work may not have
    /// completed.
    TimedOut {
        /// How long the close waited before giving up.
        waited: Duration,
    },
    /// The resource was already closed before this call.
    AlreadyClosed,
    /// Close failed with a typed reason.
    Failed {
        /// Short reason; goes straight into the discovery line.
        reason: String,
    },
}

impl CloseOutcome {
    /// `true` iff the close left the resource in a closed state without
    /// failure.
    pub const fn is_clean(&self) -> bool {
        matches!(self, Self::Clean | Self::AlreadyClosed)
    }

    /// Short wire label for the variant.
    pub const fn kind_str(&self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::TimedOut { .. } => "timeout",
            Self::AlreadyClosed => "already_closed",
            Self::Failed { .. } => "failed",
        }
    }
}

/// Explicit shutdown choreography.
///
/// The helper is small: it records steps in order, time-stamps them, and
/// flags out-of-order recordings with [`StepOutcome::OrderingViolation`].
/// It does not run anything; the service still calls `close()` /
/// `shutdown_keepalive_pool()` / `runtime.shutdown()` etc. directly. The
/// helper exists so the resulting report is uniform across services.
///
/// Typical use:
///
/// ```ignore
/// use std::time::{Duration, Instant};
/// use tina_runtime::lifecycle::{
///     CloseAdmission, ResourceCloseReport, ResourceKind, ShutdownChoreography, ShutdownStep,
///     StepOutcome,
/// };
///
/// let mut choreo = ShutdownChoreography::new("mini_saas_api");
/// let t0 = Instant::now();
/// // do close-ingress work
/// choreo.record(ShutdownStep::StopIngress, "close_ingress", t0.elapsed(), StepOutcome::Clean);
///
/// let t1 = Instant::now();
/// // drain in-flight work
/// choreo.record(ShutdownStep::DrainInFlight, "wait_in_flight", t1.elapsed(), StepOutcome::Clean);
///
/// let t2 = Instant::now();
/// // close the outbound pool
/// let close = ResourceCloseReport::clean(
///     "outbound.pool",
///     ResourceKind::Pool,
///     CloseAdmission::Drain,
///     t2.elapsed(),
/// );
/// choreo.record_close(&close, "close_outbound_pool");
///
/// let report = choreo.finish();
/// assert!(report.clean);
/// ```
#[derive(Debug, Clone)]
pub struct ShutdownChoreography {
    service: String,
    steps: Vec<ShutdownStepReport>,
    closes: Vec<ResourceCloseReport>,
    started_at: Instant,
    last_ordinal: Option<u8>,
    last_step: Option<ShutdownStep>,
}

impl ShutdownChoreography {
    /// Build a fresh choreography for `service`. The start time is now.
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            steps: Vec::new(),
            closes: Vec::new(),
            started_at: Instant::now(),
            last_ordinal: None,
            last_step: None,
        }
    }

    /// Build a choreography with an explicit start time. Test-only.
    pub fn with_started_at(service: impl Into<String>, started_at: Instant) -> Self {
        Self {
            service: service.into(),
            steps: Vec::new(),
            closes: Vec::new(),
            started_at,
            last_ordinal: None,
            last_step: None,
        }
    }

    /// Record one step with its outcome.
    ///
    /// If `step` has an ordinal strictly less than the highest previously
    /// recorded ordinal, the supplied `outcome` is **replaced** by
    /// [`StepOutcome::OrderingViolation`] with the previous step
    /// attached, so a backwards recording is visible in the terminal
    /// report rather than silently accepted. The replacement is
    /// destructive: the original `outcome` you passed in is dropped
    /// and not retained anywhere on the choreography. If you need the
    /// resource-specific truth to survive even when the step is
    /// misordered, use [`Self::record_close`], which also retains the
    /// full [`ResourceCloseReport`] in [`Self::closes`].
    pub fn record(
        &mut self,
        step: ShutdownStep,
        label: &'static str,
        elapsed: Duration,
        outcome: StepOutcome,
    ) {
        let final_outcome = match self.last_ordinal {
            Some(last) if step.ordinal() < last => StepOutcome::OrderingViolation {
                previous_step: self
                    .last_step
                    .expect("last_ordinal set implies last_step set"),
            },
            _ => outcome,
        };
        match self.last_ordinal {
            Some(last) if step.ordinal() < last => {}
            _ => {
                self.last_ordinal = Some(
                    self.last_ordinal
                        .map_or(step.ordinal(), |prev| prev.max(step.ordinal())),
                );
                self.last_step = Some(step);
            }
        }
        self.steps.push(ShutdownStepReport {
            step,
            label,
            elapsed,
            outcome: final_outcome,
        });
    }

    /// Record a [`ResourceCloseReport`] as a [`ShutdownStep::CloseResource`]
    /// step. The close report is also retained in [`Self::closes`] so the
    /// terminal report keeps the resource-specific facts.
    pub fn record_close(&mut self, close: &ResourceCloseReport, label: &'static str) {
        let outcome = match &close.outcome {
            CloseOutcome::Clean => StepOutcome::Clean,
            CloseOutcome::AlreadyClosed => StepOutcome::AlreadyDone,
            CloseOutcome::TimedOut { waited } => StepOutcome::Timeout { waited: *waited },
            CloseOutcome::Failed { reason } => StepOutcome::Failed {
                reason: reason.clone(),
            },
        };
        self.record(ShutdownStep::CloseResource, label, close.elapsed, outcome);
        self.closes.push(close.clone());
    }

    /// Borrow the recorded step reports.
    pub fn steps(&self) -> &[ShutdownStepReport] {
        &self.steps
    }

    /// Borrow the recorded close reports.
    pub fn closes(&self) -> &[ResourceCloseReport] {
        &self.closes
    }

    /// Finalize and emit a [`ServiceShutdownReport`].
    pub fn finish(self) -> ServiceShutdownReport {
        let total_elapsed = self.started_at.elapsed();
        let clean = self.steps.iter().all(|s| s.outcome.is_clean());
        ServiceShutdownReport {
            service: self.service,
            steps: self.steps,
            closes: self.closes,
            total_elapsed,
            clean,
        }
    }
}

/// Terminal report from a [`ShutdownChoreography`].
///
/// Captures every recorded step, every resource close report, the total
/// elapsed time, and a cheap `clean` flag. The clean flag is true iff every
/// step's outcome is `Clean` or `AlreadyDone`. Out-of-order steps,
/// timeouts, and failures all flip `clean` to false.
///
/// `PartialEq` includes [`Self::total_elapsed`] and per-step `elapsed`
/// durations. Two reports built from real wall-clock measurements will
/// never compare equal even with the same step sequence; tests that need
/// equality-based DST should construct steps with explicit zero durations
/// and use [`ShutdownChoreography::with_started_at`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceShutdownReport {
    /// Service label.
    pub service: String,
    /// Steps in the order they were recorded.
    pub steps: Vec<ShutdownStepReport>,
    /// Recorded resource close reports.
    pub closes: Vec<ResourceCloseReport>,
    /// Total wall-clock elapsed since the choreography started.
    pub total_elapsed: Duration,
    /// `true` iff every step was clean or already done.
    pub clean: bool,
}

impl ServiceShutdownReport {
    /// Compact one-line summary: `shutdown service=<svc> clean=<bool>
    /// steps=<n> closes=<n> elapsed_us=<u> violations=<n>`.
    pub fn summary_line(&self) -> String {
        let violations = self
            .steps
            .iter()
            .filter(|s| matches!(s.outcome, StepOutcome::OrderingViolation { .. }))
            .count();
        let timeouts = self
            .steps
            .iter()
            .filter(|s| matches!(s.outcome, StepOutcome::Timeout { .. }))
            .count();
        let failures = self
            .steps
            .iter()
            .filter(|s| matches!(s.outcome, StepOutcome::Failed { .. }))
            .count();
        format!(
            "shutdown service={service} clean={clean} steps={steps} closes={closes} \
             elapsed_us={us} violations={violations} timeouts={timeouts} failures={failures}",
            service = crate::capacity::discovery_value(&self.service),
            clean = self.clean,
            steps = self.steps.len(),
            closes = self.closes.len(),
            us = self.total_elapsed.as_micros(),
        )
    }

    /// One discovery line per recorded step and close report.
    pub fn discovery_lines(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.steps.len() + self.closes.len());
        for s in &self.steps {
            out.push(s.discovery_line());
        }
        for c in &self.closes {
            out.push(c.discovery_line());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_admits_new_work_only_when_ready_or_degraded() {
        assert!(!Lifecycle::Starting.admits_new_work());
        assert!(Lifecycle::Ready.admits_new_work());
        assert!(Lifecycle::Degraded.admits_new_work());
        assert!(!Lifecycle::Draining.admits_new_work());
        assert!(!Lifecycle::NotReady.admits_new_work());
        assert!(!Lifecycle::Stopped.admits_new_work());
    }

    #[test]
    fn readiness_legacy_body_matches_existing_wire_format() {
        let r = Readiness::ready();
        assert_eq!(r.legacy_body(), "ready\n");

        let r = Readiness::not_ready(Lifecycle::Draining, vec![ReadinessReason::IngressStopped]);
        assert_eq!(r.legacy_body(), "not_ready reasons=ingress_stopped\n");

        let r = Readiness::not_ready(
            Lifecycle::NotReady,
            vec![
                ReadinessReason::IngressStopped,
                ReadinessReason::DependencyClosed("db"),
            ],
        );
        assert_eq!(
            r.legacy_body(),
            "not_ready reasons=ingress_stopped,db_closed\n"
        );
    }

    #[test]
    fn readiness_not_ready_with_empty_reasons_falls_back_to_unknown_token() {
        // Release-mode safety: an empty reasons list MUST NOT produce
        // the ambiguous `not_ready reasons=` wire body. The fallback
        // token keeps downstream parsers honest.
        let r = Readiness::not_ready(Lifecycle::NotReady, vec![]);
        assert!(!r.ready);
        assert_eq!(r.reasons.len(), 1);
        assert_eq!(r.reasons[0], READINESS_UNKNOWN_REASON);
        assert_eq!(r.legacy_body(), "not_ready reasons=unknown\n");
    }

    #[test]
    fn readiness_degraded_with_empty_reasons_still_says_ready_on_wire() {
        // Degraded HTTP semantics: clients may still send traffic; the
        // typed report carries the state. An empty reasons list folds
        // to `unknown` so the typed report stays parseable, but the
        // wire body remains `ready\n` so existing HTTP clients do not
        // start rejecting a perfectly serviceable upstream.
        let r = Readiness::degraded(vec![]);
        assert!(r.ready);
        assert_eq!(r.state, Lifecycle::Degraded);
        assert_eq!(r.reasons, vec![READINESS_UNKNOWN_REASON]);
        assert_eq!(r.legacy_body(), "ready\n");
    }

    #[test]
    fn readiness_degraded_with_reasons_still_says_ready_on_wire() {
        let r = Readiness::degraded(vec![ReadinessReason::DependencyFull("cache")]);
        assert!(r.ready);
        assert_eq!(r.legacy_body(), "ready\n");
        assert_eq!(r.reasons_token_csv(), "cache_full");
    }

    #[test]
    fn readiness_dependency_tokens_compose_dep_plus_word() {
        assert_eq!(
            ReadinessReason::DependencyClosed("outbound")
                .as_token()
                .as_str(),
            "outbound_closed",
        );
        assert_eq!(
            ReadinessReason::DependencyFull("db").as_token().as_str(),
            "db_full",
        );
        assert_eq!(
            ReadinessReason::DependencyTimeout("cache")
                .as_token()
                .as_str(),
            "cache_timeout",
        );
        assert_eq!(
            ReadinessReason::DependencyError("bridge")
                .as_token()
                .as_str(),
            "bridge_error",
        );
        assert_eq!(
            ReadinessReason::Custom("upstream_quota")
                .as_token()
                .as_str(),
            "upstream_quota",
        );
    }

    #[test]
    fn choreography_flags_out_of_order_recording() {
        let mut choreo = ShutdownChoreography::new("svc");
        choreo.record(
            ShutdownStep::DrainInFlight,
            "drain",
            Duration::from_millis(1),
            StepOutcome::Clean,
        );
        // StopIngress has ordinal 0, less than DrainInFlight (2). The
        // service's Clean is overwritten with OrderingViolation.
        choreo.record(
            ShutdownStep::StopIngress,
            "stop_ingress_late",
            Duration::from_millis(1),
            StepOutcome::Clean,
        );
        let report = choreo.finish();
        assert_eq!(report.steps.len(), 2);
        assert!(matches!(report.steps[0].outcome, StepOutcome::Clean));
        let violation = &report.steps[1].outcome;
        match violation {
            StepOutcome::OrderingViolation { previous_step } => {
                assert_eq!(*previous_step, ShutdownStep::DrainInFlight);
            }
            other => panic!("expected OrderingViolation, got {other:?}"),
        }
        assert!(!report.clean, "ordering violation makes the report unclean");
        let summary = report.summary_line();
        assert!(summary.contains("violations=1"), "{summary}");
        assert!(summary.contains("clean=false"), "{summary}");
    }

    #[test]
    fn choreography_ordering_violation_reports_highest_completed_step() {
        let mut choreo = ShutdownChoreography::new("svc");
        choreo.record(
            ShutdownStep::DrainInFlight,
            "drain",
            Duration::from_millis(1),
            StepOutcome::Clean,
        );
        choreo.record(
            ShutdownStep::StopIngress,
            "stop_late",
            Duration::from_millis(1),
            StepOutcome::Clean,
        );
        choreo.record(
            ShutdownStep::CancelSessions,
            "cancel_late",
            Duration::from_millis(1),
            StepOutcome::Clean,
        );

        let report = choreo.finish();
        match &report.steps[2].outcome {
            StepOutcome::OrderingViolation { previous_step } => {
                assert_eq!(*previous_step, ShutdownStep::DrainInFlight);
            }
            other => panic!("expected OrderingViolation, got {other:?}"),
        }
    }

    #[test]
    fn choreography_records_close_report_as_close_resource_step() {
        let mut choreo = ShutdownChoreography::new("svc");
        let close = ResourceCloseReport::clean(
            "outbound.pool",
            ResourceKind::Pool,
            CloseAdmission::Drain,
            Duration::from_millis(2),
        )
        .with_details("requested=1 stopped=1");
        choreo.record_close(&close, "close_outbound_pool");
        let report = choreo.finish();
        assert_eq!(report.steps.len(), 1);
        assert_eq!(report.steps[0].step, ShutdownStep::CloseResource);
        assert_eq!(report.steps[0].label, "close_outbound_pool");
        assert!(matches!(report.steps[0].outcome, StepOutcome::Clean));
        assert_eq!(report.closes.len(), 1);
        assert_eq!(report.closes[0].name, "outbound.pool");
        assert!(report.clean);
    }

    #[test]
    fn close_report_with_timeout_propagates_into_step_outcome() {
        let mut choreo = ShutdownChoreography::new("svc");
        let close = ResourceCloseReport::clean(
            "db.bridge",
            ResourceKind::Bridge,
            CloseAdmission::Drain,
            Duration::from_millis(5),
        )
        .with_outcome(CloseOutcome::TimedOut {
            waited: Duration::from_millis(5),
        });
        choreo.record_close(&close, "close_db_bridge");
        let report = choreo.finish();
        assert!(!report.clean);
        match &report.steps[0].outcome {
            StepOutcome::Timeout { waited } => {
                assert_eq!(*waited, Duration::from_millis(5));
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[test]
    fn topology_summary_line_includes_state_and_pressure_head() {
        let topology = ServiceTopology::new("svc", Lifecycle::Ready);
        let line = topology.summary_line();
        assert!(line.contains("service=svc"), "{line}");
        assert!(line.contains("state=ready"), "{line}");
        assert!(line.contains("components=0"), "{line}");
        assert!(line.contains("| service=svc"), "{line}");
    }

    #[test]
    fn recording_same_step_kind_repeatedly_is_not_an_ordering_violation() {
        // Real services close multiple resources back-to-back (db,
        // pool, listeners, ...). Each lands as a `CloseResource` step.
        // Same-ordinal repetition must not trigger
        // `OrderingViolation`; the helper uses `max(prev, current)` so
        // equal ordinals are accepted.
        let mut choreo = ShutdownChoreography::new("svc");
        for label in ["close_a", "close_b", "close_c", "close_d"] {
            choreo.record(
                ShutdownStep::CloseResource,
                label,
                Duration::from_millis(0),
                StepOutcome::Clean,
            );
        }
        // Adding a higher-ordinal step after a chain of equal-ordinal
        // CloseResource records is still clean.
        choreo.record(
            ShutdownStep::StopOwner,
            "stop_runtime",
            Duration::from_millis(0),
            StepOutcome::Clean,
        );
        let report = choreo.finish();
        assert!(report.clean, "{report:#?}");
        assert_eq!(report.steps.len(), 5);
        assert!(
            report
                .steps
                .iter()
                .all(|s| matches!(s.outcome, StepOutcome::Clean))
        );
        // Now a backwards step still violates.
        let mut choreo = ShutdownChoreography::new("svc");
        choreo.record(
            ShutdownStep::CloseResource,
            "a",
            Duration::from_millis(0),
            StepOutcome::Clean,
        );
        choreo.record(
            ShutdownStep::CloseResource,
            "b",
            Duration::from_millis(0),
            StepOutcome::Clean,
        );
        choreo.record(
            ShutdownStep::StopIngress,
            "backwards",
            Duration::from_millis(0),
            StepOutcome::Clean,
        );
        let report = choreo.finish();
        assert!(!report.clean);
        match &report.steps[2].outcome {
            StepOutcome::OrderingViolation { previous_step } => {
                assert_eq!(*previous_step, ShutdownStep::CloseResource);
            }
            other => panic!("expected OrderingViolation, got {other:?}"),
        }
    }

    #[test]
    fn record_close_propagates_already_closed_and_failed_outcomes() {
        // Coverage for the close-outcome → step-outcome translation
        // table: AlreadyClosed → AlreadyDone (clean), Failed → Failed
        // (unclean), TimedOut → Timeout (unclean). Together these
        // cover every non-Clean variant of CloseOutcome.
        let mut choreo = ShutdownChoreography::new("svc");
        choreo.record_close(
            &ResourceCloseReport::clean(
                "x",
                ResourceKind::Bridge,
                CloseAdmission::NotApplicable,
                Duration::from_millis(0),
            )
            .with_outcome(CloseOutcome::AlreadyClosed),
            "close_already_closed",
        );
        choreo.record_close(
            &ResourceCloseReport::clean(
                "y",
                ResourceKind::Pool,
                CloseAdmission::Force,
                Duration::from_millis(0),
            )
            .with_outcome(CloseOutcome::Failed {
                reason: "unexpected reply".to_owned(),
            }),
            "close_failed",
        );
        choreo.record_close(
            &ResourceCloseReport::clean(
                "z",
                ResourceKind::Listener,
                CloseAdmission::Drain,
                Duration::from_millis(5),
            )
            .with_outcome(CloseOutcome::TimedOut {
                waited: Duration::from_millis(5),
            }),
            "close_timed_out",
        );
        let report = choreo.finish();
        assert!(!report.clean, "Failed + TimedOut must flip clean to false");
        assert!(matches!(report.steps[0].outcome, StepOutcome::AlreadyDone));
        match &report.steps[1].outcome {
            StepOutcome::Failed { reason } => assert!(reason.contains("unexpected reply")),
            other => panic!("expected Failed, got {other:?}"),
        }
        match report.steps[2].outcome {
            StepOutcome::Timeout { waited } => {
                assert_eq!(waited, Duration::from_millis(5));
            }
            ref other => panic!("expected Timeout, got {other:?}"),
        }
        // Summary line correctness for mixed outcomes.
        let summary = report.summary_line();
        assert!(summary.contains("clean=false"));
        assert!(summary.contains("timeouts=1"));
        assert!(summary.contains("failures=1"));
        assert!(summary.contains("closes=3"));
    }

    #[test]
    fn stuck_child_close_produces_typed_timeout_in_terminal_report() {
        // Integration-shaped scenario: a service drives the canonical
        // shutdown sequence and one resource (the outbound pool here)
        // is stuck. The plan's "shutdown with stuck child returns a
        // timeout report" requirement is met by:
        //
        //   * `ServiceShutdownReport::clean` is `false`
        //   * exactly one step has `StepOutcome::Timeout`
        //   * the corresponding `ResourceCloseReport` is retained in
        //     `closes` with `CloseOutcome::TimedOut`
        //   * the summary line surfaces `timeouts=1`
        //   * the discovery lines name the stuck resource and its
        //     `waited_us` so on-call can act without log scraping.
        let mut choreo = ShutdownChoreography::new("stuck-svc");
        choreo.record(
            ShutdownStep::StopIngress,
            "close_ingress",
            Duration::from_millis(0),
            StepOutcome::Clean,
        );
        choreo.record(
            ShutdownStep::DrainInFlight,
            "wait_in_flight",
            Duration::from_millis(0),
            StepOutcome::Clean,
        );
        choreo.record_close(
            &ResourceCloseReport::clean(
                "db.bridge",
                ResourceKind::Bridge,
                CloseAdmission::Drain,
                Duration::from_millis(0),
            ),
            "close_db_bridge",
        );
        let waited = Duration::from_millis(2_000);
        let stuck = ResourceCloseReport::clean(
            "outbound.pool",
            ResourceKind::Pool,
            CloseAdmission::Drain,
            waited,
        )
        .with_outcome(CloseOutcome::TimedOut { waited })
        .with_details("drain=TimedOut requested=1 stopped=0");
        choreo.record_close(&stuck, "close_outbound_pool");
        choreo.record_close(
            &ResourceCloseReport::clean(
                "main.listener",
                ResourceKind::Listener,
                CloseAdmission::Drain,
                Duration::from_millis(0),
            ),
            "stop_main_listener",
        );
        choreo.record(
            ShutdownStep::StopOwner,
            "shutdown_runtime",
            Duration::from_millis(0),
            StepOutcome::Clean,
        );

        let report = choreo.finish();
        assert!(
            !report.clean,
            "a stuck-child timeout must flip the terminal clean flag",
        );

        let timeouts: Vec<&ShutdownStepReport> = report
            .steps
            .iter()
            .filter(|s| matches!(s.outcome, StepOutcome::Timeout { .. }))
            .collect();
        assert_eq!(
            timeouts.len(),
            1,
            "expected exactly one Timeout step, got {timeouts:#?}",
        );
        let timeout_step = timeouts[0];
        assert_eq!(timeout_step.step, ShutdownStep::CloseResource);
        assert_eq!(timeout_step.label, "close_outbound_pool");
        match timeout_step.outcome {
            StepOutcome::Timeout { waited: w } => assert_eq!(w, waited),
            ref other => panic!("expected Timeout, got {other:?}"),
        }

        let stuck_close = report
            .closes
            .iter()
            .find(|c| c.name == "outbound.pool")
            .expect("stuck close report retained");
        assert!(matches!(stuck_close.outcome, CloseOutcome::TimedOut { .. }));
        assert!(stuck_close.details.contains("drain=TimedOut"));

        let summary = report.summary_line();
        assert!(summary.contains("clean=false"), "{summary}");
        assert!(summary.contains("timeouts=1"), "{summary}");

        let lines = report.discovery_lines();
        assert!(
            lines.iter().any(|l| l.contains("label=close_outbound_pool")
                && l.contains("outcome=timeout")
                && l.contains("waited_us=")),
            "stuck step missing from discovery: {lines:#?}",
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("name=outbound.pool") && l.contains("outcome=timeout")),
            "stuck close missing from discovery: {lines:#?}",
        );
    }

    #[test]
    fn dst_shutdown_ordering_every_backwards_pair_is_visible() {
        // Deterministic ordering proof. For every ordered pair
        // (first, second) of distinct-ordinal shutdown step kinds,
        // recording first then second must:
        //
        //   - be `Clean` when first.ordinal < second.ordinal
        //   - be `OrderingViolation { previous_step: first }` when
        //     first.ordinal > second.ordinal
        //
        // Equal-ordinal pairs are excluded; same-kind repeats are
        // covered by `recording_same_step_kind_repeatedly_*`.
        //
        // Exhausting every distinct-ordinal pair keeps the helper
        // honest: a future variant added with a misset ordinal would
        // flip exactly one pair from one bucket to the other and fail
        // this test.
        let all = [
            ShutdownStep::StopIngress,
            ShutdownStep::CancelSessions,
            ShutdownStep::DrainInFlight,
            ShutdownStep::FlushBatchers,
            ShutdownStep::CloseResource,
            ShutdownStep::EmitReport,
            ShutdownStep::StopOwner,
        ];
        for &first in &all {
            for &second in &all {
                if first.ordinal() == second.ordinal() {
                    continue;
                }
                let mut choreo = ShutdownChoreography::new("dst-svc");
                choreo.record(first, "first", Duration::from_millis(1), StepOutcome::Clean);
                choreo.record(
                    second,
                    "second",
                    Duration::from_millis(1),
                    StepOutcome::Clean,
                );
                let report = choreo.finish();
                let second_outcome = &report.steps[1].outcome;
                if first.ordinal() < second.ordinal() {
                    assert!(
                        matches!(second_outcome, StepOutcome::Clean),
                        "expected Clean for {first:?}->{second:?}, got {second_outcome:?}",
                    );
                    assert!(report.clean);
                } else {
                    match second_outcome {
                        StepOutcome::OrderingViolation { previous_step } => {
                            assert_eq!(*previous_step, first);
                        }
                        other => panic!(
                            "expected OrderingViolation for {first:?}->{second:?}, got {other:?}",
                        ),
                    }
                    assert!(!report.clean);
                }
            }
        }
    }

    #[test]
    fn dst_shutdown_report_is_deterministic_across_runs() {
        // Same input sequence → byte-identical summary line, discovery
        // lines, and clean flag. Time fields are zeroed so the test is
        // not timing-sensitive.
        fn build() -> ServiceShutdownReport {
            let mut choreo = ShutdownChoreography::with_started_at("dst-svc", Instant::now());
            choreo.record(
                ShutdownStep::StopIngress,
                "stop_ingress",
                Duration::from_millis(0),
                StepOutcome::Clean,
            );
            let close = ResourceCloseReport::clean(
                "db.bridge",
                ResourceKind::Bridge,
                CloseAdmission::Drain,
                Duration::from_millis(0),
            );
            choreo.record_close(&close, "close_db_bridge");
            choreo.record(
                ShutdownStep::StopOwner,
                "stop_runtime",
                Duration::from_millis(0),
                StepOutcome::Clean,
            );
            // Build the report but zero out total_elapsed so the test is
            // wall-clock-independent.
            let mut report = choreo.finish();
            report.total_elapsed = Duration::ZERO;
            report
        }
        let a = build();
        let b = build();
        // Structural equality across runs.
        assert_eq!(a, b);
        // Discovery line text is byte-identical.
        assert_eq!(a.discovery_lines(), b.discovery_lines());
        // Summary line text is byte-identical.
        assert_eq!(a.summary_line(), b.summary_line());
        assert!(a.clean);
    }

    #[test]
    fn health_summary_includes_optional_pressure_tail() {
        let h = Health::new("svc", Lifecycle::Ready);
        let line = h.summary_line();
        assert!(line.starts_with("health service=svc state=ready"));
        assert!(!line.contains('|'));

        let h = Health::new("svc", Lifecycle::Draining)
            .with_pressure(ServicePressureReport::new("svc"));
        let line = h.summary_line();
        assert!(line.contains("state=draining"));
        assert!(line.contains("| service=svc"));
    }
}
