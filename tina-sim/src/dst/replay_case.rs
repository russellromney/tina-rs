//! Replay-case vocabulary, saved-case format, captured-replay
//! checking, and the live ↔ replay reconciliation helpers.
//!
//! Owns `UnsupportedLiveFact`, `LiveReplayFact`, `CapacityReplayFact`,
//! `LiveReplayCapture`, `SavedReplayCase` and its on-disk format,
//! `CapturedReplayChange`, `LiveReplayReport`, `CapturedReplayMismatch`,
//! `ReplayMismatch`, and the `check_replay_case` / `check_captured_replay`
//! / `observe_replay_case` family.

use std::fmt::Debug;
use std::io;
use std::path::Path;

use tina::capacity::{CapacityMode, CapacitySurfaceReport};
use tina_runtime::{EventId, ProtocolFact, ProtocolFamily, RuntimeEvent};

use super::projection::{
    TraceProjection, TraceProjectionError, TraceShape, project_trace_shape, replay_config_hash,
};
use super::{History, ReplayCase, ReplayConfig, ReplayReport};

/// Whether the trace source was complete enough for exact replay proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceCompleteness {
    /// The capture path is known to have retained every event it saw.
    Complete,
    /// The capture path is missing named shards or dropped events.
    Partial,
    /// The capture reached an explicit event/fact/string bound.
    Truncated,
}

/// Metadata about where a live replay capture came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureSource {
    /// Human-readable source label.
    pub label: String,
    /// Runtime shape, e.g. `threaded`, `local-system`, `sim`.
    pub runtime_kind: String,
    /// Backend shape, e.g. `live`, `sim`, `tokio`.
    pub backend: String,
    /// Capture schema version.
    pub schema_version: u32,
    /// Platform string recorded by the builder.
    pub platform: String,
    /// Optional crate version or application revision.
    pub crate_revision: Option<String>,
    /// Optional git revision when the caller has it.
    pub git_revision: Option<String>,
    /// Whether the trace is complete enough for exact replay proof.
    pub trace_completeness: TraceCompleteness,
}

impl CaptureSource {
    /// Builds default source metadata from a label.
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            runtime_kind: "unknown".to_owned(),
            backend: "live".to_owned(),
            schema_version: 1,
            platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
            crate_revision: option_env!("CARGO_PKG_VERSION").map(str::to_owned),
            git_revision: option_env!("VERGEN_GIT_SHA").map(str::to_owned),
            trace_completeness: TraceCompleteness::Complete,
        }
    }

    /// Sets the runtime kind.
    pub fn runtime_kind(mut self, runtime_kind: impl Into<String>) -> Self {
        self.runtime_kind = runtime_kind.into();
        self
    }

    /// Sets the backend kind.
    pub fn backend(mut self, backend: impl Into<String>) -> Self {
        self.backend = backend.into();
        self
    }

    /// Sets optional git revision metadata.
    pub fn git_revision(mut self, git_revision: impl Into<String>) -> Self {
        self.git_revision = Some(git_revision.into());
        self
    }

    /// Marks trace completeness.
    pub const fn with_trace_completeness(mut self, completeness: TraceCompleteness) -> Self {
        self.trace_completeness = completeness;
        self
    }
}

impl std::fmt::Display for CaptureSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} runtime={} backend={} schema={} platform={} trace={:?}",
            self.label,
            self.runtime_kind,
            self.backend,
            self.schema_version,
            self.platform,
            self.trace_completeness
        )?;
        if let Some(revision) = &self.crate_revision {
            write!(f, " crate={revision}")?;
        }
        if let Some(revision) = &self.git_revision {
            write!(f, " git={revision}")?;
        }
        Ok(())
    }
}

fn encode_saved_bool(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

/// Bounds applied while turning live material into a replay capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureLimits {
    /// Maximum runtime events to hash into the captured shape.
    pub max_events: usize,
    /// Maximum typed replay facts.
    pub max_live_facts: usize,
    /// Maximum unsupported facts.
    pub max_unsupported_facts: usize,
    /// Maximum materialized history operations.
    pub max_history_ops: usize,
    /// Maximum string payload length for source/fact text.
    pub max_string_bytes: usize,
}

impl Default for CaptureLimits {
    fn default() -> Self {
        Self {
            max_events: 16_384,
            max_live_facts: 256,
            max_unsupported_facts: 256,
            max_history_ops: 16_384,
            max_string_bytes: 512,
        }
    }
}

/// Human-sized capture summary suitable for one grep-friendly line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureSummary {
    /// Captured case name.
    pub name: &'static str,
    /// Replay seed.
    pub seed: u64,
    /// Number of materialized operations.
    pub history_len: usize,
    /// Expected projected trace shape.
    pub expected: TraceShape,
    /// Number of typed replay facts.
    pub live_fact_count: usize,
    /// Number of unsupported facts.
    pub unsupported_fact_count: usize,
    /// Number of topology roles.
    pub topology_role_count: usize,
    /// Whether replay is blocked by unsupported/partial/truncated truth.
    pub replay_blocked: bool,
}

impl std::fmt::Display for CaptureSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "capture name={} seed={} ops={} events={} hash=0x{:016x} live_facts={} unsupported={} topology_roles={} replay_blocked={}",
            saved_report_value(self.name),
            self.seed,
            self.history_len,
            self.expected.event_count,
            self.expected.trace_hash,
            self.live_fact_count,
            self.unsupported_fact_count,
            self.topology_role_count,
            self.replay_blocked
        )
    }
}

fn saved_report_value(value: &str) -> String {
    if value.is_empty()
        || value.chars().any(|c| {
            c.is_whitespace()
                || c.is_control()
                || matches!(c, '=' | '[' | ']' | ',' | ':' | '"' | '\\')
        })
    {
        format!("{value:?}")
    } else {
        value.to_owned()
    }
}

/// A live fact the current replay operation alphabet cannot model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedLiveFact {
    /// Short fact name.
    pub fact: String,
    /// Why this fact is not replayable with the current history/config.
    pub reason: String,
}

impl UnsupportedLiveFact {
    /// Creates an unsupported live fact record.
    pub fn new(fact: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            fact: fact.into(),
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for UnsupportedLiveFact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.fact, self.reason)
    }
}

/// A typed live fact that replay must reproduce or reject.
///
/// These facts are deliberately small and explicit. They are recorded from
/// live reports, configs, or materialized app operations; they are not inferred
/// from raw trace text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveReplayFact {
    /// A bounded capacity surface snapshot such as HTTP body bytes or pool
    /// pressure.
    ///
    /// Boxed: a [`CapacityReplayFact`] is far larger than the small typed
    /// [`Protocol`](Self::Protocol) fact, and these facts are cloned and moved
    /// in `Vec`s through every capture, so the big variant stays on the heap.
    CapacitySurface(Box<CapacityReplayFact>),
    /// A typed protocol-layer fact (HTTP/2, HTTP body, WebSocket, gRPC).
    ///
    /// Stored as the typed [`ProtocolFact`] value, never a debug string, so
    /// replay comparison and the stable trace/fact tags stay authoritative.
    Protocol(ProtocolFact),
}

impl LiveReplayFact {
    /// Captures one [`CapacitySurfaceReport`] as replay-comparable data.
    pub fn capacity_surface(report: &CapacitySurfaceReport) -> Self {
        Self::CapacitySurface(Box::new(CapacityReplayFact::from_report(report)))
    }

    /// Wraps one typed [`ProtocolFact`] as a replay-comparable fact.
    pub const fn protocol(fact: ProtocolFact) -> Self {
        Self::Protocol(fact)
    }

    /// Returns the protocol fact when this is a [`Self::Protocol`] fact.
    pub const fn as_protocol(&self) -> Option<ProtocolFact> {
        match self {
            Self::Protocol(fact) => Some(*fact),
            Self::CapacitySurface(_) => None,
        }
    }

    /// Returns the protocol family this fact belongs to, when it is a
    /// protocol fact. Capacity facts return `None`.
    pub fn protocol_family(&self) -> Option<ProtocolFamily> {
        self.as_protocol().map(ProtocolFact::family)
    }
}

impl std::fmt::Display for LiveReplayFact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CapacitySurface(fact) => write!(f, "{fact}"),
            // Display includes the protocol family so a saved-case line names
            // the family without re-deriving it from the debug body.
            Self::Protocol(fact) => write!(f, "protocol {:?} {fact:?}", fact.family()),
        }
    }
}

