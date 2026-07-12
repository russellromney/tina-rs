//! Typed child lifecycle reports for runtime-owned local and remote children.

use std::error::Error;
use std::fmt;

use tina::{AddressGeneration, IsolateId, ShardId};

use crate::trace::{RestartSkippedReason, RuntimeEvent, RuntimeEventKind};
use crate::{RegisteredAddress, Runtime};
use tina::Shard;

/// Lifecycle state known for one child slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChildLifecycleState {
    /// Spawn has been requested but the owner has not learned an address yet.
    Starting,
    /// The current known incarnation is live.
    Live,
    /// A stop has been requested and a terminal report has not landed yet.
    Stopping,
    /// The child is stopped.
    Stopped,
    /// A restart was skipped.
    RestartSkipped,
    /// The child was restarted and has a replacement address.
    Restarted,
}

/// One child slot in a [`ChildLifecycleReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChildLifecycle {
    /// Stable per-parent ordinal.
    pub child_ordinal: usize,
    /// Current known shard.
    pub shard: ShardId,
    /// Current known isolate.
    pub isolate: IsolateId,
    /// Current known generation.
    pub generation: AddressGeneration,
    /// Current known state.
    pub state: ChildLifecycleState,
    /// Last restart skip reason, if any.
    pub last_restart_skipped: Option<RestartSkippedReason>,
}

/// Typed lifecycle report for direct children of one parent.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChildLifecycleReport {
    /// Parent isolate this report describes.
    pub parent: IsolateId,
    /// Direct child slots in ordinal order.
    pub children: Vec<ChildLifecycle>,
    /// Pending remote spawn/control messages owned by the parent.
    pub pending_remote_control_count: usize,
}

/// Why a live child lifecycle report could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildLifecycleReportError {
    /// The parent address belongs to another runtime/system incarnation.
    ForeignSystem {
        /// Incarnation owned by this runtime.
        expected: tina::SystemIncarnation,
        /// Incarnation carried by the address.
        actual: tina::SystemIncarnation,
    },
    /// The parent's shard is not owned by this runtime shell.
    ParentShardUnavailable(ShardId),
    /// The parent address is stale or stopped.
    ParentStopped,
}

impl fmt::Display for ChildLifecycleReportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignSystem { expected, actual } => write!(
                f,
                "parent belongs to foreign system {} (expected {})",
                actual.get(),
                expected.get()
            ),
            Self::ParentShardUnavailable(shard) => {
                write!(f, "parent shard {} is unavailable", shard.get())
            }
            Self::ParentStopped => write!(f, "parent is stopped or stale"),
        }
    }
}

impl Error for ChildLifecycleReportError {}

impl ChildLifecycleReport {
    pub(crate) fn from_runtime<S, F>(
        runtime: &Runtime<S, F>,
        parent: RegisteredAddress,
    ) -> Result<Self, ChildLifecycleReportError>
    where
        S: Shard,
        F: crate::mailbox::MailboxFactory,
    {
        if parent.system != runtime.system_incarnation {
            return Err(ChildLifecycleReportError::ForeignSystem {
                expected: runtime.system_incarnation,
                actual: parent.system,
            });
        }
        if parent.shard != runtime.shard.id() {
            return Err(ChildLifecycleReportError::ParentShardUnavailable(
                parent.shard,
            ));
        }
        let Some(parent_entry) = runtime.entry_by_isolate(parent.isolate) else {
            return Err(ChildLifecycleReportError::ParentStopped);
        };
        if parent_entry.generation != parent.generation || parent_entry.stopped.get() {
            return Err(ChildLifecycleReportError::ParentStopped);
        }

        let mut children: Vec<_> = runtime
            .child_records
            .iter()
            .filter(|record| record.parent == parent.isolate && record.remote_owner.is_none())
            .map(|record| {
                let mut child = ChildLifecycle {
                    child_ordinal: record.child_ordinal,
                    shard: record.child.shard,
                    isolate: record.child.isolate,
                    generation: record.child.generation,
                    state: ChildLifecycleState::Live,
                    last_restart_skipped: None,
                };
                apply_trace(runtime.trace().iter(), parent.isolate, &mut child);
                if record.terminal
                    || (child.shard == runtime.shard.id()
                        && runtime
                            .entry_index(record.child)
                            .is_some_and(|idx| runtime.entries[idx].stopped.get()))
                {
                    child.state = ChildLifecycleState::Stopped;
                }
                child
            })
            .collect();
        children.sort_by_key(|child| child.child_ordinal);

        let pending_remote_control_count = runtime
            .pending_remote_spawns
            .iter()
            .filter(|pending| pending.requester.isolate == parent.isolate)
            .count();

        Ok(Self {
            parent: parent.isolate,
            children,
            pending_remote_control_count,
        })
    }
}

