#![deny(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]
// Phase Mariner 012 substrate is Betelgeuse, which exposes its
// `IOLoopHandle<A: Allocator>` over the unstable `allocator_api`. We
// commit to nightly Rust for `tina-runtime` per the reopened
// 012 plan; the feature gate is scoped to this crate.
#![feature(allocator_api)]

//! Small deterministic single-shard runtime core for `tina-rs`.
//!
//! This crate starts Mariner with the narrowest useful runtime surface:
//!
//! - deterministic runtime event IDs
//! - causal links between runtime events
//! - a tiny single-shard runtime that can host more than one isolate
//!
//! The multi-isolate runtime still stays narrow on purpose. It can register
//! isolates, step them in deterministic order, execute local same-shard
//! [`tina::Effect::Send`] requests that use [`tina::Outbound`], spawn local
//! children, and restart direct restartable children. Reply effects are still
//! traced without execution until a later slice gives them runtime semantics.
//!
//! `Effect::Stop` stays immediate, but `Runtime` also drains and
//! traces any already-buffered messages that become abandoned when an isolate
//! stops.
//!
//! `Runtime` also captures unwinding panics from handler calls and turns
//! them into deterministic runtime events. Binaries built with `panic = "abort"`
//! remain out of scope for this crate.

use std::alloc::Global;
use std::any::Any;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use tina::{DeferredSlotRegistry, Shard};

use betelgeuse::IOLoopHandle;

pub mod admission;
pub mod bridge;
mod call;
mod call_group;
mod capabilities;
pub mod capacity;
mod clock;
pub mod deferred;
mod drain_state;
mod driver;
mod errors;
pub mod event_sink;
mod fact;
pub mod file_loops;
mod full_handling;
pub mod guarded_pending;
mod host_burst;
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
pub mod scope;
pub mod service_pressure;
pub mod sharded;
pub mod shared_scope;
pub mod shared_work;
mod shutdown;
mod single_call_gate;
pub mod tcp_loops;
mod threaded;
mod threaded_multi_shard;
mod trace;
pub mod wait_list;

pub use admission::{
    AdmissionDecision, AdmissionFailure, AdmissionReport, ConcurrencyLimit, ConcurrencyPermit,
    ConcurrencyReleaseError, KeyedLimit, KeyedPermit, KeyedReleaseError, KeyedSlotReport,
    PressureAction, RateGrant, RateKeyState, RateLimit, SurfaceName,
};
pub use drain_state::{AdmitDecision, DrainReport, DrainStage, DrainState};
pub use errors::{
    RegisterBootstrapError, SendObservedUntilError, ShutdownRequestError, ShutdownWaitError,
    SuperviseError, ThreadedRegisterBootstrapError, ThreadedRuntimeError,
    ThreadedSendObservedError, ThreadedTrySendError,
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
};
pub use multi_shard::{MultiShardRuntime, MultiShardRuntimeConfig};

mod dispatch;
mod host_call;
mod registration;
mod remote;
mod service_handle;
pub use service_handle::{SendOnlyServiceHandle, ServiceHandle, SplitServiceHandle};

use remote::{QueuedRemoteEnvelope, SendableQueuedRemoteEnvelope};

pub(crate) use dispatch::{
    AnyMailboxAdapter, ChildRecord, ErasedMailbox, ErasedMessage, ErasedSend, HandlerAdapter,
    IntoErasedSpawn, IntoErasedSpawnObserved, MailboxAdapter, RegisteredAddress, RegisteredEntry,
    SendableHandlerAdapter, SpawnOutcome, SupervisorRecord,
};
#[cfg(test)]
pub(crate) use dispatch::{ChildRecordSnapshot, SupervisorRecordSnapshot};

pub use shutdown::ThreadedShutdownHandle;
pub use single_call_gate::SingleCallGate;
pub use threaded::{DEFAULT_SHUTDOWN_LANE_DRAIN_TIMEOUT, ThreadedRuntime, ThreadedRuntimeConfig};
pub use threaded_multi_shard::ThreadedMultiShardRuntime;

pub use live_report::{
    AffinityStatus, LiveQueueReport, LiveRemoteQueueReport, LiveShardReport, LiveShardState,
    LiveTopologyReport,
};

