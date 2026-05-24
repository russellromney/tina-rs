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
    /// The parent's shard is not owned by this runtime shell.
    ParentShardUnavailable(ShardId),
    /// The parent address is stale or stopped.
    ParentStopped,
}

impl fmt::Display for ChildLifecycleReportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
                if child.shard == runtime.shard.id()
                    && runtime
                        .entry_index(record.child)
                        .is_some_and(|idx| runtime.entries[idx].stopped.get())
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
            RuntimeEventKind::RemoteChildStopRequested { child_ordinal, .. }
            | RuntimeEventKind::ChildStopped { child_ordinal, .. }
                if child_ordinal == child.child_ordinal =>
            {
                child.state = ChildLifecycleState::Stopping;
            }
            RuntimeEventKind::RemoteChildStopped { child_ordinal, .. }
                if child_ordinal == child.child_ordinal =>
            {
                child.state = ChildLifecycleState::Stopped;
            }
            RuntimeEventKind::RestartChildSkipped {
                child_ordinal,
                reason,
                ..
            } if child_ordinal == child.child_ordinal => {
                child.state = ChildLifecycleState::RestartSkipped;
                child.last_restart_skipped = Some(reason);
            }
            RuntimeEventKind::RestartChildCompleted {
                child_ordinal,
                new_isolate,
                new_generation,
                ..
            } if child_ordinal == child.child_ordinal => {
                child.isolate = new_isolate;
                child.generation = new_generation;
                child.state = ChildLifecycleState::Restarted;
            }
            _ => {}
        }
    }
}
