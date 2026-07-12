#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]
// Betelgeuse exposes `IOLoopHandle<A: Allocator>` over the unstable
// `allocator_api`, so `tina-runtime` remains nightly-only. The feature gate is
// scoped to this crate.
#![feature(allocator_api)]

//! Live runtime implementations for `tina-rs`.
//!
//! Three runtime owners share one isolate/effect contract:
//!
//! - `Runtime` — the explicit-step, single-shard primitive: deterministic
//!   event IDs, causal links, full trace. The unit-test workhorse.
//! - `LocalSystem` — single-process multi-shard runner with bounded
//!   shard-pair queues and runtime-owned I/O rails.
//! - `ThreadedRuntime` — the live thread-per-shard runtime over the vendored
//!   Betelgeuse substrate: TCP/UDP/DNS/TLS/Unix/file/process/signal and
//!   snapshot/journal persistence as typed runtime calls.
//!
//! Shared discipline across all three: bounded mailboxes and lanes with typed
//! `Full`/`Closed`/`Timeout` outcomes, isolate calls with mandatory timeouts,
//! supervision with restart budgets, capacity reports, and a runtime trace
//! (bounded retention by default on the live owners; `TraceRetention::Full`
//! stays the explicit choice for tests and replay).
//!
//! Handler panics unwind into deterministic runtime events. Binaries built
//! with `panic = "abort"` remain out of scope for this crate.

use std::alloc::Global;
use std::any::Any;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use tina::{AddressGeneration, DeferredSlotRegistry, IsolateId, Shard};

use betelgeuse::IOLoopHandle;

pub mod admission;
mod affinity;
pub mod bounded;
pub mod bridge;
pub mod broadcast;
pub mod budget;
mod budget_adapters;
mod call;
mod call_group;
mod capabilities;
pub mod capacity;
mod child_lifecycle;
mod clock;
mod concurrency_pending;
pub mod deferred;
mod drain_state;
mod driver;
pub mod durable_outbox;
mod errors;
pub mod event_sink;
mod fact;
pub mod fairness_report;
pub mod file_loops;
mod full_handling;
pub mod guarded_pending;
mod host_burst;
mod host_call_dispatcher;
mod host_call_reply_pool;
pub mod lifecycle;
mod live_report;
mod local_permit;
mod local_system;
mod mailbox;
mod multi_shard;
mod observation;
mod observer;
pub mod persistence;
#[allow(unsafe_code)]
pub mod pool;
pub mod pressure;
mod scatter_gather;
pub mod scope;
pub mod scope_timer;
pub mod service_pressure;
pub mod sharded;
pub mod shared_scope;
pub mod shared_work;
mod shutdown;
mod single_call_gate;
pub mod supervision_report;
pub mod tcp_loops;
mod threaded;
mod threaded_multi_shard;
mod trace;
pub mod unix_loops;
mod wait_list;

pub use admission::{
    AdmissionDecision, AdmissionFailure, AdmissionReport, ConcurrencyLimit, ConcurrencyPermit,
    ConcurrencyReleaseError, KeyedLimit, KeyedPermit, KeyedReleaseError, KeyedSlotReport,
    PressureAction, RateGrant, RateKeyState, RateLimit, ServicePolicy, SurfaceName,
};
pub use concurrency_pending::{
    ConcurrencyGuardedInsertError, ConcurrencyInsertError, ConcurrencyParkError,
    ConcurrencyParkTicket, ConcurrencyPendingInitError, ConcurrencyPendingReplies,
    ConcurrencyPendingReport, ConcurrencyReplyError, request_effect_after_concurrency_park,
};
pub use drain_state::{AdmitDecision, DrainReport, DrainStage, DrainState};
pub use errors::{
    RegisterBootstrapError, SendObservedUntilError, ShutdownAndWaitError, ShutdownRequestError,
    ShutdownWaitError, StartupError, SuperviseError, ThreadedRegisterBootstrapError,
    ThreadedRuntimeConfigError, ThreadedRuntimeError, ThreadedSendObservedError,
    ThreadedTrySendError,
};
pub use full_handling::{
    FullDecision, FullExhaustionReason, FullHandling, FullHandlingReport, FullHandlingToken,
    FullPolicyMode,
};
pub use host_burst::{HostBurstOutcomes, HostBurstSnapshot, HostBurstWaitError};
pub use lifecycle::{
    CloseAdmission, CloseOutcome, ComponentKind, Health, Lifecycle, READINESS_UNKNOWN_REASON,
    Readiness, ReadinessReason, ReadinessToken, ResourceCloseReport, ResourceKind,
    ServiceShutdownReport, ServiceTopology, ShutdownChoreography, ShutdownStep, ShutdownStepReport,
    StepOutcome, TopologyComponent,
};
pub use local_permit::{
    LocalPermitFull, LocalPermitGate, LocalPermitName, LocalPermitReleaseError, LocalPermitReport,
    Permit, dropped_permit_count,
};
pub use local_system::{
    LocalMultiShardSystem, LocalMultiShardSystemShutdown, LocalSystem, LocalSystemConfig,
    LocalSystemConfigError, LocalSystemMultiShardBuilder, LocalSystemShutdown,
    LocalSystemShutdownReport, LocalSystemSingleShardBuilder, LocalSystemState,
    LocalSystemTerminalReport, LocalSystemTerminalSummary, ShutdownUncleanReason, TraceSnapshot,
    UncleanShutdownError,
};
pub use multi_shard::{MultiShardRuntime, MultiShardRuntimeConfig};

mod dispatch;
mod host_call;
mod registration;
mod remote;
mod service_handle;
pub use service_handle::{
    EventServiceHandle, RequestServiceHandle, SendOnlyServiceHandle, ServiceHandle,
    SplitServiceHandle,
};

use remote::{QueuedRemoteEnvelope, SendableQueuedRemoteEnvelope};