/// Replay-comparable capacity surface fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacityReplayFact {
    /// Stable surface name.
    pub name: String,
    /// How the cap was chosen.
    pub mode: CapacityMode,
    /// Configured count cap.
    pub max_messages: Option<usize>,
    /// Live count at capture time.
    pub current_messages: usize,
    /// Highest count observed since surface construction.
    pub high_water_messages: usize,
    /// Cumulative count-cap full rejections.
    pub full_count: u64,
    /// Configured weight cap.
    pub max_weight: Option<usize>,
    /// Live weight at capture time.
    pub current_weight: Option<usize>,
    /// Highest weight observed since surface construction.
    pub high_water_weight: Option<usize>,
    /// Cumulative weight-cap full rejections.
    pub weight_full_count: u64,
    /// Unit for weight fields.
    pub weight_unit: Option<String>,
    /// Optional shard-local shared scope name.
    pub shared_scope: Option<String>,
    /// Configured shared-scope weight cap.
    pub shared_max_weight: Option<usize>,
    /// Live shared-scope weight at capture time.
    pub shared_current_weight: Option<usize>,
    /// Highest shared-scope weight observed since surface construction.
    pub shared_high_water_weight: Option<usize>,
    /// Cumulative shared-scope full rejections.
    pub shared_weight_full_count: u64,
}

impl CapacityReplayFact {
    fn from_report(report: &CapacitySurfaceReport) -> Self {
        Self {
            name: report.name.clone(),
            mode: report.mode.clone(),
            max_messages: report.max_messages,
            current_messages: report.current_messages,
            high_water_messages: report.high_water_messages,
            full_count: report.full_count,
            max_weight: report.max_weight,
            current_weight: report.current_weight,
            high_water_weight: report.high_water_weight,
            weight_full_count: report.weight_full_count,
            weight_unit: report.weight_unit.clone(),
            shared_scope: report.shared_scope.clone(),
            shared_max_weight: report.shared_max_weight,
            shared_current_weight: report.shared_current_weight,
            shared_high_water_weight: report.shared_high_water_weight,
            shared_weight_full_count: report.shared_weight_full_count,
        }
    }
}

impl std::fmt::Display for CapacityReplayFact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "capacity surface {} mode={:?} messages={}/{:?} high_water={} full={} weight={:?}/{:?} weight_high_water={:?} weight_full={} unit={:?}",
            self.name,
            self.mode,
            self.current_messages,
            self.max_messages,
            self.high_water_messages,
            self.full_count,
            self.current_weight,
            self.max_weight,
            self.high_water_weight,
            self.weight_full_count,
            self.weight_unit,
        )?;
        if let Some(scope) = &self.shared_scope {
            write!(
                f,
                " shared={} weight={:?}/{:?} high_water={:?} full={}",
                scope,
                self.shared_current_weight,
                self.shared_max_weight,
                self.shared_high_water_weight,
                self.shared_weight_full_count
            )?;
        }
        Ok(())
    }
}

/// Facts captured from a live or simulator run that are sufficient to try a
/// simulator replay.
///
/// The capture stores seed, full replay config, explicit history, invariant,
/// expected projected trace shape, projection contract, and unsupported live
/// facts. It does not pretend that arbitrary live I/O can be replayed. If the
/// operation history does not contain enough facts, [`check_captured_replay`]
/// returns a typed mismatch naming the missing facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveReplayCapture<Op> {
    /// Stable case name.
    pub name: &'static str,
    /// Replay seed.
    pub seed: u64,
    /// Visible simulator-replay knobs captured with the run.
    pub config: ReplayConfig,
    /// Role/topology labels visible to the runner.
    pub topology_roles: Vec<&'static str>,
    /// Diagnostic hash of `config`.
    config_hash: u64,
    /// Visible trace comparison contract used to compute `expected`.
    pub projection: TraceProjection,
    /// One-line scenario description.
    pub scenario: &'static str,
    /// Materialized operation history.
    pub history: History<Op>,
    /// Expected trace shape captured from the source run.
    pub expected: TraceShape,
    /// Human-readable invariant being preserved.
    pub invariant: &'static str,
    /// Short note naming where the capture came from.
    pub source: &'static str,
    /// Typed source metadata for review and replay gating.
    pub source_metadata: CaptureSource,
    /// Live facts not represented by the replay config/history.
    pub unsupported_facts: Vec<UnsupportedLiveFact>,
    /// Typed facts the replay runner must reproduce exactly.
    pub live_facts: Vec<LiveReplayFact>,
    /// True when a capture bound was reached while building this capture.
    pub truncated: bool,
}

impl<Op> LiveReplayCapture<Op> {
    /// Starts the blessed live-run capture builder.
    pub fn builder(name: &'static str) -> LiveReplayCaptureBuilder<Op> {
        LiveReplayCaptureBuilder::new(name)
    }

    /// Captures a replay attempt from explicit parts and runtime events.
    #[allow(clippy::too_many_arguments)]
    pub fn from_events(
        name: &'static str,
        seed: u64,
        config: ReplayConfig,
        scenario: &'static str,
        ops: Vec<Op>,
        invariant: &'static str,
        source: &'static str,
        events: &[RuntimeEvent],
    ) -> Self {
        Self::from_events_with_options(
            name,
            seed,
            config,
            scenario,
            ops,
            invariant,
            source,
            events,
            TraceProjection::Exact,
            Vec::new(),
        )
        .expect("exact projection cannot reject a runtime trace")
    }

    /// Captures a replay attempt with an explicit projection and unsupported
    /// fact list.
    #[allow(clippy::too_many_arguments)]
    pub fn from_events_with_options(
        name: &'static str,
        seed: u64,
        config: ReplayConfig,
        scenario: &'static str,
        ops: Vec<Op>,
        invariant: &'static str,
        source: &'static str,
        events: &[RuntimeEvent],
        projection: TraceProjection,
        unsupported_facts: Vec<UnsupportedLiveFact>,
    ) -> Result<Self, TraceProjectionError> {
        let config_hash = replay_config_hash(&config);
        let topology_roles = config.mailboxes.keys().copied().collect();
        let expected = project_trace_shape(events, &projection)?;
        Ok(Self {
            name,
            seed,
            config,
            topology_roles,
            config_hash,
            projection,
            scenario,
            history: History::new(name, seed, ops),
            expected,
            invariant,
            source,
            source_metadata: CaptureSource::new(source),
            unsupported_facts,
            live_facts: Vec::new(),
            truncated: false,
        })
    }

    /// Captures the current shape of an existing replay case and event list.
    pub fn from_case_and_events(
        case: &ReplayCase<Op>,
        source: &'static str,
        events: &[RuntimeEvent],
    ) -> Self
    where
        Op: Clone,
    {
        Self::from_events(
            case.name,
            case.seed,
            case.config.clone(),
            case.scenario,
            case.history.operations().to_vec(),
            case.invariant,
            source,
            events,
        )
    }

    /// Captures the shape reported by a runner for an existing replay case.
    pub fn from_case_and_report<Output>(
        case: &ReplayCase<Op>,
        source: &'static str,
        report: &ReplayReport<Output>,
    ) -> Self
    where
        Op: Clone,
    {
        let config = case.config.clone();
        let config_hash = replay_config_hash(&config);
        let topology_roles = config.mailboxes.keys().copied().collect();
        Self {
            name: case.name,
            seed: case.seed,
            config,
            topology_roles,
            config_hash,
            projection: TraceProjection::Exact,
            scenario: case.scenario,
            history: case.history.clone(),
            expected: TraceShape::from_report(report),
            invariant: case.invariant,
            source,
            source_metadata: CaptureSource::new(source).backend("sim"),
            unsupported_facts: Vec::new(),
            live_facts: Vec::new(),
            truncated: false,
        }
    }

    /// Returns the diagnostic hash of the captured replay config.
    pub const fn config_hash(&self) -> u64 {
        self.config_hash
    }

    /// Adds one unsupported live fact and returns the capture.
    pub fn with_unsupported_fact(
        mut self,
        fact: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        self.unsupported_facts
            .push(UnsupportedLiveFact::new(fact, reason));
        self
    }

    /// Adds one typed live fact and returns the capture.
    pub fn with_live_fact(mut self, fact: LiveReplayFact) -> Self {
        self.live_facts.push(fact);
        self
    }

    /// Replaces the typed live fact set and returns the capture.
    pub fn with_live_facts(mut self, facts: Vec<LiveReplayFact>) -> Self {
        self.live_facts = facts;
        self
    }

    /// Replaces source metadata and returns the capture.
    pub fn with_source_metadata(mut self, source_metadata: CaptureSource) -> Self {
        self.source_metadata = source_metadata;
        self
    }

    /// Marks the capture as truncated and adds visible unsupported truth.
    pub fn with_truncated(mut self, reason: impl Into<String>) -> Self {
        self.truncated = true;
        self.source_metadata.trace_completeness = TraceCompleteness::Truncated;
        self.unsupported_facts
            .push(UnsupportedLiveFact::new("CaptureTruncated", reason));
        self
    }

    /// Returns a compact summary for logs and proof-harness output.
    pub fn summary(&self) -> CaptureSummary {
        CaptureSummary {
            name: self.name,
            seed: self.seed,
            history_len: self.history.len(),
            expected: self.expected,
            live_fact_count: self.live_facts.len(),
            unsupported_fact_count: self.unsupported_facts.len(),
            topology_role_count: self.topology_roles.len(),
            replay_blocked: self.truncated
                || self.source_metadata.trace_completeness != TraceCompleteness::Complete
                || !self.unsupported_facts.is_empty(),
        }
    }

