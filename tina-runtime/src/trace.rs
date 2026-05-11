use tina::{AddressGeneration, IsolateId, RestartPolicy, ShardId};

pub use crate::call::{CallError, CallId};

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

    /// The handler returned [`tina::Effect::StopWith`].
    ///
    /// Lifecycle is identical to [`Self::Stop`]; the distinction lets trace
    /// readers tell whether a host result waiter could be served.
    StopWith,

    /// The handler returned [`tina::Effect::RestartChildren`].
    RestartChildren,

    /// The handler returned [`tina::Effect::Call`].
    Call,

    /// The handler returned [`tina::Effect::Batch`].
    Batch,

    /// The handler returned [`tina::Effect::ReplyTo`] for a deferred reply slot.
    ReplyTo,
}

/// Why a deferred-reply attempt could not deliver to the original caller.
///
/// Duplicate replies and post-drop replies are prevented by the type
/// system: `reply_to` consumes the [`tina::DeferredReply`] handle, so a
/// second use is a compile error. There is therefore no
/// `AlreadyReplied` or `AlreadyDropped` runtime reason. See the
/// `compile_fail` doctest on [`tina::reply_to`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeferredReplyRejectedReason {
    /// The original caller already closed for a reason not classified
    /// below: e.g. requester shard failure, transport issue, or a
    /// pre-066 lifecycle path we have not yet split out.
    CallerClosed,

    /// Caller invoked `cancel_call(handle)`. Distinct from
    /// `CallerClosed` (lifecycle), `CallerTimedOut` (deadline),
    /// `OwnerStopped` (owner isolate stopped), and `RuntimeStopped`
    /// (runtime shutting down) so observers can attribute the cause.
    CallerCancelled,

    /// The mandatory call timeout fired before the reply arrived.
    /// Distinct from `CallerCancelled`: the user did not explicitly
    /// `cancel_call` — the deadline elapsed.
    CallerTimedOut,

    /// The owning isolate stopped while this call was pending.
    /// Distinct from `CallerCancelled` so traces can tell explicit
    /// cancel from owner lifecycle.
    OwnerStopped,

    /// The runtime began shutting down before the reply arrived.
    /// Distinct from `OwnerStopped` so traces can tell isolate-scoped
    /// teardown from process-scoped teardown.
    RuntimeStopped,

    /// The bounded reply transport path back to the requester was full.
    ReplyPathFull,

    /// The requester shard was closed before the reply could be routed.
    RequesterShardClosed,

    /// The reply payload type did not match the dispatching
    /// `Address<_, R>`'s `R`. Surfaces when a handler captured a
    /// slot for the wrong isolate type or rebuilt a slot with the
    /// wrong reply parameter via `runtime_internal`. The original
    /// caller's translator would have panicked on downcast; this
    /// reject defangs the panic and drops the payload.
    TypeMismatch,
}

/// Trace-level kind of a runtime-owned call.
///
/// `tina-runtime`'s call vocabulary lives in
/// [`crate::call::CallInput`]; this enum exists so the trace can name the
/// shape of a call without surfacing the full request payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallKind {
    /// A TCP listener bind.
    TcpBind,

    /// A TCP listener accept.
    TcpAccept,

    /// An outbound TCP connect.
    TcpConnect,

    /// A TCP stream read.
    TcpRead,

    /// A TCP stream write.
    TcpWrite,

    /// A TCP listener close.
    TcpListenerClose,

    /// A TCP stream close.
    TcpStreamClose,

    /// A UDP socket bind.
    UdpBind,

    /// A UDP datagram send.
    UdpSendTo,

    /// A UDP datagram receive.
    UdpRecvFrom,

    /// A UDP socket close.
    UdpSocketClose,

    /// A TLS connect and handshake.
    TlsConnect,

    /// A TLS listener bind.
    TlsBind,

    /// A TLS server accept and handshake.
    TlsAccept,

    /// A TLS listener close.
    TlsListenerClose,

    /// A TLS read.
    TlsRead,

    /// A TLS write.
    TlsWrite,

    /// A TLS close.
    TlsClose,

    /// A DNS lookup.
    DnsLookup,

    /// A runtime-owned signal wait.
    SignalWait,

    /// A bounded local process run.
    ProcessRun,

    /// A file open.
    FileOpen,

    /// A positional file read.
    FileReadAt,

    /// A positional file write.
    FileWriteAt,

    /// A file fsync.
    FileFsync,

    /// A file size query.
    FileSize,

    /// A file close.
    FileClose,

    /// A directory create.
    Mkdir,

    /// A path metadata query.
    PathMetadata,

    /// A rename-replace operation.
    RenameReplace,

    /// A remove-file operation.
    RemoveFile,

    /// A read-directory operation.
    ReadDir,

    /// A parent-directory sync operation.
    SyncParent,

    /// A local snapshot commit.
    SnapshotCommit,

    /// A local snapshot load.
    SnapshotLoad,

    /// A local journal append.
    JournalAppend,

    /// A local journal replay.
    JournalReplay,

    /// A one-shot relative sleep timer.
    Sleep,

    /// A runtime-observed send attempt whose outcome is delivered back to
    /// the requester as an ordinary later message.
    ObservedSend,

    /// An isolate-to-isolate call whose reply is delivered back to the
    /// requester as an ordinary later message.
    IsolateCall,

    /// A caller-issued request to cancel one pending isolate call's wait.
    /// The runtime delivers a [`tina::CancelOutcome`] back to the
    /// requester as an ordinary later message.
    CancelCall,
}

