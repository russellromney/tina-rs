//! Overload bugbox helpers.
//!
//! This is a small user-facing layer over live replay capture. The
//! names match the workflow a service author wants:
//!
//! ```text
//! capture overload run -> save overload bug -> replay overload bug
//! ```
//!
//! The helpers do not claim arbitrary live traces are replayable. They
//! keep unsupported facts visible and let `check_captured_replay` reject
//! the case when the simulator did not reproduce the typed overload facts.

use std::fmt::Debug;
use std::path::{Path, PathBuf};

use tina::capacity::CapacitySurfaceReport;

use super::{
    CaptureSummary, CapturedReplayMismatch, LiveReplayCapture, LiveReplayCaptureBuilder,
    LiveReplayFact, LiveReplayReport, ReplayCase, SavedReplayCaseError, TraceProjectionError,
    capture_live_run, check_captured_replay, write_saved_replay_case,
};

/// Starts a live capture for an overload-shaped incident.
///
/// This is intentionally the same builder as [`capture_live_run`], with a
/// better copied name for pressure bugs. Add bounded facts with
/// `with_capacity_summary(...)`, `with_protocol_report(...)`, or
/// `with_unsupported_fact(...)`.
pub fn capture_overload_run<Op>(name: &'static str) -> LiveReplayCaptureBuilder<Op> {
    capture_live_run(name).with_source("overload bugbox")
}

/// One-line report returned after saving an overload bugbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverloadBugboxReport {
    /// Saved case path.
    pub path: PathBuf,
    /// Capture summary at save time.
    pub summary: CaptureSummary,
    /// Human line to print when a test catches an overload incident.
    pub replay_hint: String,
}

impl std::fmt::Display for OverloadBugboxReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "overload_bugbox path={} {} replay_hint={:?}",
            self.path.display(),
            self.summary,
            self.replay_hint
        )
    }
}

/// Saves a captured overload bugbox and returns a line users can paste into
/// logs or CI output.
pub fn save_overload_bug<Op, F>(
    path: impl AsRef<Path>,
    capture: &LiveReplayCapture<Op>,
    encode_op: F,
) -> Result<OverloadBugboxReport, SavedReplayCaseError>
where
    F: FnMut(&Op) -> String,
{
    let path = path.as_ref().to_path_buf();
    write_saved_replay_case(&path, capture, encode_op)?;
    let replay_hint = format!(
        "read_saved_replay_case({:?}) then replay_overload_bug(...)",
        path.display().to_string()
    );
    Ok(OverloadBugboxReport {
        path,
        summary: capture.summary(),
        replay_hint,
    })
}

/// Replays a captured overload bugbox against a candidate simulator case.
///
/// Unsupported live facts, missing capacity facts, config drift, history
/// drift, and trace-shape drift all remain typed
/// [`CapturedReplayMismatch`] values.
pub fn replay_overload_bug<Op, Output, Runner>(
    capture: &LiveReplayCapture<Op>,
    candidate: &ReplayCase<Op>,
    runner: Runner,
) -> Result<LiveReplayReport<Output>, Box<CapturedReplayMismatch<Op>>>
where
    Op: Clone + PartialEq,
    Runner: FnMut(&ReplayCase<Op>) -> Result<LiveReplayReport<Output>, TraceProjectionError>,
{
    check_captured_replay(capture, candidate, runner)
}

/// A typed pass report for overload/capacity assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverloadSurfaceAssertion {
    /// Surface name.
    pub surface: String,
    /// Whether a count cap was present and respected.
    pub count_bounded: bool,
    /// Whether a weight cap was present and respected.
    pub weight_bounded: bool,
    /// Whether a shared weight cap was present and respected.
    pub shared_weight_bounded: bool,
    /// Total visible `Full`/weight-full events on this surface.
    pub visible_full_count: u64,
}