    /// Converts the capture into a normal [`ReplayCase`] pinned to the
    /// captured trace shape.
    pub fn to_replay_case(&self) -> ReplayCase<Op>
    where
        Op: Clone,
    {
        ReplayCase {
            name: self.name,
            seed: self.seed,
            config: self.config.clone(),
            scenario: self.scenario,
            history: self.history.clone(),
            expected_event_count: self.expected.event_count,
            expected_trace_hash: self.expected.trace_hash,
            invariant: self.invariant,
        }
    }
}

/// Convenience entrypoint for [`LiveReplayCaptureBuilder`].
pub fn capture_live_run<Op>(name: &'static str) -> LiveReplayCaptureBuilder<Op> {
    LiveReplayCaptureBuilder::new(name)
}

/// Builder for turning one live-shaped run into an explicit replay capture.
#[derive(Debug, Clone)]
pub struct LiveReplayCaptureBuilder<Op> {
    name: &'static str,
    seed: Option<u64>,
    config: Option<ReplayConfig>,
    scenario: Option<&'static str>,
    history: Option<Vec<Op>>,
    invariant: Option<&'static str>,
    source: Option<&'static str>,
    source_metadata: Option<CaptureSource>,
    projection: TraceProjection,
    events: Option<Vec<RuntimeEvent>>,
    live_facts: Vec<LiveReplayFact>,
    unsupported_facts: Vec<UnsupportedLiveFact>,
    limits: CaptureLimits,
    truncated: bool,
}

impl<Op> LiveReplayCaptureBuilder<Op> {
    /// Starts a capture builder for `name`.
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            seed: None,
            config: None,
            scenario: None,
            history: None,
            invariant: None,
            source: None,
            source_metadata: None,
            projection: TraceProjection::Exact,
            events: None,
            live_facts: Vec::new(),
            unsupported_facts: Vec::new(),
            limits: CaptureLimits::default(),
            truncated: false,
        }
    }

    /// Sets capture bounds.
    pub const fn with_limits(mut self, limits: CaptureLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Sets the replay seed.
    pub const fn with_seed(mut self, seed: u64) -> Self {
        self.seed = Some(seed);
        self
    }

    /// Sets the replay config.
    pub fn with_config(mut self, config: ReplayConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Sets the scenario text.
    pub const fn with_scenario(mut self, scenario: &'static str) -> Self {
        self.scenario = Some(scenario);
        self
    }

    /// Sets the invariant text.
    pub const fn with_invariant(mut self, invariant: &'static str) -> Self {
        self.invariant = Some(invariant);
        self
    }

    /// Sets source metadata.
    pub fn with_source_metadata(mut self, source_metadata: CaptureSource) -> Self {
        self.source_metadata = Some(source_metadata);
        self
    }

    /// Sets a source label.
    pub const fn with_source(mut self, source: &'static str) -> Self {
        self.source = Some(source);
        self
    }

    /// Sets the projection contract.
    pub fn with_projection(mut self, projection: TraceProjection) -> Self {
        self.projection = projection;
        self
    }

    /// Sets the materialized replay history.
    pub fn with_history(mut self, history: Vec<Op>) -> Self {
        if history.len() > self.limits.max_history_ops {
            self.mark_truncated(UnsupportedLiveFact::new(
                "FactTruncated",
                format!(
                    "history ops {} exceeded cap {}",
                    history.len(),
                    self.limits.max_history_ops
                ),
            ));
            self.history = Some(
                history
                    .into_iter()
                    .take(self.limits.max_history_ops)
                    .collect(),
            );
        } else {
            self.history = Some(history);
        }
        self
    }

    /// Sets runtime trace events.
    pub fn with_trace(mut self, events: &[RuntimeEvent]) -> Self {
        if events.len() > self.limits.max_events {
            self.mark_truncated(UnsupportedLiveFact::new(
                "TraceTruncated",
                format!(
                    "trace events {} exceeded cap {}",
                    events.len(),
                    self.limits.max_events
                ),
            ));
            self.events = Some(events[..self.limits.max_events].to_vec());
        } else {
            self.events = Some(events.to_vec());
        }
        self
    }

    /// Adds one typed replay fact.
    pub fn with_fact(mut self, fact: LiveReplayFact) -> Self {
        if self.live_facts.len() >= self.limits.max_live_facts {
            self.mark_truncated(UnsupportedLiveFact::new(
                "FactTruncated",
                format!("live fact cap {} reached", self.limits.max_live_facts),
            ));
        } else {
            self.live_facts.push(fact);
        }
        self
    }

    /// Adds one capacity summary fact.
    pub fn with_capacity_summary(self, report: &CapacitySurfaceReport) -> Self {
        self.with_fact(LiveReplayFact::capacity_surface(report))
    }

    /// Records a typed unsupported fact.
    pub fn with_unsupported_fact(
        mut self,
        fact: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        if self.unsupported_facts.len() >= self.limits.max_unsupported_facts {
            self.mark_truncated(UnsupportedLiveFact::new(
                "FactTruncated",
                format!(
                    "unsupported fact cap {} reached",
                    self.limits.max_unsupported_facts
                ),
            ));
            return self;
        }
        let fact = truncate_string(fact.into(), self.limits.max_string_bytes);
        let reason = truncate_string(reason.into(), self.limits.max_string_bytes);
        self.unsupported_facts
            .push(UnsupportedLiveFact::new(fact, reason));
        self
    }

    /// Adapter placeholder for shutdown reports that callers have already
    /// reduced to a fact. Keeping this method explicit gives copied workflow
    /// code a stable place to hang the report conversion.
    pub fn with_shutdown_report(self, fact: LiveReplayFact) -> Self {
        self.with_fact(fact)
    }

    fn mark_truncated(&mut self, fact: UnsupportedLiveFact) {
        self.truncated = true;
        if !self
            .unsupported_facts
            .iter()
            .any(|existing| existing.fact == fact.fact && existing.reason == fact.reason)
            && self.unsupported_facts.len() < self.limits.max_unsupported_facts
        {
            self.unsupported_facts.push(fact);
        }
    }

    /// Adapter placeholder for protocol reports that callers have already
    /// reduced to a fact.
    pub fn with_protocol_report(self, fact: LiveReplayFact) -> Self {
        self.with_fact(fact)
    }

    /// Adapter placeholder for resource reports that callers have already
    /// reduced to a fact.
    pub fn with_resource_report(self, fact: LiveReplayFact) -> Self {
        self.with_fact(fact)
    }

    /// Adapter placeholder for proof-harness reports that callers have
    /// already reduced to replay facts.
    pub fn with_proof_harness_report(self, fact: LiveReplayFact) -> Self {
        self.with_fact(fact)
    }

    /// Finishes the capture. Missing required replay material is an error.
    pub fn finish(mut self) -> Result<LiveReplayCapture<Op>, LiveReplayCaptureBuildError> {
        let seed = self
            .seed
            .ok_or(LiveReplayCaptureBuildError::Missing("seed"))?;
        let config = self
            .config
            .ok_or(LiveReplayCaptureBuildError::Missing("config"))?;
        let scenario = self
            .scenario
            .ok_or(LiveReplayCaptureBuildError::Missing("scenario"))?;
        let history = self
            .history
            .ok_or(LiveReplayCaptureBuildError::Missing("history"))?;
        let invariant = self
            .invariant
            .ok_or(LiveReplayCaptureBuildError::Missing("invariant"))?;
        let source = self.source.unwrap_or("live capture");
        let events = self
            .events
            .ok_or(LiveReplayCaptureBuildError::Missing("trace"))?;
        let expected = project_trace_shape(&events, &self.projection)
            .map_err(LiveReplayCaptureBuildError::Projection)?;
        let config_hash = replay_config_hash(&config);
        let topology_roles = config.mailboxes.keys().copied().collect();
        let mut source_metadata = self
            .source_metadata
            .unwrap_or_else(|| CaptureSource::new(source));
        if self.truncated {
            source_metadata.trace_completeness = TraceCompleteness::Truncated;
            self.unsupported_facts.push(UnsupportedLiveFact::new(
                "CaptureFull",
                "capture builder reached at least one configured bound",
            ));
        }
        Ok(LiveReplayCapture {
            name: self.name,
            seed,
            config,
            topology_roles,
            config_hash,
            projection: self.projection,
            scenario,
            history: History::new(self.name, seed, history),
            expected,
            invariant,
            source,
            source_metadata,
            unsupported_facts: self.unsupported_facts,
            live_facts: self.live_facts,
            truncated: self.truncated,
        })
    }
}

fn truncate_string(mut value: String, max: usize) -> String {
    if value.len() > max {
        let mut boundary = max;
        while boundary > 0 && !value.is_char_boundary(boundary) {
            boundary -= 1;
        }
        value.truncate(boundary);
        value.push_str("...");
    }
    value
}

/// Error returned by [`LiveReplayCaptureBuilder::finish`].
#[derive(Debug)]
pub enum LiveReplayCaptureBuildError {
    /// A required field was absent.
    Missing(&'static str),
    /// The selected projection rejected the supplied trace.
    Projection(TraceProjectionError),
}

impl std::fmt::Display for LiveReplayCaptureBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(field) => write!(f, "live replay capture missing `{field}`"),
            Self::Projection(error) => write!(f, "live replay capture projection failed: {error}"),
        }
    }
}

