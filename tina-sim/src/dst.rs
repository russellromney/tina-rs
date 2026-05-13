//! Reusable deterministic-simulation-testing helpers.
//!
//! This module is intentionally small. It gives Tina tests a common shape for
//! history-as-data runs, replay checks, deletion shrinking, and trace
//! invariants without becoming a general property-testing framework.

use std::collections::BTreeMap;
use std::fmt::{Debug, Write};
use std::io;
use std::path::Path;

use tina::{AddressGeneration, IsolateId, ShardId};
use tina_runtime::{
    CallError, CallId, CauseId, EventId, RuntimeEvent, RuntimeEventKind, SendRejectedReason,
    stable_trace_hash,
};

use crate::{DurableImage, FaultConfig, SimulatorConfig};

/// One replayable generated or hand-authored operation history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History<Op> {
    name: &'static str,
    seed: u64,
    operations: Vec<Op>,
}

impl<Op> History<Op> {
    /// Creates a history with a stable name, seed, and operation list.
    pub fn new(name: &'static str, seed: u64, operations: Vec<Op>) -> Self {
        Self {
            name,
            seed,
            operations,
        }
    }

    /// Returns the workload name.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Returns the deterministic generation seed.
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    /// Returns the replayable operation list.
    pub fn operations(&self) -> &[Op] {
        &self.operations
    }

    /// Returns the number of operations in the history.
    pub fn len(&self) -> usize {
        self.operations.len()
    }

    /// Returns true when the history has no operations.
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    /// Returns a new history with the same name and seed but different
    /// operations.
    pub fn with_operations(&self, operations: Vec<Op>) -> Self {
        Self {
            name: self.name,
            seed: self.seed,
            operations,
        }
    }
}

/// Result of running one DST history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DstRun<Output, Artifact = crate::ReplayArtifact> {
    output: Output,
    artifact: Artifact,
}

impl<Output, Artifact> DstRun<Output, Artifact> {
    /// Creates a run result from a semantic output projection and replay
    /// artifact.
    pub fn new(output: Output, artifact: Artifact) -> Self {
        Self { output, artifact }
    }

    /// Returns the semantic output projection.
    pub const fn output(&self) -> &Output {
        &self.output
    }

    /// Returns the replay artifact.
    pub const fn artifact(&self) -> &Artifact {
        &self.artifact
    }

    /// Splits the run into its output and artifact.
    pub fn into_parts(self) -> (Output, Artifact) {
        (self.output, self.artifact)
    }
}

/// Runs one history twice and returns both runs.
pub fn run_twice_same_history<Op, Output, Artifact, Runner>(
    history: &History<Op>,
    mut runner: Runner,
) -> (DstRun<Output, Artifact>, DstRun<Output, Artifact>)
where
    Runner: FnMut(&History<Op>) -> DstRun<Output, Artifact>,
{
    let first = runner(history);
    let second = runner(history);
    (first, second)
}

/// Runs one history twice, asserts exact replay equality, and returns the
/// first run for additional test-specific checks.
pub fn assert_replays<Op, Output, Artifact, Runner>(
    history: &History<Op>,
    runner: Runner,
) -> DstRun<Output, Artifact>
where
    Op: Debug,
    Output: PartialEq + Debug,
    Artifact: PartialEq + Debug,
    Runner: FnMut(&History<Op>) -> DstRun<Output, Artifact>,
{
    let (first, second) = run_twice_same_history(history, runner);
    assert_eq!(
        first,
        second,
        "DST replay drift in {} seed {} history_len {} ops {:#?}",
        history.name(),
        history.seed(),
        history.len(),
        history.operations()
    );
    first
}

/// Deletion-only shrinker settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShrinkConfig {
    /// Maximum candidate deletions attempted before returning the current
    /// shrunk history.
    pub max_attempts: usize,
}

impl Default for ShrinkConfig {
    fn default() -> Self {
        Self { max_attempts: 1024 }
    }
}

/// One deletion-shrunk failing history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShrunkFailure<Op> {
    original: History<Op>,
    shrunk: History<Op>,
    reason: String,
    attempts: usize,
}

impl<Op> ShrunkFailure<Op> {
    /// Returns the original failing history.
    pub const fn original(&self) -> &History<Op> {
        &self.original
    }

    /// Returns the shrunk failing history.
    pub const fn shrunk(&self) -> &History<Op> {
        &self.shrunk
    }

    /// Returns the failure reason supplied by the caller.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// Returns how many candidate deletions were tried.
    pub const fn attempts(&self) -> usize {
        self.attempts
    }
}

/// Deletion-shrinks a history while `still_fails` remains true.
pub fn delete_shrink<Op, F>(
    history: &History<Op>,
    config: ShrinkConfig,
    reason: impl Into<String>,
    mut still_fails: F,
) -> ShrunkFailure<Op>
where
    Op: Clone,
    F: FnMut(&History<Op>) -> bool,
{
    let mut current = history.clone();
    let mut index = 0;
    let mut attempts = 0;
    while index < current.operations.len() && attempts < config.max_attempts {
        let mut candidate_ops = current.operations.clone();
        candidate_ops.remove(index);
        let candidate = current.with_operations(candidate_ops);
        attempts += 1;
        if still_fails(&candidate) {
            current = candidate;
            index = 0;
        } else {
            index += 1;
        }
    }

    ShrunkFailure {
        original: history.clone(),
        shrunk: current,
        reason: reason.into(),
        attempts,
    }
}

/// One reusable invariant failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvariantViolation {
    invariant: &'static str,
    event_id: Option<EventId>,
    reason: String,
}

impl InvariantViolation {
    /// Creates an invariant violation.
    pub fn new(
        invariant: &'static str,
        event_id: Option<EventId>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            invariant,
            event_id,
            reason: reason.into(),
        }
    }

    /// Returns the invariant name.
    pub const fn invariant(&self) -> &'static str {
        self.invariant
    }

    /// Returns the event closest to the failure, when there is one.
    pub const fn event_id(&self) -> Option<EventId> {
        self.event_id
    }

    /// Returns the human-readable failure reason.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// Reusable trace invariant set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvariantSuite {
    /// Check that event IDs increase by one from the first event.
    pub monotonic_events: bool,
    /// Check that causal links point backward and at existing events.
    pub causal_links: bool,
    /// Check that every send attempt has an accepted or rejected outcome.
    pub send_settlement: bool,
    /// Check that every runtime-call attempt settles.
    pub call_settlement: bool,
    /// Check that stopped isolates do not run later handlers.
    pub no_handler_after_stop: bool,
    /// Check that abandoned messages have a visible causal link.
    pub no_untraced_abandonment: bool,
}

impl InvariantSuite {
    /// Returns the standard Tina trace invariant set.
    pub const fn standard() -> Self {
        Self {
            monotonic_events: true,
            causal_links: true,
            send_settlement: true,
            call_settlement: true,
            no_handler_after_stop: true,
            no_untraced_abandonment: true,
        }
    }

    /// Checks all enabled invariants against one trace.
    pub fn check(self, events: &[RuntimeEvent]) -> Result<(), InvariantViolation> {
        if self.monotonic_events {
            events_are_monotonic(events)?;
        }
        if self.causal_links {
            causes_point_backward(events)?;
        }
        if self.send_settlement {
            send_attempts_settle(events)?;
        }
        if self.call_settlement {
            call_attempts_settle(events)?;
        }
        if self.no_handler_after_stop {
            no_handler_after_stop(events)?;
        }
        if self.no_untraced_abandonment {
            no_untraced_abandonment(events)?;
        }
        Ok(())
    }

    /// Panics with a useful message if any enabled invariant fails.
    pub fn assert(self, events: &[RuntimeEvent]) {
        if let Err(violation) = self.check(events) {
            panic!(
                "DST invariant {} failed near {:?}: {}",
                violation.invariant(),
                violation.event_id(),
                violation.reason()
            );
        }
    }
}

impl Default for InvariantSuite {
    fn default() -> Self {
        Self::standard()
    }
}

/// Checks that event IDs increase by one in trace order.
pub fn events_are_monotonic(events: &[RuntimeEvent]) -> Result<(), InvariantViolation> {
    let mut previous = None;
    for event in events {
        if let Some(previous) = previous {
            if event.id().get() != previous + 1 {
                return Err(InvariantViolation::new(
                    "events_are_monotonic",
                    Some(event.id()),
                    format!("event id {} followed {}", event.id().get(), previous),
                ));
            }
        }
        previous = Some(event.id().get());
    }
    Ok(())
}

/// Checks that every cause points backward and at an event in the trace.
pub fn causes_point_backward(events: &[RuntimeEvent]) -> Result<(), InvariantViolation> {
    for event in events {
        if let Some(cause) = event.cause() {
            if cause.event() >= event.id() {
                return Err(InvariantViolation::new(
                    "causes_point_backward",
                    Some(event.id()),
                    format!("cause {:?} does not point backward", cause),
                ));
            }
            if !events
                .iter()
                .any(|candidate| candidate.id() == cause.event())
            {
                return Err(InvariantViolation::new(
                    "causes_point_backward",
                    Some(event.id()),
                    format!("cause {:?} points at no event in trace", cause),
                ));
            }
        }
    }
    Ok(())
}