pub(crate) use dispatch::{
    AnyMailboxAdapter, ChildRecord, ErasedMailbox, ErasedMessage, ErasedSend, HandlerAdapter,
    IntoErasedSpawn, IntoErasedSpawnObserved, IntoSendErasedSpawnObserved, MailboxAdapter,
    PendingRemoteSpawn, RegisteredAddress, RegisteredEntry, SendErasedSpawn,
    SendableHandlerAdapter, SpawnOutcome, SupervisorRecord,
};
#[cfg(test)]
pub(crate) use dispatch::{ChildRecordSnapshot, SupervisorRecordSnapshot};

pub use shutdown::ThreadedShutdownHandle;
pub use single_call_gate::SingleCallGate;
pub use threaded::{
    DEFAULT_SHUTDOWN_LANE_DRAIN_TIMEOUT, DEFAULT_STARTUP_HANDSHAKE_TIMEOUT,
    HOST_CALL_DISPATCHER_POOL_SIZE, ThreadedRuntime, ThreadedRuntimeConfig,
};
pub use threaded_multi_shard::ThreadedMultiShardRuntime;

pub use live_report::{
    AffinityStatus, LiveQueueReport, LiveRemoteQueueReport, LiveShardReport, LiveShardState,
    LiveTopologyReport,
};

pub use capabilities::{
    CancellationSupport, DriverRuntimeRequirement, DurabilityCapability, RailClass,
    ResourceCapability, ResourceExecutionShape, ResourceSupport, RuntimeCapabilities,
    RuntimeCapabilityReport, RuntimeCapabilityRow, ShutdownSupport, TINA_DRIVER_RUNTIME_CONTRACT,
    TinaDriverRuntimeContract,
};
#[cfg(test)]
use clock::ManualClock;
use clock::{Clock, MonotonicClock};
pub use mailbox::{DefaultMailboxFactory, DefaultThreadedMailboxFactory, MailboxFactory};