impl OverloadSurfaceAssertion {
    fn from_report(report: &CapacitySurfaceReport) -> Self {
        Self {
            surface: report.name.clone(),
            count_bounded: report.max_messages.is_some(),
            weight_bounded: report.max_weight.is_some(),
            shared_weight_bounded: report.shared_max_weight.is_some(),
            visible_full_count: visible_full_count(report),
        }
    }
}

/// Why an overload assertion failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverloadAssertionError {
    /// No bounded count, weight, or shared-weight axis was configured.
    MissingConfiguredCap {
        /// Surface name.
        surface: String,
    },
    /// Observed high-water exceeded the configured cap.
    HiddenBuffering {
        /// Surface name.
        surface: String,
        /// Axis that exceeded its cap.
        axis: &'static str,
        /// Configured cap.
        cap: usize,
        /// Observed high-water.
        high_water: usize,
    },
    /// A test expected overload, but no visible `Full` fact was recorded.
    OverloadNotVisible {
        /// Surface name.
        surface: String,
    },
}

impl std::fmt::Display for OverloadAssertionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingConfiguredCap { surface } => {
                write!(f, "surface {surface:?} has no configured overload cap")
            }
            Self::HiddenBuffering {
                surface,
                axis,
                cap,
                high_water,
            } => write!(
                f,
                "surface {surface:?} buffered past {axis} cap {cap}: high_water={high_water}"
            ),
            Self::OverloadNotVisible { surface } => {
                write!(
                    f,
                    "surface {surface:?} expected overload but recorded no Full fact"
                )
            }
        }
    }
}

impl std::error::Error for OverloadAssertionError {}

/// Result-shaped check for hidden buffering.
///
/// The report must declare at least one hard cap, and every observed
/// high-water must stay within its configured cap.
pub fn check_no_hidden_buffering(
    report: &CapacitySurfaceReport,
) -> Result<OverloadSurfaceAssertion, OverloadAssertionError> {
    let mut bounded = false;
    if let Some(cap) = report.max_messages {
        bounded = true;
        if report.high_water_messages > cap || report.current_messages > cap {
            return Err(OverloadAssertionError::HiddenBuffering {
                surface: report.name.clone(),
                axis: "message",
                cap,
                high_water: report.high_water_messages.max(report.current_messages),
            });
        }
    }
    if let Some(cap) = report.max_weight {
        bounded = true;
        let high_water = report
            .high_water_weight
            .unwrap_or(report.current_weight.unwrap_or(0));
        let current = report.current_weight.unwrap_or(0);
        if high_water > cap || current > cap {
            return Err(OverloadAssertionError::HiddenBuffering {
                surface: report.name.clone(),
                axis: "weight",
                cap,
                high_water: high_water.max(current),
            });
        }
    }
    if let Some(cap) = report.shared_max_weight {
        bounded = true;
        let high_water = report
            .shared_high_water_weight
            .unwrap_or(report.shared_current_weight.unwrap_or(0));
        let current = report.shared_current_weight.unwrap_or(0);
        if high_water > cap || current > cap {
            return Err(OverloadAssertionError::HiddenBuffering {
                surface: report.name.clone(),
                axis: "shared_weight",
                cap,
                high_water: high_water.max(current),
            });
        }
    }
    if !bounded {
        return Err(OverloadAssertionError::MissingConfiguredCap {
            surface: report.name.clone(),
        });
    }
    Ok(OverloadSurfaceAssertion::from_report(report))
}

/// Panic sugar for tests that should fail loudly on hidden buffering.
pub fn assert_no_hidden_buffering(report: &CapacitySurfaceReport) -> OverloadSurfaceAssertion {
    check_no_hidden_buffering(report).unwrap_or_else(|error| panic!("{error}"))
}

/// Result-shaped check that an overload test recorded visible pressure.
pub fn check_overload_visible(
    report: &CapacitySurfaceReport,
) -> Result<OverloadSurfaceAssertion, OverloadAssertionError> {
    let assertion = check_no_hidden_buffering(report)?;
    if assertion.visible_full_count == 0 {
        return Err(OverloadAssertionError::OverloadNotVisible {
            surface: report.name.clone(),
        });
    }
    Ok(assertion)
}

