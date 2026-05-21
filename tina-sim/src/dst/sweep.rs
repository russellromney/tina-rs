//! Seed sweep helpers for DST.
//!
//! [`sweep_seeds`] runs the same case shape across a bounded set of
//! seeds and reports the first failure pasteable as a regression case,
//! or the count of seeds that passed.

use std::fmt::Debug;

use super::{ReplayCase, ReplayReport};

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
/// `run_case` is the same runner used with [`crate::dst::assert_replay_case`].
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
        assert_eq!(
            case.seed, seed,
            "sweep make_case returned case.seed {} for swept seed {}",
            case.seed, seed
        );
        assert_eq!(
            case.history.seed(),
            seed,
            "sweep make_case returned history.seed {} for swept seed {}",
            case.history.seed(),
            seed
        );
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