impl std::error::Error for LiveReplayCaptureBuildError {}

/// A saved captured case with owned strings.
///
/// The file helper intentionally does not deserialize a full
/// [`ReplayConfig`]. Tina has no serde dependency here, and config often
/// contains service-owned role constants. Instead the saved file carries
/// `config_debug` and `config_hash` for drift detection; the caller supplies
/// the typed config when converting back into a [`ReplayCase`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedReplayCase<Op> {
    /// Saved case name.
    pub name: String,
    /// Saved replay seed.
    pub seed: u64,
    /// Saved scenario text.
    pub scenario: String,
    /// Saved invariant text.
    pub invariant: String,
    /// Capture source label.
    pub source: String,
    /// Typed capture source metadata.
    pub source_metadata: CaptureSource,
    /// Debug rendering of the replay config at capture time.
    pub config_debug: String,
    /// Diagnostic config hash at capture time.
    pub config_hash: u64,
    /// Saved topology role labels.
    pub topology_roles: Vec<String>,
    /// Debug rendering of the trace projection contract.
    pub projection_debug: String,
    /// Unsupported live facts recorded with the capture.
    pub unsupported_facts: Vec<UnsupportedLiveFact>,
    /// Display form of typed live facts recorded with the capture.
    pub live_facts: Vec<String>,
    /// Captured expected trace shape.
    pub expected: TraceShape,
    /// Whether capture bounds were reached.
    pub truncated: bool,
    /// Materialized operation history.
    pub history: Vec<Op>,
}

impl<Op> SavedReplayCase<Op> {
    /// Builds an owned saved-case view from a live replay capture.
    pub fn from_capture<F>(
        capture: &LiveReplayCapture<Op>,
        mut encode_op: F,
    ) -> SavedReplayCase<String>
    where
        F: FnMut(&Op) -> String,
    {
        SavedReplayCase {
            name: capture.name.to_owned(),
            seed: capture.seed,
            scenario: capture.scenario.to_owned(),
            invariant: capture.invariant.to_owned(),
            source: capture.source.to_owned(),
            source_metadata: capture.source_metadata.clone(),
            config_debug: format!("{:?}", capture.config),
            config_hash: capture.config_hash(),
            topology_roles: capture
                .topology_roles
                .iter()
                .map(|role| (*role).to_owned())
                .collect(),
            projection_debug: format!("{:?}", capture.projection),
            unsupported_facts: capture.unsupported_facts.clone(),
            live_facts: capture.live_facts.iter().map(ToString::to_string).collect(),
            expected: capture.expected,
            truncated: capture.truncated,
            history: capture
                .history
                .operations()
                .iter()
                .map(&mut encode_op)
                .collect(),
        }
    }
}

impl<Op: Clone> SavedReplayCase<Op> {
    /// Converts an owned saved case into a typed [`ReplayCase`].
    ///
    /// The caller supplies the static labels and typed config used by the
    /// runner. The helper verifies the supplied config hash against the saved
    /// one so config drift is reported before replay.
    pub fn to_replay_case(
        &self,
        name: &'static str,
        config: ReplayConfig,
        scenario: &'static str,
        invariant: &'static str,
    ) -> Result<ReplayCase<Op>, SavedReplayCaseError> {
        let actual_hash = replay_config_hash(&config);
        if actual_hash != self.config_hash {
            return Err(SavedReplayCaseError::ConfigHashMismatch {
                expected: self.config_hash,
                actual: actual_hash,
            });
        }
        if self.name != name {
            return Err(SavedReplayCaseError::FieldMismatch {
                field: "name",
                expected: self.name.clone(),
                actual: name.to_owned(),
            });
        }
        if self.scenario != scenario {
            return Err(SavedReplayCaseError::FieldMismatch {
                field: "scenario",
                expected: self.scenario.clone(),
                actual: scenario.to_owned(),
            });
        }
        if self.invariant != invariant {
            return Err(SavedReplayCaseError::FieldMismatch {
                field: "invariant",
                expected: self.invariant.clone(),
                actual: invariant.to_owned(),
            });
        }
        Ok(ReplayCase::new(
            name,
            self.seed,
            config,
            scenario,
            self.history.clone(),
            invariant,
        )
        .expecting(self.expected.event_count, self.expected.trace_hash))
    }
}

/// Error from the tiny saved-case file helper.
#[derive(Debug)]
pub enum SavedReplayCaseError {
    /// Filesystem I/O failed.
    Io(io::Error),
    /// The line-oriented file could not be decoded.
    Decode {
        /// One-based line number.
        line: usize,
        /// Human-readable decode failure.
        reason: String,
    },
    /// A required field was absent.
    MissingField(&'static str),
    /// The caller supplied config whose diagnostic hash differs.
    ConfigHashMismatch {
        /// Hash recorded in the saved case.
        expected: u64,
        /// Hash of the caller-supplied config.
        actual: u64,
    },
    /// A caller-supplied static field does not match the saved case.
    FieldMismatch {
        /// Field name.
        field: &'static str,
        /// Saved value.
        expected: String,
        /// Caller-supplied value.
        actual: String,
    },
}

impl std::fmt::Display for SavedReplayCaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "saved replay case I/O failed: {error}"),
            Self::Decode { line, reason } => {
                write!(
                    f,
                    "saved replay case decode failed on line {line}: {reason}"
                )
            }
            Self::MissingField(field) => write!(f, "saved replay case missing field `{field}`"),
            Self::ConfigHashMismatch { expected, actual } => write!(
                f,
                "saved replay case config changed: expected hash 0x{expected:016x}, got 0x{actual:016x}"
            ),
            Self::FieldMismatch {
                field,
                expected,
                actual,
            } => write!(
                f,
                "saved replay case field `{field}` changed: expected {expected:?}, got {actual:?}"
            ),
        }
    }
}

impl std::error::Error for SavedReplayCaseError {}

impl From<io::Error> for SavedReplayCaseError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

fn reject_newline(field: &str, value: &str) -> Result<(), SavedReplayCaseError> {
    if value.contains('\n') || value.contains('\r') {
        return Err(SavedReplayCaseError::Decode {
            line: 0,
            reason: format!("{field} may not contain a newline"),
        });
    }
    Ok(())
}

fn reject_saved_delimiter(field: &str, value: &str) -> Result<(), SavedReplayCaseError> {
    if value.contains(" | ") {
        return Err(SavedReplayCaseError::Decode {
            line: 0,
            reason: format!("{field} may not contain the saved-case delimiter ` | `"),
        });
    }
    Ok(())
}