/// Panic sugar for tests that expect visible overload.
pub fn assert_overload_visible(report: &CapacitySurfaceReport) -> OverloadSurfaceAssertion {
    check_overload_visible(report).unwrap_or_else(|error| panic!("{error}"))
}

fn visible_full_count(report: &CapacitySurfaceReport) -> u64 {
    report
        .full_count
        .saturating_add(report.weight_full_count)
        .saturating_add(report.shared_weight_full_count)
}

/// Turns a capacity report into a replay fact after proving it is bounded.
pub fn overload_capacity_fact(
    report: &CapacitySurfaceReport,
) -> Result<LiveReplayFact, OverloadAssertionError> {
    check_no_hidden_buffering(report)?;
    Ok(LiveReplayFact::capacity_surface(report))
}

#[cfg(test)]
mod tests {
    use tina::capacity::{CapacityMode, CapacitySurfaceReport};
    use tina_runtime::stable_trace_hash;

    use super::*;
    use crate::dst::{History, ReplayConfig, ReplayReport};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Op {
        Broadcast(usize),
        AcquirePool,
        Drain,
    }

    fn empty_trace_hash() -> u64 {
        stable_trace_hash(std::iter::empty::<&tina_runtime::RuntimeEvent>())
    }

    fn case(name: &'static str, ops: Vec<Op>) -> ReplayCase<Op> {
        ReplayCase {
            name,
            seed: 143,
            config: ReplayConfig::new()
                .with_mailbox("source", 4)
                .with_mailbox("sink", 4),
            scenario: "overload fact must be replayed from materialized history",
            history: History::new(name, 143, ops),
            expected_event_count: 0,
            expected_trace_hash: empty_trace_hash(),
            invariant: "overload facts are visible and bounded",
        }
    }

    fn report_with_fact(
        case: &ReplayCase<Op>,
        fact: LiveReplayFact,
    ) -> Result<LiveReplayReport<usize>, TraceProjectionError> {
        Ok(
            LiveReplayReport::exact(ReplayReport::from_case_and_events(case, &[], 1))
                .with_live_fact(fact),
        )
    }

    #[test]
    fn no_hidden_buffering_requires_a_real_cap() {
        let bounded = CapacitySurfaceReport::count("pool.waiters", CapacityMode::Fixed, 4, 1, 4, 0);
        let pass = check_no_hidden_buffering(&bounded).expect("bounded high-water passes");
        assert_eq!(pass.surface, "pool.waiters");
        assert!(pass.count_bounded);

        let too_high =
            CapacitySurfaceReport::count("pool.waiters", CapacityMode::Fixed, 4, 1, 5, 0);
        let error = check_no_hidden_buffering(&too_high).expect_err("hidden buffer is rejected");
        assert!(matches!(
            error,
            OverloadAssertionError::HiddenBuffering {
                axis: "message",
                ..
            }
        ));

        let mut missing =
            CapacitySurfaceReport::count("pool.waiters", CapacityMode::Fixed, 4, 0, 0, 0);
        missing.max_messages = None;
        let error = check_no_hidden_buffering(&missing).expect_err("missing cap is rejected");
        assert!(matches!(
            error,
            OverloadAssertionError::MissingConfiguredCap { .. }
        ));
    }

    #[test]
    fn overload_visible_requires_full_pressure() {
        let clean = CapacitySurfaceReport::count("chat.broadcast", CapacityMode::Fixed, 4, 0, 3, 0);
        let error = check_overload_visible(&clean).expect_err("no Full count means no overload");
        assert!(matches!(
            error,
            OverloadAssertionError::OverloadNotVisible { .. }
        ));

        let full = CapacitySurfaceReport::count("chat.broadcast", CapacityMode::Fixed, 4, 0, 4, 2);
        let pass = check_overload_visible(&full).expect("Full count is visible overload");
        assert_eq!(pass.visible_full_count, 2);
    }

