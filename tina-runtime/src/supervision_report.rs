//! Supervision diagnostics.
//!
//! The trace already records every spawn, supervised restart trigger,
//! restart rejection, per-child restart attempt/skip/completion, and
//! isolate stop. This module folds those facts for one owner (the parent
//! isolate) into a typed terminal report, so host code asks "how did my
//! supervised children fare, and did the supervisor give up?" without
//! hand-rolling the match-and-count loop.
//!
//! Pure trace reader, like [`PressureSummary`](crate::PressureSummary):
//! it changes no runtime behavior and composes with the other report
//! readers over the same event slice.
//!
//! Cross-shard caveat: a child spawned on *another* shard via
//! `spawn_observed(child).on_shard(...)` records its `Spawned` fact on the
//! child's shard (under the child), not under the owner, so it does **not**
//! appear in `children_spawned` here. Same-shard children — including
//! `.on_shard(my_shard)` — are owned and counted normally.
//!
//! Scope: per-child entries appear only for children the supervisor
//! *acted on* — every restart attempt, skip, completion, and rejected
//! failure carries the stable per-parent `child_ordinal`. A child that
//! was spawned but never failed has no supervision history to report; it
//! contributes only to `children_spawned`. Read final per-child identity
//! from [`Runtime::child_record_snapshot`](crate::Runtime) when the live
//! records (not the trace history) are what you want.

use std::collections::BTreeMap;
use std::fmt;

use tina::{AddressGeneration, IsolateId};

use crate::trace::{
    RestartSkippedReason, RuntimeEvent, RuntimeEventKind, SupervisionRejectedReason,
};

/// Why a supervisor stopped restarting a failed child, when it did.
///
/// Distinct variants keep budget exhaustion and post-stop failure from
/// collapsing into one vague "closed": each names a different operator
/// fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisorHalt {
    /// The restart budget was exhausted. Carries the rejected restart
    /// ordinal and the configured maximum so the report states the final
    /// budget position, not just that it ran out.
    BudgetExhausted {
        /// The restart attempt number that was rejected.
        attempted_restart: u32,
        /// The configured maximum restarts for the budget window.
        max_restarts: u32,
    },

    /// A child failed after its supervisor parent had already stopped, so
    /// no replacement was created.
    SupervisorStopped,
}

/// Per-child supervision history, keyed by the stable per-parent ordinal.
///
/// `latest_isolate`/`latest_generation` track the newest incarnation the
/// trace named for this ordinal: the most recent restart completion's
/// replacement, or — when every attempt was skipped or rejected — the
/// incarnation that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChildSupervision {
    /// The stable per-parent child ordinal this entry describes.
    pub child_ordinal: usize,
    /// The newest incarnation isolate id the trace named for this ordinal.
    pub latest_isolate: IsolateId,
    /// The generation of that newest incarnation.
    pub latest_generation: AddressGeneration,
    /// Restart attempts that entered the restart path for this ordinal.
    pub restarts_attempted: u64,
    /// Replacement children successfully created for this ordinal.
    pub restarts_completed: u64,
    /// Restart attempts skipped because the child was not restartable.
    pub skipped_not_restartable: u64,
    /// Restart attempts skipped because the replacement factory panicked.
    pub skipped_factory_panicked: u64,
    /// The owner stopped this child via supervised shutdown
    /// ([`tina::Effect::StopChildren`]).
    pub stopped_by_owner: bool,
}

impl ChildSupervision {
    fn seed(child_ordinal: usize, isolate: IsolateId, generation: AddressGeneration) -> Self {
        Self {
            child_ordinal,
            latest_isolate: isolate,
            latest_generation: generation,
            restarts_attempted: 0,
            restarts_completed: 0,
            skipped_not_restartable: 0,
            skipped_factory_panicked: 0,
            stopped_by_owner: false,
        }
    }
}