/// Checks that every send attempt has a same-target accepted or rejected
/// outcome caused by the attempt.
pub fn send_attempts_settle(events: &[RuntimeEvent]) -> Result<(), InvariantViolation> {
    for event in events {
        let RuntimeEventKind::SendDispatchAttempted {
            target_shard,
            target_isolate,
            target_generation,
        } = event.kind()
        else {
            continue;
        };
        let cause = Some(CauseId::new(event.id()));
        if !events.iter().any(|candidate| {
            candidate.cause() == cause
                && send_outcome_matches(
                    candidate.kind(),
                    target_shard,
                    target_isolate,
                    target_generation,
                )
        }) {
            return Err(InvariantViolation::new(
                "send_attempts_settle",
                Some(event.id()),
                "send attempt had no accepted/rejected outcome",
            ));
        }
    }
    Ok(())
}

fn send_outcome_matches(
    kind: RuntimeEventKind,
    target_shard: ShardId,
    target_isolate: IsolateId,
    target_generation: AddressGeneration,
) -> bool {
    match kind {
        RuntimeEventKind::SendAccepted {
            target_shard: accepted_shard,
            target_isolate: accepted_isolate,
            target_generation: accepted_generation,
        } => {
            accepted_shard == target_shard
                && accepted_isolate == target_isolate
                && accepted_generation == target_generation
        }
        RuntimeEventKind::SendRejected {
            target_shard: rejected_shard,
            target_isolate: rejected_isolate,
            target_generation: rejected_generation,
            reason: SendRejectedReason::Full | SendRejectedReason::Closed,
        } => {
            rejected_shard == target_shard
                && rejected_isolate == target_isolate
                && rejected_generation == target_generation
        }
        _ => false,
    }
}

/// Checks that every runtime-owned call attempt settles as completed,
/// failed, or completion-rejected.
pub fn call_attempts_settle(events: &[RuntimeEvent]) -> Result<(), InvariantViolation> {
    for event in events {
        let RuntimeEventKind::CallDispatchAttempted { call_id, .. } = event.kind() else {
            continue;
        };
        let cause = Some(CauseId::new(event.id()));
        if !events.iter().any(|candidate| {
            candidate.cause() == cause && call_outcome_matches(candidate.kind(), call_id)
        }) {
            return Err(InvariantViolation::new(
                "call_attempts_settle",
                Some(event.id()),
                "call attempt had no completed/failed/rejected outcome",
            ));
        }
    }
    Ok(())
}

fn call_outcome_matches(kind: RuntimeEventKind, expected: CallId) -> bool {
    matches!(
        kind,
        RuntimeEventKind::CallCompleted {
            call_id,
            ..
        } | RuntimeEventKind::CallFailed {
            call_id,
            ..
        } | RuntimeEventKind::CallCompletionRejected {
            call_id,
            ..
        } | RuntimeEventKind::CallCancelled {
            call_id,
            ..
        } if call_id == expected
    )
}

/// Checks that no handler starts for an isolate identity after that
/// identity has stopped.
pub fn no_handler_after_stop(events: &[RuntimeEvent]) -> Result<(), InvariantViolation> {
    let mut stopped = Vec::new();
    for event in events {
        let identity = (event.shard(), event.isolate());
        if matches!(event.kind(), RuntimeEventKind::HandlerStarted) && stopped.contains(&identity) {
            return Err(InvariantViolation::new(
                "no_handler_after_stop",
                Some(event.id()),
                format!(
                    "isolate {:?} on shard {:?} handled after stop",
                    event.isolate(),
                    event.shard()
                ),
            ));
        }
        if matches!(
            event.kind(),
            RuntimeEventKind::IsolateStopped | RuntimeEventKind::HandlerPanicked
        ) {
            stopped.push(identity);
        }
    }
    Ok(())
}

/// Checks that abandoned messages have visible causal links to earlier
/// events instead of appearing as silent drops.
pub fn no_untraced_abandonment(events: &[RuntimeEvent]) -> Result<(), InvariantViolation> {
    for event in events {
        if matches!(event.kind(), RuntimeEventKind::MessageAbandoned) && event.cause().is_none() {
            return Err(InvariantViolation::new(
                "no_untraced_abandonment",
                Some(event.id()),
                "abandoned message had no cause",
            ));
        }
    }
    Ok(())
}

/// Checks that a durable journal image can be replayed.
pub fn persistence_image_replays(
    image: &DurableImage,
    path: impl AsRef<std::path::Path>,
) -> Result<(), InvariantViolation> {
    let path = path.as_ref();
    let bytes = image.get(path).ok_or_else(|| {
        InvariantViolation::new(
            "persistence_image_replays",
            None,
            format!("durable image has no file at {}", path.display()),
        )
    })?;
    tina_runtime::persistence::replay_journal_bytes(bytes).map_err(|error| {
        InvariantViolation::new(
            "persistence_image_replays",
            None,
            format!(
                "durable journal at {} did not replay: {error:?}",
                path.display()
            ),
        )
    })?;
    Ok(())
}

/// Returns true when the trace contains a visible bounded-pressure
/// rejection.
pub fn contains_visible_pressure(events: &[RuntimeEvent]) -> bool {
    events.iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::SendRejected {
                reason: SendRejectedReason::Full | SendRejectedReason::Closed,
                ..
            } | RuntimeEventKind::CallFailed {
                reason: CallError::TargetFull
                    | CallError::TargetClosed
                    | CallError::StorageFull
                    | CallError::StorageClosed,
                ..
            } | RuntimeEventKind::JournalAppendFailed {
                reason: CallError::StorageFull | CallError::StorageClosed,
                ..
            } | RuntimeEventKind::SnapshotCommitFailed {
                reason: CallError::StorageFull | CallError::StorageClosed,
                ..
            }
        )
    })
}

/// Compares two semantic projections and prints both labels on mismatch.
pub fn assert_projection_eq<T>(left_name: &str, left: &T, right_name: &str, right: &T)
where
    T: PartialEq + Debug,
{
    assert_eq!(
        left, right,
        "semantic projection mismatch between {left_name} and {right_name}"
    );
}

/// Visible simulator-replay knobs needed to redo one story.
///
/// `ReplayConfig` is plain data on a [`ReplayCase`]. It carries every
/// simulator-replay knob the runner needs: the full
/// [`SimulatorConfig`] (faults plus scripted TCP/UDP/DNS/TLS/signal/
/// process/storage configs) and the per-isolate mailbox capacities.
///
/// The `simulator.seed` field is overridden by `case.seed` at run
/// time, so the case's `seed` is the source of truth.
///
/// Mailbox capacities are keyed by a stable `&'static str` role name
/// the runner picks. Use [`ReplayConfig::mailbox`] to read them. A
/// missing entry is a loud panic — the runner is asking for a
/// capacity the case never declared.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReplayConfig {
    /// Full simulator config. `simulator.seed` is overridden by
    /// `case.seed` when the runner builds its `Simulator`.
    pub simulator: SimulatorConfig,
    /// Per-isolate mailbox capacities, keyed by a runner-chosen role
    /// name.
    pub mailboxes: BTreeMap<&'static str, usize>,
}

impl ReplayConfig {
    /// Returns a `ReplayConfig` with default simulator config (no
    /// seeded faults, no scripted IO) and no declared mailboxes.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a `ReplayConfig` with the given seeded faults and no
    /// declared mailboxes.
    pub fn with_faults(faults: FaultConfig) -> Self {
        Self {
            simulator: SimulatorConfig {
                faults,
                ..SimulatorConfig::default()
            },
            mailboxes: BTreeMap::new(),
        }
    }

    /// Inserts one mailbox capacity by role name and returns `self`.
    pub fn with_mailbox(mut self, role: &'static str, capacity: usize) -> Self {
        self.mailboxes.insert(role, capacity);
        self
    }

    /// Returns the declared mailbox capacity for `role`, panicking if
    /// the case never declared one. Loud panic surfaces missing
    /// declarations early.
    pub fn mailbox(&self, role: &'static str) -> usize {
        *self.mailboxes.get(role).unwrap_or_else(|| {
            panic!(
                "ReplayConfig.mailboxes has no entry for role {role:?}; \
                 declare it on the ReplayCase so the saved case is self-contained"
            )
        })
    }
}

/// A bug captured as visible Rust data.
///
/// A `ReplayCase` is everything a coding agent needs to redo a Tina
/// failure: a stable name, the seed, visible simulator knobs, an
/// explicit operation history, the human-readable scenario and
/// invariant, and the pinned event count + trace hash that prove the
/// run is the same one that was saved.
///
/// Construct one in a small `pub fn case() -> ReplayCase<Op>` and call
/// [`assert_replay_case`] from a `#[test]`. Pin
/// `expected_event_count`/`expected_trace_hash` on first run; bump them
/// only after a conscious trace-shape review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayCase<Op> {
    /// Stable case name (matches `history.name()` in normal use).
    pub name: &'static str,
    /// Replay seed (matches `history.seed()` in normal use).
    pub seed: u64,
    /// Visible simulator-replay knobs.
    pub config: ReplayConfig,
    /// One-line scenario description for failure messages and bug reports.
    pub scenario: &'static str,
    /// Explicit operation history.
    pub history: History<Op>,
    /// Pinned event count for the saved replay.
    pub expected_event_count: usize,
    /// Pinned `stable_trace_hash` for the saved replay.
    pub expected_trace_hash: u64,
    /// Human-readable invariant the case proves.
    pub invariant: &'static str,
}