/// Writes a small line-oriented saved replay capture.
///
/// Each operation is already materialized in the file as an `op=` line. The
/// caller chooses the op encoding so domain-specific enums can stay copyable
/// and tiny.
pub fn write_saved_replay_case<Op, F>(
    path: impl AsRef<Path>,
    capture: &LiveReplayCapture<Op>,
    mut encode_op: F,
) -> Result<(), SavedReplayCaseError>
where
    F: FnMut(&Op) -> String,
{
    reject_newline("name", capture.name)?;
    reject_newline("scenario", capture.scenario)?;
    reject_newline("invariant", capture.invariant)?;
    reject_newline("source", capture.source)?;
    reject_newline("source_label", &capture.source_metadata.label)?;
    reject_newline("source_runtime_kind", &capture.source_metadata.runtime_kind)?;
    reject_newline("source_backend", &capture.source_metadata.backend)?;
    reject_newline("source_platform", &capture.source_metadata.platform)?;
    if let Some(revision) = &capture.source_metadata.crate_revision {
        reject_newline("source_crate_revision", revision)?;
    }
    if let Some(revision) = &capture.source_metadata.git_revision {
        reject_newline("source_git_revision", revision)?;
    }
    for role in &capture.topology_roles {
        reject_newline("topology_role", role)?;
    }
    for fact in &capture.unsupported_facts {
        reject_newline("unsupported_fact", &fact.fact)?;
        reject_newline("unsupported_reason", &fact.reason)?;
        reject_saved_delimiter("unsupported_fact", &fact.fact)?;
        reject_saved_delimiter("unsupported_reason", &fact.reason)?;
    }
    for fact in &capture.live_facts {
        reject_newline("live_fact", &fact.to_string())?;
    }

    let mut body = String::new();
    body.push_str("tina-replay-case-v1\n");
    body.push_str(&format!("name={}\n", capture.name));
    body.push_str(&format!("seed={}\n", capture.seed));
    body.push_str(&format!("scenario={}\n", capture.scenario));
    body.push_str(&format!("invariant={}\n", capture.invariant));
    body.push_str(&format!("source={}\n", capture.source));
    body.push_str(&format!("source_label={}\n", capture.source_metadata.label));
    body.push_str(&format!(
        "source_runtime_kind={}\n",
        capture.source_metadata.runtime_kind
    ));
    body.push_str(&format!(
        "source_backend={}\n",
        capture.source_metadata.backend
    ));
    body.push_str(&format!(
        "source_schema_version={}\n",
        capture.source_metadata.schema_version
    ));
    body.push_str(&format!(
        "source_platform={}\n",
        capture.source_metadata.platform
    ));
    if let Some(revision) = &capture.source_metadata.crate_revision {
        body.push_str(&format!("source_crate_revision={revision}\n"));
    }
    if let Some(revision) = &capture.source_metadata.git_revision {
        body.push_str(&format!("source_git_revision={revision}\n"));
    }
    body.push_str(&format!(
        "trace_completeness={:?}\n",
        capture.source_metadata.trace_completeness
    ));
    body.push_str(&format!(
        "capture_truncated={}\n",
        encode_saved_bool(capture.truncated)
    ));
    body.push_str(&format!("config_hash=0x{:016x}\n", capture.config_hash()));
    body.push_str(&format!("config_debug={:?}\n", capture.config));
    body.push_str(&format!("projection_debug={:?}\n", capture.projection));
    for role in &capture.topology_roles {
        body.push_str("topology_role=");
        body.push_str(role);
        body.push('\n');
    }
    for fact in &capture.unsupported_facts {
        body.push_str("unsupported_fact=");
        body.push_str(&fact.fact);
        body.push_str(" | ");
        body.push_str(&fact.reason);
        body.push('\n');
    }
    for fact in &capture.live_facts {
        body.push_str("live_fact=");
        body.push_str(&fact.to_string());
        body.push('\n');
    }
    body.push_str(&format!(
        "expected_event_count={}\n",
        capture.expected.event_count
    ));
    body.push_str(&format!(
        "expected_trace_hash=0x{:016x}\n",
        capture.expected.trace_hash
    ));
    for op in capture.history.operations() {
        let encoded = encode_op(op);
        reject_newline("op", &encoded)?;
        body.push_str("op=");
        body.push_str(&encoded);
        body.push('\n');
    }
    std::fs::write(path, body)?;
    Ok(())
}

/// Reads a saved replay capture written by [`write_saved_replay_case`].
pub fn read_saved_replay_case<Op, F>(
    path: impl AsRef<Path>,
    mut decode_op: F,
) -> Result<SavedReplayCase<Op>, SavedReplayCaseError>
where
    F: FnMut(&str) -> Result<Op, String>,
{
    let text = std::fs::read_to_string(path)?;
    let mut lines = text.lines();
    match lines.next() {
        Some("tina-replay-case-v1") => {}
        Some(other) => {
            return Err(SavedReplayCaseError::Decode {
                line: 1,
                reason: format!("unknown header {other:?}"),
            });
        }
        None => return Err(SavedReplayCaseError::MissingField("header")),
    }

    let mut name = None;
    let mut seed = None;
    let mut scenario = None;
    let mut invariant = None;
    let mut source = None;
    let mut source_label = None;
    let mut source_runtime_kind = None;
    let mut source_backend = None;
    let mut source_schema_version = None;
    let mut source_platform = None;
    let mut source_crate_revision = None;
    let mut source_git_revision = None;
    let mut trace_completeness = None;
    let mut capture_truncated = None;
    let mut config_debug = None;
    let mut config_hash = None;
    let mut topology_roles = Vec::new();
    let mut projection_debug = None;
    let mut unsupported_facts = Vec::new();
    let mut live_facts = Vec::new();
    let mut expected_event_count = None;
    let mut expected_trace_hash = None;
    let mut history = Vec::new();

    for (index, line) in lines.enumerate() {
        let line_no = index + 2;
        let Some((key, value)) = line.split_once('=') else {
            return Err(SavedReplayCaseError::Decode {
                line: line_no,
                reason: "expected key=value".to_owned(),
            });
        };
        match key {
            "name" => name = Some(value.to_owned()),
            "seed" => {
                seed = Some(
                    value
                        .parse::<u64>()
                        .map_err(|error| SavedReplayCaseError::Decode {
                            line: line_no,
                            reason: format!("invalid seed: {error}"),
                        })?,
                )
            }
            "scenario" => scenario = Some(value.to_owned()),
            "invariant" => invariant = Some(value.to_owned()),
            "source" => source = Some(value.to_owned()),
            "source_metadata" => source_label = Some(value.to_owned()),
            "source_label" => source_label = Some(value.to_owned()),
            "source_runtime_kind" => source_runtime_kind = Some(value.to_owned()),
            "source_backend" => source_backend = Some(value.to_owned()),
            "source_schema_version" => {
                source_schema_version =
                    Some(
                        value
                            .parse::<u32>()
                            .map_err(|error| SavedReplayCaseError::Decode {
                                line: line_no,
                                reason: format!("invalid source_schema_version: {error}"),
                            })?,
                    )
            }
            "source_platform" => source_platform = Some(value.to_owned()),
            "source_crate_revision" => source_crate_revision = Some(value.to_owned()),
            "source_git_revision" => source_git_revision = Some(value.to_owned()),
            "trace_completeness" => {
                trace_completeness = Some(match value {
                    "Complete" => TraceCompleteness::Complete,
                    "Partial" => TraceCompleteness::Partial,
                    "Truncated" => TraceCompleteness::Truncated,
                    other => {
                        return Err(SavedReplayCaseError::Decode {
                            line: line_no,
                            reason: format!("unknown trace_completeness {other:?}"),
                        });
                    }
                })
            }
            "capture_truncated" => {
                capture_truncated =
                    Some(
                        value
                            .parse::<bool>()
                            .map_err(|error| SavedReplayCaseError::Decode {
                                line: line_no,
                                reason: format!("invalid capture_truncated: {error}"),
                            })?,
                    )
            }
            "config_debug" => config_debug = Some(value.to_owned()),
            "config_hash" => config_hash = Some(parse_hex_u64(value, line_no, "config_hash")?),
            "projection_debug" => projection_debug = Some(value.to_owned()),
            "topology_role" => topology_roles.push(value.to_owned()),
            "unsupported_fact" => {
                let Some((fact, reason)) = value.split_once(" | ") else {
                    return Err(SavedReplayCaseError::Decode {
                        line: line_no,
                        reason: "unsupported_fact must be `fact | reason`".to_owned(),
                    });
                };
                unsupported_facts.push(UnsupportedLiveFact::new(fact, reason));
            }
            "live_fact" => live_facts.push(value.to_owned()),
            "expected_event_count" => {
                expected_event_count =
                    Some(
                        value
                            .parse::<usize>()
                            .map_err(|error| SavedReplayCaseError::Decode {
                                line: line_no,
                                reason: format!("invalid expected_event_count: {error}"),
                            })?,
                    )
            }
            "expected_trace_hash" => {
                expected_trace_hash = Some(parse_hex_u64(value, line_no, "expected_trace_hash")?)
            }
            "op" => {
                history.push(
                    decode_op(value).map_err(|reason| SavedReplayCaseError::Decode {
                        line: line_no,
                        reason,
                    })?,
                )
            }
            other => {
                return Err(SavedReplayCaseError::Decode {
                    line: line_no,
                    reason: format!("unknown key {other:?}"),
                });
            }
        }
    }

    Ok(SavedReplayCase {
        name: name.ok_or(SavedReplayCaseError::MissingField("name"))?,
        seed: seed.ok_or(SavedReplayCaseError::MissingField("seed"))?,
        scenario: scenario.ok_or(SavedReplayCaseError::MissingField("scenario"))?,
        invariant: invariant.ok_or(SavedReplayCaseError::MissingField("invariant"))?,
        source: source.ok_or(SavedReplayCaseError::MissingField("source"))?,
        source_metadata: {
            let mut metadata = CaptureSource::new(
                source_label.unwrap_or_else(|| "legacy saved capture".to_owned()),
            )
            .with_trace_completeness(trace_completeness.unwrap_or(TraceCompleteness::Complete));
            if let Some(runtime_kind) = source_runtime_kind {
                metadata.runtime_kind = runtime_kind;
            }
            if let Some(backend) = source_backend {
                metadata.backend = backend;
            }
            if let Some(schema_version) = source_schema_version {
                metadata.schema_version = schema_version;
            }
            if let Some(platform) = source_platform {
                metadata.platform = platform;
            }
            metadata.crate_revision = source_crate_revision;
            metadata.git_revision = source_git_revision;
            metadata
        },
        config_debug: config_debug.ok_or(SavedReplayCaseError::MissingField("config_debug"))?,
        config_hash: config_hash.ok_or(SavedReplayCaseError::MissingField("config_hash"))?,
        topology_roles,
        projection_debug: projection_debug
            .ok_or(SavedReplayCaseError::MissingField("projection_debug"))?,
        unsupported_facts,
        live_facts,
        expected: TraceShape {
            event_count: expected_event_count
                .ok_or(SavedReplayCaseError::MissingField("expected_event_count"))?,
            trace_hash: expected_trace_hash
                .ok_or(SavedReplayCaseError::MissingField("expected_trace_hash"))?,
        },
        truncated: capture_truncated.unwrap_or(false),
        history,
    })
}

