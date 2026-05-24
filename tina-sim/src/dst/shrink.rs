//! Deletion-only shrink helpers for DST.
//!
//! [`ShrinkConfig`] / [`delete_shrink`] explore a failing operation
//! history by deleting one element at a time; [`shrink_replay_case`]
//! does the same for a [`ReplayCase`] and refreshes the resulting
//! `(expected_event_count, expected_trace_hash)` so the shrunk case
//! is ready for `assert_replay_case`.

use std::fmt::Debug;

use super::{
    History, LiveReplayCapture, LiveReplayFact, LiveReplayReport, ReplayCase, ReplayReport,
    TraceProjectionError,
};

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

/// One shrunk live-derived replay capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShrinkCapturedReport<Op, Output> {
    /// Original history length.
    pub original_len: usize,
    /// Shrunk history length.
    pub shrunk_len: usize,
    /// How many candidate deletions were attempted.
    pub attempts: usize,
    /// Caller-supplied reason describing the bug being preserved.
    pub reason: String,
    /// Shrunk capture with refreshed expected trace shape.
    pub shrunk_capture: LiveReplayCapture<Op>,
    /// Runner's report for the shrunk case.
    pub shrunk_report: LiveReplayReport<Output>,
    /// Candidate deletions refused because they changed the proving fact set.
    pub fact_set_rejections: Vec<FactSetDelta>,
}

/// Fact-set delta observed while trying to shrink a captured replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactSetDelta {
    /// Candidate deletion index that caused the delta.
    pub deletion_index: usize,
    /// Expected fact display lines.
    pub expected: Vec<String>,
    /// Actual fact display lines.
    pub actual: Vec<String>,
}

/// Error returned when shrinking a live-derived capture cannot safely proceed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShrinkCapturedReplayError {
    /// The runner could not apply the capture projection.
    Projection(TraceProjectionError),
    /// The initial or candidate run changed the proving fact set.
    FactSetChanged {
        /// Context for the fact-set drift.
        phase: &'static str,
        /// Expected fact display lines.
        expected: Vec<String>,
        /// Actual fact display lines.
        actual: Vec<String>,
    },
}

impl std::fmt::Display for ShrinkCapturedReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Projection(error) => {
                write!(f, "captured replay shrink projection failed: {error}")
            }
            Self::FactSetChanged {
                phase,
                expected,
                actual,
            } => write!(
                f,
                "captured replay shrink changed live fact set during {phase}: expected {:?}, got {:?}",
                expected, actual
            ),
        }
    }
}

impl std::error::Error for ShrinkCapturedReplayError {}

impl From<TraceProjectionError> for ShrinkCapturedReplayError {
    fn from(value: TraceProjectionError) -> Self {
        Self::Projection(value)
    }
}

impl<Op, Output> ShrinkCapturedReport<Op, Output> {
    /// Returns the shrunk capture.
    pub const fn capture(&self) -> &LiveReplayCapture<Op> {
        &self.shrunk_capture
    }
}

impl<Op, Output> std::fmt::Display for ShrinkCapturedReport<Op, Output>
where
    Op: Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "shrunk captured replay `{}` from {} ops to {} ops in {} attempts",
            self.shrunk_capture.name, self.original_len, self.shrunk_len, self.attempts
        )?;
        writeln!(f, "reason: {}", self.reason)?;
        writeln!(f, "summary: {}", self.shrunk_capture.summary())?;
        writeln!(f, "history ({} ops):", self.shrunk_capture.history.len())?;
        for op in self.shrunk_capture.history.operations() {
            writeln!(f, "    - {op:?}")?;
        }
        if !self.fact_set_rejections.is_empty() {
            writeln!(f, "fact-set-preserving refusals:")?;
            for delta in &self.fact_set_rejections {
                writeln!(
                    f,
                    "    deletion_index={} expected={:?} actual={:?}",
                    delta.deletion_index, delta.expected, delta.actual
                )?;
            }
        }
        write!(
            f,
            "review step: fact set was preserved exactly; commit the shrunk \
             capture only after confirming it still proves the same live bug."
        )
    }
}

/// Deletion-shrinks a live-derived capture while preserving its fact set.
///
/// By default, a candidate is accepted only when `still_fails` is true and the
/// simulator report reproduces the exact same typed live facts as the original
/// capture. The returned capture refreshes `expected_event_count` and
/// `expected_trace_hash` from the accepted candidate, so it can be saved
/// directly with `write_saved_replay_case`.
pub fn shrink_captured_replay<Op, Output, Runner, StillFails>(
    capture: &LiveReplayCapture<Op>,
    config: ShrinkConfig,
    reason: impl Into<String>,
    mut runner: Runner,
    mut still_fails: StillFails,
) -> Result<ShrinkCapturedReport<Op, Output>, ShrinkCapturedReplayError>
where
    Op: Clone,
    Runner: FnMut(&ReplayCase<Op>) -> Result<LiveReplayReport<Output>, TraceProjectionError>,
    StillFails: FnMut(&LiveReplayReport<Output>) -> bool,
{
    let original_len = capture.history.len();
    let mut current_capture = capture.clone();
    let expected_facts = capture.live_facts.clone();
    let mut current_report = runner(&current_capture.to_replay_case())?;
    if !same_fact_set(&expected_facts, &current_report.live_facts) {
        return Err(ShrinkCapturedReplayError::FactSetChanged {
            phase: "initial replay",
            expected: fact_lines(&expected_facts),
            actual: fact_lines(&current_report.live_facts),
        });
    }
    let mut index = 0;
    let mut attempts = 0;
    let mut fact_set_rejections = Vec::new();

    while index < current_capture.history.len() && attempts < config.max_attempts {
        let mut candidate_ops = current_capture.history.operations().to_vec();
        candidate_ops.remove(index);
        let candidate_history = current_capture.history.with_operations(candidate_ops);
        let mut candidate_capture = current_capture.clone();
        candidate_capture.history = candidate_history;
        let candidate_case = candidate_capture.to_replay_case();
        attempts += 1;

        let candidate_report = runner(&candidate_case)?;
        if still_fails(&candidate_report) {
            if !same_fact_set(&expected_facts, &candidate_report.live_facts) {
                fact_set_rejections.push(FactSetDelta {
                    deletion_index: index,
                    expected: fact_lines(&expected_facts),
                    actual: fact_lines(&candidate_report.live_facts),
                });
                index += 1;
                continue;
            }
            candidate_capture.expected.event_count = candidate_report.replay.event_count;
            candidate_capture.expected.trace_hash = candidate_report.replay.trace_hash;
            current_capture = candidate_capture;
            current_report = candidate_report;
            index = 0;
        } else {
            index += 1;
        }
    }

    current_capture.expected.event_count = current_report.replay.event_count;
    current_capture.expected.trace_hash = current_report.replay.trace_hash;

    Ok(ShrinkCapturedReport {
        original_len,
        shrunk_len: current_capture.history.len(),
        attempts,
        reason: reason.into(),
        shrunk_capture: current_capture,
        shrunk_report: current_report,
        fact_set_rejections,
    })
}

fn same_fact_set(left: &[LiveReplayFact], right: &[LiveReplayFact]) -> bool {
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

fn fact_lines(facts: &[LiveReplayFact]) -> Vec<String> {
    let mut left = facts.iter().map(ToString::to_string).collect::<Vec<_>>();
    left.sort();
    left
}