/// Counted terminal summary of one owner's supervision activity.
///
/// Build with [`SupervisorReport::from_events`]. The per-child breakdown
/// is in [`Self::children`]; [`Self::halt`] is `Some` when the supervisor
/// gave up on a failed child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorReport {
    /// The owner (supervisor parent) this report describes.
    pub parent: IsolateId,
    /// `Spawned` events recorded under this owner.
    pub children_spawned: u64,
    /// Supervised restart responses that began (a failure matched a live
    /// supervisor with budget remaining).
    pub restarts_triggered: u64,
    /// Per-child restart attempts that entered the restart path.
    pub restarts_attempted: u64,
    /// Replacement children successfully created.
    pub restarts_completed: u64,
    /// Restart attempts skipped: child not restartable.
    pub skipped_not_restartable: u64,
    /// Restart attempts skipped: replacement factory panicked.
    pub skipped_factory_panicked: u64,
    /// Failures rejected because the restart budget was exhausted.
    pub rejected_budget_exceeded: u64,
    /// Failures rejected because the supervisor parent had stopped.
    pub rejected_supervisor_stopped: u64,
    /// Children the owner closed via supervised shutdown
    /// ([`tina::Effect::StopChildren`]).
    pub children_stopped: u64,
    /// The supervisor's final give-up reason, if it gave up. Reflects the
    /// last rejection seen in the slice.
    pub halt: Option<SupervisorHalt>,
    /// Per-child breakdown, ordered by `child_ordinal`.
    pub children: Vec<ChildSupervision>,
}

impl SupervisorReport {
    /// Walks `events` and folds the supervision facts recorded under
    /// `parent` into a report. Events for other owners are ignored.
    pub fn from_events<'a, I>(events: I, parent: IsolateId) -> Self
    where
        I: IntoIterator<Item = &'a RuntimeEvent>,
    {
        let mut children: BTreeMap<usize, ChildSupervision> = BTreeMap::new();
        let mut children_spawned = 0u64;
        let mut restarts_triggered = 0u64;
        let mut restarts_attempted = 0u64;
        let mut restarts_completed = 0u64;
        let mut skipped_not_restartable = 0u64;
        let mut skipped_factory_panicked = 0u64;
        let mut rejected_budget_exceeded = 0u64;
        let mut rejected_supervisor_stopped = 0u64;
        let mut children_stopped = 0u64;
        let mut halt = None;

        for event in events {
            if event.isolate() != parent {
                continue;
            }
            match event.kind() {
                RuntimeEventKind::Spawned { .. } => children_spawned += 1,
                RuntimeEventKind::SupervisorRestartTriggered { .. } => restarts_triggered += 1,
                RuntimeEventKind::SupervisorRestartRejected { reason, .. } => match reason {
                    SupervisionRejectedReason::BudgetExceeded {
                        attempted_restart,
                        max_restarts,
                    } => {
                        rejected_budget_exceeded += 1;
                        halt = Some(SupervisorHalt::BudgetExhausted {
                            attempted_restart,
                            max_restarts,
                        });
                    }
                    SupervisionRejectedReason::SupervisorStopped => {
                        rejected_supervisor_stopped += 1;
                        halt = Some(SupervisorHalt::SupervisorStopped);
                    }
                },
                RuntimeEventKind::RestartChildAttempted {
                    child_ordinal,
                    old_isolate,
                    old_generation,
                } => {
                    restarts_attempted += 1;
                    children
                        .entry(child_ordinal)
                        .or_insert_with(|| {
                            ChildSupervision::seed(child_ordinal, old_isolate, old_generation)
                        })
                        .restarts_attempted += 1;
                }
                RuntimeEventKind::RestartChildCompleted {
                    child_ordinal,
                    old_isolate,
                    old_generation,
                    new_isolate,
                    new_generation,
                } => {
                    restarts_completed += 1;
                    let child = children.entry(child_ordinal).or_insert_with(|| {
                        ChildSupervision::seed(child_ordinal, old_isolate, old_generation)
                    });
                    child.restarts_completed += 1;
                    child.latest_isolate = new_isolate;
                    child.latest_generation = new_generation;
                }
                RuntimeEventKind::RestartChildSkipped {
                    child_ordinal,
                    old_isolate,
                    old_generation,
                    reason,
                } => {
                    let child = children.entry(child_ordinal).or_insert_with(|| {
                        ChildSupervision::seed(child_ordinal, old_isolate, old_generation)
                    });
                    match reason {
                        RestartSkippedReason::NotRestartable => {
                            skipped_not_restartable += 1;
                            child.skipped_not_restartable += 1;
                        }
                        RestartSkippedReason::FactoryPanicked => {
                            skipped_factory_panicked += 1;
                            child.skipped_factory_panicked += 1;
                        }
                    }
                }
                RuntimeEventKind::ChildStopped {
                    child_ordinal,
                    child_isolate,
                    child_generation,
                } => {
                    children_stopped += 1;
                    let child = children.entry(child_ordinal).or_insert_with(|| {
                        ChildSupervision::seed(child_ordinal, child_isolate, child_generation)
                    });
                    child.stopped_by_owner = true;
                    child.latest_isolate = child_isolate;
                    child.latest_generation = child_generation;
                }
                _ => {}
            }
        }

        Self {
            parent,
            children_spawned,
            restarts_triggered,
            restarts_attempted,
            restarts_completed,
            skipped_not_restartable,
            skipped_factory_panicked,
            rejected_budget_exceeded,
            rejected_supervisor_stopped,
            children_stopped,
            halt,
            children: children.into_values().collect(),
        }
    }