fn parse_hex_u64(
    value: &str,
    line: usize,
    field: &'static str,
) -> Result<u64, SavedReplayCaseError> {
    let digits = value.strip_prefix("0x").unwrap_or(value);
    u64::from_str_radix(digits, 16).map_err(|error| SavedReplayCaseError::Decode {
        line,
        reason: format!("invalid {field}: {error}"),
    })
}

/// Which replay facts changed between a capture and a simulator attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapturedReplayChange {
    /// Case name changed.
    Name,
    /// Replay seed changed.
    Seed,
    /// Scenario text changed.
    Scenario,
    /// Source metadata makes exact replay unsafe.
    Source,
    /// Capture reached a bound or came from an incomplete trace.
    Capture,
    /// Capture contains live facts that are not modeled by replay history/config.
    UnsupportedFact,
    /// Typed live facts changed or were not reproduced by the candidate.
    LiveFact,
    /// Trace projection contract changed or could not be applied.
    Projection,
    /// Replay config changed.
    Config,
    /// Materialized operation history changed.
    History,
    /// Event count changed.
    EventCount,
    /// Trace hash changed.
    Hash,
    /// Invariant label changed.
    Invariant,
}

/// Projected live-replay report returned by a simulator runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveReplayReport<Output> {
    /// Underlying replay report using the projected trace shape.
    pub replay: ReplayReport<Output>,
    /// Projection contract used to compute `replay.event_count` and
    /// `replay.trace_hash`.
    pub projection: TraceProjection,
    /// Typed live facts reproduced by the runner.
    pub live_facts: Vec<LiveReplayFact>,
}

impl<Output> LiveReplayReport<Output> {
    /// Builds a live-replay report from events and a visible projection.
    pub fn from_case_and_events<Op>(
        case: &ReplayCase<Op>,
        events: &[RuntimeEvent],
        output: Output,
        projection: TraceProjection,
    ) -> Result<Self, TraceProjectionError> {
        let shape = project_trace_shape(events, &projection)?;
        Ok(Self {
            replay: ReplayReport {
                name: case.name,
                seed: case.seed,
                config: case.config.clone(),
                scenario: case.scenario,
                event_count: shape.event_count,
                trace_hash: shape.trace_hash,
                output,
            },
            projection,
            live_facts: Vec::new(),
        })
    }

    /// Wraps an existing exact [`ReplayReport`].
    pub fn exact(replay: ReplayReport<Output>) -> Self {
        Self {
            replay,
            projection: TraceProjection::Exact,
            live_facts: Vec::new(),
        }
    }

    /// Adds one typed live fact and returns the report.
    pub fn with_live_fact(mut self, fact: LiveReplayFact) -> Self {
        self.live_facts.push(fact);
        self
    }

    /// Replaces the typed live fact set and returns the report.
    pub fn with_live_facts(mut self, facts: Vec<LiveReplayFact>) -> Self {
        self.live_facts = facts;
        self
    }
}

/// Typed mismatch returned when a captured live story cannot be replayed
/// exactly in the simulator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedReplayMismatch<Op> {
    /// Captured case name.
    pub name: &'static str,
    /// Candidate replay case name.
    pub actual_name: &'static str,
    /// Captured replay seed.
    pub seed: u64,
    /// Candidate replay seed.
    pub actual_seed: u64,
    /// Capture source label.
    pub source: &'static str,
    /// Captured scenario.
    pub scenario: &'static str,
    /// Candidate replay scenario.
    pub actual_scenario: &'static str,
    /// Captured projection contract.
    pub expected_projection: TraceProjection,
    /// Candidate projection contract.
    pub actual_projection: TraceProjection,
    /// Unsupported live facts from the capture.
    pub unsupported_facts: Vec<UnsupportedLiveFact>,
    /// Typed live facts from the capture.
    pub expected_live_facts: Vec<LiveReplayFact>,
    /// Typed live facts reproduced by the candidate.
    pub actual_live_facts: Vec<LiveReplayFact>,
    /// Projection error from the candidate runner, when projection failed.
    pub projection_error: Option<TraceProjectionError>,
    /// Diagnostic config hash from the capture.
    pub expected_config_hash: u64,
    /// Diagnostic config hash from the candidate replay case.
    pub actual_config_hash: u64,
    /// Captured operation history.
    pub expected_history: History<Op>,
    /// Candidate replay operation history.
    pub actual_history: History<Op>,
    /// Captured trace shape.
    pub expected: TraceShape,
    /// Candidate replay trace shape.
    pub actual: TraceShape,
    /// Captured invariant.
    pub expected_invariant: &'static str,
    /// Candidate replay invariant.
    pub actual_invariant: &'static str,
    /// Changed fact categories.
    pub changes: Vec<CapturedReplayChange>,
}

impl<Op> CapturedReplayMismatch<Op> {
    /// Returns true when this mismatch includes `change`.
    pub fn includes(&self, change: CapturedReplayChange) -> bool {
        self.changes.contains(&change)
    }
}

impl<Op: Debug> std::fmt::Display for CapturedReplayMismatch<Op> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "captured replay `{}` from {} did not replay exactly",
            self.name, self.source
        )?;
        writeln!(f, "  seed:      {}", self.seed)?;
        if self.includes(CapturedReplayChange::Name) {
            writeln!(
                f,
                "  name:      expected {:?}, got {:?} (changed)",
                self.name, self.actual_name
            )?;
        }
        writeln!(
            f,
            "  seed:      expected {}, got {}{}",
            self.seed,
            self.actual_seed,
            if self.includes(CapturedReplayChange::Seed) {
                " (changed)"
            } else {
                ""
            },
        )?;
        writeln!(
            f,
            "  scenario:  expected {:?}, got {:?}{}",
            self.scenario,
            self.actual_scenario,
            if self.includes(CapturedReplayChange::Scenario) {
                " (changed)"
            } else {
                ""
            },
        )?;
        writeln!(
            f,
            "  changed:   {}",
            self.changes
                .iter()
                .map(|change| match change {
                    CapturedReplayChange::Name => "name",
                    CapturedReplayChange::Seed => "seed",
                    CapturedReplayChange::Scenario => "scenario",
                    CapturedReplayChange::Source => "source",
                    CapturedReplayChange::Capture => "capture",
                    CapturedReplayChange::UnsupportedFact => "unsupported fact",
                    CapturedReplayChange::LiveFact => "live fact",
                    CapturedReplayChange::Projection => "projection",
                    CapturedReplayChange::Config => "config",
                    CapturedReplayChange::History => "history",
                    CapturedReplayChange::EventCount => "event count",
                    CapturedReplayChange::Hash => "hash",
                    CapturedReplayChange::Invariant => "invariant",
                })
                .collect::<Vec<_>>()
                .join(", ")
        )?;
        if !self.unsupported_facts.is_empty() {
            writeln!(f, "  unsupported facts:")?;
            for fact in &self.unsupported_facts {
                writeln!(f, "      - {fact}")?;
            }
        }
        if self.includes(CapturedReplayChange::Source)
            || self.includes(CapturedReplayChange::Capture)
        {
            writeln!(f, "  source:    {}", self.source)?;
        }
        if self.includes(CapturedReplayChange::LiveFact) {
            writeln!(f, "  live facts:")?;
            writeln!(f, "    expected:")?;
            for fact in &self.expected_live_facts {
                writeln!(f, "      - {fact}")?;
            }
            writeln!(f, "    actual:")?;
            for fact in &self.actual_live_facts {
                writeln!(f, "      - {fact}")?;
            }
        }
        if self.includes(CapturedReplayChange::Projection) {
            writeln!(
                f,
                "  projection: expected {:?}, got {:?} (changed)",
                self.expected_projection, self.actual_projection
            )?;
            if !self.expected_projection.ignored().is_empty() {
                writeln!(
                    f,
                    "  ignored event kinds: {:?}",
                    self.expected_projection.ignored()
                )?;
            }
        }
        if let Some(error) = &self.projection_error {
            writeln!(f, "  projection error: {error}")?;
        }
        writeln!(
            f,
            "  config:    expected 0x{:016x}, got 0x{:016x}{}",
            self.expected_config_hash,
            self.actual_config_hash,
            if self.includes(CapturedReplayChange::Config) {
                " (changed)"
            } else {
                ""
            },
        )?;
        writeln!(
            f,
            "  history:   expected {} ops, got {} ops{}",
            self.expected_history.len(),
            self.actual_history.len(),
            if self.includes(CapturedReplayChange::History) {
                " (changed)"
            } else {
                ""
            },
        )?;
        writeln!(
            f,
            "  events:    expected {}, got {}{}",
            self.expected.event_count,
            self.actual.event_count,
            if self.includes(CapturedReplayChange::EventCount) {
                " (changed)"
            } else {
                ""
            },
        )?;
        writeln!(
            f,
            "  hash:      expected 0x{:016x}, got 0x{:016x}{}",
            self.expected.trace_hash,
            self.actual.trace_hash,
            if self.includes(CapturedReplayChange::Hash) {
                " (changed)"
            } else {
                ""
            },
        )?;
        writeln!(
            f,
            "  invariant: expected {:?}, got {:?}{}",
            self.expected_invariant,
            self.actual_invariant,
            if self.includes(CapturedReplayChange::Invariant) {
                " (changed)"
            } else {
                ""
            },
        )?;
        writeln!(f, "  expected history:")?;
        for op in self.expected_history.operations() {
            writeln!(f, "      - {op:?}")?;
        }
        writeln!(f, "  actual history:")?;
        for op in self.actual_history.operations() {
            writeln!(f, "      - {op:?}")?;
        }
        write!(
            f,
            "  next step: if history lacks a live input or resource completion, \
             materialize that fact as an op before treating the trace hash as a \
             simulator regression."
        )
    }
}