pub use crate::durable_outbox::{
    AppendFailed, ApplyStatus, CommitConfidence, CommittedWork, CompletionFailed, CompletionStart,
    DurableCompletion, DurableOutbox, DurablePayload, DurableWork, OutboxFull,
    OutboxShutdownReport, RecordError, RecordedWork, RecoveryError, RecoveryReport, ResumeQueue,
    StaleWork, TailStatus, WorkId,
};
pub use crate::persistence::{
    LOCAL_PERSISTENCE_SUPPORT, LocalPersistenceSupport, PersistenceSupportLevel,
};
pub use bounded::{
    BoundedEffects, BoundedEffectsError, BoundedItems, BoundedItemsError, ServiceOwnedBoundError,
    assert_service_owned_bound, bounded_batch,
};
pub use broadcast::{
    BroadcastAssertError, BroadcastOutcome, BroadcastRecordError, BroadcastReport, BroadcastTarget,
    BroadcastTargets, BroadcastTargetsError, BroadcastTracker, broadcast_observed,
};
pub use budget::{
    BUDGET_SCHEMA_VERSION, BudgetBuildError, BudgetCap, BudgetConsistencyReport,
    BudgetConsistencyRow, BudgetKind, BudgetManifestReport, BudgetManifestRow, BudgetReplayExport,
    BudgetSource, BudgetSurface, BudgetUnit, BudgetValidationError, ObservedBudget,
    ReplayBudgetEntry, ReplayImpact, ServiceBudgetManifest,
};
#[allow(deprecated)]
pub use call::{
    AdmitWorkError, CallError, CallId, CallInput, CallOutcome, CallOutput, CallReply,
    CancelableCall, CancelableWork, CancelableWorkSnapshot, DeferredCancelableCall,
    DeferredIsolateCall, DeferredObservedSend, DeferredTypedCall, DnsLookupReply, ErasedCall,
    ErasedRuntimeCallCompletion, FileCloseReply, FileFsyncReply, FileId, FileOpenOptions,
    FileOpenReply, FileReadReply, FileSizeReply, FileWriteOwnedReply, FileWriteReply,
    IntoErasedCall, IsolateCall, IsolateCallWithHandle, JournalAppendReply, JournalRecord,
    JournalReplay, JournalReplayReply, JournalReplayWarning, ListenerId, MkdirReply, PathKind,
    PathMetadata, PathMetadataReply, PendingCancelableCall, PendingCancelableCallSet,
    PendingCancelableInsertError, PendingCancelableRemoveError, PendingCancelableTicket,
    PersistenceTraceInfo, ProcessRunReply, ProcessRunResult, ProcessStatus, ReadDirReply,
    RemoveFileReply, RenameReplaceReply, RequestDeferredCancelableCall, RequestDeferredIsolateCall,
    RequestDeferredObservedSend, RequestDeferredTypedCall, RequestPendingCancelableInsertError,
    RuntimeCall, RuntimeCallCompletion, RuntimeCallParts, RuntimeCallable, SendOutcome,
    SignalWaitReply, SleepCall, SleepReply, SnapshotCommitReply, SnapshotImage, SnapshotLoadReply,
    StreamId, SyncParentReply, TcpAcceptReply, TcpBindReply, TcpConnectReply,
    TcpListenerCloseReply, TcpReadBufReply, TcpReadReply, TcpStreamCloseReply,
    TcpWriteOwnedCloseReply, TcpWriteOwnedReply, TcpWriteReply, TlsAcceptReply, TlsBindReply,
    TlsCloseReply, TlsConnectReply, TlsListenerCloseReply, TlsListenerId, TlsReadBufReply,
    TlsReadReply, TlsStreamId, TlsWriteOwnedReply, TlsWriteReply, TypedCall, UdpBindReply,
    UdpCloseSocketReply, UdpRecvFromReply, UdpSendToReply, UdpSocketId, UnixAcceptReply,
    UnixBindReply, UnixConnectReply, UnixListenerCloseReply, UnixListenerId, UnixReadReply,
    UnixStreamCloseReply, UnixStreamId, UnixWriteOwnedReply, UnixWriteReply, WorkTicket,
    WriteOwnedError, WriteOwnedReply, call, call_cancelable, call_cancelable_request,
    call_handle_call_id, call_request, call_typed, call_with_handle, cancel_call, dns_lookup,
    file_close, file_create, file_fsync, file_open, file_read, file_read_at, file_size, file_write,
    file_write_at, file_write_at_owned, journal_append, journal_replay, mkdir, path_metadata,
    process_run, read_dir, remove_file, rename_replace, send_observed, signal_wait, sleep,
    sleep_then, snapshot_commit, snapshot_load, sync_parent, tcp_accept, tcp_bind,
    tcp_close_listener, tcp_close_stream, tcp_connect, tcp_read, tcp_read_buf, tcp_write,
    tcp_write_owned, tcp_write_owned_close, tls_accept, tls_accept_alpn, tls_bind, tls_bind_alpn,
    tls_close, tls_close_listener, tls_connect, tls_connect_alpn, tls_read, tls_read_buf,
    tls_write, tls_write_owned, udp_bind, udp_close_socket, udp_recv_from, udp_send_to,
    unix_accept, unix_bind, unix_close_listener, unix_close_stream, unix_connect, unix_read,
    unix_write, unix_write_owned,
};
pub use call_group::{
    CallGroup, CallGroupBranchOutcome, CallGroupCancelOutcome, CallGroupCancelRequest,
    CallGroupInsertError, CallGroupRecordCancelError, CallGroupRecordReplyError,
    CallGroupReplyStep, CallGroupReport, CallGroupReserveError, CallGroupStartError,
    CallGroupToken, CallJoinReport, CallJoinSet, CallJoinToken, CallSelectClassifiedStep,
    CallSelectReport, CallSelectSet, CallSelectToken, CallSetBranchOutcome, CallSetCancelOutcome,
    CallSetCancelRequest, CallSetInsertError, CallSetRecordCancelError, CallSetRecordReplyError,
    CallSetStartError, SelectedCall, SelectedCallOutcome,
};
pub use capacity::{
    CapacityAssertError, CapacityNameError, CapacitySummary, SurfaceAssertion,
    format_assertion_failure, format_discovery_line, format_discovery_report,
};
pub use child_lifecycle::{
    ChildLifecycle, ChildLifecycleReport, ChildLifecycleReportError, ChildLifecycleState,
};
pub use deferred::{
    InsertError as PendingRepliesInsertError, ParkCallError, ParkError, ParkTicket, PendingReplies,
    ReplyParkedError, TakeParkedError, TryCaptureError as PendingRepliesTryCaptureError,
    request_effect_after_park,
};
use driver::DriverCompletion;
pub use driver::os_signal_capture_supported;
use driver::{BetelgeuseDriver, RuntimeDriver};
pub use event_sink::{
    BoundedEventSink, DropPolicy, EventSinkDrain, EventSinkReport, EventSinkSurface,
};
pub use fact::{
    GrpcStatusCode, GrpcStreamId, Http2CloseReason, Http2FlowControlSide, Http2ResetReason,
    Http2StreamId, IntoRuntimeFact, ProtocolConnectionId, ProtocolDirection, ProtocolFact,
    ProtocolFamily, RuntimeFact, WebSocketCloseReason, WebSocketSessionId,
};
pub use fairness_report::{FairnessReport, IsolateProgress, LagObservation, StarvationWarning};
pub use file_loops::{
    CopyLeg, FileCopyBounded, FileCopyProgress, FileCopyStep, FileLoopEnd, FileLoopReport,
    FileLoopStep, FileReadChunks, FileWriteAll,
};
pub use guarded_pending::{
    GuardedInsertError, GuardedParkCallError, GuardedParkError, GuardedParkTicket,
    GuardedPendingReplies, GuardedReplyError, GuardedTakeError,
};
pub use observation::{
    BoundAddressWaiter, ChildRestarted, ChildRestartedWaiter, IsolateCompleteWaiter,
    IsolateResultWaiter, OperationDoneWaiter, ResultWaitError, WaitError,
};
pub use observer::{
    BufferedTraceDrain, BufferedTraceDrainError, BufferedTraceObserver, TraceObserver,
};
pub use pressure::{MailboxBudget, PressureReport, PressureSummary, format_pressure_line};
pub use scatter_gather::{
    ScatterGather, ScatterGatherAdvance, ScatterGatherAdvanceResult, ScatterGatherCompleted,
    ScatterGatherEvent, ScatterGatherOperations, ScatterGatherOperationsAdvanceResult,
    ScatterGatherOperationsError, ScatterGatherOperationsStart, ScatterGatherRecordError,
    ScatterGatherRecordResult, ScatterGatherStart, ScatterGatherStartError,
    ScatterGatherStartFailure, ScatterGatherToken,
};
pub use scope::{
    CallContextScopeExt, DeferScopedThrough, DeferredScopedCall, RequestScope, RequestScopeId,
    RequestScopeInsertError, RequestScopeRemoveError, RequestScopeSet,
    RequestScopeSetCapacityReport, ScopeCancelCause, ScopeCancelReport, ScopeChildReport,
    ScopeRegisterError, ScopeRegisterSharedError, ScopedAdmitError, ScopedCallHandle,
    ScopedReplyError, ScopedRequestReport, UnsupportedScopeRow, scope_register,
};
pub use scope_timer::{
    ScopedTimer, ScopedTimerArmError, ScopedTimerFire, ScopedTimerId, ScopedTimerSet,
};
pub use service_pressure::{ServicePressureReport, ServicePressureSurface, ServiceSurfaceState};
pub use shared_scope::{
    SharedCapacityCharge, SharedCapacityReservation, SharedCapacityScope, SharedLease,
    SharedScopeFull, SharedScopeReport,
};
pub use shared_work::{
    SharedWork, SharedWorkCallError, SharedWorkError, SharedWorkReplyError, SharedWorkSnapshot,
    SharedWorkTicket, request_effect_after_shared_wait,
};
pub use supervision_report::{ChildSupervision, SupervisorHalt, SupervisorReport};
pub use tcp_loops::{LoopStep, ReadExactStep, TcpReadExact, TcpReadToEof, TcpWriteAll};
/// Declares a Tina isolate whose I/O payload defaults to [`RuntimeCall<Message>`](RuntimeCall).
///
/// This is the preferred runtime authoring path. It keeps the handler as normal
/// Rust code and only fills the repetitive [`tina::Isolate`] associated types.
///
/// Choose the smallest service shape that matches the public contract:
///
/// - `message = Message` uses the legacy combined-message `handle` method.
/// - `event = Event` uses only `handle_event` and has no callable lane.
/// - `request = Request, reply = Reply` uses only `handle_request`.
/// - `event = Event, request = Request, reply = Reply` uses both typed lanes.
///
/// Event/request forms keep the internal [`tina::ServiceMessage`] envelope out
/// of handlers and work with the matching `Runtime::register_*_service`
/// methods.
///
/// **The expansion is rooted at `::tina`.** The generated impl names
/// `::tina::Isolate`, `::tina::Effect`, `::tina::Context`, and friends, so the
/// crate using this macro must depend on `tina` and have it reachable as
/// `::tina` (the default crate name). Only the I/O payload is rooted at
/// `::tina_runtime`. A crate that depends on `tina-runtime` alone will fail to
/// compile with `unresolved import ::tina`; add `tina` as a direct dependency,
/// or override the root with `#[tina_runtime::isolate(.., tina_crate = ::your_path)]`.
pub use tina_macros::runtime_isolate as isolate;
pub use trace::{
    CallCompletionRejectedReason, CallKind, CallReplyRejectedReason, CauseId,
    DeferredReplyRejectedReason, DeferredSlotId, EffectKind, EventId, RestartSkippedReason,
    RuntimeEvent, RuntimeEventKind, RuntimeTraceExt, SendRejectedReason, SupervisionRejectedReason,
    TerminalCompletionAction, stable_trace_hash,
};
pub use unix_loops::{UnixReadToEof, UnixWriteAll};