pub use capabilities::{
    CancellationSupport, DriverRuntimeRequirement, DurabilityCapability, ResourceCapability,
    ResourceExecutionShape, ResourceSupport, RuntimeCapabilities, ShutdownSupport,
    TINA_DRIVER_RUNTIME_CONTRACT, TinaDriverRuntimeContract,
};
#[cfg(test)]
use clock::ManualClock;
use clock::{Clock, MonotonicClock};
pub use mailbox::{DefaultMailboxFactory, DefaultThreadedMailboxFactory, MailboxFactory};

pub use crate::persistence::{
    LOCAL_PERSISTENCE_SUPPORT, LocalPersistenceSupport, PersistenceSupportLevel,
};
#[allow(deprecated)]
pub use call::{
    AdmitWorkError, CallError, CallId, CallInput, CallOutcome, CallOutput, CallReply,
    CancelableCall, CancelableWork, CancelableWorkSnapshot, DeferredCancelableCall,
    DeferredIsolateCall, DeferredObservedSend, DeferredTypedCall, DnsLookupReply, ErasedCall,
    FileCloseReply, FileFsyncReply, FileId, FileOpenOptions, FileOpenReply, FileReadReply,
    FileSizeReply, FileWriteReply, IntoErasedCall, IsolateCall, IsolateCallWithHandle,
    JournalAppendReply, JournalRecord, JournalReplay, JournalReplayReply, JournalReplayWarning,
    ListenerId, MkdirReply, PathKind, PathMetadata, PathMetadataReply, PendingCancelableCall,
    PendingCancelableCallSet, PendingCancelableInsertError, PendingCancelableRemoveError,
    PendingCancelableTicket, PersistenceTraceInfo, ProcessRunReply, ProcessRunResult,
    ProcessStatus, ReadDirReply, RemoveFileReply, RenameReplaceReply,
    RequestDeferredCancelableCall, RequestDeferredIsolateCall, RequestDeferredObservedSend,
    RequestDeferredTypedCall, RuntimeCall, RuntimeCallParts, RuntimeCallable, SendOutcome,
    SignalWaitReply, SleepCall, SleepReply, SnapshotCommitReply, SnapshotImage, SnapshotLoadReply,
    StreamId, SyncParentReply, TcpAcceptReply, TcpBindReply, TcpConnectReply,
    TcpListenerCloseReply, TcpReadReply, TcpStreamCloseReply, TcpWriteReply, TlsAcceptReply,
    TlsBindReply, TlsCloseReply, TlsConnectReply, TlsListenerCloseReply, TlsListenerId,
    TlsReadReply, TlsStreamId, TlsWriteReply, TypedCall, UdpBindReply, UdpCloseSocketReply,
    UdpRecvFromReply, UdpSendToReply, UdpSocketId, UnixAcceptReply, UnixBindReply,
    UnixConnectReply, UnixListenerCloseReply, UnixListenerId, UnixReadReply, UnixStreamCloseReply,
    UnixStreamId, UnixWriteReply, WorkTicket, call, call_cancelable, call_handle_call_id,
    call_request, call_typed, call_with_handle, cancel_call, dns_lookup, file_close, file_create,
    file_fsync, file_open, file_read, file_read_at, file_size, file_write, file_write_at,
    journal_append, journal_replay, mkdir, path_metadata, process_run, read_dir, remove_file,
    rename_replace, send_observed, signal_wait, sleep, sleep_then, snapshot_commit, snapshot_load,
    sync_parent, tcp_accept, tcp_bind, tcp_close_listener, tcp_close_stream, tcp_connect, tcp_read,
    tcp_write, tls_accept, tls_accept_alpn, tls_bind, tls_bind_alpn, tls_close, tls_close_listener,
    tls_connect, tls_connect_alpn, tls_read, tls_write, udp_bind, udp_close_socket, udp_recv_from,
    udp_send_to, unix_accept, unix_bind, unix_close_listener, unix_close_stream, unix_connect,
    unix_read, unix_write,
};
pub use call_group::{
    CallGroup, CallGroupBranchOutcome, CallGroupCancelOutcome, CallGroupCancelRequest,
    CallGroupInsertError, CallGroupRecordCancelError, CallGroupRecordReplyError,
    CallGroupReplyStep, CallGroupReport, CallGroupReserveError, CallGroupToken,
};
pub use capacity::{
    CapacityAssertError, CapacityNameError, CapacitySummary, SurfaceAssertion,
    format_assertion_failure, format_discovery_line, format_discovery_report,
};
pub use deferred::{
    InsertError as PendingRepliesInsertError, ParkCallError, ParkError, ParkTicket, PendingReplies,
    ReplyParkedError, TakeParkedError, TryCaptureError as PendingRepliesTryCaptureError,
    request_effect_after_park,
};
use driver::DriverCompletion;
pub use event_sink::{
    BoundedEventSink, DropPolicy, EventSinkDrain, EventSinkReport, EventSinkSurface,
};
pub use fact::{
    GrpcStatusCode, GrpcStreamId, Http2CloseReason, Http2FlowControlSide, Http2ResetReason,
    Http2StreamId, IntoRuntimeFact, ProtocolConnectionId, ProtocolDirection, ProtocolFact,
    ProtocolFamily, RuntimeFact, WebSocketCloseReason, WebSocketSessionId,
};
pub use file_loops::{
    CopyLeg, FileCopyBounded, FileLoopEnd, FileLoopReport, FileLoopStep, FileReadChunks,
    FileWriteAll,
};
pub use guarded_pending::{
    GuardedInsertError, GuardedParkCallError, GuardedParkError, GuardedParkTicket,
    GuardedPendingReplies, GuardedReplyError, GuardedTakeError,
};
pub use observation::{
    BoundAddressWaiter, ChildRestarted, ChildRestartedWaiter, IsolateCompleteWaiter,
    IsolateResultWaiter, OperationDoneWaiter, ResultWaitError, WaitError,
};
pub use observer::{BufferedTraceObserver, TraceObserver};
pub use pressure::{MailboxBudget, PressureReport, PressureSummary, format_pressure_line};
pub use scope::{
    CallContextScopeExt, DeferScopedThrough, DeferredScopedCall, RequestScope, RequestScopeId,
    RequestScopeInsertError, RequestScopeRemoveError, RequestScopeSet,
    RequestScopeSetCapacityReport, ScopeCancelCause, ScopeCancelReport, ScopeChildReport,
    ScopeRegisterError, ScopeRegisterSharedError, ScopedAdmitError, ScopedCallHandle,
    ScopedReplyError, scope_register,
};
pub use service_pressure::{ServicePressureReport, ServicePressureSurface, ServiceSurfaceState};
pub use shared_scope::{SharedCapacityScope, SharedLease, SharedScopeFull, SharedScopeReport};
pub use shared_work::{
    SharedWork, SharedWorkCallError, SharedWorkError, SharedWorkReplyError, SharedWorkSnapshot,
    SharedWorkTicket, request_effect_after_shared_wait,
};
pub use tcp_loops::{LoopStep, ReadExactStep, TcpReadExact, TcpReadToEof, TcpWriteAll};
/// Declares a Tina isolate whose call channel defaults to [`RuntimeCall<Message>`](RuntimeCall).
///
/// This is the preferred runtime authoring path. It keeps the handler as normal
/// Rust code and only fills the repetitive [`tina::Isolate`] associated types.
///
/// **The expansion is rooted at `::tina`.** The generated impl names
/// `::tina::Isolate`, `::tina::Effect`, `::tina::Context`, and friends, so the
/// crate using this macro must depend on `tina` and have it reachable as
/// `::tina` (the default crate name). Only the call channel is rooted at
/// `::tina_runtime`. A crate that depends on `tina-runtime` alone will fail to
/// compile with `unresolved import ::tina`; add `tina` as a direct dependency,
/// or override the root with `#[tina_runtime::isolate(.., tina_crate = ::your_path)]`.
pub use tina_macros::runtime_isolate as isolate;
pub use trace::{
    CallCompletionRejectedReason, CallKind, CallReplyRejectedReason, CauseId,
    DeferredReplyRejectedReason, DeferredSlotId, EffectKind, EventId, RestartSkippedReason,
    RuntimeEvent, RuntimeEventKind, RuntimeTraceExt, SendRejectedReason, SupervisionRejectedReason,
    stable_trace_hash,
};
pub use wait_list::{
    WaitCallError as WaitListCallError, WaitError as WaitListError, WaitList,
    WaitReplyError as WaitListReplyError, WaitSnapshot as WaitListSnapshot, WaitTicket,
    request_effect_after_wait_park,
};

