//! Reusable deterministic-simulation-testing helpers.
//!
//! This module is intentionally small. It gives Tina tests a common shape for
//! history-as-data runs, replay checks, deletion shrinking, and trace
//! invariants without becoming a general property-testing framework.
//!
//! ## Module map (Phase 115 reorg)
//!
//! The `dst` module is split into submodules so future agents can find
//! where new code belongs without scanning the old oversized file. Submodule
//! items are re-exported from `dst::*` so the public API is unchanged.
//!
//! - `discovery` — `DiscoveredConstants` and `discover_constants` for
//!   reporting observed `(event_count, trace_hash)` rows.
//! - `invariants` — `InvariantViolation`, `InvariantSuite`, the
//!   per-invariant check functions, `contains_visible_pressure`, and
//!   `assert_projection_eq`.
//! - `projection` — `TraceShape`, `RuntimeEventKindName`,
//!   `TraceProjection`, `TraceProjectionError`, `ProtocolReplayMismatch`,
//!   `project_trace_shape`, `replay_config_hash`, and the `encode_*`
//!   family.
//! - `replay_case` — `LiveReplayFact`, `CapacityReplayFact`,
//!   `LiveReplayCapture`, `SavedReplayCase` and on-disk format,
//!   `CapturedReplayChange`, `LiveReplayReport`,
//!   `CapturedReplayMismatch`, `ReplayMismatch`,
//!   `check_replay_case`/`check_captured_replay`/`observe_replay_case`.
//! - `shrink` — `ShrinkConfig`, `ShrunkFailure`, `delete_shrink`,
//!   `ShrinkReport`, `shrink_replay_case`.
//! - `sweep` — `sweep_seeds`, `SweepFailure`, `SweepSuccess`.
//!
//! Core types (`History`, `DstRun`, `ReplayConfig`, `ReplayCase`,
//! `ReplayReport`) stay in this file.

mod discovery;
mod invariants;
mod overload;
mod projection;
mod replay_case;
mod shrink;
mod sweep;

pub use discovery::*;
pub use invariants::*;
pub use overload::*;
pub use projection::*;
pub use replay_case::*;
pub use shrink::*;
pub use sweep::*;

use std::collections::BTreeMap;
use std::fmt::Debug;

use tina_runtime::{RuntimeEvent, stable_trace_hash};

