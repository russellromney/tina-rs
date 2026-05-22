//! Fairness diagnostics.
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
//! `CallKind::Sleep` (one per timer fire) — both already recorded under
//! the isolate they belong to.
//!
//! What this does *not* claim: wall-clock or real-time guarantees.
//! "Progress" here is turns taken and timers fired, which are
//! deterministic under the simulator and the single-shard runner. If a
//! caller's scenario can starve a victim (for example a handler that
//! monopolizes a turn with long synchronous work), [`Self::starvation`]
//! reports the bad condition by name rather than hiding it.

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
    /// Timer fires received (`CallCompleted` with `CallKind::Sleep`).
    pub timer_ticks: u64,
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
                .map(|(isolate, (handler_turns, timer_ticks))| IsolateProgress {
                    isolate,
                    handler_turns,
                    timer_ticks,
                })
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

    /// Timer fires received by `isolate` (zero if it armed none).
    pub fn timer_ticks(&self, isolate: IsolateId) -> u64 {
        self.progress(isolate).map_or(0, |p| p.timer_ticks)
    }

    /// Checks whether `victim` was starved relative to `hot`.
    ///
    /// Returns a [`StarvationWarning`] when the victim cleared fewer than
    /// `expected_min_victim_turns` while the hot isolate ran at all. The
    /// floor is the caller's: it knows how many rounds it drove and what
    /// progress a fair scheduler owes a continuously-ready isolate. Under
    /// the round-robin runner a steadily-ready victim earns one turn per
    /// round, so the floor is normally the round count.
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
                "isolate={} turns={} timers={}",
                row.isolate.get(),
                row.handler_turns,
                row.timer_ticks,
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
    fn counts_turns_and_timer_ticks_per_isolate() {
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
        assert_eq!(report.timer_ticks(IsolateId::new(2)), 1);
        assert_eq!(report.timer_ticks(IsolateId::new(1)), 0);
        assert_eq!(report.turns(IsolateId::new(99)), 0);
        assert!(report.progress(IsolateId::new(99)).is_none());
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
    fn display_line_lists_each_isolate() {
        let events = [turn(1, 1), sleep_fire(2, 1), turn(3, 2)];
        let report = FairnessReport::from_events(events.iter());
        assert_eq!(
            report.to_string(),
            "fairness [isolate=1 turns=1 timers=1 isolate=2 turns=1 timers=0]"
        );
    }
}
