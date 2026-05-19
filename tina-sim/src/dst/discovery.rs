//! Constant discovery helpers for DST.
//!
//! [`discover_constants`] runs one sweep across labelled cases and
//! reports the observed `(event_count, trace_hash)` rows so a
//! coding agent can copy them into `.expecting(...)` chains.

use super::{ReplayCase, ReplayReport, observe_replay_case};

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