/// Compares captured live facts against a simulator replay candidate.
///
/// The candidate case is supplied separately so tests can prove config or
/// history drift. Use `capture.to_replay_case()` for the normal path.
pub fn check_captured_replay<Op, Output, Runner>(
    capture: &LiveReplayCapture<Op>,
    candidate: &ReplayCase<Op>,
    mut runner: Runner,
) -> Result<LiveReplayReport<Output>, Box<CapturedReplayMismatch<Op>>>
where
    Op: Clone + PartialEq,
    Runner: FnMut(&ReplayCase<Op>) -> Result<LiveReplayReport<Output>, TraceProjectionError>,
{
    if let Some(error) = case_history_coherence_error(candidate) {
        return Err(Box::new(captured_mismatch(
            capture,
            candidate,
            TraceShape {
                event_count: 0,
                trace_hash: 0,
            },
            capture.projection.clone(),
            Some(TraceProjectionError {
                event_id: EventId::new(0),
                kind: None,
                reason: error,
            }),
            vec![CapturedReplayChange::Projection],
        )));
    }
    let projection_result = runner(candidate);
    let (report, actual, actual_projection, actual_live_facts, projection_error) =
        match projection_result {
            Ok(report) => {
                if let Some(error) = report_identity_error(candidate, &report.replay) {
                    let mismatch = captured_mismatch(
                        capture,
                        candidate,
                        TraceShape {
                            event_count: report.replay.event_count,
                            trace_hash: report.replay.trace_hash,
                        },
                        report.projection.clone(),
                        Some(TraceProjectionError {
                            event_id: EventId::new(0),
                            kind: None,
                            reason: error,
                        }),
                        vec![CapturedReplayChange::Projection],
                    );
                    return Err(Box::new(mismatch));
                }
                let actual = TraceShape::from_report(&report.replay);
                let projection = report.projection.clone();
                let live_facts = report.live_facts.clone();
                (Some(report), actual, projection, live_facts, None)
            }
            Err(error) => (
                None,
                TraceShape {
                    event_count: 0,
                    trace_hash: 0,
                },
                capture.projection.clone(),
                Vec::new(),
                Some(error),
            ),
        };
    let actual_config_hash = replay_config_hash(&candidate.config);
    let expected_config_hash = replay_config_hash(&capture.config);
    let mut changes = Vec::new();
    if !capture.unsupported_facts.is_empty() {
        changes.push(CapturedReplayChange::UnsupportedFact);
    }
    if capture.truncated
        || capture.source_metadata.trace_completeness != TraceCompleteness::Complete
    {
        changes.push(CapturedReplayChange::Capture);
        changes.push(CapturedReplayChange::Source);
    }
    if !live_fact_sets_match(&capture.live_facts, &actual_live_facts) {
        changes.push(CapturedReplayChange::LiveFact);
    }
    if let Some(error) = &projection_error {
        let _ = error;
        changes.push(CapturedReplayChange::Projection);
    }
    if capture.name != candidate.name {
        changes.push(CapturedReplayChange::Name);
    }
    if capture.seed != candidate.seed {
        changes.push(CapturedReplayChange::Seed);
    }
    if capture.scenario != candidate.scenario {
        changes.push(CapturedReplayChange::Scenario);
    }
    if expected_config_hash != actual_config_hash {
        changes.push(CapturedReplayChange::Config);
    }
    if capture.history != candidate.history {
        changes.push(CapturedReplayChange::History);
    }
    if capture.expected.event_count != actual.event_count {
        changes.push(CapturedReplayChange::EventCount);
    }
    if capture.expected.trace_hash != actual.trace_hash {
        changes.push(CapturedReplayChange::Hash);
    }
    if capture.invariant != candidate.invariant {
        changes.push(CapturedReplayChange::Invariant);
    }
    if let Some(report) = &report {
        if capture.projection != report.projection {
            changes.push(CapturedReplayChange::Projection);
        }
    }

    if changes.is_empty() {
        Ok(report.expect("report exists when there are no changes"))
    } else {
        Err(Box::new(CapturedReplayMismatch {
            name: capture.name,
            actual_name: candidate.name,
            seed: capture.seed,
            actual_seed: candidate.seed,
            source: capture.source,
            scenario: capture.scenario,
            actual_scenario: candidate.scenario,
            expected_projection: capture.projection.clone(),
            actual_projection,
            unsupported_facts: capture.unsupported_facts.clone(),
            expected_live_facts: capture.live_facts.clone(),
            actual_live_facts,
            projection_error,
            expected_config_hash,
            actual_config_hash,
            expected_history: capture.history.clone(),
            actual_history: candidate.history.clone(),
            expected: capture.expected,
            actual,
            expected_invariant: capture.invariant,
            actual_invariant: candidate.invariant,
            changes,
        }))
    }
}

fn captured_mismatch<Op: Clone>(
    capture: &LiveReplayCapture<Op>,
    candidate: &ReplayCase<Op>,
    actual: TraceShape,
    actual_projection: TraceProjection,
    projection_error: Option<TraceProjectionError>,
    changes: Vec<CapturedReplayChange>,
) -> CapturedReplayMismatch<Op> {
    CapturedReplayMismatch {
        name: capture.name,
        actual_name: candidate.name,
        seed: capture.seed,
        actual_seed: candidate.seed,
        source: capture.source,
        scenario: capture.scenario,
        actual_scenario: candidate.scenario,
        expected_projection: capture.projection.clone(),
        actual_projection,
        unsupported_facts: capture.unsupported_facts.clone(),
        expected_live_facts: capture.live_facts.clone(),
        actual_live_facts: Vec::new(),
        projection_error,
        expected_config_hash: replay_config_hash(&capture.config),
        actual_config_hash: replay_config_hash(&candidate.config),
        expected_history: capture.history.clone(),
        actual_history: candidate.history.clone(),
        expected: capture.expected,
        actual,
        expected_invariant: capture.invariant,
        actual_invariant: candidate.invariant,
        changes,
    }
}

fn live_fact_sets_match(left: &[LiveReplayFact], right: &[LiveReplayFact]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut unmatched = right.to_vec();
    for fact in left {
        let Some(index) = unmatched.iter().position(|candidate| candidate == fact) else {
            return false;
        };
        unmatched.remove(index);
    }
    unmatched.is_empty()
}

/// Test sugar over [`check_captured_replay`].
pub fn assert_captured_replay<Op, Output, Runner>(
    capture: &LiveReplayCapture<Op>,
    candidate: &ReplayCase<Op>,
    runner: Runner,
) -> LiveReplayReport<Output>
where
    Op: Clone + PartialEq + Debug,
    Runner: FnMut(&ReplayCase<Op>) -> Result<LiveReplayReport<Output>, TraceProjectionError>,
{
    match check_captured_replay(capture, candidate, runner) {
        Ok(report) => report,
        Err(mismatch) => panic!("{mismatch}"),
    }
}

fn case_history_coherence_error<Op>(case: &ReplayCase<Op>) -> Option<String> {
    if case.name != case.history.name() {
        return Some(format!(
            "ReplayCase.name and history.name() drifted: {:?} != {:?}",
            case.name,
            case.history.name()
        ));
    }
    if case.seed != case.history.seed() {
        return Some(format!(
            "ReplayCase.seed and history.seed() drifted: {} != {}",
            case.seed,
            case.history.seed()
        ));
    }
    None
}