/// Why a runtime-owned call's completion could not be delivered to the
/// requesting isolate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallCompletionRejectedReason {
    /// The requesting isolate's mailbox was full when the runtime tried to
    /// enqueue the completion.
    MailboxFull,

    /// The requesting isolate had stopped (or its incarnation was
    /// replaced) by the time the completion arrived.
    RequesterClosed,

    /// User code closed the resource (TCP listener/stream, UDP socket)
    /// while this call was pending. The call is silently cancelled;
    /// the caller's continuation never fires.
    ResourceClosed,
}

/// Why a reply from a callee could not complete its original isolate call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallReplyRejectedReason {
    /// The call was no longer pending by the time the callee replied.
    ///
    /// Used as the fall-through when no more specific cause is on
    /// record — for example, after the bounded recently-cancelled
    /// ring evicts the call_id. The more specific
    /// `CallerCancelled` / `CallerTimedOut` / `OwnerStopped` /
    /// `RuntimeStopped` reasons take precedence when known.
    NoPendingCall,

    /// The original caller explicitly cancelled the wait via
    /// `cancel_call(handle)` before the reply arrived.
    CallerCancelled,

    /// The mandatory call timeout fired before the reply arrived.
    /// Distinct from `CallerCancelled` (explicit) and
    /// `NoPendingCall` (no record).
    CallerTimedOut,

    /// The owning isolate stopped while this call was pending.
    OwnerStopped,

    /// The runtime began shutting down before the reply arrived.
    RuntimeStopped,

    /// The bounded reply transport path back to the requester was full.
    ReplyPathFull,

    /// The requester shard was closed or failed before the reply could be routed.
    RequesterShardClosed,
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
    /// The child was spawned from a one-shot [`tina::ChildDefinition`] and has no
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

    /// The runtime accepted a runtime-owned call request from an isolate.
    CallDispatchAttempted {
        /// The runtime-assigned identifier for the call.
        call_id: CallId,

        /// The trace-level kind of the call.
        call_kind: CallKind,
    },

    /// The runtime delivered a successful call completion back to the
    /// requesting isolate as an ordinary later-turn message.
    ///
    /// This event fires only when the underlying
    /// [`crate::CallOutput`] is a success. Failed results are
    /// recorded by `CallFailed`; if a failed result's translated
    /// message also could not reach the requester's mailbox a
    /// `CallCompletionRejected` event captures that, and
    /// `CallCompleted` is *not* emitted in either case.
    CallCompleted {
        /// The runtime-assigned identifier for the call.
        call_id: CallId,

        /// The trace-level kind of the call.
        call_kind: CallKind,
    },

    /// The runtime observed a failure outcome for a runtime-owned call.
    /// The translated failure result was still delivered to the
    /// requesting isolate; this event records the reason.
    CallFailed {
        /// The runtime-assigned identifier for the call.
        call_id: CallId,

        /// The trace-level kind of the call.
        call_kind: CallKind,

        /// Why the call failed.
        reason: CallError,
    },

    /// The runtime could not deliver a completion to the requesting
    /// isolate.
    CallCompletionRejected {
        /// The runtime-assigned identifier for the call.
        call_id: CallId,

        /// The trace-level kind of the call.
        call_kind: CallKind,

        /// Why the completion could not be delivered.
        reason: CallCompletionRejectedReason,
    },

    /// The runtime observed a reply from a callee, but the original
    /// isolate-to-isolate call was no longer pending.
    CallReplyRejected {
        /// The runtime-assigned identifier for the original call.
        call_id: CallId,

        /// Why the reply could not settle the call.
        reason: CallReplyRejectedReason,
    },

    /// The runtime closed the caller-side wait of an in-flight isolate
    /// call because of an explicit cancel, owner stop, or other
    /// lifecycle event named by `cause`.
    ///
    /// The callee may still finish work it already accepted; any reply
    /// that arrives later is rejected as a separate
    /// `CallReplyRejected` (or `DeferredReplyRejected`) event with
    /// reason `CallerCancelled` / `CallerClosed`. This event records
    /// the cancel itself.
    CallCancelled {
        /// The runtime-assigned identifier for the cancelled call.
        call_id: CallId,

        /// Why the caller-side wait closed.
        cause: tina::CancelCause,
    },

    /// A local snapshot was committed.
    SnapshotCommitted,

    /// A local snapshot commit failed.
    SnapshotCommitFailed {
        /// Why the snapshot commit failed.
        reason: CallError,
    },

    /// A local journal record was appended.
    JournalAppended {
        /// Appended journal record index.
        record_index: u64,
    },

    /// A local journal append failed.
    JournalAppendFailed {
        /// Journal record index that failed to append.
        record_index: u64,
        /// Why the journal append failed.
        reason: CallError,
    },

    /// A persistence recovery call started.
    RecoveryStarted,

    /// A persistence recovery call finished.
    RecoveryFinished,

    /// A persistence recovery call failed.
    RecoveryFailed {
        /// Why recovery failed.
        reason: CallError,
    },

    /// A handler captured the current caller promise as a deferred reply slot.
    ///
    /// The `call_id` matches the original caller's pending isolate call so
    /// trace consumers can correlate capture, send, drop, and rejection
    /// against the original `CallDispatchAttempted`.
    DeferredReplyCaptured {
        /// Runtime-assigned slot identifier.
        slot_id: DeferredSlotId,
        /// The original isolate call this slot will eventually answer.
        call_id: CallId,
    },

    /// A `reply_to(slot, value)` effect successfully routed a reply through
    /// the slot to the original caller.
    DeferredReplySent {
        /// Runtime-assigned slot identifier.
        slot_id: DeferredSlotId,
        /// The original isolate call that was answered.
        call_id: CallId,
    },

    /// A `reply_to(slot, value)` effect could not deliver a reply.
    DeferredReplyRejected {
        /// Runtime-assigned slot identifier.
        slot_id: DeferredSlotId,
        /// The original isolate call this slot referenced.
        call_id: CallId,
        /// Why the reply could not be delivered.
        reason: DeferredReplyRejectedReason,
    },

    /// A live deferred slot was dropped without ever being replied to.
    DeferredReplyDropped {
        /// Runtime-assigned slot identifier.
        slot_id: DeferredSlotId,
        /// The original isolate call this slot referenced.
        call_id: CallId,
    },
}