impl<Op> ReplayCase<Op> {
    /// Builds one [`ReplayCase`] with `expected_event_count` /
    /// `expected_trace_hash` left at zero. Use [`ReplayCase::expecting`]
    /// to pin the saved constants once they are known.
    ///
    /// Internally constructs `History::new(name, seed, ops)` so the
    /// case name and seed are typed exactly once.
    pub fn new(
        name: &'static str,
        seed: u64,
        config: ReplayConfig,
        scenario: &'static str,
        ops: Vec<Op>,
        invariant: &'static str,
    ) -> Self {
        Self {
            name,
            seed,
            config,
            scenario,
            history: History::new(name, seed, ops),
            expected_event_count: 0,
            expected_trace_hash: 0,
            invariant,
        }
    }

    /// Pins the saved-replay constants. Chain after [`ReplayCase::new`]
    /// once the values have been observed via [`observe_replay_case`].
    pub fn expecting(mut self, expected_event_count: usize, expected_trace_hash: u64) -> Self {
        self.expected_event_count = expected_event_count;
        self.expected_trace_hash = expected_trace_hash;
        self
    }

    /// Returns a [`SimulatorConfig`] whose `seed` is `case.seed` and
    /// whose other fields come from `case.config.simulator`. The
    /// runner uses this to build its `Simulator` in one line:
    ///
    /// ```ignore
    /// let mut sim = Simulator::new(MyShard, case.simulator_config());
    /// ```
    pub fn simulator_config(&self) -> SimulatorConfig {
        let mut config = self.config.simulator.clone();
        config.seed = self.seed;
        config
    }
}

/// What one runner observed for a [`ReplayCase`].
///
/// A runner is a normal function `fn(&ReplayCase<Op>) -> ReplayReport<Output>`
/// that builds the simulator with `case.seed` + `case.config.faults`,
/// drives `case.history.operations()`, and returns the projected output
/// alongside the observed event count and `stable_trace_hash`.
///
/// Use [`ReplayReport::from_case_and_events`] to fill in the boilerplate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayReport<Output> {
    /// Case name.
    pub name: &'static str,
    /// Replay seed.
    pub seed: u64,
    /// Visible simulator-replay knobs from the case.
    pub config: ReplayConfig,
    /// Scenario tag from the case.
    pub scenario: &'static str,
    /// Observed event count for this run.
    pub event_count: usize,
    /// Observed `stable_trace_hash` for this run.
    pub trace_hash: u64,
    /// Caller-supplied semantic projection.
    pub output: Output,
}

impl<Output> ReplayReport<Output> {
    /// Builds a report from a case, the full event trace, and a caller
    /// projection.
    ///
    /// The trace hash is computed via
    /// [`tina_runtime::stable_trace_hash`]; never via debug strings.
    pub fn from_case_and_events<Op>(
        case: &ReplayCase<Op>,
        events: &[RuntimeEvent],
        output: Output,
    ) -> Self {
        Self {
            name: case.name,
            seed: case.seed,
            config: case.config.clone(),
            scenario: case.scenario,
            event_count: events.len(),
            trace_hash: stable_trace_hash(events.iter()),
            output,
        }
    }

    /// Returns the observed `expected_event_count` / `expected_trace_hash`
    /// pair as a multi-line string ready to paste into a `ReplayCase`.
    /// Use after [`observe_replay_case`] when first pinning a case.
    pub fn pinned_constants(&self) -> String {
        format!(
            "expected_event_count: {}\nexpected_trace_hash: 0x{:016x}",
            self.event_count, self.trace_hash,
        )
    }
}

/// Stable, copyable shape of a trace run.
///
/// A `TraceShape` is deliberately smaller than a trace: it records the
/// number of typed events and the canonical [`stable_trace_hash`]. That is
/// enough to tell whether a simulator replay is still the same story while
/// keeping live captures and bug reports compact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceShape {
    /// Observed event count.
    pub event_count: usize,
    /// Observed `stable_trace_hash`.
    pub trace_hash: u64,
}

impl TraceShape {
    /// Builds a trace shape from runtime events.
    pub fn from_events(events: &[RuntimeEvent]) -> Self {
        Self {
            event_count: events.len(),
            trace_hash: stable_trace_hash(events.iter()),
        }
    }

    /// Builds a trace shape from a replay report.
    pub const fn from_report<Output>(report: &ReplayReport<Output>) -> Self {
        Self {
            event_count: report.event_count,
            trace_hash: report.trace_hash,
        }
    }
}