#[derive(Debug, Clone, Copy)]
pub(crate) enum MessageCallContext {
    Local {
        call_id: CallId,
    },
    Remote {
        call_id: CallId,
        requester: RegisteredAddress,
        cause: CauseId,
        expected_reply_type_id: std::any::TypeId,
    },
}

pub(crate) struct DeliveredMessage {
    pub(crate) message: Box<dyn Any>,
    pub(crate) call_context: Option<MessageCallContext>,
}

/// Id source for one shard.
///
/// Event ids are per-shard-local: each shard counts its own events starting
/// at one. The shard owner is single-threaded, so its event sequence is
/// deterministic regardless of how shard worker threads interleave, and the
/// shard-plus-event-id pair is the stable per-event key the trace hash and
/// the snapshot sort rely on. A single global atomic event counter shared
/// across shards would hand the id of a given logical event to whichever
/// thread won a `fetch_add` race, which made the multishard trace hash flap.
///
/// Call ids stay global: cross-shard call routing needs an id unique across
/// every shard, so `next_call_id` is the shared counter.
#[derive(Debug, Clone)]
pub(crate) struct IdSource {
    /// Per-shard event counter. Not shared across shards.
    next_event_id: Arc<AtomicU64>,
    /// Global call counter, shared across every shard.
    next_call_id: Arc<AtomicU64>,
}