/// Stable identifier for one runtime-issued deferred reply slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeferredSlotId(u64);

impl DeferredSlotId {
    /// Creates a slot identifier from a raw integer.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw slot identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
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

    /// Returns a deterministic 64-bit fingerprint of this runtime event.
    ///
    /// The hash is stable for the Tina trace semantics in this version of the
    /// crate: same inputs (same seed, same workload) produce the same value,
    /// and runs whose traces differ in any deterministic field (id, cause,
    /// shard, isolate, kind, kind payload) produce different values with
    /// extremely high probability. This is **not** a forever external wire
    /// format. Cross-version stability is not promised; consumers that need
    /// long-term persistence should serialize the event explicitly.
    ///
    /// `stable_hash` is intentionally independent of `std::hash::Hash` and
    /// `DefaultHasher`: the variant tags are assigned explicitly so that
    /// adding a new variant in the middle of the enum never silently changes
    /// the fingerprint of existing variants. The hash is FNV-1a over a
    /// canonical byte walk of every field; field order is part of the
    /// contract.
    pub fn stable_hash(&self) -> u64 {
        let mut hasher = StableHasher::new();
        self.write_stable(&mut hasher);
        hasher.finish()
    }

    fn write_stable(&self, hasher: &mut StableHasher) {
        hasher.write_u64(self.id.get());
        match self.cause {
            None => hasher.write_u8(0),
            Some(cause) => {
                hasher.write_u8(1);
                hasher.write_u64(cause.event().get());
            }
        }
        hasher.write_u32(self.shard.get());
        hasher.write_u64(self.isolate.get());
        write_kind_stable(self.kind, hasher);
    }
}

/// Convenience queries over runtime traces.
///
/// These helpers summarize facts that are already present in the trace. They
/// do not hide call outcomes or infer missing causality; they just replace the
/// repeated `matches!(event.kind(), ...)` scans that show up in tests and
/// specimen harnesses.
pub trait RuntimeTraceExt {
    /// Counts runtime-owned calls of `call_kind` that completed successfully.
    fn count_completed(&self, call_kind: CallKind) -> usize;

    /// Returns true when any runtime-owned call of `call_kind` completed
    /// successfully.
    fn any_completed(&self, call_kind: CallKind) -> bool {
        self.count_completed(call_kind) > 0
    }

    /// Counts runtime-owned calls of `call_kind` that failed before their
    /// translated continuation was delivered.
    fn count_failed(&self, call_kind: CallKind) -> usize;

    /// Returns true when any runtime-owned call of `call_kind` failed before
    /// its translated continuation was delivered.
    fn any_failed(&self, call_kind: CallKind) -> bool {
        self.count_failed(call_kind) > 0
    }

    /// Counts failed runtime-owned calls of `call_kind` whose reason matches
    /// `predicate`.
    fn count_failed_with(
        &self,
        call_kind: CallKind,
        predicate: impl FnMut(CallError) -> bool,
    ) -> usize;

    /// Returns true when any failed runtime-owned call of `call_kind` has a
    /// reason matching `predicate`.
    fn any_failed_with(
        &self,
        call_kind: CallKind,
        predicate: impl FnMut(CallError) -> bool,
    ) -> bool {
        self.count_failed_with(call_kind, predicate) > 0
    }