fn stable_text_hash(text: &str) -> u64 {
    // FNV-1a over UTF-8 bytes. This hash is for diagnostics and saved-case
    // drift checks, not for trace identity; traces still use stable_trace_hash.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Diagnostic fingerprint of visible replay config.
///
/// This is intentionally separate from [`stable_trace_hash`]. The trace hash
/// is the replay identity. The config hash is a small bug-report aid so a
/// changed mailbox cap or fault knob is called out before anyone stares at
/// event hashes.
pub fn replay_config_hash(config: &ReplayConfig) -> u64 {
    let mut encoded = String::new();
    encode_replay_config(&mut encoded, config);
    stable_text_hash(&encoded)
}

fn encode_string(out: &mut String, value: &str) {
    let _ = write!(out, "{}:", value.len());
    out.push_str(value);
}

fn encode_bytes(out: &mut String, bytes: &[u8]) {
    let _ = write!(out, "{}:", bytes.len());
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
}

fn encode_option_usize(out: &mut String, value: Option<usize>) {
    match value {
        Some(value) => {
            let _ = write!(out, "some({value})");
        }
        None => out.push_str("none"),
    }
}

fn encode_option_u64(out: &mut String, value: Option<u64>) {
    match value {
        Some(value) => {
            let _ = write!(out, "some({value})");
        }
        None => out.push_str("none"),
    }
}

fn encode_option_i32(out: &mut String, value: Option<i32>) {
    match value {
        Some(value) => {
            let _ = write!(out, "some({value})");
        }
        None => out.push_str("none"),
    }
}

fn encode_socket_addr(out: &mut String, addr: std::net::SocketAddr) {
    match addr {
        std::net::SocketAddr::V4(addr) => {
            let _ = write!(out, "v4({}:{})", addr.ip(), addr.port());
        }
        std::net::SocketAddr::V6(addr) => {
            let _ = write!(
                out,
                "v6({}:{}:{}:{})",
                addr.ip(),
                addr.port(),
                addr.flowinfo(),
                addr.scope_id()
            );
        }
    }
}

fn encode_replay_config(out: &mut String, config: &ReplayConfig) {
    out.push_str("ReplayConfig/v1{sim=");
    encode_simulator_config(out, &config.simulator);
    out.push_str(";mailboxes=[");
    for (role, capacity) in &config.mailboxes {
        encode_string(out, role);
        let _ = write!(out, "={capacity};");
    }
    out.push_str("]}");
}

fn encode_simulator_config(out: &mut String, config: &SimulatorConfig) {
    let _ = write!(out, "{{seed={};faults=", config.seed);
    encode_fault_config(out, config.faults);
    out.push_str(";tcp=");
    encode_tcp_config(out, &config.tcp);
    out.push_str(";udp=");
    encode_udp_config(out, &config.udp);
    out.push_str(";dns=");
    encode_dns_config(out, &config.dns);
    out.push_str(";tls=");
    encode_tls_config(out, &config.tls);
    out.push_str(";signal=");
    encode_signal_config(out, &config.signal);
    out.push_str(";process=");
    encode_process_config(out, &config.process);
    out.push_str(";storage=");
    encode_storage_config(out, config.storage);
    out.push('}');
}

fn encode_fault_config(out: &mut String, faults: FaultConfig) {
    out.push_str("{local=");
    match faults.local_send {
        crate::LocalSendFaultMode::None => out.push_str("none"),
        crate::LocalSendFaultMode::DelayByRounds { one_in, rounds } => {
            let _ = write!(out, "delay-rounds({one_in},{rounds})");
        }
    }
    out.push_str(";timer=");
    match faults.timer_wake {
        crate::FaultMode::None => out.push_str("none"),
        crate::FaultMode::DelayBy { one_in, by } => {
            let _ = write!(out, "delay-by({one_in},{})", by.as_nanos());
        }
    }
    out.push_str(";tcp=");
    match faults.tcp_completion {
        crate::TcpCompletionFaultMode::None => out.push_str("none"),
        crate::TcpCompletionFaultMode::DelayBySteps { one_in, steps } => {
            let _ = write!(out, "delay-steps({one_in},{steps})");
        }
        crate::TcpCompletionFaultMode::ReorderReady { one_in } => {
            let _ = write!(out, "reorder-ready({one_in})");
        }
    }
    out.push('}');
}

fn encode_tcp_config(out: &mut String, config: &crate::ScriptedTcpConfig) {
    let _ = write!(
        out,
        "{{pending={};listeners=[",
        config.pending_completion_capacity
    );
    for listener in &config.listeners {
        out.push_str("{bind=");
        encode_socket_addr(out, listener.bind_addr);
        out.push_str(";local=");
        encode_socket_addr(out, listener.local_addr);
        let _ = write!(out, ";backlog={};peers=[", listener.backlog_capacity);
        for peer in &listener.peers {
            let _ = write!(out, "{{after={};peer=", peer.accept_after_step);
            encode_socket_addr(out, peer.peer_addr);
            out.push_str(";in=[");
            for chunk in &peer.inbound_chunks {
                encode_bytes(out, chunk);
                out.push(';');
            }
            let _ = write!(out, "];in_cap={};read_cap=", peer.inbound_capacity);
            encode_option_usize(out, peer.read_chunk_cap);
            let _ = write!(
                out,
                ";write_cap={};out_cap={};}}",
                peer.write_cap, peer.output_capacity
            );
        }
        out.push_str("]}");
    }
    out.push_str("]}");
}

fn encode_udp_config(out: &mut String, config: &crate::ScriptedUdpConfig) {
    let _ = write!(
        out,
        "{{pending={};sockets=[",
        config.pending_completion_capacity
    );
    for socket in &config.sockets {
        out.push_str("{bind=");
        encode_socket_addr(out, socket.bind_addr);
        out.push_str(";local=");
        encode_socket_addr(out, socket.local_addr);
        let _ = write!(out, ";recv_cap={};datagrams=[", socket.recv_capacity);
        for datagram in &socket.inbound_datagrams {
            let _ = write!(out, "{{after={};peer=", datagram.deliver_after_step);
            encode_socket_addr(out, datagram.peer_addr);
            out.push_str(";bytes=");
            encode_bytes(out, &datagram.bytes);
            out.push('}');
        }
        out.push_str("]}");
    }
    out.push_str("]}");
}

fn encode_dns_config(out: &mut String, config: &crate::ScriptedDnsConfig) {
    let _ = write!(
        out,
        "{{pending={};lookups=[",
        config.pending_completion_capacity
    );
    for lookup in &config.lookups {
        out.push_str("{host=");
        encode_string(out, &lookup.host);
        let _ = write!(
            out,
            ";port={};after={};result=",
            lookup.port, lookup.complete_after_step
        );
        match &lookup.result {
            crate::ScriptedDnsResult::Resolved(addrs) => {
                out.push_str("resolved[");
                for addr in addrs {
                    encode_socket_addr(out, *addr);
                    out.push(';');
                }
                out.push(']');
            }
            crate::ScriptedDnsResult::Failed => out.push_str("failed"),
            crate::ScriptedDnsResult::Timeout => out.push_str("timeout"),
        }
        out.push('}');
    }
    out.push_str("]}");
}

fn encode_tls_config(out: &mut String, config: &crate::ScriptedTlsConfig) {
    let _ = write!(
        out,
        "{{pending={};connects=[",
        config.pending_completion_capacity
    );
    for connect in &config.connects {
        out.push_str("{addr=");
        encode_socket_addr(out, connect.addr);
        out.push_str(";server=");
        encode_string(out, &connect.server_name);
        let _ = write!(out, ";after={};result=", connect.complete_after_step);
        match &connect.result {
            crate::ScriptedTlsConnectResult::Connected { reads, writes } => {
                out.push_str("connected{reads=[");
                for read in reads {
                    match read {
                        crate::ScriptedTlsReadResult::Bytes(bytes) => {
                            out.push_str("bytes(");
                            encode_bytes(out, bytes);
                            out.push(')');
                        }
                        crate::ScriptedTlsReadResult::Eof => out.push_str("eof"),
                        crate::ScriptedTlsReadResult::Failed => out.push_str("failed"),
                        crate::ScriptedTlsReadResult::Timeout => out.push_str("timeout"),
                    }
                    out.push(';');
                }
                out.push_str("];writes=[");
                for write in writes {
                    match write {
                        crate::ScriptedTlsWriteResult::Wrote(bytes) => {
                            let _ = write!(out, "wrote({bytes})");
                        }
                        crate::ScriptedTlsWriteResult::Failed => out.push_str("failed"),
                        crate::ScriptedTlsWriteResult::Timeout => out.push_str("timeout"),
                    }
                    out.push(';');
                }
                out.push_str("]}");
            }
            crate::ScriptedTlsConnectResult::Failed => out.push_str("failed"),
            crate::ScriptedTlsConnectResult::Certificate => out.push_str("certificate"),
            crate::ScriptedTlsConnectResult::Name => out.push_str("name"),
            crate::ScriptedTlsConnectResult::Timeout => out.push_str("timeout"),
        }
        out.push('}');
    }
    out.push_str("]}");
}

fn encode_signal_config(out: &mut String, config: &crate::ScriptedSignalConfig) {
    let _ = write!(
        out,
        "{{pending={};events=[",
        config.pending_completion_capacity
    );
    for event in &config.events {
        out.push_str("{name=");
        encode_string(out, &event.name);
        let _ = write!(out, ";after={};result=", event.deliver_after_step);
        match event.result {
            crate::ScriptedSignalResult::Received => out.push_str("received"),
            crate::ScriptedSignalResult::Failed => out.push_str("failed"),
            crate::ScriptedSignalResult::Timeout => out.push_str("timeout"),
        }
        out.push('}');
    }
    out.push_str("]}");
}

fn encode_process_config(out: &mut String, config: &crate::ScriptedProcessConfig) {
    let _ = write!(
        out,
        "{{pending={};runs=[",
        config.pending_completion_capacity
    );
    for run in &config.runs {
        out.push_str("{command=");
        encode_string(out, &run.command);
        out.push_str(";args=[");
        for arg in &run.args {
            encode_string(out, arg);
            out.push(';');
        }
        let _ = write!(out, "];after={};result=", run.complete_after_step);
        match &run.result {
            crate::ScriptedProcessResult::Exited {
                code,
                stdout,
                stderr,
            } => {
                out.push_str("exited(");
                encode_option_i32(out, *code);
                out.push(',');
                encode_bytes(out, stdout);
                out.push(',');
                encode_bytes(out, stderr);
                out.push(')');
            }
            crate::ScriptedProcessResult::Failed => out.push_str("failed"),
            crate::ScriptedProcessResult::Timeout => out.push_str("timeout"),
            crate::ScriptedProcessResult::KillUncertain => out.push_str("kill-uncertain"),
        }
        out.push('}');
    }
    out.push_str("]}");
}

fn encode_storage_config(out: &mut String, config: crate::ScriptedStorageFaultConfig) {
    out.push_str("{fail_journal=");
    encode_option_u64(out, config.fail_journal_append_at);
    out.push_str(";fail_snapshot=");
    encode_option_u64(out, config.fail_snapshot_commit_at);
    out.push_str(";truncate_journal=");
    encode_option_u64(out, config.truncate_journal_tail_at);
    out.push_str(";corrupt_journal=");
    encode_option_u64(out, config.corrupt_journal_record_at);
    out.push_str(";uncertain_snapshot=");
    encode_option_u64(out, config.commit_uncertain_snapshot_at);
    out.push('}');
}

/// Facts captured from a live or simulator run that are sufficient to try a
/// simulator replay.
///
/// The capture stores seed, full replay config, explicit history, invariant,
/// and the expected trace shape. It does not pretend that arbitrary live I/O
/// can be replayed. If the operation history does not contain enough facts,
/// [`check_captured_replay`] returns a typed mismatch naming history and trace
/// drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveReplayCapture<Op> {
    /// Stable case name.
    pub name: &'static str,
    /// Replay seed.
    pub seed: u64,
    /// Visible simulator-replay knobs captured with the run.
    pub config: ReplayConfig,
    /// Diagnostic hash of `config`.
    config_hash: u64,
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
}

