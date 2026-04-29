use tina::{AddressGeneration, IsolateId, RestartPolicy, ShardId};

/// Stable identifier for one runtime event in a deterministic trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventId(u64);

impl EventId {
    /// Creates an event identifier from a raw integer.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw event identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Identifier used to point at the event that caused another event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CauseId(EventId);

impl CauseId {
    /// Creates a cause identifier from an event identifier.
    pub const fn new(event: EventId) -> Self {
        Self(event)
    }

    /// Returns the event identifier this cause points to.
    pub const fn event(self) -> EventId {
        self.0
    }
}

impl From<EventId> for CauseId {
    fn from(value: EventId) -> Self {
        Self(value)
    }
}

/// Trace-level view of a handler effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EffectKind {
    /// The handler returned [`tina::Effect::Noop`].
    Noop,

    /// The handler returned [`tina::Effect::Reply`].
    Reply,

    /// The handler returned [`tina::Effect::Send`].
    Send,

    /// The handler returned [`tina::Effect::Spawn`].
    Spawn,

    /// The handler returned [`tina::Effect::Stop`].
    Stop,

    /// The handler returned [`tina::Effect::RestartChildren`].
    RestartChildren,
}

/// Why a local send could not be enqueued into the target mailbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SendRejectedReason {
    /// The target mailbox was full.
    Full,

    /// The target mailbox was closed.
    Closed,
}

/// Why a child was not restarted after a restart attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RestartSkippedReason {
    /// The child was spawned from a one-shot [`tina::SpawnSpec`] and has no
    /// restart recipe.
    NotRestartable,
}

/// Why a supervised restart response did not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SupervisionRejectedReason {
    /// The restart budget was already exhausted.
    BudgetExceeded {
        /// The restart ordinal that was rejected.
        attempted_restart: u32,

        /// The configured maximum number of restarts for this runtime-lifetime
        /// budget window.
        max_restarts: u32,
    },

    /// The configured supervisor parent had already stopped.
    SupervisorStopped,
}

/// Kind of one runtime event emitted by the runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeEventKind {
    /// The runner accepted one message from a mailbox for delivery.
    MailboxAccepted,

    /// The runner began one handler invocation.
    HandlerStarted,

    /// The handler unwound with a panic instead of returning an effect.
    HandlerPanicked,

    /// The handler returned, including the effect kind it produced.
    HandlerFinished {
        /// The effect kind returned by the handler.
        effect: EffectKind,
    },

    /// The runner observed an effect without executing it.
    EffectObserved {
        /// The effect kind that was observed.
        effect: EffectKind,
    },

    /// The runtime tried to route a local send to another isolate on the same
    /// shard.
    SendDispatchAttempted {
        /// The destination shard.
        target_shard: ShardId,

        /// The destination isolate on that shard.
        target_isolate: IsolateId,

        /// The destination generation for that isolate.
        target_generation: AddressGeneration,
    },

    /// The runtime accepted a local send into the target mailbox.
    SendAccepted {
        /// The destination shard.
        target_shard: ShardId,

        /// The destination isolate on that shard.
        target_isolate: IsolateId,

        /// The destination generation for that isolate.
        target_generation: AddressGeneration,
    },

    /// The runtime rejected a local send.
    SendRejected {
        /// The destination shard.
        target_shard: ShardId,

        /// The destination isolate on that shard.
        target_isolate: IsolateId,

        /// The destination generation for that isolate.
        target_generation: AddressGeneration,

        /// Why the target mailbox rejected the send.
        reason: SendRejectedReason,
    },

    /// The runtime created one local child isolate from a spawn effect.
    Spawned {
        /// The isolate identifier assigned to the new child.
        child_isolate: IsolateId,
    },

    /// The runtime began a supervised restart response to a child panic.
    SupervisorRestartTriggered {
        /// The policy that selected child restart records.
        policy: RestartPolicy,

        /// The child whose panic triggered the supervised response.
        failed_child: IsolateId,

        /// The stable per-parent ordinal of the failed child record.
        failed_ordinal: usize,
    },

    /// The runtime rejected a supervised restart response.
    SupervisorRestartRejected {
        /// The child whose panic would have triggered the supervised response.
        failed_child: IsolateId,

        /// The stable per-parent ordinal of the failed child record.
        failed_ordinal: usize,

        /// Why the supervised response was rejected.
        reason: SupervisionRejectedReason,
    },

    /// The runtime began processing one child record for a restart request.
    RestartChildAttempted {
        /// Stable per-parent child ordinal.
        child_ordinal: usize,

        /// The child incarnation being replaced or skipped.
        old_isolate: IsolateId,

        /// The generation of the child incarnation being replaced or skipped.
        old_generation: AddressGeneration,
    },

    /// The runtime skipped one child during restart execution.
    RestartChildSkipped {
        /// Stable per-parent child ordinal.
        child_ordinal: usize,

        /// The child incarnation that was skipped.
        old_isolate: IsolateId,

        /// The generation of the child incarnation that was skipped.
        old_generation: AddressGeneration,

        /// Why the child was skipped.
        reason: RestartSkippedReason,
    },

    /// The runtime created a replacement child for one restartable child
    /// record.
    RestartChildCompleted {
        /// Stable per-parent child ordinal.
        child_ordinal: usize,

        /// The child incarnation that was replaced.
        old_isolate: IsolateId,

        /// The generation of the child incarnation that was replaced.
        old_generation: AddressGeneration,

        /// The fresh replacement isolate.
        new_isolate: IsolateId,

        /// The generation of the fresh replacement isolate.
        new_generation: AddressGeneration,
    },

    /// The runner applied the stopped state after observing [`tina::Effect::Stop`].
    IsolateStopped,

    /// The runtime drained one already-buffered message from a stopped
    /// isolate's mailbox without delivering it to the handler.
    MessageAbandoned,
}

/// One deterministic runtime event with a causal link to an earlier event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RuntimeEvent {
    id: EventId,
    cause: Option<CauseId>,
    shard: ShardId,
    isolate: IsolateId,
    kind: RuntimeEventKind,
}

impl RuntimeEvent {
    /// Creates a new runtime event.
    pub const fn new(
        id: EventId,
        cause: Option<CauseId>,
        shard: ShardId,
        isolate: IsolateId,
        kind: RuntimeEventKind,
    ) -> Self {
        Self {
            id,
            cause,
            shard,
            isolate,
            kind,
        }
    }

    /// Returns the event identifier.
    pub const fn id(self) -> EventId {
        self.id
    }

    /// Returns the optional cause identifier.
    pub const fn cause(self) -> Option<CauseId> {
        self.cause
    }

    /// Returns the shard that emitted the event.
    pub const fn shard(self) -> ShardId {
        self.shard
    }

    /// Returns the isolate that emitted the event.
    pub const fn isolate(self) -> IsolateId {
        self.isolate
    }

    /// Returns the event kind.
    pub const fn kind(self) -> RuntimeEventKind {
        self.kind
    }
}