    /// Counts runtime-owned call completions of `call_kind` that could not be
    /// delivered back to their requester.
    fn count_completion_rejected(&self, call_kind: CallKind) -> usize;

    /// Returns true when any runtime-owned call completion of `call_kind`
    /// could not be delivered back to its requester.
    fn any_completion_rejected(&self, call_kind: CallKind) -> bool {
        self.count_completion_rejected(call_kind) > 0
    }

    /// Counts events whose kind satisfies `predicate`.
    fn count_matching(&self, predicate: impl FnMut(RuntimeEventKind) -> bool) -> usize;

    /// Returns true when any event's kind satisfies `predicate`.
    fn any_matching(&self, predicate: impl FnMut(RuntimeEventKind) -> bool) -> bool {
        self.count_matching(predicate) > 0
    }

    /// Counts `Spawned` events.
    fn count_spawned(&self) -> usize;

    /// Returns true when any isolate was spawned.
    fn any_spawned(&self) -> bool {
        self.count_spawned() > 0
    }

    /// Counts runtime-owned calls of `call_kind` that were dispatched to
    /// the driver lane.
    fn count_call_dispatched(&self, call_kind: CallKind) -> usize;

    /// Returns true when any runtime-owned call of `call_kind` was
    /// dispatched to the driver lane.
    fn any_call_dispatched(&self, call_kind: CallKind) -> bool {
        self.count_call_dispatched(call_kind) > 0
    }
}

impl RuntimeTraceExt for [RuntimeEvent] {
    fn count_completed(&self, call_kind: CallKind) -> usize {
        self.iter()
            .filter(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::CallCompleted {
                        call_kind: actual,
                        ..
                    } if actual == call_kind
                )
            })
            .count()
    }

    fn count_failed(&self, call_kind: CallKind) -> usize {
        self.iter()
            .filter(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::CallFailed {
                        call_kind: actual,
                        ..
                    } if actual == call_kind
                )
            })
            .count()
    }

    fn count_failed_with(
        &self,
        call_kind: CallKind,
        mut predicate: impl FnMut(CallError) -> bool,
    ) -> usize {
        self.iter()
            .filter(|event| match event.kind() {
                RuntimeEventKind::CallFailed {
                    call_kind: actual,
                    reason,
                    ..
                } => actual == call_kind && predicate(reason),
                _ => false,
            })
            .count()
    }

    fn count_completion_rejected(&self, call_kind: CallKind) -> usize {
        self.iter()
            .filter(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::CallCompletionRejected {
                        call_kind: actual,
                        ..
                    } if actual == call_kind
                )
            })
            .count()
    }

    fn count_matching(&self, mut predicate: impl FnMut(RuntimeEventKind) -> bool) -> usize {
        self.iter().filter(|event| predicate(event.kind())).count()
    }

    fn count_spawned(&self) -> usize {
        self.iter()
            .filter(|event| matches!(event.kind(), RuntimeEventKind::Spawned { .. }))
            .count()
    }

    fn count_call_dispatched(&self, call_kind: CallKind) -> usize {
        self.iter()
            .filter(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::CallDispatchAttempted {
                        call_kind: actual,
                        ..
                    } if actual == call_kind
                )
            })
            .count()
    }

    fn any_matching(&self, mut predicate: impl FnMut(RuntimeEventKind) -> bool) -> bool {
        self.iter().any(|event| predicate(event.kind()))
    }
}

/// Stable, allocation-free FNV-1a hasher used for trace fingerprints.
///
/// FNV-1a is chosen over `std::hash::DefaultHasher` because its bytewise
/// behavior is documented and unchanging, which lets `stable_hash` be a
/// real contract instead of an implementation detail of stdlib.
struct StableHasher {
    state: u64,
}

impl StableHasher {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;

    fn new() -> Self {
        Self {
            state: Self::OFFSET_BASIS,
        }
    }

    fn write_u8(&mut self, byte: u8) {
        self.state ^= byte as u64;
        self.state = self.state.wrapping_mul(Self::PRIME);
    }

    fn write_u32(&mut self, value: u32) {
        for shift in 0..4 {
            self.write_u8((value >> (shift * 8)) as u8);
        }
    }

    fn write_u64(&mut self, value: u64) {
        for shift in 0..8 {
            self.write_u8((value >> (shift * 8)) as u8);
        }
    }

    fn finish(&self) -> u64 {
        self.state
    }
}

/// Stable variant tag for [`RestartPolicy`].
fn restart_policy_tag(policy: RestartPolicy) -> u8 {
    match policy {
        RestartPolicy::OneForOne => 1,
        RestartPolicy::OneForAll => 2,
        RestartPolicy::RestForOne => 3,
    }
}

/// Stable variant tag for [`EffectKind`].
fn effect_kind_tag(effect: EffectKind) -> u8 {
    match effect {
        EffectKind::Noop => 1,
        EffectKind::Reply => 2,
        EffectKind::Send => 3,
        EffectKind::Spawn => 4,
        EffectKind::Stop => 5,
        EffectKind::RestartChildren => 6,
        EffectKind::Call => 7,
        EffectKind::Batch => 8,
        EffectKind::StopWith => 9,
        EffectKind::ReplyTo => 10,
    }
}

