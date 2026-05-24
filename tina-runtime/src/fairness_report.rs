//! Fairness diagnostics: per-isolate **progress counts**.
//!
//! The scheduler gives each ready isolate at most one handler turn per
//! delivery round (round-robin in registration order). That bounds how
//! far a hot, self-flooding isolate can pull ahead of a quiet one: a
//! continuously-ready isolate still earns one turn per round. This module
//! makes that progress *observable* by folding the trace into per-isolate
//! counts so a test or host can prove "the quiet actor kept moving" and
//! "the timer kept firing" instead of guessing from latency.
//!
//! Pure trace reader, like [`PressureSummary`](crate::PressureSummary)
//! and [`SupervisorReport`](crate::SupervisorReport): it changes no
//! scheduling and adds no events. Two trace facts carry the signal —
//! `HandlerStarted` (one per handler turn) and `CallCompleted` with
//! `CallKind::Sleep` (one per successful sleep). Both are already recorded
//! under the isolate they belong to.
//!
//! ## Scope (what this is and is not)
//!
//! This ships the *progress-count* slice of fairness: handler turns and
//! sleep completions, plus typed [`LagObservation`] and
//! [`StarvationWarning`] values. It is **not** the full lag/latency surface.
//! In particular:
//!
//! - `sleep_completions` counts every successful `Sleep` completion (a
//!   recurring timer's fires re-arm one-shot sleeps, so this is its fire
//!   count — but a single one-shot `sleep()` also counts as one). It is not
//!   a "missed ticks" or "late by" measure.
//! - Ready-turn lag (turns waited while ready), timer lateness in runtime
//!   time, and remote-drain yield counts are **not** implemented here — they
//!   need instrumentation the trace does not yet carry (a per-turn
//!   ready signal and event timestamps). They remain future work.
//! - [`LagObservation`] therefore names `progress_gap_turns`, not
//!   scheduler latency. It is a user-visible "one isolate made N more
//!   handler turns than another" fact folded from existing trace events.
//!
//! What this does *not* claim: wall-clock or real-time guarantees.
//! "Progress" here is turns taken and sleeps completed, which are
//! deterministic under the simulator and the single-shard runner. If a
//! caller's scenario can starve a victim (for example a handler that
//! monopolizes a turn with long synchronous work),
//! [`FairnessReport::starvation`] / [`FairnessReport::starvation_by_gap`]
//! report the bad condition by name rather than hiding it.

use std::collections::BTreeMap;
use std::fmt;

use tina::IsolateId;

use crate::trace::{CallKind, RuntimeEvent, RuntimeEventKind};

/// Per-isolate progress over the observed trace window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct IsolateProgress {
    /// The isolate this row describes.
    pub isolate: IsolateId,
    /// Handler turns taken (`HandlerStarted` events). One per delivered
    /// message the isolate actually handled.
    pub handler_turns: u64,
    /// Successful `Sleep` completions (`CallCompleted` with
    /// `CallKind::Sleep`). For a recurring timer this is its fire count;
    /// one-shot sleeps count too. Not a missed-ticks or late-by measure.
    pub sleep_completions: u64,
}

/// A named starvation condition: `victim` made far less progress than
/// `hot` over the same window. Returned by [`FairnessReport::starvation`]
/// so the bad case surfaces as a typed value, never a silent gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StarvationWarning {
    /// The isolate that fell behind.
    pub victim: IsolateId,
    /// The isolate that ran ahead.
    pub hot: IsolateId,
    /// Handler turns the victim took.
    pub victim_turns: u64,
    /// Handler turns the hot isolate took.
    pub hot_turns: u64,
    /// The floor the victim was expected to clear but did not.
    pub expected_min_victim_turns: u64,
}

/// Tina-visible fairness lag folded from existing trace facts.
///
/// This intentionally does **not** claim wall-clock latency. The only lag
/// kind currently reported is `progress_gap_turns`: the difference between
/// one isolate's handler turns and another's over the same trace window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LagObservation {
    /// Stable observation kind. Kept as a string so user-facing report
    /// lines do not need an enum formatter.
    pub kind: &'static str,
    /// The isolate that fell behind.
    pub subject: IsolateId,
    /// The isolate used as the comparison point.
    pub reference: IsolateId,
    /// Observed lag amount, in the units named by [`Self::kind`].
    pub observed: u64,
    /// Caller-supplied bound, in the same units. `None` means the caller
    /// asked only to report the observation, not to judge it.
    pub bound: Option<u64>,
}