impl<Op> LiveReplayCapture<Op> {
    /// Captures a replay attempt from explicit parts and runtime events.
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
        let config_hash = replay_config_hash(&config);
        Self {
            name,
            seed,
            config,
            config_hash,
            scenario,
            history: History::new(name, seed, ops),
            expected: TraceShape::from_events(events),
            invariant,
            source,
        }
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
        Self {
            name: case.name,
            seed: case.seed,
            config,
            config_hash,
            scenario: case.scenario,
            history: case.history.clone(),
            expected: TraceShape::from_report(report),
            invariant: case.invariant,
            source,
        }
    }

    /// Returns the diagnostic hash of the captured replay config.
    pub const fn config_hash(&self) -> u64 {
        self.config_hash
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
    /// Debug rendering of the replay config at capture time.
    pub config_debug: String,
    /// Diagnostic config hash at capture time.
    pub config_hash: u64,
    /// Captured expected trace shape.
    pub expected: TraceShape,
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
            config_debug: format!("{:?}", capture.config),
            config_hash: capture.config_hash(),
            expected: capture.expected,
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

    let mut body = String::new();
    body.push_str("tina-replay-case-v1\n");
    body.push_str(&format!("name={}\n", capture.name));
    body.push_str(&format!("seed={}\n", capture.seed));
    body.push_str(&format!("scenario={}\n", capture.scenario));
    body.push_str(&format!("invariant={}\n", capture.invariant));
    body.push_str(&format!("source={}\n", capture.source));
    body.push_str(&format!("config_hash=0x{:016x}\n", capture.config_hash()));
    body.push_str(&format!("config_debug={:?}\n", capture.config));
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
    let mut config_debug = None;
    let mut config_hash = None;
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
            "config_debug" => config_debug = Some(value.to_owned()),
            "config_hash" => config_hash = Some(parse_hex_u64(value, line_no, "config_hash")?),
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
        config_debug: config_debug.ok_or(SavedReplayCaseError::MissingField("config_debug"))?,
        config_hash: config_hash.ok_or(SavedReplayCaseError::MissingField("config_hash"))?,
        expected: TraceShape {
            event_count: expected_event_count
                .ok_or(SavedReplayCaseError::MissingField("expected_event_count"))?,
            trace_hash: expected_trace_hash
                .ok_or(SavedReplayCaseError::MissingField("expected_trace_hash"))?,
        },
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
                    CapturedReplayChange::Config => "config",
                    CapturedReplayChange::History => "history",
                    CapturedReplayChange::EventCount => "event count",
                    CapturedReplayChange::Hash => "hash",
                    CapturedReplayChange::Invariant => "invariant",
                })
                .collect::<Vec<_>>()
                .join(", ")
        )?;
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
) -> Result<ReplayReport<Output>, Box<CapturedReplayMismatch<Op>>>
where
    Op: Clone + PartialEq,
    Runner: FnMut(&ReplayCase<Op>) -> ReplayReport<Output>,
{
    assert_case_history_coherent(candidate);
    let report = runner(candidate);
    assert_report_identity(candidate, &report);
    let actual = TraceShape::from_report(&report);
    let actual_config_hash = replay_config_hash(&candidate.config);
    let expected_config_hash = replay_config_hash(&capture.config);
    let mut changes = Vec::new();
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

    if changes.is_empty() {
        Ok(report)
    } else {
        Err(Box::new(CapturedReplayMismatch {
            name: capture.name,
            actual_name: candidate.name,
            seed: capture.seed,
            actual_seed: candidate.seed,
            source: capture.source,
            scenario: capture.scenario,
            actual_scenario: candidate.scenario,
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

fn assert_case_history_coherent<Op>(case: &ReplayCase<Op>) {
    debug_assert_eq!(
        case.name,
        case.history.name(),
        "ReplayCase.name and history.name() drifted",
    );
    debug_assert_eq!(
        case.seed,
        case.history.seed(),
        "ReplayCase.seed and history.seed() drifted",
    );
}

fn assert_report_identity<Op, Output>(case: &ReplayCase<Op>, report: &ReplayReport<Output>) {
    debug_assert_eq!(
        report.name, case.name,
        "runner returned report.name {:?} for case {:?}; \
         build the report with ReplayReport::from_case_and_events",
        report.name, case.name,
    );
    debug_assert_eq!(
        report.seed, case.seed,
        "runner returned report.seed {} for case {:?} (expected {})",
        report.seed, case.name, case.seed,
    );
    debug_assert_eq!(
        report.scenario, case.scenario,
        "runner returned report.scenario for case {:?} that does not match the case",
        case.name,
    );
    debug_assert!(
        report.config == case.config,
        "runner returned report.config for case {:?} that does not match the case",
        case.name,
    );
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
    assert_case_history_coherent(case);
    let report = runner(case);
    assert_report_identity(case, &report);
    report
}

/// One labelled `(event_count, trace_hash)` pair from a
/// [`discover_constants`] sweep. The `Display` impl prints a
/// commented block ready to paste into a `.expecting(...)` chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredConstants {
    /// Caller-supplied label naming which case this row is for.
    pub label: &'static str,
    /// Observed event count.
    pub event_count: usize,
    /// Observed `stable_trace_hash`.
    pub trace_hash: u64,
}

impl std::fmt::Display for DiscoveredConstants {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "// {}", self.label)?;
        writeln!(f, "expected_event_count: {}", self.event_count)?;
        write!(f, "expected_trace_hash: 0x{:016x}", self.trace_hash)
    }
}

/// Bulk discovery: runs each `(label, case)` pair through `runner` once
/// without comparing to pinned constants, and returns one
/// [`DiscoveredConstants`] per case. Use when first pinning a batch of
/// related cases that share the same `Op` type and runner — typical for
/// a single test file with three or four saved-seed regressions.
///
/// ```ignore
/// #[test]
/// #[ignore] // local discovery, run with --ignored after adding a case
/// fn discover_constants_for_service_cases() {
///     let cases = [
///         ("portable_service_case", portable_service_case()),
///         ("audit_full_case", audit_full_case()),
///         ("requester_stop_case", requester_stop_case()),
///         ("shard_failure_case", shard_failure_case()),
///     ];
///     for d in discover_constants(cases, run_service_case) {
///         eprintln!("{d}\n");
///     }
/// }
/// ```
///
/// Each call passes through [`observe_replay_case`], so the same
/// case/runner identity guards apply.
pub fn discover_constants<Op, Output, Runner, Cases>(
    cases: Cases,
    mut runner: Runner,
) -> Vec<DiscoveredConstants>
where
    Op: Clone,
    Cases: IntoIterator<Item = (&'static str, ReplayCase<Op>)>,
    Runner: FnMut(&ReplayCase<Op>) -> ReplayReport<Output>,
{
    cases
        .into_iter()
        .map(|(label, case)| {
            let report = observe_replay_case(&case, &mut runner);
            DiscoveredConstants {
                label,
                event_count: report.event_count,
                trace_hash: report.trace_hash,
            }
        })
        .collect()
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
    assert_case_history_coherent(case);
    let report = runner(case);
    assert_report_identity(case, &report);
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

/// One shrunk replay case with refreshed expected count/hash.
///
/// Returned by [`shrink_replay_case`]. The `shrunk_case` is ready for
/// `assert_replay_case` — its `expected_event_count` and
/// `expected_trace_hash` reflect the smaller history's observed shape,
/// not the original case's pinned values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShrinkReport<Op, Output> {
    /// Original history length.
    pub original_len: usize,
    /// Shrunk history length.
    pub shrunk_len: usize,
    /// How many candidate deletions were attempted.
    pub attempts: usize,
    /// Caller-supplied reason describing the bug being preserved.
    pub reason: String,
    /// Shrunk case with refreshed `expected_*` constants.
    pub shrunk_case: ReplayCase<Op>,
    /// Runner's report for the shrunk case.
    pub shrunk_report: ReplayReport<Output>,
}

impl<Op, Output> std::fmt::Display for ShrinkReport<Op, Output>
where
    Op: Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "shrunk `{}` from {} ops to {} ops in {} attempts",
            self.shrunk_case.name, self.original_len, self.shrunk_len, self.attempts
        )?;
        writeln!(f, "reason: {}", self.reason)?;
        writeln!(f, "case:")?;
        writeln!(f, "    name:      {}", self.shrunk_case.name)?;
        writeln!(f, "    seed:      {}", self.shrunk_case.seed)?;
        writeln!(f, "    config:    {:?}", self.shrunk_case.config)?;
        writeln!(f, "    scenario:  {}", self.shrunk_case.scenario)?;
        writeln!(f, "    invariant: {}", self.shrunk_case.invariant)?;
        writeln!(f, "    history ({} ops):", self.shrunk_case.history.len())?;
        for op in self.shrunk_case.history.operations() {
            writeln!(f, "        - {op:?}")?;
        }
        writeln!(
            f,
            "    expected_event_count: {}",
            self.shrunk_case.expected_event_count
        )?;
        writeln!(
            f,
            "    expected_trace_hash:  0x{:016x}",
            self.shrunk_case.expected_trace_hash
        )?;
        write!(
            f,
            "review step: paste these constants only after confirming the smaller \
             case still proves the same bug/invariant the larger case did."
        )
    }
}

/// Deletion-shrinks a [`ReplayCase`] while the bug still reproduces.
///
/// For every candidate (one operation removed), a new `ReplayCase` is
/// built that preserves `name`, `seed`, `config`, `scenario`, and
/// `invariant`. The runner produces a fresh report; `still_fails`
/// decides whether the candidate still exhibits the bug. Accepted
/// candidates seed further deletion attempts.
///
/// On return, `shrunk_case.expected_event_count` and
/// `shrunk_case.expected_trace_hash` are refreshed to match the smaller
/// case's observed shape — the larger case's pinned constants do not
/// carry over.
///
/// The shrinker honors `config.max_attempts`. The operation list stays
/// visible Rust data throughout.
pub fn shrink_replay_case<Op, Output, Runner, StillFails>(
    case: &ReplayCase<Op>,
    config: ShrinkConfig,
    reason: impl Into<String>,
    mut runner: Runner,
    mut still_fails: StillFails,
) -> ShrinkReport<Op, Output>
where
    Op: Clone,
    Runner: FnMut(&ReplayCase<Op>) -> ReplayReport<Output>,
    StillFails: FnMut(&ReplayReport<Output>) -> bool,
{
    let original_len = case.history.len();
    let mut current_case = case.clone();
    let mut current_report = runner(&current_case);
    let mut index = 0;
    let mut attempts = 0;
    while index < current_case.history.len() && attempts < config.max_attempts {
        let mut candidate_ops = current_case.history.operations().to_vec();
        candidate_ops.remove(index);
        let candidate_history = current_case.history.with_operations(candidate_ops);
        let candidate = ReplayCase {
            name: current_case.name,
            seed: current_case.seed,
            config: current_case.config.clone(),
            scenario: current_case.scenario,
            history: candidate_history,
            expected_event_count: current_case.expected_event_count,
            expected_trace_hash: current_case.expected_trace_hash,
            invariant: current_case.invariant,
        };
        attempts += 1;
        let candidate_report = runner(&candidate);
        if still_fails(&candidate_report) {
            current_case = candidate;
            current_report = candidate_report;
            index = 0;
        } else {
            index += 1;
        }
    }

    current_case.expected_event_count = current_report.event_count;
    current_case.expected_trace_hash = current_report.trace_hash;

    ShrinkReport {
        original_len,
        shrunk_len: current_case.history.len(),
        attempts,
        reason: reason.into(),
        shrunk_case: current_case,
        shrunk_report: current_report,
    }
}