impl IdSource {
    pub(crate) fn new() -> Self {
        Self {
            next_event_id: Arc::new(AtomicU64::new(1)),
            next_call_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Derives the id source for a sibling shard: a fresh per-shard event
    /// counter, the same shared global call counter. Use this instead of
    /// `clone()` when fanning one source out to multiple shards so each
    /// shard's event ids stay independent and deterministic.
    pub(crate) fn per_shard(&self) -> Self {
        Self {
            next_event_id: Arc::new(AtomicU64::new(1)),
            next_call_id: Arc::clone(&self.next_call_id),
        }
    }

    pub(crate) fn next_event_id(&self) -> EventId {
        let raw = self.next_event_id.fetch_add(1, Ordering::Relaxed);
        EventId::new(raw)
    }

    pub(crate) fn next_call_id(&self) -> CallId {
        let raw = self.next_call_id.fetch_add(1, Ordering::Relaxed);
        CallId::new(raw)
    }
}

/// Default per-step backend completion drain budget. High enough that a normal
/// warm turn (one or a few ready completions) delivers them all in the same
/// step; low enough that a burst of ready completions cannot turn one step into
/// an unbounded delivery loop. The remainder carries over to the next step.
pub const DEFAULT_DRIVER_COMPLETION_DRAIN_BUDGET: usize = 64;

/// Small deterministic single-shard runtime.
///
/// The runtime owns one shard value plus a private registry of isolates and
/// mailboxes. [`step`](Self::step) walks registered isolates in registration
/// order and gives each isolate at most one delivery chance per round.
pub struct Runtime<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    pub(crate) shard: S,
    pub(crate) mailbox_factory: F,
    pub(crate) entries: Vec<RegisteredEntry<S, F>>,
    pub(crate) entry_indexes: HashMap<IsolateId, usize>,
    /// Set when any entry is stopped; lets the per-step GC skip its full
    /// scan entirely while every isolate is live. Re-derived by the GC.
    pub(crate) has_stopped_entries: bool,
    pub(crate) child_records: Vec<ChildRecord<S, F>>,
    pub(crate) supervisors: Vec<SupervisorRecord>,
    pub(crate) next_isolate_id: u64,
    pub(crate) ids: IdSource,
    pub(crate) trace: Vec<RuntimeEvent>,
    pub(crate) trace_start: usize,
    pub(crate) trace_retention: TraceRetention,
    pub(crate) trace_dropped: u64,
    pub(crate) driver: Box<dyn RuntimeDriver>,
    /// Single owner of all in-flight call bookkeeping, keyed by `CallId`.
    /// Folds the former parallel `in_flight_calls`/`translators`/
    /// `pending_isolate_calls` Vecs, their index maps, and the isolate-call
    /// deadline index into one type. Translators are stored inline, so a
    /// present-entry/missing-translator split can no longer arise.
    pub(crate) call_table: CallTable,
    pub(crate) clock: Box<dyn Clock>,
    /// Cross-shard `spawn_observed(...).on_shard(...)` requests awaiting their
    /// address reply from the destination shard. Keyed by request id.
    pub(crate) pending_remote_spawns: Vec<PendingRemoteSpawn>,
    pub(crate) remote_spawn_cancel_tombstones: std::collections::VecDeque<CallId>,
    pub(crate) remote_child_control_capacity: usize,
    pub(crate) remote_child_control_full: u64,
    pub(crate) round_messages: Vec<Option<DeliveredMessage>>,
    pub(crate) driver_completions: Vec<DriverCompletion>,
    /// Backend completions harvested from the driver but not yet delivered this
    /// step because the per-step drain budget was reached. Deterministic FIFO:
    /// carried entries are delivered before fresh ones on the next advance, so
    /// completion order is stable across the budget boundary. Drained until
    /// empty, so no completion is dropped.
    pub(crate) pending_completions: std::collections::VecDeque<DriverCompletion>,
    /// Max backend completions delivered into mailboxes per driver-advance.
    /// Bounds the per-step completion work so a burst of ready completions
    /// cannot monopolise one turn; the remainder carries over.
    pub(crate) driver_completion_drain_budget: usize,
    pub(crate) next_isolate_call_ordinal: u64,
    pub(crate) observation: observation::ObservationRegistry,
    /// Live trace observer. Fires before retention. See [`crate::TraceObserver`].
    pub(crate) trace_observer: observer::StoredObserver,
    /// Tina-owned slot-id source and pending-capture queue. Cloned
    /// (refcount bump, no allocation) into each per-message
    /// [`MessageCaller`].
    pub(crate) deferred_registry: Rc<DeferredSlotRegistry>,
    /// Promoted-slot table. Owned solely by the runtime.
    pub(crate) promoted_slots: deferred::PromotedSlots,
    /// Bounded ring of recently-cancelled calls plus the cause that
    /// closed each one. Late callee replies for these settle as a
    /// reason that matches the cause (`CallerCancelled`,
    /// `CallerTimedOut`, `OwnerStopped`, `RuntimeStopped`) instead of
    /// the less-specific `NoPendingCall` / `CallerClosed`. Bounded at
    /// [`CANCELLED_CALL_RING_CAPACITY`] — older entries are evicted,
    /// at which point fall-through to the generic reason is honest.
    ///
    /// Single-writer (this shard's runtime); no concurrent access.
    pub(crate) cancelled_calls: std::collections::VecDeque<(CallId, tina::CancelCause)>,
    pub(crate) cancelled_call_cause_evictions: u64,
    /// Debug tripwire: counts call rejections that resolve as
    /// `UnsupportedMessage` — the signature of the default `handle_call`.
    /// An isolate that answers `call()` traffic but only implements `handle`
    /// keeps the default `handle_call`, which auto-rejects every call. That
    /// whole bug class shipped invisibly once. Incremented only in debug builds;
    /// never in release, so it is zero-cost there and the accessor reads a
    /// constant 0.
    pub(crate) unsupported_message_rejections: u64,
}

/// Capacity of the per-runtime recently-cancelled-calls ring. The sim
/// keeps a parallel ring sized to this same constant so simulator
/// parity holds for late-reply reason classification.
pub const CANCELLED_CALL_RING_CAPACITY: usize = 64;

/// Maps a `CancelCause` to the matching `CallReplyRejectedReason` for
/// late-reply rejection. Kept inline here so the runtime and the
/// simulator both call the same translation.
pub fn call_reply_reason_for_cause(cause: tina::CancelCause) -> trace::CallReplyRejectedReason {
    match cause {
        tina::CancelCause::CallerCancelled => trace::CallReplyRejectedReason::CallerCancelled,
        tina::CancelCause::CallerTimedOut => trace::CallReplyRejectedReason::CallerTimedOut,
        tina::CancelCause::OwnerStopped => trace::CallReplyRejectedReason::OwnerStopped,
        tina::CancelCause::RuntimeStopped => trace::CallReplyRejectedReason::RuntimeStopped,
    }
}

/// Maps a `CancelCause` to the matching `DeferredReplyRejectedReason`
/// for late deferred-reply rejection.
pub fn deferred_reply_reason_for_cause(
    cause: tina::CancelCause,
) -> trace::DeferredReplyRejectedReason {
    match cause {
        tina::CancelCause::CallerCancelled => trace::DeferredReplyRejectedReason::CallerCancelled,
        tina::CancelCause::CallerTimedOut => trace::DeferredReplyRejectedReason::CallerTimedOut,
        tina::CancelCause::OwnerStopped => trace::DeferredReplyRejectedReason::OwnerStopped,
        tina::CancelCause::RuntimeStopped => trace::DeferredReplyRejectedReason::RuntimeStopped,
    }
}

/// Copy-able descriptor of a driver/host backend call, separate from its
/// (non-Copy) translator so trace/event code can read the call's identity while
/// the translator is moved out to run.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DriverCallHead {
    pub(crate) call_id: CallId,
    pub(crate) call_kind: CallKind,
    pub(crate) requester: RegisteredAddress,
    pub(crate) cause: CauseId,
    pub(crate) persistence: Option<call::PersistenceTraceInfo>,
    pub(crate) continuation_context: Option<MessageCallContext>,
}