impl LagObservation {
    /// True when the observation exceeds its bound.
    pub fn exceeded_bound(&self) -> bool {
        self.bound.is_some_and(|bound| self.observed > bound)
    }

    /// One-line key=value shape for specimen output.
    pub fn summary_line(&self) -> String {
        format!(
            "lag kind={} subject={} reference={} observed={} bound={} exceeded={}",
            self.kind,
            self.subject.get(),
            self.reference.get(),
            self.observed,
            self.bound
                .map(|bound| bound.to_string())
                .unwrap_or_else(|| "none".to_string()),
            self.exceeded_bound(),
        )
    }
}

/// Per-isolate fairness counts folded from a trace slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FairnessReport {
    /// Per-isolate progress rows, ordered by isolate id.
    pub isolates: Vec<IsolateProgress>,
}

impl FairnessReport {
    /// Walks `events` and counts handler turns and timer fires per
    /// isolate.
    pub fn from_events<'a, I>(events: I) -> Self
    where
        I: IntoIterator<Item = &'a RuntimeEvent>,
    {
        let mut rows: BTreeMap<IsolateId, (u64, u64)> = BTreeMap::new();
        for event in events {
            match event.kind() {
                RuntimeEventKind::HandlerStarted => {
                    rows.entry(event.isolate()).or_default().0 += 1;
                }
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::Sleep,
                    ..
                } => {
                    rows.entry(event.isolate()).or_default().1 += 1;
                }
                _ => {}
            }
        }
        Self {
            isolates: rows
                .into_iter()
                .map(
                    |(isolate, (handler_turns, sleep_completions))| IsolateProgress {
                        isolate,
                        handler_turns,
                        sleep_completions,
                    },
                )
                .collect(),
        }
    }

    /// Progress row for one isolate, if it took any turn or timer fire.
    pub fn progress(&self, isolate: IsolateId) -> Option<IsolateProgress> {
        self.isolates.iter().copied().find(|p| p.isolate == isolate)
    }

    /// Handler turns taken by `isolate` (zero if it never ran).
    pub fn turns(&self, isolate: IsolateId) -> u64 {
        self.progress(isolate).map_or(0, |p| p.handler_turns)
    }

    /// Successful sleep completions for `isolate` (zero if it armed none).
    pub fn sleep_completions(&self, isolate: IsolateId) -> u64 {
        self.progress(isolate).map_or(0, |p| p.sleep_completions)
    }

    /// Checks whether `victim` was starved relative to `hot`, against a
    /// caller-supplied floor.
    ///
    /// Returns a [`StarvationWarning`] when the victim cleared fewer than
    /// `expected_min_victim_turns` while the hot isolate ran at all. The
    /// floor is the caller's: it knows how many rounds it drove and what
    /// progress a fair scheduler owes a continuously-ready isolate. Under
    /// the round-robin runner a steadily-ready victim earns one turn per
    /// round, so the floor is normally the round count. When you do not
    /// track rounds, prefer [`Self::starvation_by_gap`], which compares the
    /// two isolates directly from data this report already holds.
    pub fn starvation(
        &self,
        victim: IsolateId,
        hot: IsolateId,
        expected_min_victim_turns: u64,
    ) -> Option<StarvationWarning> {
        let victim_turns = self.turns(victim);
        let hot_turns = self.turns(hot);
        if hot_turns > 0 && victim_turns < expected_min_victim_turns {
            Some(StarvationWarning {
                victim,
                hot,
                victim_turns,
                hot_turns,
                expected_min_victim_turns,
            })
        } else {
            None
        }
    }

    /// Checks starvation by the *gap* between two isolates, needing no
    /// external round count.
    ///
    /// Fires when the hot isolate ran ahead of the victim by more than
    /// `max_allowed_gap` turns (`hot_turns - victim_turns > max_allowed_gap`)
    /// while the hot isolate ran at all. Under the round-robin runner two
    /// continuously-ready isolates stay within one turn, so a gap of `1` is
    /// the natural threshold. The reported `expected_min_victim_turns` is
    /// `hot_turns - max_allowed_gap` (the floor the gap implies).
    pub fn starvation_by_gap(
        &self,
        victim: IsolateId,
        hot: IsolateId,
        max_allowed_gap: u64,
    ) -> Option<StarvationWarning> {
        let victim_turns = self.turns(victim);
        let hot_turns = self.turns(hot);
        if hot_turns > 0 && hot_turns.saturating_sub(victim_turns) > max_allowed_gap {
            Some(StarvationWarning {
                victim,
                hot,
                victim_turns,
                hot_turns,
                expected_min_victim_turns: hot_turns.saturating_sub(max_allowed_gap),
            })
        } else {
            None
        }
    }

    /// Reports the handler-turn progress gap between `reference` and
    /// `subject`.
    ///
    /// A positive observation means `reference` took more handler turns
    /// than `subject`. The caller decides whether that is bad by supplying
    /// an optional `bound`; this method only reports a Tina-visible fact.
    pub fn progress_gap(
        &self,
        subject: IsolateId,
        reference: IsolateId,
        bound: Option<u64>,
    ) -> LagObservation {
        let subject_turns = self.turns(subject);
        let reference_turns = self.turns(reference);
        LagObservation {
            kind: "progress_gap_turns",
            subject,
            reference,
            observed: reference_turns.saturating_sub(subject_turns),
            bound,
        }
    }

    /// Progress-gap observations for every isolate compared with `reference`.
    pub fn progress_gaps_from(
        &self,
        reference: IsolateId,
        bound: Option<u64>,
    ) -> Vec<LagObservation> {
        self.isolates
            .iter()
            .filter(|row| row.isolate != reference)
            .map(|row| self.progress_gap(row.isolate, reference, bound))
            .collect()
    }
}