pub use driver::os_signal_capture_supported;
use driver::{BetelgeuseDriver, RuntimeDriver};

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

#[derive(Debug, Clone)]
pub(crate) struct IdSource {
    pub(crate) next_event_id: Arc<AtomicU64>,
    pub(crate) next_call_id: Arc<AtomicU64>,
}

impl IdSource {
    pub(crate) fn new() -> Self {
        Self {
            next_event_id: Arc::new(AtomicU64::new(1)),
            next_call_id: Arc::new(AtomicU64::new(1)),
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

/// Small deterministic single-shard runtime for the second Mariner slice.
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
    pub(crate) child_records: Vec<ChildRecord<S, F>>,
    pub(crate) supervisors: Vec<SupervisorRecord>,
    pub(crate) next_isolate_id: u64,
    pub(crate) ids: IdSource,
    pub(crate) trace: Vec<RuntimeEvent>,
    pub(crate) trace_start: usize,
    pub(crate) trace_retention: TraceRetention,
    pub(crate) trace_dropped: u64,
    pub(crate) driver: Box<dyn RuntimeDriver>,
    pub(crate) in_flight_calls: Vec<InFlightCall>,
    pub(crate) translators: Vec<StoredTranslator>,
    pub(crate) clock: Box<dyn Clock>,
    pub(crate) pending_isolate_calls: Vec<PendingIsolateCall>,
    pub(crate) round_messages: Vec<Option<DeliveredMessage>>,
    pub(crate) driver_completions: Vec<DriverCompletion>,
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

#[derive(Debug)]
pub(crate) struct InFlightCall {
    pub(crate) call_id: CallId,
    pub(crate) call_kind: CallKind,
    pub(crate) requester: RegisteredAddress,
    pub(crate) cause: CauseId,
    pub(crate) persistence: Option<call::PersistenceTraceInfo>,
    pub(crate) continuation_context: Option<MessageCallContext>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CallDispatchContext {
    pub(crate) call_id: CallId,
    pub(crate) requester: RegisteredAddress,
    pub(crate) cause: CauseId,
    pub(crate) continuation_context: Option<MessageCallContext>,
}

pub(crate) type ErasedTranslator = Box<dyn FnOnce(CallOutput) -> Box<dyn Any>>;
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

/// Runtime trace retention policy.
///
/// Tests usually want [`Full`](Self::Full) so replay artifacts keep every
/// event. Live services can use [`Bounded`](Self::Bounded) or [`Off`](Self::Off)
/// so observability does not become another hidden unbounded queue.
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

pub(crate) struct StoredTranslator {
    pub(crate) call_id: CallId,
    pub(crate) translator: Option<ErasedTranslator>,
}

impl std::fmt::Debug for StoredTranslator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoredTranslator")
            .field("call_id", &self.call_id)
            .finish_non_exhaustive()
    }
}

pub(crate) struct PendingIsolateCall {
    pub(crate) call_id: CallId,
    pub(crate) requester: RegisteredAddress,
    pub(crate) cause: CauseId,
    pub(crate) deadline: Instant,
    pub(crate) insertion_order: u64,
    pub(crate) continuation_context: Option<MessageCallContext>,
    pub(crate) translator: Option<ErasedIsolateCallTranslator>,
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
            child_records: Vec::with_capacity(preallocation.child_record_capacity),
            supervisors: Vec::with_capacity(preallocation.supervisor_capacity),
            next_isolate_id: 1,
            ids,
            trace: Vec::with_capacity(preallocation.trace_capacity),
            trace_start: 0,
            trace_retention: TraceRetention::Full,
            trace_dropped: 0,
            driver,
            in_flight_calls: Vec::with_capacity(preallocation.call_capacity),
            translators: Vec::with_capacity(preallocation.call_capacity),
            clock,
            pending_isolate_calls: Vec::with_capacity(preallocation.call_capacity),
            round_messages: Vec::with_capacity(preallocation.round_scratch_capacity),
            driver_completions: Vec::with_capacity(preallocation.call_capacity),
            next_isolate_call_ordinal: 0,
            observation: observation::ObservationRegistry::new(),
            trace_observer: None,
            deferred_registry: Rc::new(DeferredSlotRegistry::new()),
            promoted_slots: deferred::PromotedSlots::default(),
            cancelled_calls: std::collections::VecDeque::with_capacity(
                CANCELLED_CALL_RING_CAPACITY,
            ),
            cancelled_call_cause_evictions: 0,
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

    /// Returns the number of trace events dropped by the retention policy.
    pub const fn trace_dropped(&self) -> u64 {
        self.trace_dropped
    }
}

#[cfg(test)]
mod tests;