/// A driver/host backend call awaiting completion. Translator stored inline: the
/// entry and its translator are inserted and removed together, so a
/// present-entry/missing-translator split cannot arise.
pub(crate) struct DriverCall {
    pub(crate) head: DriverCallHead,
    pub(crate) translator: ErasedTranslator,
}

impl std::fmt::Debug for DriverCall {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DriverCall")
            .field("head", &self.head)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CallDispatchContext {
    pub(crate) call_id: CallId,
    pub(crate) requester: RegisteredAddress,
    pub(crate) cause: CauseId,
    pub(crate) continuation_context: Option<MessageCallContext>,
}

pub(crate) type ErasedTranslator = Box<dyn FnOnce(CallOutput) -> call::ErasedRuntimeCallCompletion>;
pub(crate) type ErasedIsolateCallTranslator =
    Box<dyn FnOnce(CallOutcome<Box<dyn Any>>) -> Box<dyn Any>>;

const INITIAL_ENTRY_CAPACITY: usize = 8;
const INITIAL_CHILD_RECORD_CAPACITY: usize = 8;
const INITIAL_SUPERVISOR_CAPACITY: usize = 4;
const INITIAL_TRACE_CAPACITY: usize = 256;
const INITIAL_CALL_CAPACITY: usize = 8;

/// Setup-time reserves for runtime-owned metadata.
///
/// These knobs reserve only Tina-owned vectors and scratch buffers. They do
/// not pool user messages, erased replies, durable storage buffers, or
/// backend-owned completion slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreallocationConfig {
    /// Registered isolate table reserve.
    pub entry_capacity: usize,
    /// Child-record table reserve.
    pub child_record_capacity: usize,
    /// Supervisor table reserve.
    pub supervisor_capacity: usize,
    /// Runtime trace event reserve.
    pub trace_capacity: usize,
    /// In-flight call, translator, isolate-call, and driver-completion reserve.
    pub call_capacity: usize,
    /// Per-step round scratch reserve.
    pub round_scratch_capacity: usize,
}

impl Default for PreallocationConfig {
    fn default() -> Self {
        Self {
            entry_capacity: INITIAL_ENTRY_CAPACITY,
            child_record_capacity: INITIAL_CHILD_RECORD_CAPACITY,
            supervisor_capacity: INITIAL_SUPERVISOR_CAPACITY,
            trace_capacity: INITIAL_TRACE_CAPACITY,
            call_capacity: INITIAL_CALL_CAPACITY,
            round_scratch_capacity: INITIAL_ENTRY_CAPACITY,
        }
    }
}

/// Default trace ring for live runtime owners.
///
/// Live workers ([`ThreadedRuntimeConfig`], [`LocalSystemConfig`]) keep the most recent
/// this-many runtime events, so a long-running service does not grow memory with
/// uptime. Generous on purpose: shutdown reports and last-N debugging keep a
/// deep tail, and `trace_dropped` still reports what fell off. A `RuntimeEvent`
/// is a few dozen bytes, so this ring costs well under a megabyte. Replay and
/// simulation want every event instead — set [`TraceRetention::Full`]
/// explicitly there.
pub const DEFAULT_LIVE_TRACE_RETENTION: usize = 16_384;

/// Runtime trace retention policy.
///
/// Live runtime owners default to [`Bounded`](Self::Bounded) with
/// [`DEFAULT_LIVE_TRACE_RETENTION`] so the trace stays bounded with uptime.
/// Replay/simulation/tests that need every event set [`Full`](Self::Full)
/// explicitly. [`Off`](Self::Off) drops events after assigning their ids, for
/// when even a tail is unwanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceRetention {
    /// Keep every runtime event.
    Full,
    /// Keep only the most recent `usize` events.
    Bounded(usize),
    /// Drop runtime events after assigning their event IDs.
    Off,
}

pub(crate) fn reserve_round_message_scratch(
    round_messages: &mut Vec<Option<DeliveredMessage>>,
    entry_count: usize,
) {
    debug_assert!(round_messages.is_empty());
    if round_messages.capacity() < entry_count {
        round_messages.reserve(entry_count);
    }
}

/// An isolate-to-isolate call awaiting reply or timeout. Translator stored
/// inline (non-Option): removing the entry from [`CallTable`] yields the
/// translator, so an "already consumed" state cannot arise.
pub(crate) struct PendingIsolateCall {
    pub(crate) call_id: CallId,
    pub(crate) requester: RegisteredAddress,
    pub(crate) cause: CauseId,
    pub(crate) deadline: Instant,
    pub(crate) insertion_order: u64,
    pub(crate) continuation_context: Option<MessageCallContext>,
    pub(crate) translator: ErasedIsolateCallTranslator,
    /// `TypeId::of::<R>()` for the dispatching `Address<_, R>`. Used
    /// to typecheck deferred-reply payloads before they reach the
    /// translator's downcast.
    pub(crate) expected_reply_type_id: std::any::TypeId,
    /// Optional caller-owned cancellation cell. The runtime updates
    /// `state` to `Settled` on completion/timeout/closed, or
    /// `Cancelled` on explicit `cancel_call`.
    pub(crate) handle_shared: Option<std::sync::Arc<tina::CallHandleShared>>,
}

impl std::fmt::Debug for PendingIsolateCall {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingIsolateCall")
            .field("call_id", &self.call_id)
            .field("requester", &self.requester)
            .field("cause", &self.cause)
            .field("deadline", &self.deadline)
            .field("insertion_order", &self.insertion_order)
            .finish_non_exhaustive()
    }
}