fn deferred_reply_rejected_tag(reason: DeferredReplyRejectedReason) -> u8 {
    match reason {
        DeferredReplyRejectedReason::CallerClosed => 1,
        DeferredReplyRejectedReason::ReplyPathFull => 2,
        DeferredReplyRejectedReason::RequesterShardClosed => 3,
        DeferredReplyRejectedReason::TypeMismatch => 4,
        DeferredReplyRejectedReason::CallerCancelled => 5,
        DeferredReplyRejectedReason::CallerTimedOut => 6,
        DeferredReplyRejectedReason::OwnerStopped => 7,
        DeferredReplyRejectedReason::RuntimeStopped => 8,
    }
}

/// Stable variant tag for [`CallKind`].
fn call_kind_tag(kind: CallKind) -> u8 {
    match kind {
        CallKind::TcpBind => 1,
        CallKind::TcpAccept => 2,
        CallKind::TcpConnect => 3,
        CallKind::TcpRead => 4,
        CallKind::TcpWrite => 5,
        CallKind::TcpListenerClose => 6,
        CallKind::TcpStreamClose => 7,
        CallKind::UdpBind => 8,
        CallKind::UdpSendTo => 9,
        CallKind::UdpRecvFrom => 10,
        CallKind::UdpSocketClose => 11,
        CallKind::TlsConnect => 12,
        CallKind::TlsBind => 13,
        CallKind::TlsAccept => 14,
        CallKind::TlsListenerClose => 15,
        CallKind::TlsRead => 16,
        CallKind::TlsWrite => 17,
        CallKind::TlsClose => 18,
        CallKind::DnsLookup => 19,
        CallKind::SignalWait => 20,
        CallKind::ProcessRun => 21,
        CallKind::FileOpen => 22,
        CallKind::FileReadAt => 23,
        CallKind::FileWriteAt => 24,
        CallKind::FileFsync => 25,
        CallKind::FileSize => 26,
        CallKind::FileClose => 27,
        CallKind::Mkdir => 28,
        CallKind::PathMetadata => 29,
        CallKind::RenameReplace => 30,
        CallKind::RemoveFile => 31,
        CallKind::ReadDir => 32,
        CallKind::SyncParent => 33,
        CallKind::SnapshotCommit => 34,
        CallKind::SnapshotLoad => 35,
        CallKind::JournalAppend => 36,
        CallKind::JournalReplay => 37,
        CallKind::Sleep => 38,
        CallKind::ObservedSend => 39,
        CallKind::IsolateCall => 40,
        CallKind::CancelCall => 41,
    }
}

/// Stable variant tag for [`CallError`].
fn call_error_tag(error: CallError) -> u8 {
    match error {
        CallError::InvalidResource => 1,
        CallError::NotFound => 2,
        CallError::Io => 3,
        CallError::Unsupported => 4,
        CallError::ResourceBusy => 5,
        CallError::CorruptRecord => 6,
        CallError::CommitUncertain => 7,
        CallError::StorageFull => 8,
        CallError::StorageClosed => 9,
        CallError::TargetFull => 10,
        CallError::TargetClosed => 11,
        CallError::Timeout => 12,
        CallError::DnsFull => 13,
        CallError::DnsClosed => 14,
        CallError::TlsFull => 15,
        CallError::TlsClosed => 16,
        CallError::TlsCertificate => 17,
        CallError::TlsName => 18,
        CallError::TlsHandshake => 19,
        CallError::SignalFull => 20,
        CallError::SignalClosed => 21,
        CallError::ProcessFull => 22,
        CallError::ProcessClosed => 23,
        CallError::KillUncertain => 24,
    }
}

fn send_rejected_tag(reason: SendRejectedReason) -> u8 {
    match reason {
        SendRejectedReason::Full => 1,
        SendRejectedReason::Closed => 2,
    }
}

fn restart_skipped_tag(reason: RestartSkippedReason) -> u8 {
    match reason {
        RestartSkippedReason::NotRestartable => 1,
    }
}

fn supervision_rejected_write(reason: SupervisionRejectedReason, hasher: &mut StableHasher) {
    match reason {
        SupervisionRejectedReason::BudgetExceeded {
            attempted_restart,
            max_restarts,
        } => {
            hasher.write_u8(1);
            hasher.write_u32(attempted_restart);
            hasher.write_u32(max_restarts);
        }
        SupervisionRejectedReason::SupervisorStopped => hasher.write_u8(2),
    }
}

fn call_completion_rejected_tag(reason: CallCompletionRejectedReason) -> u8 {
    match reason {
        CallCompletionRejectedReason::MailboxFull => 1,
        CallCompletionRejectedReason::RequesterClosed => 2,
        CallCompletionRejectedReason::ResourceClosed => 3,
    }
}