use crate::{FaultConfig, SimulatorConfig};

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

    /// Reserves `n` `IsolateId`s in the simulator so user-isolate ids stay
    /// in parity with a live `ThreadedRuntime` that registers system
    /// isolates at worker startup (e.g. its host-call dispatcher pool, of
    /// size [`tina_runtime::HOST_CALL_DISPATCHER_POOL_SIZE`]). Set this in
    /// live-replay runners so the captured trace replays exactly.
    pub fn with_reserved_system_isolates(mut self, n: usize) -> Self {
        self.simulator.reserved_system_isolates = n;
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

#[cfg(test)]
mod replay_case_tests {
    use super::*;
    use tina::capacity::{CapacityMode, CapacitySurfaceReport};
    use tina::{IsolateId, ShardId};
    use tina_runtime::{EventId, RuntimeEventKind};

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

    fn run_three_events_live(
        case: &ReplayCase<u32>,
    ) -> Result<LiveReplayReport<u32>, TraceProjectionError> {
        Ok(LiveReplayReport::exact(run_three_events(case)))
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
    fn check_replay_case_rejects_seed_drift() {
        let mut bad = case();
        bad.seed = 999;
        let mismatch = check_replay_case(&bad, run_three_events).expect_err("seed drift");
        assert_eq!(
            mismatch.identity_mismatch.as_deref(),
            Some("ReplayCase.seed and history.seed() drifted: 999 != 7")
        );
    }

    #[test]
    fn check_replay_case_rejects_name_drift() {
        let mut bad = case();
        bad.name = "drifted name";
        let mismatch = check_replay_case(&bad, run_three_events).expect_err("name drift");
        assert_eq!(
            mismatch.identity_mismatch.as_deref(),
            Some(
                "ReplayCase.name and history.name() drifted: \"drifted name\" != \"fake replay case\""
            )
        );
    }

    #[test]
    fn check_replay_case_rejects_report_identity_mismatch() {
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
        let mismatch =
            check_replay_case(&case(), lying_runner).expect_err("report identity mismatch");
        assert!(
            mismatch
                .identity_mismatch
                .as_deref()
                .is_some_and(|message| message.contains("runner returned report.name"))
        );
    }

    #[test]
    fn check_replay_case_rejects_report_config_mismatch() {
        fn lying_runner(case: &ReplayCase<u32>) -> ReplayReport<u32> {
            let events: Vec<RuntimeEvent> = (1..=3).map(fake_event).collect();
            ReplayReport {
                name: case.name,
                seed: case.seed,
                config: ReplayConfig::new().with_mailbox("forged", 1),
                scenario: case.scenario,
                event_count: events.len(),
                trace_hash: stable_trace_hash(events.iter()),
                output: 0,
            }
        }
        let mismatch = check_replay_case(&case(), lying_runner).expect_err("report config forged");
        assert!(
            mismatch
                .identity_mismatch
                .as_deref()
                .is_some_and(|message| message.contains("runner returned report.config"))
        );
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

        let report = check_captured_replay(&capture, &replay_case, run_three_events_live)
            .expect("captured facts replay");
        assert_eq!(report.replay.event_count, capture.expected.event_count);
        assert_eq!(report.replay.trace_hash, capture.expected.trace_hash);
    }

    #[test]
    fn live_capture_requires_typed_capacity_facts_to_replay() {
        let c = case();
        let events: Vec<RuntimeEvent> = (1..=3).map(fake_event).collect();
        let fact = LiveReplayFact::capacity_surface(&CapacitySurfaceReport::weighted(
            "http.keepalive.request_body",
            CapacityMode::Fixed,
            16,
            0,
            5,
            1,
            "bytes",
        ));
        let capture =
            LiveReplayCapture::from_case_and_events(&c, "http/1 keepalive body pressure", &events)
                .with_live_fact(fact.clone());
        let replay_case = capture.to_replay_case();

        let missing = check_captured_replay(&capture, &replay_case, run_three_events_live)
            .expect_err("missing typed live fact must fail closed");
        assert!(missing.includes(CapturedReplayChange::LiveFact));
        assert!(missing.to_string().contains("http.keepalive.request_body"));

        fn runner_with_fact(
            case: &ReplayCase<u32>,
        ) -> Result<LiveReplayReport<u32>, TraceProjectionError> {
            let events: Vec<RuntimeEvent> = (1..=3).map(fake_event).collect();
            let fact = LiveReplayFact::capacity_surface(&CapacitySurfaceReport::weighted(
                "http.keepalive.request_body",
                CapacityMode::Fixed,
                16,
                0,
                5,
                1,
                "bytes",
            ));
            Ok(
                LiveReplayReport::exact(ReplayReport::from_case_and_events(case, &events, 6))
                    .with_live_fact(fact),
            )
        }

        check_captured_replay(&capture, &replay_case, runner_with_fact)
            .expect("matching typed live fact replays");
    }

    #[test]
    fn live_capture_compares_typed_facts_as_a_set() {
        let c = case();
        let events: Vec<RuntimeEvent> = (1..=3).map(fake_event).collect();
        let request = LiveReplayFact::capacity_surface(&CapacitySurfaceReport::weighted(
            "http.keepalive.request_body",
            CapacityMode::Fixed,
            16,
            0,
            5,
            1,
            "bytes",
        ));
        let response = LiveReplayFact::capacity_surface(&CapacitySurfaceReport::weighted(
            "http.keepalive.response_body",
            CapacityMode::Fixed,
            16,
            0,
            7,
            0,
            "bytes",
        ));
        let capture =
            LiveReplayCapture::from_case_and_events(&c, "http/1 keepalive body pressure", &events)
                .with_live_facts(vec![request.clone(), response.clone()]);
        let replay_case = capture.to_replay_case();

        fn runner_with_reversed_facts(
            case: &ReplayCase<u32>,
        ) -> Result<LiveReplayReport<u32>, TraceProjectionError> {
            let events: Vec<RuntimeEvent> = (1..=3).map(fake_event).collect();
            let request = LiveReplayFact::capacity_surface(&CapacitySurfaceReport::weighted(
                "http.keepalive.request_body",
                CapacityMode::Fixed,
                16,
                0,
                5,
                1,
                "bytes",
            ));
            let response = LiveReplayFact::capacity_surface(&CapacitySurfaceReport::weighted(
                "http.keepalive.response_body",
                CapacityMode::Fixed,
                16,
                0,
                7,
                0,
                "bytes",
            ));
            Ok(
                LiveReplayReport::exact(ReplayReport::from_case_and_events(case, &events, 6))
                    .with_live_facts(vec![response, request]),
            )
        }

        check_captured_replay(&capture, &replay_case, runner_with_reversed_facts)
            .expect("same typed facts in a different order are still the same fact set");
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

        let mismatch = check_captured_replay(&capture, &candidate, run_full_history_case_live)
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

        fn identity_ignoring_runner(
            case: &ReplayCase<u32>,
        ) -> Result<LiveReplayReport<u32>, TraceProjectionError> {
            let events: Vec<RuntimeEvent> = (1..=3).map(fake_event).collect();
            Ok(LiveReplayReport::exact(ReplayReport::from_case_and_events(
                case, &events, 6,
            )))
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
            source_metadata: capture.source_metadata.clone(),
            config_debug: format!("{:?}", c.config),
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

    #[test]
    #[should_panic(expected = "sweep make_case returned case.seed 0 for swept seed 1")]
    fn sweep_seeds_rejects_case_that_ignores_swept_seed() {
        let _ = sweep_seeds(
            "bad sweep",
            1..2,
            |_seed| make_seeded_case(0),
            run_seeded_case,
            |_report| Ok(()),
        );
    }

    #[test]
    #[should_panic(expected = "sweep make_case returned history.seed 0 for swept seed 1")]
    fn sweep_seeds_rejects_history_that_ignores_swept_seed() {
        let _ = sweep_seeds(
            "bad sweep",
            1..2,
            |seed| ReplayCase {
                name: "sweep fixture",
                seed,
                config: ReplayConfig::new(),
                scenario: "case seed is right but history seed is wrong",
                history: History::new("sweep fixture", 0, vec![1, 2, 3]),
                expected_event_count: 0,
                expected_trace_hash: 0,
                invariant: "history seed matches swept seed",
            },
            run_seeded_case,
            |_report| Ok(()),
        );
    }

    #[test]
    #[should_panic(expected = "runner returned report.name")]
    fn sweep_seeds_rejects_report_for_the_wrong_case() {
        // sweep_seeds must run the same report-identity guard that
        // discover_constants runs via observe_replay_case: a runner that
        // returns a report for a different case must trip the panic, not get
        // its constants pasted onto the failing case.
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
        let _ = sweep_seeds(
            "fixture sweep",
            0..1,
            make_seeded_case,
            lying_runner,
            |_report| Ok(()),
        );
    }

    #[test]
    #[should_panic(expected = "runner returned report.seed")]
    fn sweep_seeds_rejects_report_for_the_wrong_seed() {
        fn wrong_seed_runner(case: &ReplayCase<u32>) -> ReplayReport<u32> {
            let events: Vec<RuntimeEvent> = (1..=3).map(fake_event).collect();
            ReplayReport {
                name: case.name,
                seed: case.seed.wrapping_add(1),
                config: case.config.clone(),
                scenario: case.scenario,
                event_count: events.len(),
                trace_hash: stable_trace_hash(events.iter()),
                output: 0,
            }
        }
        let _ = sweep_seeds(
            "fixture sweep",
            0..1,
            make_seeded_case,
            wrong_seed_runner,
            |_report| Ok(()),
        );
    }

    fn run_full_history_case(case: &ReplayCase<u32>) -> ReplayReport<u32> {
        // Each operation contributes one fake event; sum is the projection.
        let events: Vec<RuntimeEvent> = (1..=case.history.len() as u64).map(fake_event).collect();
        let sum: u32 = case.history.operations().iter().sum();
        ReplayReport::from_case_and_events(case, &events, sum)
    }

    fn run_full_history_case_live(
        case: &ReplayCase<u32>,
    ) -> Result<LiveReplayReport<u32>, TraceProjectionError> {
        Ok(LiveReplayReport::exact(run_full_history_case(case)))
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

    fn ws_close_fact() -> tina_runtime::ProtocolFact {
        tina_runtime::ProtocolFact::WebSocketSessionClosed {
            session: tina_runtime::WebSocketSessionId::new(1),
            reason: tina_runtime::WebSocketCloseReason::ProtocolError,
            code: Some(1002),
        }
    }

    fn h2_reset_fact() -> tina_runtime::ProtocolFact {
        tina_runtime::ProtocolFact::Http2StreamReset {
            connection: tina_runtime::ProtocolConnectionId::new(1),
            stream: tina_runtime::Http2StreamId::new(3),
            direction: tina_runtime::ProtocolDirection::Inbound,
            reason: tina_runtime::Http2ResetReason::FrameSizeError,
        }
    }

    fn grpc_status_fact() -> tina_runtime::ProtocolFact {
        tina_runtime::ProtocolFact::GrpcFinalStatusReceived {
            connection: tina_runtime::ProtocolConnectionId::new(1),
            stream: tina_runtime::GrpcStreamId::new(7),
            status: tina_runtime::GrpcStatusCode::ResourceExhausted,
        }
    }

    #[test]
    fn live_replay_fact_protocol_display_names_family() {
        let ws = LiveReplayFact::protocol(ws_close_fact());
        assert_eq!(
            ws.protocol_family(),
            Some(tina_runtime::ProtocolFamily::WebSocket)
        );
        let rendered = ws.to_string();
        assert!(rendered.contains("protocol WebSocket"), "{rendered}");
        assert!(rendered.contains("WebSocketSessionClosed"), "{rendered}");

        let h2 = LiveReplayFact::protocol(h2_reset_fact());
        assert!(h2.to_string().contains("protocol Http2"), "{}", h2);
        let grpc = LiveReplayFact::protocol(grpc_status_fact());
        assert!(grpc.to_string().contains("protocol Grpc"), "{}", grpc);
    }

    #[test]
    fn live_capture_can_save_websocket_http2_grpc_protocol_facts() {
        let c = case();
        let events: Vec<RuntimeEvent> = (1..=3).map(fake_event).collect();
        let facts = vec![
            LiveReplayFact::protocol(ws_close_fact()),
            LiveReplayFact::protocol(h2_reset_fact()),
            LiveReplayFact::protocol(grpc_status_fact()),
        ];
        let capture =
            LiveReplayCapture::from_case_and_events(&c, "protocol chaos capture", &events)
                .with_live_facts(facts.clone());
        let replay_case = capture.to_replay_case();

        fn runner_with_facts(
            facts: Vec<LiveReplayFact>,
        ) -> impl FnMut(&ReplayCase<u32>) -> Result<LiveReplayReport<u32>, TraceProjectionError>
        {
            move |case: &ReplayCase<u32>| {
                let events: Vec<RuntimeEvent> = (1..=3).map(fake_event).collect();
                Ok(
                    LiveReplayReport::exact(ReplayReport::from_case_and_events(case, &events, 6))
                        .with_live_facts(facts.clone()),
                )
            }
        }

        check_captured_replay(&capture, &replay_case, runner_with_facts(facts.clone()))
            .expect("reproducing all three protocol families replays clean");

        // Dropping the HTTP/2 fact fails closed as a LiveFact change.
        let dropped: Vec<LiveReplayFact> = facts
            .iter()
            .filter(|fact| fact.protocol_family() != Some(tina_runtime::ProtocolFamily::Http2))
            .cloned()
            .collect();
        let mismatch = check_captured_replay(&capture, &replay_case, runner_with_facts(dropped))
            .expect_err("missing protocol fact must fail closed");
        assert!(mismatch.includes(CapturedReplayChange::LiveFact));
        assert!(mismatch.to_string().contains("Http2StreamReset"));
    }

    #[test]
    fn mixed_protocol_and_capacity_capture_fails_if_either_family_diverges() {
        let c = case();
        let events: Vec<RuntimeEvent> = (1..=3).map(fake_event).collect();
        let capacity = LiveReplayFact::capacity_surface(&CapacitySurfaceReport::weighted(
            "ws.outbound.queue",
            CapacityMode::Fixed,
            16,
            0,
            12,
            2,
            "frames",
        ));
        let protocol = LiveReplayFact::protocol(ws_close_fact());
        let capture =
            LiveReplayCapture::from_case_and_events(&c, "mixed protocol+capacity", &events)
                .with_live_facts(vec![capacity.clone(), protocol.clone()]);
        let replay_case = capture.to_replay_case();

        fn runner(
            facts: Vec<LiveReplayFact>,
        ) -> impl FnMut(&ReplayCase<u32>) -> Result<LiveReplayReport<u32>, TraceProjectionError>
        {
            move |case: &ReplayCase<u32>| {
                let events: Vec<RuntimeEvent> = (1..=3).map(fake_event).collect();
                Ok(
                    LiveReplayReport::exact(ReplayReport::from_case_and_events(case, &events, 6))
                        .with_live_facts(facts.clone()),
                )
            }
        }

        // Both families reproduced: clean.
        check_captured_replay(
            &capture,
            &replay_case,
            runner(vec![capacity.clone(), protocol.clone()]),
        )
        .expect("reproducing both families replays");

        // Protocol family diverges (capacity intact): fail.
        let protocol_drift =
            check_captured_replay(&capture, &replay_case, runner(vec![capacity.clone()]))
                .expect_err("dropping the protocol fact must fail the whole capture");
        assert!(protocol_drift.includes(CapturedReplayChange::LiveFact));

        // Capacity family diverges (protocol intact): fail.
        let capacity_drift =
            check_captured_replay(&capture, &replay_case, runner(vec![protocol.clone()]))
                .expect_err("dropping the capacity fact must fail the whole capture");
        assert!(capacity_drift.includes(CapturedReplayChange::LiveFact));
    }
}