/// One pasteable failing case from a [`sweep_seeds`] run.
///
/// The `failing_case` is ready for `assert_replay_case`: its
/// `expected_event_count` and `expected_trace_hash` are refreshed to
/// the observed values so the bug is pinned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepFailure<Op, Output> {
    /// Sweep name.
    pub name: &'static str,
    /// How many seeds the sweep examined before stopping.
    pub seeds_examined: usize,
    /// The failing seed.
    pub failing_seed: u64,
    /// The failing case with refreshed `expected_*` constants.
    pub failing_case: ReplayCase<Op>,
    /// The runner's report for the failing case.
    pub failing_report: ReplayReport<Output>,
    /// Why the caller's `check` rejected the report.
    pub reason: String,
}

impl<Op, Output> std::fmt::Display for SweepFailure<Op, Output>
where
    Op: Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "sweep `{}` failed at seed {} after {} seeds examined",
            self.name, self.failing_seed, self.seeds_examined
        )?;
        writeln!(f, "reason: {}", self.reason)?;
        writeln!(f, "case:")?;
        writeln!(f, "    name:      {}", self.failing_case.name)?;
        writeln!(f, "    seed:      {}", self.failing_case.seed)?;
        writeln!(f, "    config:    {:?}", self.failing_case.config)?;
        writeln!(f, "    scenario:  {}", self.failing_case.scenario)?;
        writeln!(f, "    invariant: {}", self.failing_case.invariant)?;
        writeln!(f, "    history ({} ops):", self.failing_case.history.len())?;
        for op in self.failing_case.history.operations() {
            writeln!(f, "        - {op:?}")?;
        }
        writeln!(
            f,
            "    expected_event_count: {}",
            self.failing_case.expected_event_count
        )?;
        writeln!(
            f,
            "    expected_trace_hash:  0x{:016x}",
            self.failing_case.expected_trace_hash
        )?;
        write!(
            f,
            "paste this case into a `#[test]` and call \
             `assert_replay_case(&CASE, run_case)`."
        )
    }
}

/// One successful [`sweep_seeds`] run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepSuccess {
    /// Sweep name.
    pub name: &'static str,
    /// How many seeds the sweep examined.
    pub seeds_examined: usize,
}

/// Sweeps a list of seeds, materializing one [`ReplayCase`] per seed and
/// returning the first failing case as a pasteable [`SweepFailure`].
///
/// `make_case` must be pure and deterministic in `seed` — every
/// generated operation must be materialized into the returned
/// `ReplayCase.history` before the simulator runs. There is no hidden
/// random generator in this helper.
///
/// `run_case` is the same runner used with [`assert_replay_case`].
///
/// `check` is the caller's pass/fail predicate over the report. Return
/// `Err(reason)` to declare the seed a failure; the sweep stops and
/// returns the case + report so the caller can paste them into a
/// regression test.
///
/// The returned `SweepFailure.failing_case` has its
/// `expected_event_count` and `expected_trace_hash` refreshed to the
/// observed values, so it can be replayed by `assert_replay_case`
/// directly — pinning the bug as a saved seed.
pub fn sweep_seeds<Op, Output, Seeds, MakeCase, Runner, Check>(
    name: &'static str,
    seeds: Seeds,
    mut make_case: MakeCase,
    mut runner: Runner,
    mut check: Check,
) -> Result<SweepSuccess, Box<SweepFailure<Op, Output>>>
where
    Seeds: IntoIterator<Item = u64>,
    MakeCase: FnMut(u64) -> ReplayCase<Op>,
    Runner: FnMut(&ReplayCase<Op>) -> ReplayReport<Output>,
    Check: FnMut(&ReplayReport<Output>) -> Result<(), String>,
{
    let mut seeds_examined = 0;
    for seed in seeds {
        let case = make_case(seed);
        seeds_examined += 1;
        let report = runner(&case);
        if let Err(reason) = check(&report) {
            let mut failing_case = case;
            failing_case.expected_event_count = report.event_count;
            failing_case.expected_trace_hash = report.trace_hash;
            return Err(Box::new(SweepFailure {
                name,
                seeds_examined,
                failing_seed: seed,
                failing_case,
                failing_report: report,
                reason,
            }));
        }
    }
    Ok(SweepSuccess {
        name,
        seeds_examined,
    })
}

#[cfg(test)]
mod replay_case_tests {
    use super::*;
    use crate::{ScriptedStorageFaultConfig, ScriptedTcpConfig};

    fn fake_event(id: u64) -> RuntimeEvent {
        RuntimeEvent::new(
            EventId::new(id),
            None,
            ShardId::new(0),
            IsolateId::new(1),
            RuntimeEventKind::HandlerStarted,
        )
    }

    fn case() -> ReplayCase<u32> {
        let events: Vec<RuntimeEvent> = (1..=3).map(fake_event).collect();
        ReplayCase {
            name: "fake replay case",
            seed: 7,
            config: ReplayConfig::new(),
            scenario: "produces three handler-started events",
            history: History::new("fake replay case", 7, vec![1, 2, 3]),
            expected_event_count: events.len(),
            expected_trace_hash: stable_trace_hash(events.iter()),
            invariant: "trace shape matches saved fixture",
        }
    }

    fn run_three_events(case: &ReplayCase<u32>) -> ReplayReport<u32> {
        let events: Vec<RuntimeEvent> = (1..=3).map(fake_event).collect();
        let sum: u32 = case.history.operations().iter().sum();
        ReplayReport::from_case_and_events(case, &events, sum)
    }

    #[test]
    fn assert_replay_case_passes_for_pinned_shape() {
        let report = assert_replay_case(&case(), run_three_events);
        assert_eq!(report.event_count, 3);
        assert_eq!(report.output, 6);
        assert_eq!(report.trace_hash, case().expected_trace_hash);
    }

    #[test]
    fn check_replay_case_returns_mismatch_when_count_drifts() {
        let mut drifted = case();
        drifted.expected_event_count = 99;
        let mismatch = check_replay_case(&drifted, run_three_events).expect_err("count drift");
        assert!(mismatch.count_diverged());
        assert_eq!(mismatch.actual_event_count, 3);
        assert_eq!(mismatch.expected_event_count, 99);

        let rendered = mismatch.to_string();
        assert!(rendered.contains("fake replay case"));
        assert!(rendered.contains("seed:      7"));
        assert!(rendered.contains("expected 99, got 3"));
        assert!(rendered.contains("next step"));
        // History must be in the failure message so an agent reading
        // only the panic can see what the case did.
        assert!(rendered.contains("history (3 ops)"));
        assert!(rendered.contains("- 1"));
        assert!(rendered.contains("- 2"));
        assert!(rendered.contains("- 3"));
    }

    #[test]
    #[should_panic(expected = "ReplayCase.seed and history.seed() drifted")]
    fn check_replay_case_debug_asserts_seed_drift() {
        let mut bad = case();
        bad.seed = 999;
        let _ = check_replay_case(&bad, run_three_events);
    }

    #[test]
    #[should_panic(expected = "ReplayCase.name and history.name() drifted")]
    fn check_replay_case_debug_asserts_name_drift() {
        let mut bad = case();
        bad.name = "drifted name";
        let _ = check_replay_case(&bad, run_three_events);
    }

    #[test]
    #[should_panic(expected = "runner returned report.name")]
    fn check_replay_case_debug_asserts_report_identity_mismatch() {
        // A misbehaving runner that hand-builds a report for the
        // wrong case must trip the identity guard, not slip through
        // because the count/hash happen to align.
        fn lying_runner(case: &ReplayCase<u32>) -> ReplayReport<u32> {
            let events: Vec<RuntimeEvent> = (1..=3).map(fake_event).collect();
            ReplayReport {
                name: "some other case",
                seed: case.seed,
                config: case.config.clone(),
                scenario: case.scenario,
                event_count: events.len(),
                trace_hash: stable_trace_hash(events.iter()),
                output: 0,
            }
        }
        let _ = check_replay_case(&case(), lying_runner);
    }

    #[test]
    fn replay_config_mailbox_returns_declared_capacity() {
        let cfg = ReplayConfig::new()
            .with_mailbox("source", 8)
            .with_mailbox("sink", 2);
        assert_eq!(cfg.mailbox("source"), 8);
        assert_eq!(cfg.mailbox("sink"), 2);
    }

    #[test]
    #[should_panic(expected = "ReplayConfig.mailboxes has no entry for role \"missing-role\"")]
    fn replay_config_mailbox_panics_on_missing_role() {
        // Pin the role name in the panic so a regression that drops
        // the {role:?} interpolation breaks this test.
        let cfg = ReplayConfig::new();
        let _ = cfg.mailbox("missing-role");
    }