fn call_reply_rejected_tag(reason: CallReplyRejectedReason) -> u8 {
    match reason {
        CallReplyRejectedReason::NoPendingCall => 1,
        CallReplyRejectedReason::ReplyPathFull => 2,
        CallReplyRejectedReason::RequesterShardClosed => 3,
        CallReplyRejectedReason::CallerCancelled => 4,
        CallReplyRejectedReason::CallerTimedOut => 5,
        CallReplyRejectedReason::OwnerStopped => 6,
        CallReplyRejectedReason::RuntimeStopped => 7,
    }
}

fn cancel_cause_tag(cause: tina::CancelCause) -> u8 {
    match cause {
        tina::CancelCause::CallerCancelled => 1,
        tina::CancelCause::CallerTimedOut => 2,
        tina::CancelCause::OwnerStopped => 3,
        tina::CancelCause::RuntimeStopped => 4,
    }
}

fn write_kind_stable(kind: RuntimeEventKind, hasher: &mut StableHasher) {
    match kind {
        RuntimeEventKind::MailboxAccepted => hasher.write_u8(1),
        RuntimeEventKind::HandlerStarted => hasher.write_u8(2),
        RuntimeEventKind::HandlerPanicked => hasher.write_u8(3),
        RuntimeEventKind::HandlerFinished { effect } => {
            hasher.write_u8(4);
            hasher.write_u8(effect_kind_tag(effect));
        }
        RuntimeEventKind::EffectObserved { effect } => {
            hasher.write_u8(5);
            hasher.write_u8(effect_kind_tag(effect));
        }
        RuntimeEventKind::SendDispatchAttempted {
            target_shard,
            target_isolate,
            target_generation,
        } => {
            hasher.write_u8(6);
            hasher.write_u32(target_shard.get());
            hasher.write_u64(target_isolate.get());
            hasher.write_u64(target_generation.get());
        }
        RuntimeEventKind::SendAccepted {
            target_shard,
            target_isolate,
            target_generation,
        } => {
            hasher.write_u8(7);
            hasher.write_u32(target_shard.get());
            hasher.write_u64(target_isolate.get());
            hasher.write_u64(target_generation.get());
        }
        RuntimeEventKind::SendRejected {
            target_shard,
            target_isolate,
            target_generation,
            reason,
        } => {
            hasher.write_u8(8);
            hasher.write_u32(target_shard.get());
            hasher.write_u64(target_isolate.get());
            hasher.write_u64(target_generation.get());
            hasher.write_u8(send_rejected_tag(reason));
        }
        RuntimeEventKind::Spawned { child_isolate } => {
            hasher.write_u8(9);
            hasher.write_u64(child_isolate.get());
        }
        RuntimeEventKind::SupervisorRestartTriggered {
            policy,
            failed_child,
            failed_ordinal,
        } => {
            hasher.write_u8(10);
            hasher.write_u8(restart_policy_tag(policy));
            hasher.write_u64(failed_child.get());
            hasher.write_u64(failed_ordinal as u64);
        }
        RuntimeEventKind::SupervisorRestartRejected {
            failed_child,
            failed_ordinal,
            reason,
        } => {
            hasher.write_u8(11);
            hasher.write_u64(failed_child.get());
            hasher.write_u64(failed_ordinal as u64);
            supervision_rejected_write(reason, hasher);
        }
        RuntimeEventKind::RestartChildAttempted {
            child_ordinal,
            old_isolate,
            old_generation,
        } => {
            hasher.write_u8(12);
            hasher.write_u64(child_ordinal as u64);
            hasher.write_u64(old_isolate.get());
            hasher.write_u64(old_generation.get());
        }
        RuntimeEventKind::RestartChildSkipped {
            child_ordinal,
            old_isolate,
            old_generation,
            reason,
        } => {
            hasher.write_u8(13);
            hasher.write_u64(child_ordinal as u64);
            hasher.write_u64(old_isolate.get());
            hasher.write_u64(old_generation.get());
            hasher.write_u8(restart_skipped_tag(reason));
        }
        RuntimeEventKind::RestartChildCompleted {
            child_ordinal,
            old_isolate,
            old_generation,
            new_isolate,
            new_generation,
        } => {
            hasher.write_u8(14);
            hasher.write_u64(child_ordinal as u64);
            hasher.write_u64(old_isolate.get());
            hasher.write_u64(old_generation.get());
            hasher.write_u64(new_isolate.get());
            hasher.write_u64(new_generation.get());
        }
        RuntimeEventKind::IsolateStopped => hasher.write_u8(15),
        RuntimeEventKind::MessageAbandoned => hasher.write_u8(16),
        RuntimeEventKind::CallDispatchAttempted { call_id, call_kind } => {
            hasher.write_u8(17);
            hasher.write_u64(call_id.get());
            hasher.write_u8(call_kind_tag(call_kind));
        }
        RuntimeEventKind::CallCompleted { call_id, call_kind } => {
            hasher.write_u8(18);
            hasher.write_u64(call_id.get());
            hasher.write_u8(call_kind_tag(call_kind));
        }
        RuntimeEventKind::CallFailed {
            call_id,
            call_kind,
            reason,
        } => {
            hasher.write_u8(19);
            hasher.write_u64(call_id.get());
            hasher.write_u8(call_kind_tag(call_kind));
            hasher.write_u8(call_error_tag(reason));
        }
        RuntimeEventKind::CallCompletionRejected {
            call_id,
            call_kind,
            reason,
        } => {
            hasher.write_u8(20);
            hasher.write_u64(call_id.get());
            hasher.write_u8(call_kind_tag(call_kind));
            hasher.write_u8(call_completion_rejected_tag(reason));
        }
        RuntimeEventKind::CallReplyRejected { call_id, reason } => {
            hasher.write_u8(21);
            hasher.write_u64(call_id.get());
            hasher.write_u8(call_reply_rejected_tag(reason));
        }
        RuntimeEventKind::SnapshotCommitted => hasher.write_u8(22),
        RuntimeEventKind::SnapshotCommitFailed { reason } => {
            hasher.write_u8(23);
            hasher.write_u8(call_error_tag(reason));
        }
        RuntimeEventKind::JournalAppended { record_index } => {
            hasher.write_u8(24);
            hasher.write_u64(record_index);
        }
        RuntimeEventKind::JournalAppendFailed {
            record_index,
            reason,
        } => {
            hasher.write_u8(25);
            hasher.write_u64(record_index);
            hasher.write_u8(call_error_tag(reason));
        }
        RuntimeEventKind::RecoveryStarted => hasher.write_u8(26),
        RuntimeEventKind::RecoveryFinished => hasher.write_u8(27),
        RuntimeEventKind::RecoveryFailed { reason } => {
            hasher.write_u8(28);
            hasher.write_u8(call_error_tag(reason));
        }
        RuntimeEventKind::DeferredReplyCaptured { slot_id, call_id } => {
            hasher.write_u8(29);
            hasher.write_u64(slot_id.get());
            hasher.write_u64(call_id.get());
        }
        RuntimeEventKind::DeferredReplySent { slot_id, call_id } => {
            hasher.write_u8(30);
            hasher.write_u64(slot_id.get());
            hasher.write_u64(call_id.get());
        }
        RuntimeEventKind::DeferredReplyRejected {
            slot_id,
            call_id,
            reason,
        } => {
            hasher.write_u8(31);
            hasher.write_u64(slot_id.get());
            hasher.write_u64(call_id.get());
            hasher.write_u8(deferred_reply_rejected_tag(reason));
        }
        RuntimeEventKind::DeferredReplyDropped { slot_id, call_id } => {
            hasher.write_u8(32);
            hasher.write_u64(slot_id.get());
            hasher.write_u64(call_id.get());
        }
        RuntimeEventKind::CallCancelled { call_id, cause } => {
            hasher.write_u8(33);
            hasher.write_u64(call_id.get());
            hasher.write_u8(cancel_cause_tag(cause));
        }
    }
}