    /// True if the supervisor recorded any restart, skip, or rejection.
    /// Spawns alone do not count as supervision activity.
    pub fn non_zero(&self) -> bool {
        self.restarts_triggered > 0
            || self.restarts_attempted > 0
            || self.restarts_completed > 0
            || self.skipped_not_restartable > 0
            || self.skipped_factory_panicked > 0
            || self.rejected_budget_exceeded > 0
            || self.rejected_supervisor_stopped > 0
            || self.children_stopped > 0
    }

    /// True if the supervisor stopped restarting a failed child.
    pub fn halted(&self) -> bool {
        self.halt.is_some()
    }
}

impl fmt::Display for SupervisorReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "supervisor parent={} spawned={} restarts[triggered={} attempted={} completed={}] \
             skipped[not_restartable={} factory_panicked={}] \
             rejected[budget={} supervisor_stopped={}] stopped_by_owner={} halt={}",
            self.parent.get(),
            self.children_spawned,
            self.restarts_triggered,
            self.restarts_attempted,
            self.restarts_completed,
            self.skipped_not_restartable,
            self.skipped_factory_panicked,
            self.rejected_budget_exceeded,
            self.rejected_supervisor_stopped,
            self.children_stopped,
            match self.halt {
                None => "none".to_string(),
                Some(SupervisorHalt::BudgetExhausted {
                    attempted_restart,
                    max_restarts,
                }) => format!("budget_exhausted({attempted_restart}/{max_restarts})"),
                Some(SupervisorHalt::SupervisorStopped) => "supervisor_stopped".to_string(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::{CauseId, EventId, RuntimeEvent};
    use tina::{RestartPolicy, ShardId};

    const PARENT: u64 = 7;
    const OTHER_PARENT: u64 = 8;

    fn event(id: u64, owner: u64, kind: RuntimeEventKind) -> RuntimeEvent {
        RuntimeEvent::new(
            EventId::new(id),
            Some(CauseId::from(EventId::new(0))),
            ShardId::new(0),
            IsolateId::new(owner),
            kind,
        )
    }

    fn isolate(raw: u64) -> IsolateId {
        IsolateId::new(raw)
    }

    fn generation(raw: u64) -> AddressGeneration {
        AddressGeneration::new(raw)
    }

    #[test]
    fn empty_trace_reports_quiet_supervisor() {
        let report = SupervisorReport::from_events([].iter(), isolate(PARENT));
        assert_eq!(report.parent, isolate(PARENT));
        assert!(!report.non_zero());
        assert!(!report.halted());
        assert!(report.children.is_empty());
    }

    #[test]
    fn report_ignores_events_for_other_owners() {
        let events = [
            event(
                1,
                OTHER_PARENT,
                RuntimeEventKind::Spawned {
                    child_isolate: isolate(100),
                },
            ),
            event(
                2,
                OTHER_PARENT,
                RuntimeEventKind::SupervisorRestartTriggered {
                    policy: RestartPolicy::OneForOne,
                    failed_child: isolate(100),
                    failed_ordinal: 0,
                },
            ),
        ];
        let report = SupervisorReport::from_events(events.iter(), isolate(PARENT));
        assert_eq!(report.children_spawned, 0);
        assert_eq!(report.restarts_triggered, 0);
        assert!(report.children.is_empty());
    }

    #[test]
    fn latest_incarnation_tracks_the_newest_completed_restart() {
        // Two completed restarts for the same ordinal: latest must be the
        // second replacement, not the first.
        let events = [
            event(
                1,
                PARENT,
                RuntimeEventKind::RestartChildAttempted {
                    child_ordinal: 0,
                    old_isolate: isolate(10),
                    old_generation: generation(0),
                },
            ),
            event(
                2,
                PARENT,
                RuntimeEventKind::RestartChildCompleted {
                    child_ordinal: 0,
                    old_isolate: isolate(10),
                    old_generation: generation(0),
                    new_isolate: isolate(11),
                    new_generation: generation(0),
                },
            ),
            event(
                3,
                PARENT,
                RuntimeEventKind::RestartChildAttempted {
                    child_ordinal: 0,
                    old_isolate: isolate(11),
                    old_generation: generation(0),
                },
            ),
            event(
                4,
                PARENT,
                RuntimeEventKind::RestartChildCompleted {
                    child_ordinal: 0,
                    old_isolate: isolate(11),
                    old_generation: generation(0),
                    new_isolate: isolate(12),
                    new_generation: generation(0),
                },
            ),
        ];
        let report = SupervisorReport::from_events(events.iter(), isolate(PARENT));
        assert_eq!(report.restarts_attempted, 2);
        assert_eq!(report.restarts_completed, 2);
        assert_eq!(report.children.len(), 1);
        let entry = report.children[0];
        assert_eq!(entry.child_ordinal, 0);
        assert_eq!(entry.restarts_completed, 2);
        assert_eq!(entry.latest_isolate, isolate(12));
    }

    #[test]
    fn skips_are_attributed_per_reason_and_per_child() {
        let events = [
            event(
                1,
                PARENT,
                RuntimeEventKind::RestartChildSkipped {
                    child_ordinal: 0,
                    old_isolate: isolate(10),
                    old_generation: generation(0),
                    reason: RestartSkippedReason::NotRestartable,
                },
            ),
            event(
                2,
                PARENT,
                RuntimeEventKind::RestartChildSkipped {
                    child_ordinal: 1,
                    old_isolate: isolate(20),
                    old_generation: generation(0),
                    reason: RestartSkippedReason::FactoryPanicked,
                },
            ),
        ];
        let report = SupervisorReport::from_events(events.iter(), isolate(PARENT));
        assert_eq!(report.skipped_not_restartable, 1);
        assert_eq!(report.skipped_factory_panicked, 1);
        assert_eq!(report.children.len(), 2);
        assert_eq!(report.children[0].child_ordinal, 0);
        assert_eq!(report.children[0].skipped_not_restartable, 1);
        assert_eq!(report.children[1].child_ordinal, 1);
        assert_eq!(report.children[1].skipped_factory_panicked, 1);
        // A skip is supervision activity even though no replacement ran.
        assert!(report.non_zero());
        assert!(!report.halted());
    }

    #[test]
    fn supervisor_stopped_rejection_halts_distinctly_from_budget() {
        let events = [event(
            1,
            PARENT,
            RuntimeEventKind::SupervisorRestartRejected {
                failed_child: isolate(10),
                failed_ordinal: 0,
                reason: SupervisionRejectedReason::SupervisorStopped,
            },
        )];
        let report = SupervisorReport::from_events(events.iter(), isolate(PARENT));
        assert_eq!(report.rejected_supervisor_stopped, 1);
        assert_eq!(report.rejected_budget_exceeded, 0);
        assert_eq!(report.halt, Some(SupervisorHalt::SupervisorStopped));
    }

    #[test]
    fn display_line_is_stable_key_value_shape() {
        let report = SupervisorReport::from_events(
            [event(
                1,
                PARENT,
                RuntimeEventKind::SupervisorRestartRejected {
                    failed_child: isolate(10),
                    failed_ordinal: 0,
                    reason: SupervisionRejectedReason::BudgetExceeded {
                        attempted_restart: 4,
                        max_restarts: 3,
                    },
                },
            )]
            .iter(),
            isolate(PARENT),
        );
        assert_eq!(
            report.to_string(),
            "supervisor parent=7 spawned=0 restarts[triggered=0 attempted=0 completed=0] \
             skipped[not_restartable=0 factory_panicked=0] \
             rejected[budget=1 supervisor_stopped=0] stopped_by_owner=0 halt=budget_exhausted(4/3)"
        );
    }

    #[test]
    fn child_stopped_by_owner_is_counted_and_named() {
        let events = [
            event(
                1,
                PARENT,
                RuntimeEventKind::ChildStopped {
                    child_ordinal: 0,
                    child_isolate: isolate(10),
                    child_generation: generation(0),
                },
            ),
            event(
                2,
                PARENT,
                RuntimeEventKind::ChildStopped {
                    child_ordinal: 1,
                    child_isolate: isolate(20),
                    child_generation: generation(0),
                },
            ),
        ];
        let report = SupervisorReport::from_events(events.iter(), isolate(PARENT));
        assert_eq!(report.children_stopped, 2);
        assert!(report.non_zero());
        assert_eq!(report.children.len(), 2);
        assert!(report.children[0].stopped_by_owner);
        assert_eq!(report.children[0].latest_isolate, isolate(10));
        assert!(report.children[1].stopped_by_owner);
        assert_eq!(report.children[1].latest_isolate, isolate(20));
    }
}