    #[test]
    fn replay_config_with_faults_carries_faults_and_keeps_other_fields_default() {
        let faults = FaultConfig {
            local_send: crate::LocalSendFaultMode::DelayByRounds {
                one_in: 3,
                rounds: 1,
            },
            timer_wake: crate::FaultMode::DelayBy {
                one_in: 5,
                by: std::time::Duration::from_millis(2),
            },
            ..Default::default()
        };
        let cfg = ReplayConfig::with_faults(faults);
        assert_eq!(cfg.simulator.faults, faults);
        // Other simulator fields stay at default — sanity-check a couple
        // so a regression that swaps the default for something else
        // surfaces here rather than in distant integration tests.
        assert_eq!(cfg.simulator.tcp, ScriptedTcpConfig::default());
        assert_eq!(cfg.simulator.storage, ScriptedStorageFaultConfig::default());
        assert!(cfg.mailboxes.is_empty());
    }

    #[test]
    fn replay_config_with_mailbox_last_call_wins() {
        let cfg = ReplayConfig::new()
            .with_mailbox("sink", 4)
            .with_mailbox("sink", 16);
        assert_eq!(cfg.mailbox("sink"), 16);
    }

    #[test]
    fn observe_replay_case_returns_report_without_comparing() {
        // A case with placeholder constants (the new-case state) must
        // not panic — observe is the discovery path.
        let pending = ReplayCase::<u32>::new(
            "fake replay case",
            7,
            ReplayConfig::new(),
            "produces three handler-started events",
            vec![1, 2, 3],
            "trace shape matches saved fixture",
        );
        let report = observe_replay_case(&pending, run_three_events);
        assert_eq!(report.event_count, 3);
        assert_eq!(report.trace_hash, case().expected_trace_hash);
        // Pin the exact paste-in format. Users copy this into
        // `.expecting(...)`; a stray newline or rename would break
        // every discovery workflow silently.
        let printed = report.pinned_constants();
        let expected = format!(
            "expected_event_count: 3\nexpected_trace_hash: 0x{:016x}",
            case().expected_trace_hash,
        );
        assert_eq!(printed, expected);
    }

    #[test]
    #[should_panic(expected = "ReplayCase.seed and history.seed() drifted")]
    fn observe_replay_case_debug_asserts_case_history_drift() {
        // observe must run the same case-coherence guards as
        // check_replay_case; a future refactor that drops the call
        // must trip this test.
        let mut bad = case();
        bad.seed = 12345;
        let _ = observe_replay_case(&bad, run_three_events);
    }

    #[test]
    #[should_panic(expected = "runner returned report.name")]
    fn observe_replay_case_debug_asserts_runner_identity() {
        // observe must run the same runner-identity guards as
        // check_replay_case.
        fn lying_runner(case: &ReplayCase<u32>) -> ReplayReport<u32> {
            let events: Vec<RuntimeEvent> = (1..=3).map(fake_event).collect();
            ReplayReport {
                name: "some other case",
                seed: case.seed,
                config: case.config.clone(),
                scenario: case.scenario,
                event_count: events.len(),
                trace_hash: stable_trace_hash(events.iter()),
                output: 0,
            }
        }
        let _ = observe_replay_case(&case(), lying_runner);
    }

    #[test]
    fn replay_case_new_then_expecting_matches_struct_literal() {
        let built = ReplayCase::<u32>::new(
            "fake replay case",
            7,
            ReplayConfig::new(),
            "produces three handler-started events",
            vec![1, 2, 3],
            "trace shape matches saved fixture",
        )
        .expecting(case().expected_event_count, case().expected_trace_hash);
        assert_eq!(built, case());
    }

    #[test]
    fn replay_case_new_threads_name_and_seed_into_history() {
        // Direct field check so a regression in `History::new(...)`
        // wiring is caught locally, not only via the equality test.
        let built = ReplayCase::<u32>::new(
            "direct-check",
            41,
            ReplayConfig::new(),
            "scenario",
            vec![10, 20],
            "invariant",
        );
        assert_eq!(built.name, "direct-check");
        assert_eq!(built.seed, 41);
        assert_eq!(built.history.name(), "direct-check");
        assert_eq!(built.history.seed(), 41);
        assert_eq!(built.history.operations(), &[10, 20]);
        assert_eq!(built.expected_event_count, 0);
        assert_eq!(built.expected_trace_hash, 0);
    }

    #[test]
    fn discover_constants_returns_one_entry_per_case() {
        // Two different cases sharing the same Op type and runner.
        let case_a = ReplayCase::<u32>::new(
            "fake replay case",
            7,
            ReplayConfig::new(),
            "produces three handler-started events",
            vec![1, 2, 3],
            "trace shape matches saved fixture",
        );
        let case_b = ReplayCase::<u32>::new(
            "fake replay case",
            7,
            ReplayConfig::new(),
            "produces three handler-started events",
            vec![10, 20, 30],
            "trace shape matches saved fixture",
        );
        let discovered = discover_constants(
            [("alpha", case_a.clone()), ("beta", case_b.clone())],
            run_three_events,
        );
        assert_eq!(discovered.len(), 2);
        assert_eq!(discovered[0].label, "alpha");
        assert_eq!(discovered[1].label, "beta");
        // The runner ignores history operations, so both cases observe
        // the same trace shape — but the discover sweep returns both
        // rows, not a deduped one.
        assert_eq!(discovered[0].event_count, 3);
        assert_eq!(discovered[1].event_count, 3);
        assert_eq!(discovered[0].trace_hash, discovered[1].trace_hash);
    }

    #[test]
    fn discovered_constants_display_is_pasteable() {
        let row = DiscoveredConstants {
            label: "audit_full_case",
            event_count: 22,
            trace_hash: 0x73e4_304f_3390_e1bd,
        };
        let printed = row.to_string();
        // Pin the exact format users paste under each case factory.
        assert_eq!(
            printed,
            "// audit_full_case\nexpected_event_count: 22\nexpected_trace_hash: 0x73e4304f3390e1bd",
        );
    }

    #[test]
    #[should_panic(expected = "ReplayCase.seed and history.seed() drifted")]
    fn discover_constants_runs_through_observe_guards() {
        // A case with case.seed != history.seed must still trip the
        // identity guard during bulk discovery.
        let mut bad = case();
        bad.seed = 12345;
        let _ = discover_constants([("bad", bad)], run_three_events);
    }

    #[test]
    fn case_simulator_config_overrides_seed_and_keeps_other_fields() {
        // Build a case with non-default faults and a non-zero
        // simulator.seed so we can prove (a) seed wins from the case
        // and (b) faults carry through unchanged.
        let faults = FaultConfig {
            local_send: crate::LocalSendFaultMode::DelayByRounds {
                one_in: 7,
                rounds: 2,
            },
            ..Default::default()
        };
        let mut config = ReplayConfig::with_faults(faults);
        config.simulator.seed = 0xdead;
        let mut c = case();
        c.config = config;
        c.seed = 999;
        c.history = History::new(c.name, 999, c.history.operations().to_vec());

        let sim_config = c.simulator_config();
        assert_eq!(
            sim_config.seed, 999,
            "case.seed wins over config.simulator.seed"
        );
        assert_eq!(sim_config.faults, faults, "faults carry through unchanged");
        assert_eq!(sim_config.tcp, ScriptedTcpConfig::default());
        assert_eq!(sim_config.storage, ScriptedStorageFaultConfig::default());
    }

    #[test]
    fn check_replay_case_returns_mismatch_when_hash_drifts() {
        let mut drifted = case();
        drifted.expected_trace_hash = drifted.expected_trace_hash.wrapping_add(1);
        let mismatch = check_replay_case(&drifted, run_three_events).expect_err("hash drift");
        assert!(mismatch.hash_diverged());
        assert!(!mismatch.count_diverged());
        let rendered = mismatch.to_string();
        assert!(rendered.contains("hash:      expected"));
        assert!(rendered.contains("(diverged)"));
    }

    #[test]
    fn live_capture_replays_when_case_matches_captured_facts() {
        let c = case();
        let events: Vec<RuntimeEvent> = (1..=3).map(fake_event).collect();
        let capture =
            LiveReplayCapture::from_case_and_events(&c, "threaded-runtime smoke", &events);
        let replay_case = capture.to_replay_case();

        let report = check_captured_replay(&capture, &replay_case, run_three_events)
            .expect("captured facts replay");
        assert_eq!(report.event_count, capture.expected.event_count);
        assert_eq!(report.trace_hash, capture.expected.trace_hash);
    }

    #[test]
    fn captured_replay_mismatch_names_every_changed_fact() {
        let c = case();
        let events: Vec<RuntimeEvent> = (1..=3).map(fake_event).collect();
        let capture = LiveReplayCapture::from_case_and_events(&c, "live-thread", &events);

        let changed_config = ReplayConfig::new().with_mailbox("sink", 1);
        let candidate = ReplayCase::new(
            c.name,
            c.seed,
            changed_config,
            c.scenario,
            vec![1, 2],
            "different invariant",
        )
        .expecting(c.expected_event_count, c.expected_trace_hash);

        let mismatch = check_captured_replay(&capture, &candidate, run_full_history_case)
            .expect_err("candidate should drift from capture");
        assert!(mismatch.includes(CapturedReplayChange::Config));
        assert!(mismatch.includes(CapturedReplayChange::History));
        assert!(mismatch.includes(CapturedReplayChange::EventCount));
        assert!(mismatch.includes(CapturedReplayChange::Hash));
        assert!(mismatch.includes(CapturedReplayChange::Invariant));

        let rendered = mismatch.to_string();
        assert!(rendered.contains("changed:   config, history, event count, hash, invariant"));
        assert!(rendered.contains("config:    expected"));
        assert!(rendered.contains("history:   expected 3 ops, got 2"));
        assert!(rendered.contains("events:    expected 3, got 2"));
        assert!(rendered.contains("next step: if history lacks a live input"));
    }