/// Single owner of all in-flight call bookkeeping, keyed by `CallId`.
///
/// Two families share the `CallId` space but never the same id (each id is
/// minted once): driver/host backend calls and isolate-to-isolate calls.
/// `BTreeMap` keeps iteration in ascending call-id order, which — because call
/// ids are monotonic — equals insertion order. Cancel sweeps and the owner-stop
/// partition rely on that ordering; the simulator mirrors it so trace ordering
/// is identical across both. The isolate deadline index is folded in.
pub(crate) struct CallTable {
    driver: BTreeMap<CallId, DriverCall>,
    isolate: BTreeMap<CallId, PendingIsolateCall>,
    /// Earliest-deadline index over isolate calls only.
    isolate_deadlines: BTreeMap<(Instant, u64), CallId>,
}

impl CallTable {
    pub(crate) fn new() -> Self {
        Self {
            driver: BTreeMap::new(),
            isolate: BTreeMap::new(),
            isolate_deadlines: BTreeMap::new(),
        }
    }

    // --- driver/host backend calls ---

    pub(crate) fn insert_driver(&mut self, call: DriverCall) {
        let call_id = call.head.call_id;
        let previous = self.driver.insert(call_id, call);
        assert!(
            previous.is_none(),
            "duplicate in-flight call id {call_id:?}"
        );
    }

    pub(crate) fn remove_driver(&mut self, call_id: CallId) -> Option<DriverCall> {
        self.driver.remove(&call_id)
    }

    /// Driver call ids for `requester`, ascending (== insertion order). Cancel
    /// sweeps collect ids first, then remove, to sidestep borrow conflicts.
    pub(crate) fn driver_call_ids_for_requester(
        &self,
        requester: RegisteredAddress,
    ) -> Vec<CallId> {
        self.driver
            .iter()
            .filter(|(_, call)| call.head.requester == requester)
            .map(|(id, _)| *id)
            .collect()
    }

    pub(crate) fn has_driver_call_for_requester(&self, requester: RegisteredAddress) -> bool {
        self.driver
            .values()
            .any(|call| call.head.requester == requester)
    }

    pub(crate) fn has_driver_calls(&self) -> bool {
        !self.driver.is_empty()
    }

    /// Drains every driver call, ascending. Translators drop with their entries.
    pub(crate) fn drain_driver(&mut self) -> impl Iterator<Item = DriverCall> {
        std::mem::take(&mut self.driver).into_values()
    }

    // --- isolate-to-isolate calls ---

    pub(crate) fn insert_isolate(&mut self, call: PendingIsolateCall) {
        let call_id = call.call_id;
        self.isolate_deadlines
            .insert((call.deadline, call.insertion_order), call_id);
        let previous = self.isolate.insert(call_id, call);
        assert!(
            previous.is_none(),
            "duplicate pending isolate call {call_id:?}"
        );
    }

    pub(crate) fn remove_isolate(&mut self, call_id: CallId) -> Option<PendingIsolateCall> {
        let removed = self.isolate.remove(&call_id)?;
        self.isolate_deadlines
            .remove(&(removed.deadline, removed.insertion_order));
        Some(removed)
    }

    /// The earliest-deadline isolate call due at or before `now`, if any.
    pub(crate) fn next_due_isolate(&self, now: Instant) -> Option<CallId> {
        self.isolate_deadlines
            .first_key_value()
            .and_then(|(&(deadline, _), &call_id)| (deadline <= now).then_some(call_id))
    }

    /// Removes and returns every isolate call owned by `owner`, ascending call
    /// id (== insertion order), so the owner-stop trace order is stable.
    pub(crate) fn take_isolate_calls_for_owner(
        &mut self,
        owner_isolate: IsolateId,
        owner_generation: AddressGeneration,
    ) -> Vec<PendingIsolateCall> {
        let ids: Vec<CallId> = self
            .isolate
            .iter()
            .filter(|(_, call)| {
                call.requester.isolate == owner_isolate
                    && call.requester.generation == owner_generation
            })
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter()
            .map(|id| {
                self.remove_isolate(id)
                    .expect("indexed isolate call exists")
            })
            .collect()
    }

    pub(crate) fn isolate_expected_reply_type_id(
        &self,
        call_id: CallId,
    ) -> Option<std::any::TypeId> {
        self.isolate.get(&call_id).map(|c| c.expected_reply_type_id)
    }

    pub(crate) fn has_isolate_call_for_requester(&self, requester: RegisteredAddress) -> bool {
        self.isolate
            .values()
            .any(|call| call.requester == requester)
    }

    pub(crate) fn has_isolate_calls(&self) -> bool {
        !self.isolate.is_empty()
    }

    pub(crate) fn has_isolate_deadlines(&self) -> bool {
        !self.isolate_deadlines.is_empty()
    }

    /// Drains every isolate call, ascending. Clears the deadline index too.
    pub(crate) fn drain_isolate(&mut self) -> impl Iterator<Item = PendingIsolateCall> {
        self.isolate_deadlines.clear();
        std::mem::take(&mut self.isolate).into_values()
    }
}