    #[test]
    fn two_overload_saved_cases_replay_supported_facts() {
        let broadcast_surface =
            CapacitySurfaceReport::count("chat.room.broadcast", CapacityMode::Fixed, 2, 0, 2, 3);
        let pool_surface =
            CapacitySurfaceReport::count("db.pool.waiters", CapacityMode::Fixed, 4, 1, 4, 1);

        let cases = [
            (
                case(
                    "overload_broadcast_slow_peer",
                    vec![Op::Broadcast(3), Op::Drain],
                ),
                broadcast_surface,
            ),
            (
                case(
                    "overload_pool_waiters",
                    vec![Op::AcquirePool, Op::AcquirePool, Op::Drain],
                ),
                pool_surface,
            ),
        ];

        for (case, surface) in cases {
            assert_overload_visible(&surface);
            let fact = overload_capacity_fact(&surface).expect("bounded overload fact");
            let replay = ReplayReport::from_case_and_events(&case, &[], 1);
            let capture = LiveReplayCapture::from_case_and_report(&case, "overload unit", &replay)
                .with_live_fact(fact.clone());
            let candidate = capture.to_replay_case();
            let path = std::env::temp_dir().join(format!(
                "tina-overload-{}-{}.case",
                std::process::id(),
                case.name
            ));
            let saved =
                save_overload_bug(&path, &capture, |op| format!("{op:?}")).expect("save case");
            assert!(saved.replay_hint.contains(case.name));

            replay_overload_bug(&capture, &candidate, |case| {
                report_with_fact(case, fact.clone())
            })
            .expect("supported overload fact replays");
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn replay_overload_bug_fails_closed_on_unsupported_fact() {
        let surface =
            CapacitySurfaceReport::count("chat.room.broadcast", CapacityMode::Fixed, 2, 0, 2, 1);
        let fact = overload_capacity_fact(&surface).expect("bounded overload fact");
        let case = case(
            "overload_unsupported_fact",
            vec![Op::Broadcast(4), Op::Drain],
        );
        let replay = ReplayReport::from_case_and_events(&case, &[], 1);
        let capture = LiveReplayCapture::from_case_and_report(&case, "overload unit", &replay)
            .with_live_fact(fact.clone())
            .with_unsupported_fact(
                "kernel send buffer saturation",
                "sim history has no socket write-backpressure op",
            );
        let candidate = capture.to_replay_case();

        let mismatch = replay_overload_bug(&capture, &candidate, |case| {
            report_with_fact(case, fact.clone())
        })
        .expect_err("unsupported live facts fail closed");
        assert!(mismatch.to_string().contains("kernel send buffer"));
    }

    #[test]
    fn save_overload_bug_report_names_path_and_replay_hint() {
        let surface =
            CapacitySurfaceReport::count("db.pool.waiters", CapacityMode::Fixed, 4, 1, 4, 1);
        let fact = overload_capacity_fact(&surface).expect("bounded overload fact");
        let case = case("overload_save_hint", vec![Op::AcquirePool, Op::Drain]);
        let replay = ReplayReport::from_case_and_events(&case, &[], 1);
        let capture = LiveReplayCapture::from_case_and_report(&case, "overload unit", &replay)
            .with_live_fact(fact);
        let path = std::env::temp_dir().join(format!(
            "tina-overload-bugbox-{}-{}.case",
            std::process::id(),
            case.name
        ));

        let report = save_overload_bug(&path, &capture, |op| format!("{op:?}"))
            .expect("save overload bugbox");
        assert!(report.path.ends_with(path.file_name().expect("file name")));
        assert!(report.replay_hint.contains("replay_overload_bug"));
        assert!(report.replay_hint.contains(&path.display().to_string()));
        let _ = std::fs::remove_file(path);
    }
}