/// Compute a stable fingerprint of an entire trace as the FNV-1a chain of
/// each event's [`RuntimeEvent::stable_hash`] in iteration order.
///
/// Same stability promise as [`RuntimeEvent::stable_hash`]: stable for the
/// current Tina version, not a long-term wire format. Trace consumers that
/// need cross-run comparisons typically already sort by [`EventId`] before
/// calling this; callers that want order-insensitive comparison must sort
/// before passing in.
pub fn stable_trace_hash<'a, I>(events: I) -> u64
where
    I: IntoIterator<Item = &'a RuntimeEvent>,
{
    let mut hasher = StableHasher::new();
    for event in events {
        hasher.write_u64(event.stable_hash());
    }
    hasher.finish()
}

#[cfg(test)]
mod stable_hash_tests {
    use super::*;
    use tina::{AddressGeneration, IsolateId, ShardId};

    fn sample_event() -> RuntimeEvent {
        RuntimeEvent::new(
            EventId::new(7),
            Some(CauseId::new(EventId::new(3))),
            ShardId::new(0),
            IsolateId::new(42),
            RuntimeEventKind::SendAccepted {
                target_shard: ShardId::new(0),
                target_isolate: IsolateId::new(99),
                target_generation: AddressGeneration::new(1),
            },
        )
    }

    #[test]
    fn stable_hash_is_deterministic() {
        let event = sample_event();
        assert_eq!(event.stable_hash(), event.stable_hash());
    }

    #[test]
    fn stable_hash_changes_with_id() {
        let mut event = sample_event();
        let original = event.stable_hash();
        event = RuntimeEvent::new(
            EventId::new(8),
            event.cause(),
            event.shard(),
            event.isolate(),
            event.kind(),
        );
        assert_ne!(event.stable_hash(), original);
    }

    #[test]
    fn stable_hash_changes_with_kind_payload() {
        let original = sample_event();
        let mutated = RuntimeEvent::new(
            original.id(),
            original.cause(),
            original.shard(),
            original.isolate(),
            RuntimeEventKind::SendAccepted {
                target_shard: ShardId::new(0),
                target_isolate: IsolateId::new(99),
                target_generation: AddressGeneration::new(2),
            },
        );
        assert_ne!(original.stable_hash(), mutated.stable_hash());
    }