    #[test]
    fn captured_replay_mismatch_names_identity_drift() {
        let c = case();
        let events: Vec<RuntimeEvent> = (1..=3).map(fake_event).collect();
        let capture = LiveReplayCapture::from_case_and_events(&c, "live-thread", &events);
        let candidate = ReplayCase::new(
            "different replay case",
            c.seed + 1,
            c.config.clone(),
            "different scenario",
            c.history.operations().to_vec(),
            c.invariant,
        );

        fn identity_ignoring_runner(case: &ReplayCase<u32>) -> ReplayReport<u32> {
            let events: Vec<RuntimeEvent> = (1..=3).map(fake_event).collect();
            ReplayReport::from_case_and_events(case, &events, 6)
        }

        let mismatch = check_captured_replay(&capture, &candidate, identity_ignoring_runner)
            .expect_err("identity drift should invalidate capture replay");
        assert!(mismatch.includes(CapturedReplayChange::Name));
        assert!(mismatch.includes(CapturedReplayChange::Seed));
        assert!(mismatch.includes(CapturedReplayChange::Scenario));

        let rendered = mismatch.to_string();
        assert!(rendered.contains("changed:   name, seed, scenario"));
        assert!(rendered.contains("seed:      expected 7, got 8 (changed)"));
        assert!(rendered.contains("scenario:  expected"));
    }

    #[test]
    fn saved_replay_case_round_trips_history_and_constants() {
        let c = case();
        let events: Vec<RuntimeEvent> = (1..=3).map(fake_event).collect();
        let capture = LiveReplayCapture::from_case_and_events(&c, "live export", &events);
        let path = std::env::temp_dir().join(format!(
            "tina-saved-replay-{}-{}.case",
            std::process::id(),
            capture.expected.trace_hash
        ));

        write_saved_replay_case(&path, &capture, |op| op.to_string()).expect("write saved case");
        let saved = read_saved_replay_case(&path, |text| {
            text.parse::<u32>().map_err(|error| error.to_string())
        })
        .expect("read saved case");
        let _ = std::fs::remove_file(&path);

        assert_eq!(saved.name, c.name);
        assert_eq!(saved.seed, c.seed);
        assert_eq!(saved.expected, capture.expected);
        assert_eq!(saved.history, c.history.operations());

        let replay_case = saved
            .to_replay_case(c.name, c.config.clone(), c.scenario, c.invariant)
            .expect("typed config matches saved config hash");
        assert_eq!(replay_case.expected_event_count, c.expected_event_count);
        assert_eq!(replay_case.expected_trace_hash, c.expected_trace_hash);
        assert_eq!(replay_case.history.operations(), c.history.operations());
    }

    #[test]
    fn saved_replay_case_rejects_changed_config_before_replay() {
        let c = case();
        let events: Vec<RuntimeEvent> = (1..=3).map(fake_event).collect();
        let capture = LiveReplayCapture::from_case_and_events(&c, "live export", &events);
        let saved = SavedReplayCase {
            name: c.name.to_owned(),
            seed: c.seed,
            scenario: c.scenario.to_owned(),
            invariant: c.invariant.to_owned(),
            source: capture.source.to_owned(),
            config_debug: format!("{:?}", c.config),
            config_hash: capture.config_hash(),
            expected: capture.expected,
            history: c.history.operations().to_vec(),
        };

        let changed = ReplayConfig::new().with_mailbox("sink", 99);
        let err = saved
            .to_replay_case(c.name, changed, c.scenario, c.invariant)
            .expect_err("config hash should change");
        let rendered = err.to_string();
        assert!(rendered.contains("config changed"));
    }

    fn make_seeded_case(seed: u64) -> ReplayCase<u32> {
        let history = History::new("sweep fixture", seed, vec![1, 2, 3]);
        ReplayCase {
            name: "sweep fixture",
            seed,
            config: ReplayConfig::new(),
            scenario: "seed sweep over a fixed history",
            history,
            expected_event_count: 0,
            expected_trace_hash: 0,
            invariant: "report.output stays under threshold",
        }
    }

    fn run_seeded_case(case: &ReplayCase<u32>) -> ReplayReport<u32> {
        let events: Vec<RuntimeEvent> = (1..=(case.seed % 5 + 1)).map(fake_event).collect();
        let output = case.history.operations().iter().sum::<u32>() + case.seed as u32;
        ReplayReport::from_case_and_events(case, &events, output)
    }

    #[test]
    fn sweep_seeds_returns_success_when_all_pass() {
        let outcome = sweep_seeds(
            "fixture sweep",
            0..5,
            make_seeded_case,
            run_seeded_case,
            |_report| Ok(()),
        );
        let success = outcome.expect("all good");
        assert_eq!(success.name, "fixture sweep");
        assert_eq!(success.seeds_examined, 5);
    }

    #[test]
    fn sweep_seeds_returns_first_failing_pasteable_case() {
        let outcome = sweep_seeds(
            "fixture sweep",
            0..10,
            make_seeded_case,
            run_seeded_case,
            |report| {
                if report.output >= 9 {
                    Err(format!("output {} >= 9", report.output))
                } else {
                    Ok(())
                }
            },
        );
        let failure = outcome.expect_err("seed 3 should fail (sum=6, +3 = 9)");
        assert_eq!(failure.failing_seed, 3);
        assert!(failure.seeds_examined >= 4);
        // Refreshed expected constants make the case pasteable.
        assert_eq!(
            failure.failing_case.expected_event_count,
            failure.failing_report.event_count
        );
        assert_eq!(
            failure.failing_case.expected_trace_hash,
            failure.failing_report.trace_hash
        );
        // The case can be replayed by `assert_replay_case`.
        let replay = assert_replay_case(&failure.failing_case, run_seeded_case);
        assert_eq!(replay.event_count, failure.failing_report.event_count);
        assert_eq!(replay.trace_hash, failure.failing_report.trace_hash);

        let rendered = failure.to_string();
        assert!(rendered.contains("sweep `fixture sweep` failed at seed 3"));
        assert!(rendered.contains("expected_trace_hash:"));
        assert!(rendered.contains("paste this case"));
    }

    #[test]
    fn make_case_is_deterministic_per_seed() {
        // Two calls produce the same visible case before any simulator runs.
        let a = make_seeded_case(7);
        let b = make_seeded_case(7);
        assert_eq!(a, b);
    }

    fn run_full_history_case(case: &ReplayCase<u32>) -> ReplayReport<u32> {
        // Each operation contributes one fake event; sum is the projection.
        let events: Vec<RuntimeEvent> = (1..=case.history.len() as u64).map(fake_event).collect();
        let sum: u32 = case.history.operations().iter().sum();
        ReplayReport::from_case_and_events(case, &events, sum)
    }

    #[test]
    fn shrink_replay_case_drops_irrelevant_ops_and_refreshes_constants() {
        let case = ReplayCase {
            name: "shrink fixture",
            seed: 11,
            config: ReplayConfig::new(),
            scenario: "history sum stays >= 5",
            history: History::new("shrink fixture", 11, vec![5, 1, 1, 1]),
            // Original constants come from a hypothetical larger run; the
            // shrinker must refresh them.
            expected_event_count: 999,
            expected_trace_hash: 0xdead_beef,
            invariant: "sum invariant survives deletion",
        };

        let report = shrink_replay_case(
            &case,
            ShrinkConfig::default(),
            "sum stays >= 5",
            run_full_history_case,
            |report| report.output >= 5,
        );

        assert!(report.shrunk_len <= report.original_len);
        assert_eq!(report.shrunk_case.history.operations(), &[5]);
        // Refreshed for the smaller case, not inherited from the original.
        assert_ne!(report.shrunk_case.expected_event_count, 999);
        assert_ne!(report.shrunk_case.expected_trace_hash, 0xdead_beef);
        assert_eq!(
            report.shrunk_case.expected_event_count,
            report.shrunk_report.event_count
        );
        assert_eq!(
            report.shrunk_case.expected_trace_hash,
            report.shrunk_report.trace_hash
        );
        // The shrunk case is itself replayable.
        let replay = assert_replay_case(&report.shrunk_case, run_full_history_case);
        assert_eq!(replay.event_count, report.shrunk_report.event_count);

        let rendered = report.to_string();
        assert!(rendered.contains("shrunk `shrink fixture`"));
        assert!(rendered.contains("review step"));
    }

    #[test]
    fn shrink_replay_case_honors_max_attempts() {
        let case = ReplayCase {
            name: "shrink cap",
            seed: 0,
            config: ReplayConfig::new(),
            scenario: "all ops contribute",
            history: History::new("shrink cap", 0, vec![1; 16]),
            expected_event_count: 0,
            expected_trace_hash: 0,
            invariant: "no op may be removed",
        };
        let report = shrink_replay_case(
            &case,
            ShrinkConfig { max_attempts: 3 },
            "no op is droppable",
            run_full_history_case,
            |_report| false,
        );
        assert_eq!(report.attempts, 3);
        assert_eq!(report.shrunk_len, report.original_len);
    }
}