impl<S, F> Runtime<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    /// Creates a new runtime for one shard plus one runtime-owned mailbox
    /// factory for future spawned children.
    pub fn new(shard: S, mailbox_factory: F) -> Self {
        Self::with_clock_and_ids(
            shard,
            mailbox_factory,
            Box::new(MonotonicClock),
            IdSource::new(),
        )
    }

    /// Creates a runtime over an explicit Betelgeuse I/O loop.
    ///
    /// This is the narrow substrate seam used by deterministic simulated I/O
    /// tests and alternate Betelgeuse loop implementations. Normal live code
    /// should use [`Runtime::new`] or [`ThreadedRuntime`].
    pub fn with_betelgeuse_io_loop(
        shard: S,
        mailbox_factory: F,
        io_loop: IOLoopHandle<Global>,
    ) -> Self {
        Self::with_clock_and_ids_and_driver(
            shard,
            mailbox_factory,
            Box::new(MonotonicClock),
            IdSource::new(),
            Box::new(BetelgeuseDriver::with_io_loop(io_loop)),
        )
    }

    #[cfg(test)]
    pub(crate) fn with_clock(shard: S, mailbox_factory: F, clock: Box<dyn Clock>) -> Self {
        Self::with_clock_and_ids(shard, mailbox_factory, clock, IdSource::new())
    }

    pub(crate) fn with_clock_and_ids(
        shard: S,
        mailbox_factory: F,
        clock: Box<dyn Clock>,
        ids: IdSource,
    ) -> Self {
        Self::with_clock_and_ids_and_driver(
            shard,
            mailbox_factory,
            clock,
            ids,
            Box::new(BetelgeuseDriver::new()),
        )
    }

    pub(crate) fn with_clock_and_ids_and_driver(
        shard: S,
        mailbox_factory: F,
        clock: Box<dyn Clock>,
        ids: IdSource,
        driver: Box<dyn RuntimeDriver>,
    ) -> Self {
        Self::with_clock_and_ids_and_driver_and_preallocation(
            shard,
            mailbox_factory,
            clock,
            ids,
            driver,
            PreallocationConfig::default(),
        )
    }

    pub(crate) fn with_clock_and_ids_and_driver_and_preallocation(
        shard: S,
        mailbox_factory: F,
        clock: Box<dyn Clock>,
        ids: IdSource,
        driver: Box<dyn RuntimeDriver>,
        preallocation: PreallocationConfig,
    ) -> Self {
        Self {
            shard,
            mailbox_factory,
            entries: Vec::with_capacity(preallocation.entry_capacity),
            entry_indexes: HashMap::with_capacity(preallocation.entry_capacity),
            has_stopped_entries: false,
            child_records: Vec::with_capacity(preallocation.child_record_capacity),
            supervisors: Vec::with_capacity(preallocation.supervisor_capacity),
            next_isolate_id: 1,
            ids,
            trace: Vec::with_capacity(preallocation.trace_capacity),
            trace_start: 0,
            trace_retention: TraceRetention::Full,
            trace_dropped: 0,
            driver,
            call_table: CallTable::new(),
            clock,
            pending_remote_spawns: Vec::new(),
            remote_spawn_cancel_tombstones: std::collections::VecDeque::with_capacity(64),
            remote_child_control_capacity: 64,
            remote_child_control_full: 0,
            round_messages: Vec::with_capacity(preallocation.round_scratch_capacity),
            driver_completions: Vec::with_capacity(preallocation.call_capacity),
            pending_completions: std::collections::VecDeque::new(),
            driver_completion_drain_budget: DEFAULT_DRIVER_COMPLETION_DRAIN_BUDGET,
            next_isolate_call_ordinal: 0,
            observation: observation::ObservationRegistry::new(),
            trace_observer: None,
            deferred_registry: Rc::new(DeferredSlotRegistry::new()),
            promoted_slots: deferred::PromotedSlots::default(),
            cancelled_calls: std::collections::VecDeque::with_capacity(
                CANCELLED_CALL_RING_CAPACITY,
            ),
            cancelled_call_cause_evictions: 0,
            unsupported_message_rejections: 0,
        }
    }

    /// Returns a shared reference to the shard.
    pub const fn shard(&self) -> &S {
        &self.shard
    }

    /// Returns the active trace retention policy.
    pub const fn trace_retention(&self) -> TraceRetention {
        self.trace_retention
    }

    /// Returns how many recently-cancelled call causes were evicted
    /// from the bounded attribution ring.
    ///
    /// Late replies for evicted calls still reject honestly, but they
    /// fall back to generic caller-closed/no-pending reasons because
    /// the exact cancellation cause is no longer retained.
    pub const fn cancelled_call_cause_evictions(&self) -> u64 {
        self.cancelled_call_cause_evictions
    }

    /// Debug tripwire count: how many `call()` messages this shard has
    /// rejected as `UnsupportedMessage`.
    ///
    /// `UnsupportedMessage` is the default `handle_call`'s signature — an
    /// isolate that receives `call()` traffic but never defined `handle_call`.
    /// A non-zero count in a debug build almost always means a target answers
    /// calls but only implements `handle`. Always 0 in release builds (the
    /// increment is compiled out for zero cost).
    pub const fn unsupported_message_rejections(&self) -> u64 {
        self.unsupported_message_rejections
    }

    /// Returns the number of trace events dropped by the retention policy.
    pub const fn trace_dropped(&self) -> u64 {
        self.trace_dropped
    }

    /// Whether the retention policy has dropped any events.
    ///
    /// When true, [`trace`](Self::trace) returns only the retained suffix,
    /// so hashing or summarizing it reflects a partial run. Proof helpers
    /// should read [`trace_for_proof`](Self::trace_for_proof) instead.
    pub const fn trace_is_truncated(&self) -> bool {
        self.trace_dropped > 0
    }

    /// Returns the trace only when it is the whole run.
    ///
    /// Fails closed with [`TraceTruncated`] once the retention policy has
    /// dropped events, so `stable_trace_hash` / `PressureSummary` cannot be
    /// computed over a silent partial suffix. Under the default `Full`
    /// retention this always returns the trace.
    pub fn trace_for_proof(&self) -> Result<&[RuntimeEvent], TraceTruncated> {
        if self.trace_dropped > 0 {
            return Err(TraceTruncated {
                dropped_events: self.trace_dropped,
            });
        }
        Ok(self.trace())
    }
}

/// The retention policy dropped trace events, so the in-memory trace is a
/// partial suffix that must not back a hash or pressure summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceTruncated {
    /// How many events the retention policy dropped.
    pub dropped_events: u64,
}

impl std::fmt::Display for TraceTruncated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "trace retention dropped {} event(s); the in-memory trace is a partial suffix \
             and cannot back a proof hash or pressure summary",
            self.dropped_events
        )
    }
}

impl std::error::Error for TraceTruncated {}

#[cfg(test)]
mod tests;