    #[test]
    fn stable_hash_distinguishes_variants_with_same_payload_layout() {
        let send_accepted = RuntimeEvent::new(
            EventId::new(1),
            None,
            ShardId::new(0),
            IsolateId::new(1),
            RuntimeEventKind::SendAccepted {
                target_shard: ShardId::new(0),
                target_isolate: IsolateId::new(2),
                target_generation: AddressGeneration::new(0),
            },
        );
        let send_dispatch = RuntimeEvent::new(
            EventId::new(1),
            None,
            ShardId::new(0),
            IsolateId::new(1),
            RuntimeEventKind::SendDispatchAttempted {
                target_shard: ShardId::new(0),
                target_isolate: IsolateId::new(2),
                target_generation: AddressGeneration::new(0),
            },
        );
        assert_ne!(send_accepted.stable_hash(), send_dispatch.stable_hash());
    }

    #[test]
    fn stable_trace_hash_is_order_sensitive() {
        let a = sample_event();
        let b = RuntimeEvent::new(
            EventId::new(8),
            a.cause(),
            a.shard(),
            a.isolate(),
            RuntimeEventKind::IsolateStopped,
        );
        let forward = stable_trace_hash([&a, &b]);
        let reversed = stable_trace_hash([&b, &a]);
        assert_ne!(forward, reversed);
    }

    #[test]
    fn trace_query_helpers_count_call_facts() {
        let trace = [
            RuntimeEvent::new(
                EventId::new(1),
                None,
                ShardId::new(0),
                IsolateId::new(1),
                RuntimeEventKind::CallCompleted {
                    call_id: CallId::new(1),
                    call_kind: CallKind::TlsBind,
                },
            ),
            RuntimeEvent::new(
                EventId::new(2),
                None,
                ShardId::new(0),
                IsolateId::new(1),
                RuntimeEventKind::CallFailed {
                    call_id: CallId::new(2),
                    call_kind: CallKind::TlsBind,
                    reason: CallError::TlsCertificate,
                },
            ),
            RuntimeEvent::new(
                EventId::new(3),
                None,
                ShardId::new(0),
                IsolateId::new(1),
                RuntimeEventKind::CallCompletionRejected {
                    call_id: CallId::new(3),
                    call_kind: CallKind::TlsRead,
                    reason: CallCompletionRejectedReason::MailboxFull,
                },
            ),
        ];

        assert_eq!(trace.count_completed(CallKind::TlsBind), 1);
        assert!(trace.any_completed(CallKind::TlsBind));
        assert_eq!(trace.count_failed(CallKind::TlsBind), 1);
        assert!(trace.any_failed_with(CallKind::TlsBind, |reason| {
            reason == CallError::TlsCertificate
        }));
        assert_eq!(trace.count_completion_rejected(CallKind::TlsRead), 1);
        assert!(!trace.any_failed(CallKind::TlsRead));
    }

    #[test]
    fn trace_query_helpers_count_spawned_and_dispatched() {
        let trace = [
            RuntimeEvent::new(
                EventId::new(1),
                None,
                ShardId::new(0),
                IsolateId::new(1),
                RuntimeEventKind::Spawned {
                    child_isolate: IsolateId::new(2),
                },
            ),
            RuntimeEvent::new(
                EventId::new(2),
                None,
                ShardId::new(0),
                IsolateId::new(1),
                RuntimeEventKind::CallDispatchAttempted {
                    call_id: CallId::new(1),
                    call_kind: CallKind::Sleep,
                },
            ),
        ];

        assert_eq!(trace.count_spawned(), 1);
        assert!(trace.any_spawned());
        assert_eq!(trace.count_call_dispatched(CallKind::Sleep), 1);
        assert!(trace.any_call_dispatched(CallKind::Sleep));
        assert_eq!(trace.count_call_dispatched(CallKind::TcpRead), 0);
        assert!(!trace.any_call_dispatched(CallKind::TcpRead));
    }

    #[test]
    fn trace_query_helpers_count_matching() {
        let trace = [
            RuntimeEvent::new(
                EventId::new(1),
                None,
                ShardId::new(0),
                IsolateId::new(1),
                RuntimeEventKind::HandlerStarted,
            ),
            RuntimeEvent::new(
                EventId::new(2),
                None,
                ShardId::new(0),
                IsolateId::new(1),
                RuntimeEventKind::HandlerFinished {
                    effect: EffectKind::Noop,
                },
            ),
        ];

        assert_eq!(
            trace.count_matching(|kind| matches!(kind, RuntimeEventKind::HandlerStarted)),
            1
        );
        assert!(trace.any_matching(|kind| matches!(kind, RuntimeEventKind::HandlerStarted)));
        assert!(!trace.any_matching(|kind| matches!(kind, RuntimeEventKind::Spawned { .. })));
    }
}