fn apply_trace<'a>(
    events: impl IntoIterator<Item = &'a RuntimeEvent>,
    parent: IsolateId,
    child: &mut ChildLifecycle,
) {
    for event in events {
        if event.isolate() != parent {
            continue;
        }
        match event.kind() {
            RuntimeEventKind::RemoteChildStopRequested {
                child_ordinal,
                child_isolate,
                child_generation,
                ..
            }
            | RuntimeEventKind::ChildStopped {
                child_ordinal,
                child_isolate,
                child_generation,
            } if child_ordinal == child.child_ordinal
                && child_isolate == child.isolate
                && child_generation == child.generation =>
            {
                child.state = ChildLifecycleState::Stopping;
            }
            RuntimeEventKind::RemoteChildStopped {
                child_ordinal,
                child_isolate,
                child_generation,
                ..
            } if child_ordinal == child.child_ordinal
                && child_isolate == child.isolate
                && child_generation == child.generation =>
            {
                child.state = ChildLifecycleState::Stopped;
            }
            RuntimeEventKind::RestartChildSkipped {
                child_ordinal,
                old_isolate,
                old_generation,
                reason,
                ..
            } if child_ordinal == child.child_ordinal
                && old_isolate == child.isolate
                && old_generation == child.generation =>
            {
                child.state = ChildLifecycleState::RestartSkipped;
                child.last_restart_skipped = Some(reason);
            }
            RuntimeEventKind::RestartChildCompleted {
                child_ordinal,
                old_isolate,
                old_generation,
                new_isolate,
                new_generation,
                ..
            } if child_ordinal == child.child_ordinal => {
                if old_isolate == child.isolate && old_generation == child.generation {
                    child.isolate = new_isolate;
                    child.generation = new_generation;
                    child.state = ChildLifecycleState::Restarted;
                } else if new_isolate == child.isolate && new_generation == child.generation {
                    child.state = ChildLifecycleState::Restarted;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EventId, RuntimeEventKind};

    fn event(id: u64, parent: IsolateId, kind: RuntimeEventKind) -> RuntimeEvent {
        RuntimeEvent::new(EventId::new(id), None, ShardId::new(1), parent, kind)
    }

    fn child() -> ChildLifecycle {
        ChildLifecycle {
            child_ordinal: 0,
            shard: ShardId::new(2),
            isolate: IsolateId::new(10),
            generation: AddressGeneration::new(1),
            state: ChildLifecycleState::Live,
            last_restart_skipped: None,
        }
    }

    #[test]
    fn stale_old_incarnation_stop_does_not_clobber_restarted_child() {
        let parent = IsolateId::new(1);
        let mut child = child();
        let events = [
            event(
                1,
                parent,
                RuntimeEventKind::RestartChildCompleted {
                    child_ordinal: 0,
                    old_isolate: IsolateId::new(10),
                    old_generation: AddressGeneration::new(1),
                    new_isolate: IsolateId::new(11),
                    new_generation: AddressGeneration::new(2),
                },
            ),
            event(
                2,
                parent,
                RuntimeEventKind::RemoteChildStopped {
                    child_shard: ShardId::new(2),
                    child_ordinal: 0,
                    child_isolate: IsolateId::new(10),
                    child_generation: AddressGeneration::new(1),
                },
            ),
        ];

        apply_trace(events.iter(), parent, &mut child);

        assert_eq!(child.isolate, IsolateId::new(11));
        assert_eq!(child.generation, AddressGeneration::new(2));
        assert_eq!(child.state, ChildLifecycleState::Restarted);
    }

    #[test]
    fn current_incarnation_stop_after_restart_marks_replacement_stopped() {
        let parent = IsolateId::new(1);
        let mut child = child();
        let events = [
            event(
                1,
                parent,
                RuntimeEventKind::RestartChildCompleted {
                    child_ordinal: 0,
                    old_isolate: IsolateId::new(10),
                    old_generation: AddressGeneration::new(1),
                    new_isolate: IsolateId::new(11),
                    new_generation: AddressGeneration::new(2),
                },
            ),
            event(
                2,
                parent,
                RuntimeEventKind::RemoteChildStopped {
                    child_shard: ShardId::new(2),
                    child_ordinal: 0,
                    child_isolate: IsolateId::new(11),
                    child_generation: AddressGeneration::new(2),
                },
            ),
        ];

        apply_trace(events.iter(), parent, &mut child);

        assert_eq!(child.isolate, IsolateId::new(11));
        assert_eq!(child.generation, AddressGeneration::new(2));
        assert_eq!(child.state, ChildLifecycleState::Stopped);
    }
}