fn report_identity_error<Op, Output>(
    case: &ReplayCase<Op>,
    report: &ReplayReport<Output>,
) -> Option<String> {
    if report.name != case.name {
        return Some(format!(
            "runner returned report.name {:?} for case {:?}; build the report with ReplayReport::from_case_and_events",
            report.name, case.name
        ));
    }
    if report.seed != case.seed {
        return Some(format!(
            "runner returned report.seed {} for case {:?} (expected {})",
            report.seed, case.name, case.seed
        ));
    }
    if report.scenario != case.scenario {
        return Some(format!(
            "runner returned report.scenario for case {:?} that does not match the case",
            case.name
        ));
    }
    if report.config != case.config {
        return Some(format!(
            "runner returned report.config for case {:?} that does not match the case",
            case.name
        ));
    }
    None
}

/// Runs `runner` once and returns the observed [`ReplayReport`] without
/// comparing it to any pinned constants. The blessed way to discover
/// `expected_event_count` and `expected_trace_hash` for a new case.
///
/// Workflow for a new case:
///
/// ```ignore
/// // 1. Write case() via ReplayCase::new(...) without `.expecting(...)`,
/// //    plus a runner that returns ReplayReport::from_case_and_events.
/// // 2. Run once via observe_replay_case to discover the constants:
/// let report = observe_replay_case(&case(), run_case);
/// println!("{}", report.pinned_constants());
/// // 3. Chain `.expecting(printed_count, printed_hash)` on the case.
/// // 4. The regression test then uses assert_replay_case forever.
/// ```
///
/// Debug-asserts the same case/runner identity guards as
/// [`check_replay_case`], so a buggy runner trips the same panic.
pub fn observe_replay_case<Op, Output, Runner>(
    case: &ReplayCase<Op>,
    mut runner: Runner,
) -> ReplayReport<Output>
where
    Op: Clone,
    Runner: FnMut(&ReplayCase<Op>) -> ReplayReport<Output>,
{
    if let Some(error) = case_history_coherence_error(case) {
        panic!("{error}");
    }
    let report = runner(case);
    if let Some(error) = report_identity_error(case, &report) {
        panic!("{error}");
    }
    report
}

/// Why a [`ReplayCase`] did not match what was pinned.
///
/// Returned by [`check_replay_case`] when the observed event count or
/// trace hash diverged from `expected_event_count` /
/// `expected_trace_hash`. The `Display` impl is the actionable message
/// `assert_replay_case` panics with; it includes the case's history so
/// a coding agent reading only the panic can see what the case did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayMismatch<Op> {
    /// Case name.
    pub name: &'static str,
    /// Replay seed.
    pub seed: u64,
    /// Visible simulator-replay knobs from the case.
    pub config: ReplayConfig,
    /// Scenario tag from the case.
    pub scenario: &'static str,
    /// Invariant tag from the case.
    pub invariant: &'static str,
    /// Pinned event count.
    pub expected_event_count: usize,
    /// Observed event count.
    pub actual_event_count: usize,
    /// Pinned trace hash.
    pub expected_trace_hash: u64,
    /// Observed trace hash.
    pub actual_trace_hash: u64,
    /// History from the case, included so the panic message is
    /// self-contained.
    pub history: History<Op>,
    /// Case/runner identity failure, when replay could not be trusted.
    pub identity_mismatch: Option<String>,
}

impl<Op> ReplayMismatch<Op> {
    /// Returns true when the observed event count diverged.
    pub const fn count_diverged(&self) -> bool {
        self.expected_event_count != self.actual_event_count
    }

    /// Returns true when the observed trace hash diverged.
    pub const fn hash_diverged(&self) -> bool {
        self.expected_trace_hash != self.actual_trace_hash
    }
}

impl<Op: Debug> std::fmt::Display for ReplayMismatch<Op> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "replay case `{}` no longer matches saved trace shape",
            self.name
        )?;
        writeln!(f, "  seed:      {}", self.seed)?;
        writeln!(f, "  scenario:  {}", self.scenario)?;
        writeln!(f, "  invariant: {}", self.invariant)?;
        if let Some(error) = &self.identity_mismatch {
            writeln!(f, "  identity:  {error}")?;
        }
        writeln!(f, "  config:    {:?}", self.config)?;
        writeln!(f, "  history ({} ops):", self.history.len())?;
        for op in self.history.operations() {
            writeln!(f, "      - {op:?}")?;
        }
        writeln!(
            f,
            "  events:    expected {}, got {}{}",
            self.expected_event_count,
            self.actual_event_count,
            if self.count_diverged() {
                " (diverged)"
            } else {
                ""
            },
        )?;
        writeln!(
            f,
            "  hash:      expected 0x{:016x}, got 0x{:016x}{}",
            self.expected_trace_hash,
            self.actual_trace_hash,
            if self.hash_diverged() {
                " (diverged)"
            } else {
                ""
            },
        )?;
        write!(
            f,
            "  next step: decide whether behavior changed or only trace vocabulary/order \
             changed, then either fix the regression or update \
             expected_event_count/expected_trace_hash on the case."
        )
    }
}

/// Runs `runner` once and checks the report against the case's pinned
/// `expected_event_count` and `expected_trace_hash`.
///
/// The runner is `FnMut` so stateful runners are allowed:
///
/// ```ignore
/// fn run_case(case: &ReplayCase<Op>) -> ReplayReport<Output>;
/// ```
///
/// Returns the report on success and a boxed [`ReplayMismatch`] on
/// divergence (boxed so the `Result` payload stays small for clippy's
/// `result_large_err`). Use [`assert_replay_case`] from a `#[test]`
/// for the "saved seed, saved bug" workflow.
///
/// Debug-asserts:
/// - `case.name` and `case.seed` match the bundled `case.history`, so
///   the two source-of-truth fields cannot silently drift;
/// - the runner's [`ReplayReport`] identity (`name`, `seed`,
///   `scenario`, `config`) matches the case it was given, so a runner
///   cannot return a report for a different case and pass by accident.
///   The blessed `ReplayReport::from_case_and_events` constructor
///   copies these fields straight from the case, so a correct runner
///   never trips this check.
pub fn check_replay_case<Op, Output, Runner>(
    case: &ReplayCase<Op>,
    mut runner: Runner,
) -> Result<ReplayReport<Output>, Box<ReplayMismatch<Op>>>
where
    Op: Clone,
    Runner: FnMut(&ReplayCase<Op>) -> ReplayReport<Output>,
{
    if let Some(error) = case_history_coherence_error(case) {
        return Err(Box::new(ReplayMismatch {
            name: case.name,
            seed: case.seed,
            config: case.config.clone(),
            scenario: case.scenario,
            invariant: case.invariant,
            expected_event_count: case.expected_event_count,
            actual_event_count: 0,
            expected_trace_hash: case.expected_trace_hash,
            actual_trace_hash: 0,
            history: case.history.clone(),
            identity_mismatch: Some(error),
        }));
    }
    let report = runner(case);
    if let Some(error) = report_identity_error(case, &report) {
        return Err(Box::new(ReplayMismatch {
            name: case.name,
            seed: case.seed,
            config: case.config.clone(),
            scenario: case.scenario,
            invariant: case.invariant,
            expected_event_count: case.expected_event_count,
            actual_event_count: report.event_count,
            expected_trace_hash: case.expected_trace_hash,
            actual_trace_hash: report.trace_hash,
            history: case.history.clone(),
            identity_mismatch: Some(error),
        }));
    }
    if report.event_count != case.expected_event_count
        || report.trace_hash != case.expected_trace_hash
    {
        return Err(Box::new(ReplayMismatch {
            name: case.name,
            seed: case.seed,
            config: case.config.clone(),
            scenario: case.scenario,
            invariant: case.invariant,
            expected_event_count: case.expected_event_count,
            actual_event_count: report.event_count,
            expected_trace_hash: case.expected_trace_hash,
            actual_trace_hash: report.trace_hash,
            history: case.history.clone(),
            identity_mismatch: None,
        }));
    }
    Ok(report)
}

/// Test sugar over [`check_replay_case`].
///
/// Panics with the [`ReplayMismatch`] message when the case no longer
/// reproduces. The panic message names the case, seed, scenario,
/// invariant, config, history operations, expected vs actual
/// count/hash, and the next review step.
pub fn assert_replay_case<Op, Output, Runner>(
    case: &ReplayCase<Op>,
    runner: Runner,
) -> ReplayReport<Output>
where
    Op: Clone + Debug,
    Runner: FnMut(&ReplayCase<Op>) -> ReplayReport<Output>,
{
    match check_replay_case(case, runner) {
        Ok(report) => report,
        Err(mismatch) => panic!("{mismatch}"),
    }
}