impl fmt::Display for FairnessReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "fairness [")?;
        for (index, row) in self.isolates.iter().enumerate() {
            if index > 0 {
                write!(formatter, " ")?;
            }
            write!(
                formatter,
                "isolate={} turns={} sleeps={}",
                row.isolate.get(),
                row.handler_turns,
                row.sleep_completions,
            )?;
        }
        write!(formatter, "]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::CallId;
    use crate::trace::{CauseId, EventId, RuntimeEvent};
    use tina::ShardId;

    fn event(id: u64, isolate: u64, kind: RuntimeEventKind) -> RuntimeEvent {
        RuntimeEvent::new(
            EventId::new(id),
            Some(CauseId::from(EventId::new(0))),
            ShardId::new(0),
            IsolateId::new(isolate),
            kind,
        )
    }

    fn turn(id: u64, isolate: u64) -> RuntimeEvent {
        event(id, isolate, RuntimeEventKind::HandlerStarted)
    }

    fn sleep_fire(id: u64, isolate: u64) -> RuntimeEvent {
        event(
            id,
            isolate,
            RuntimeEventKind::CallCompleted {
                call_id: CallId::new(id),
                call_kind: CallKind::Sleep,
            },
        )
    }

    #[test]
    fn counts_turns_and_sleep_completions_per_isolate() {
        let events = [
            turn(1, 1),
            turn(2, 2),
            turn(3, 1),
            sleep_fire(4, 2),
            turn(5, 1),
        ];
        let report = FairnessReport::from_events(events.iter());
        assert_eq!(report.turns(IsolateId::new(1)), 3);
        assert_eq!(report.turns(IsolateId::new(2)), 1);
        assert_eq!(report.sleep_completions(IsolateId::new(2)), 1);
        assert_eq!(report.sleep_completions(IsolateId::new(1)), 0);
        assert_eq!(report.turns(IsolateId::new(99)), 0);
        assert!(report.progress(IsolateId::new(99)).is_none());
    }

    #[test]
    fn starvation_by_gap_needs_no_external_round_count() {
        // Hot 10, victim 2 -> gap 8 > 1 -> warns.
        let mut events = Vec::new();
        let mut id = 1;
        for _ in 0..10 {
            events.push(turn(id, 1));
            id += 1;
        }
        for _ in 0..2 {
            events.push(turn(id, 2));
            id += 1;
        }
        let report = FairnessReport::from_events(events.iter());
        let warning = report
            .starvation_by_gap(IsolateId::new(2), IsolateId::new(1), 1)
            .expect("gap of 8 must warn");
        assert_eq!(warning.victim_turns, 2);
        assert_eq!(warning.hot_turns, 10);
        // Within-one-turn (fair round-robin) does not warn.
        assert!(
            report
                .starvation_by_gap(IsolateId::new(1), IsolateId::new(2), 1)
                .is_none()
        );
    }

    #[test]
    fn fair_round_robin_yields_no_starvation_warning() {
        // Hot and victim each took one turn per round (5 rounds).
        let mut events = Vec::new();
        let mut id = 1;
        for _ in 0..5 {
            events.push(turn(id, 1));
            id += 1;
            events.push(turn(id, 2));
            id += 1;
        }
        let report = FairnessReport::from_events(events.iter());
        assert_eq!(report.turns(IsolateId::new(1)), 5);
        assert_eq!(report.turns(IsolateId::new(2)), 5);
        assert!(
            report
                .starvation(IsolateId::new(2), IsolateId::new(1), 5)
                .is_none()
        );
    }

    #[test]
    fn starved_victim_is_named_not_hidden() {
        // Hot ran 10 turns; victim ran 1 while we expected at least 8.
        let mut events = Vec::new();
        let mut id = 1;
        for _ in 0..10 {
            events.push(turn(id, 1));
            id += 1;
        }
        events.push(turn(id, 2));
        let report = FairnessReport::from_events(events.iter());
        let warning = report
            .starvation(IsolateId::new(2), IsolateId::new(1), 8)
            .expect("starvation must surface");
        assert_eq!(warning.victim, IsolateId::new(2));
        assert_eq!(warning.hot, IsolateId::new(1));
        assert_eq!(warning.victim_turns, 1);
        assert_eq!(warning.hot_turns, 10);
        assert_eq!(warning.expected_min_victim_turns, 8);
    }

    #[test]
    fn idle_hot_isolate_never_triggers_starvation() {
        // No hot turns at all -> nothing is starving anyone.
        let report = FairnessReport::from_events([].iter());
        assert!(
            report
                .starvation(IsolateId::new(2), IsolateId::new(1), 100)
                .is_none()
        );
    }

    #[test]
    fn progress_gap_reports_tina_visible_lag_without_latency_claims() {
        let events = [turn(1, 1), turn(2, 1), turn(3, 1), turn(4, 2)];
        let report = FairnessReport::from_events(events.iter());

        let lag = report.progress_gap(IsolateId::new(2), IsolateId::new(1), Some(1));
        assert_eq!(
            lag,
            LagObservation {
                kind: "progress_gap_turns",
                subject: IsolateId::new(2),
                reference: IsolateId::new(1),
                observed: 2,
                bound: Some(1),
            }
        );
        assert!(lag.exceeded_bound());
        assert_eq!(
            lag.summary_line(),
            "lag kind=progress_gap_turns subject=2 reference=1 observed=2 bound=1 exceeded=true"
        );
    }

    #[test]
    fn progress_gap_can_name_a_victim_that_never_ran() {
        let events = [turn(1, 1), turn(2, 1)];
        let report = FairnessReport::from_events(events.iter());

        let lag = report.progress_gap(IsolateId::new(99), IsolateId::new(1), Some(0));
        assert_eq!(lag.observed, 2);
        assert!(lag.exceeded_bound());
        assert_eq!(report.turns(IsolateId::new(99)), 0);
    }

    #[test]
    fn unbounded_progress_gap_reports_without_failing() {
        let events = [turn(1, 1), turn(2, 1), turn(3, 2)];
        let report = FairnessReport::from_events(events.iter());

        let lag = report.progress_gap(IsolateId::new(2), IsolateId::new(1), None);
        assert_eq!(lag.observed, 1);
        assert!(!lag.exceeded_bound());
        assert!(lag.summary_line().contains("bound=none"));
    }

    #[test]
    fn display_line_lists_each_isolate() {
        let events = [turn(1, 1), sleep_fire(2, 1), turn(3, 2)];
        let report = FairnessReport::from_events(events.iter());
        assert_eq!(
            report.to_string(),
            "fairness [isolate=1 turns=1 sleeps=1 isolate=2 turns=1 sleeps=0]"
        );
    }
}
