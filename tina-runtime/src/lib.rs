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
//! [`Effect::Send`] requests that use [`tina::Outbound`], spawn local
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
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tina::{
    Address, AddressGeneration, CallContext, CallRejectedReason, CallRouting, ChildRef,
    ChildRelation, Context, DeferredReplyHandle, DeferredSlotRegistry, DeferredSlotState, Effect,
    Isolate, IsolateId, Mailbox, MessageCaller, Outbound as TinaOutbound, RestartBudgetState,
    Shard, ShardId, SpawnObservedError, StopResult, TrySendError,
};
use tina_supervisor::SupervisorConfig;

use betelgeuse::IOLoopHandle;

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

/// Capability-typed handles for one registered callable service.
///
/// Returned by [`Runtime::register_service`]. The `send` lane is a
/// [`SendAddress<M>`](tina::SendAddress) for ordinary send/continuation
/// traffic; the `call` lane is a [`CallAddress<M, R>`](tina::CallAddress) for
/// callable traffic. Splitting the address into capabilities at the boundary
/// turns "called a send-only handle" / "sent through a call-only handle" into
/// compile errors rather than runtime
/// [`tina::CallRejectedReason::UnsupportedMessage`] rejections.
///
/// Both lanes point at the same underlying isolate. The split is purely a
/// type-level capability check at the caller; the runtime continues to route
/// based on whether the inbound message arrived with a reply slot.
#[derive(Debug)]
pub struct ServiceHandle<M, R> {
    /// Send-only capability for the service's mailbox.
    pub send: tina::SendAddress<M>,
    /// Callable capability for the service's mailbox.
    pub call: tina::CallAddress<M, R>,
}

impl<M, R> Copy for ServiceHandle<M, R> {}

impl<M, R> Clone for ServiceHandle<M, R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<M, R> ServiceHandle<M, R> {
    /// Wraps a raw [`Address<M, R>`](tina::Address) as both capabilities.
    pub const fn from_address(address: tina::Address<M, R>) -> Self {
        Self {
            send: address.send_only(),
            call: address.callable(),
        }
    }

    /// Returns the underlying raw [`Address<M, R>`](tina::Address).
    pub const fn address(self) -> tina::Address<M, R> {
        self.call.address()
    }
}

/// Capability-typed handle for one registered send-only service.
///
/// Returned by [`Runtime::register_service_send_only`]. Exposes only a
/// [`SendAddress`](tina::SendAddress): no callable lane is constructed.
#[derive(Debug)]
pub struct SendOnlyServiceHandle<M> {
    /// Send-only capability for the service's mailbox.
    pub send: tina::SendAddress<M>,
}

impl<M> Copy for SendOnlyServiceHandle<M> {}

impl<M> Clone for SendOnlyServiceHandle<M> {
    fn clone(&self) -> Self {
        *self
    }
}

/// Capability-typed handles for one split event/request service.
///
/// Returned by [`Runtime::register_split_service`]. The `events` lane accepts
/// only public fire-and-forget events through [`tina::send_event`]. The
/// `requests` lane accepts only callable requests through [`call_request`].
#[derive(Debug)]
pub struct SplitServiceHandle<Event, Request, Reply> {
    /// Send capability for service events.
    pub events: tina::ServiceEventAddress<Event, Request>,
    /// Call capability for service requests.
    pub requests: tina::ServiceRequestAddress<Event, Request, Reply>,
}

impl<Event, Request, Reply> Copy for SplitServiceHandle<Event, Request, Reply> {}

impl<Event, Request, Reply> Clone for SplitServiceHandle<Event, Request, Reply> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Event, Request, Reply> SplitServiceHandle<Event, Request, Reply> {
    /// Wraps the raw service envelope address as split capabilities.
    pub const fn from_address(
        address: tina::Address<tina::ServiceMessage<Event, Request>, Reply>,
    ) -> Self {
        Self {
            events: tina::ServiceEventAddress::from_send_address(address.send_only()),
            requests: tina::ServiceRequestAddress::from_call_address(address.callable()),
        }
    }

    /// Returns the underlying raw envelope address.
    pub const fn address(self) -> tina::Address<tina::ServiceMessage<Event, Request>, Reply> {
        self.requests.address().address()
    }
}
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
    UdpRecvFromReply, UdpSendToReply, UdpSocketId, WorkTicket, call, call_cancelable,
    call_handle_call_id, call_request, call_typed, call_with_handle, cancel_call, dns_lookup,
    file_close, file_create, file_fsync, file_open, file_read, file_read_at, file_size, file_write,
    file_write_at, journal_append, journal_replay, mkdir, path_metadata, process_run, read_dir,
    remove_file, rename_replace, send_observed, signal_wait, sleep, sleep_then, snapshot_commit,
    snapshot_load, sync_parent, tcp_accept, tcp_bind, tcp_close_listener, tcp_close_stream,
    tcp_connect, tcp_read, tcp_write, tls_accept, tls_bind, tls_close, tls_close_listener,
    tls_connect, tls_read, tls_write, udp_bind, udp_close_socket, udp_recv_from, udp_send_to,
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
use driver::{BetelgeuseDriver, DriverResourceReport, DriverShutdownError, RuntimeDriver};

#[derive(Debug, Clone, Copy)]
enum MessageCallContext {
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

struct DeliveredMessage {
    message: Box<dyn Any>,
    call_context: Option<MessageCallContext>,
}

#[derive(Debug, Clone)]
struct IdSource {
    next_event_id: Arc<AtomicU64>,
    next_call_id: Arc<AtomicU64>,
}

impl IdSource {
    fn new() -> Self {
        Self {
            next_event_id: Arc::new(AtomicU64::new(1)),
            next_call_id: Arc::new(AtomicU64::new(1)),
        }
    }

    fn next_event_id(&self) -> EventId {
        let raw = self.next_event_id.fetch_add(1, Ordering::Relaxed);
        EventId::new(raw)
    }

    fn next_call_id(&self) -> CallId {
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
    shard: S,
    mailbox_factory: F,
    entries: Vec<RegisteredEntry<S, F>>,
    child_records: Vec<ChildRecord<S, F>>,
    supervisors: Vec<SupervisorRecord>,
    next_isolate_id: u64,
    ids: IdSource,
    trace: Vec<RuntimeEvent>,
    trace_start: usize,
    trace_retention: TraceRetention,
    trace_dropped: u64,
    driver: Box<dyn RuntimeDriver>,
    in_flight_calls: Vec<InFlightCall>,
    translators: Vec<StoredTranslator>,
    clock: Box<dyn Clock>,
    pending_isolate_calls: Vec<PendingIsolateCall>,
    round_messages: Vec<Option<DeliveredMessage>>,
    driver_completions: Vec<DriverCompletion>,
    next_isolate_call_ordinal: u64,
    observation: observation::ObservationRegistry,
    /// Live trace observer. Fires before retention. See [`crate::TraceObserver`].
    trace_observer: observer::StoredObserver,
    /// Tina-owned slot-id source and pending-capture queue. Cloned
    /// (refcount bump, no allocation) into each per-message
    /// [`MessageCaller`].
    deferred_registry: Rc<DeferredSlotRegistry>,
    /// Promoted-slot table. Owned solely by the runtime.
    promoted_slots: deferred::PromotedSlots,
    /// Bounded ring of recently-cancelled calls plus the cause that
    /// closed each one. Late callee replies for these settle as a
    /// reason that matches the cause (`CallerCancelled`,
    /// `CallerTimedOut`, `OwnerStopped`, `RuntimeStopped`) instead of
    /// the less-specific `NoPendingCall` / `CallerClosed`. Bounded at
    /// [`CANCELLED_CALL_RING_CAPACITY`] — older entries are evicted,
    /// at which point fall-through to the generic reason is honest.
    ///
    /// Single-writer (this shard's runtime); no concurrent access.
    cancelled_calls: std::collections::VecDeque<(CallId, tina::CancelCause)>,
    cancelled_call_cause_evictions: u64,
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
struct InFlightCall {
    call_id: CallId,
    call_kind: CallKind,
    requester: RegisteredAddress,
    cause: CauseId,
    persistence: Option<call::PersistenceTraceInfo>,
    continuation_context: Option<MessageCallContext>,
}

#[derive(Debug, Clone, Copy)]
struct CallDispatchContext {
    call_id: CallId,
    requester: RegisteredAddress,
    cause: CauseId,
    continuation_context: Option<MessageCallContext>,
}

type ErasedTranslator = Box<dyn FnOnce(CallOutput) -> Box<dyn Any>>;
type ErasedIsolateCallTranslator = Box<dyn FnOnce(CallOutcome<Box<dyn Any>>) -> Box<dyn Any>>;

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

fn reserve_round_message_scratch(
    round_messages: &mut Vec<Option<DeliveredMessage>>,
    entry_count: usize,
) {
    debug_assert!(round_messages.is_empty());
    if round_messages.capacity() < entry_count {
        round_messages.reserve(entry_count);
    }
}

struct StoredTranslator {
    call_id: CallId,
    translator: Option<ErasedTranslator>,
}

impl std::fmt::Debug for StoredTranslator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoredTranslator")
            .field("call_id", &self.call_id)
            .finish_non_exhaustive()
    }
}

struct PendingIsolateCall {
    call_id: CallId,
    requester: RegisteredAddress,
    cause: CauseId,
    deadline: Instant,
    insertion_order: u64,
    continuation_context: Option<MessageCallContext>,
    translator: Option<ErasedIsolateCallTranslator>,
    /// `TypeId::of::<R>()` for the dispatching `Address<_, R>`. Used
    /// to typecheck deferred-reply payloads before they reach the
    /// translator's downcast.
    expected_reply_type_id: std::any::TypeId,
    /// Optional caller-owned cancellation cell. The runtime updates
    /// `state` to `Settled` on completion/timeout/closed, or
    /// `Cancelled` on explicit `cancel_call`.
    handle_shared: Option<std::sync::Arc<tina::CallHandleShared>>,
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
    fn with_clock(shard: S, mailbox_factory: F, clock: Box<dyn Clock>) -> Self {
        Self::with_clock_and_ids(shard, mailbox_factory, clock, IdSource::new())
    }

    fn with_clock_and_ids(
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

    fn with_clock_and_ids_and_driver(
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

    fn with_clock_and_ids_and_driver_and_preallocation(
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

    /// Returns whether the runtime has any in-flight calls that have not
    /// yet been delivered. Tests use this to know when stepping further
    /// can produce more I/O completions.
    pub fn has_in_flight_calls(&self) -> bool {
        !self.in_flight_calls.is_empty()
            || self.driver.has_pending()
            || !self.pending_isolate_calls.is_empty()
    }

    #[cfg(test)]
    fn io_pending_count(&self) -> usize {
        self.driver.io_pending_count()
    }

    fn resource_report(&self) -> DriverResourceReport {
        self.driver.resource_report()
    }

    /// Returns a shared reference to the shard.
    pub const fn shard(&self) -> &S {
        &self.shard
    }

    /// Returns the accumulated runtime trace.
    pub fn trace(&self) -> &[RuntimeEvent] {
        &self.trace[self.trace_start..]
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

    #[cfg(test)]
    fn trace_storage_len(&self) -> usize {
        self.trace.len()
    }

    #[cfg(test)]
    fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Walks the current trace and returns a counted summary of
    /// pressure-shaped events (mailbox-full, reply-path-full,
    /// send-full, lifecycle-closed). See [`PressureSummary`].
    pub fn pressure_summary(&self) -> pressure::PressureSummary {
        pressure::PressureSummary::from_events(self.trace.iter())
    }

    /// Registers a typed waiter for the next `tcp_bind` completion.
    ///
    /// Returns a [`BoundAddressWaiter`] that the host can `wait` on to
    /// receive the bound `SocketAddr` (or a typed error). Each call returns
    /// a fresh waiter; multiple registrations are served in registration
    /// order as `tcp_bind` calls complete. The waiter is bounded one-slot:
    /// no hidden queue is created.
    ///
    /// The trace remains the source of audit truth: this method does not
    /// add a new event class, it only surfaces the bound address that
    /// [`CallOutput::TcpBound`] already carries inside the runtime.
    pub fn observe_next_bound(&mut self) -> BoundAddressWaiter {
        self.observation.register_bound()
    }

    /// Registers a typed waiter for the next `tls_bind` completion.
    /// Mirrors [`Self::observe_next_bound`] for the TLS rail. The
    /// waiter resolves with the bound `SocketAddr` carried by
    /// [`CallOutput::TlsBound`], or with the typed runtime error.
    pub fn observe_next_tls_bound(&mut self) -> BoundAddressWaiter {
        self.observation.register_tls_bound()
    }

    /// Registers a typed waiter for the targeted isolate's `IsolateStopped`.
    ///
    /// The waiter resolves the next time the isolate identified by `address`
    /// (matched by isolate id and generation) emits
    /// [`RuntimeEventKind::IsolateStopped`]. Replaces `Arc<AtomicBool>` done
    /// flags in user code. Bounded one-slot.
    pub fn observe_isolate_complete<M, R>(
        &mut self,
        address: Address<M, R>,
    ) -> observation::IsolateCompleteWaiter {
        self.observation
            .register_isolate_complete(address.isolate(), address.generation())
    }

    /// Registers a typed waiter for the next runtime call of `call_kind`
    /// issued by the isolate identified by `address` that completes (success
    /// or failure).
    ///
    /// Replaces `complete_trace()` polling for a specific
    /// `CallKind::TcpStreamClose` / `CallKind::Sleep` / etc. event in user
    /// code. Bounded one-slot; the runtime drops the slot once a matching
    /// completion lands.
    pub fn observe_operation_done<M, R>(
        &mut self,
        address: Address<M, R>,
        call_kind: CallKind,
    ) -> observation::OperationDoneWaiter {
        self.observation
            .register_operation_done(address.isolate(), call_kind)
    }

    /// Registers a typed waiter for the next supervised restart of any
    /// direct child of the parent identified by `parent_address`.
    ///
    /// The resolved [`observation::ChildRestarted`] carries the new child
    /// incarnation's isolate id and generation. Bounded one-slot.
    pub fn observe_child_restarted<M, R>(
        &mut self,
        parent_address: Address<M, R>,
    ) -> observation::ChildRestartedWaiter {
        self.observation
            .register_child_restarted(parent_address.isolate())
    }

    /// Registers a typed result waiter for the isolate at `address`.
    ///
    /// Resolves when the isolate stops via [`tina::stop_with`] with a value
    /// of type `T`. Single-claim per `(IsolateId, AddressGeneration)`.
    /// Eager errors:
    ///
    /// - `AlreadyStopped` — isolate is no longer alive at this generation
    ///   (no replay cache);
    /// - `AlreadyClaimed` — another waiter holds the slot;
    /// - `ObservationFull` — observation cap reached.
    ///
    /// `wait` outcomes: `Timeout`, `RuntimeStopped`, `StoppedWithoutResult`
    /// (isolate used `stop()` not `stop_with(_)`), `TypeMismatch`.
    pub fn observe_result<T, M, R>(
        &mut self,
        address: Address<M, R>,
    ) -> Result<observation::IsolateResultWaiter<T>, observation::ResultWaitError>
    where
        T: Send + 'static,
    {
        let isolate = address.isolate();
        let generation = address.generation();
        let alive = self.entries.iter().any(|entry| {
            entry.id == isolate && entry.generation == generation && !entry.stopped.get()
        });
        if !alive {
            return Err(observation::ResultWaitError::AlreadyStopped);
        }
        self.observation
            .register_isolate_result::<T>(isolate, generation)
    }

    /// Sets the trace retention policy for future events.
    ///
    /// Lowering retention trims the current trace immediately so callers can
    /// rely on the memory bound after this returns.
    pub fn set_trace_retention(&mut self, retention: TraceRetention) {
        self.trace_retention = retention;
        self.enforce_trace_retention();
    }

    /// Sets the live trace observer. `None` detaches. See
    /// [`crate::TraceObserver`] for hook rules. On `ThreadedRuntime` /
    /// `LocalSystem`, prefer the build-time wiring so no events fire
    /// before the hook is in place.
    pub fn set_trace_observer(&mut self, observer: Option<Arc<dyn TraceObserver>>) {
        self.trace_observer = observer;
    }

    /// Cancels every in-flight runtime-owned call ahead of shutdown.
    ///
    /// The terminal-outcome priority for any call that could resolve
    /// multiple ways is fixed:
    /// 1. **requester stopped/full**: a stopped or full requester wins
    ///    its local completion path (the in-flight-call entry was
    ///    already removed when the requester stopped, so any later
    ///    completion routes through `RequesterClosed` tombstoning).
    /// 2. **shard failed**: a failed source/destination shard wins over
    ///    a later success because [`LiveShardState::Failed`] gates
    ///    ingress and the worker thread has stopped delivering.
    /// 3. **timeout**: a deadline that fired before the failure was
    ///    observed wins the call's result via `CallError::Timeout`.
    /// 4. **full transport/mailbox**: full reasons only stick when no
    ///    higher-priority terminal state already exists.
    ///
    /// The "exactly one terminal outcome" property is enforced
    /// structurally by the `in_flight_calls` map: the first terminal
    /// event removes the entry; subsequent attempts hit a missing
    /// call_id and tombstone (or get dropped at the lane's
    /// `finish_completion` when the user-cancelled flag is set).
    fn cancel_in_flight_calls_for_shutdown(
        &mut self,
        deadline: Instant,
    ) -> Result<(), DriverShutdownError> {
        let driver_result = self.driver.cancel_pending(deadline);
        self.translators.clear();

        let in_flight_calls = std::mem::take(&mut self.in_flight_calls);
        for call in in_flight_calls {
            self.push_event(
                call.requester.isolate,
                Some(call.cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id: call.call_id,
                    call_kind: call.call_kind,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                },
            );
        }

        let pending_isolate_calls = std::mem::take(&mut self.pending_isolate_calls);
        for call in pending_isolate_calls {
            // Mark any caller-held CallHandle as Cancelled so a late
            // poll of `handle.state()` reflects the truth instead of
            // staying `Pending` forever.
            if let Some(shared) = &call.handle_shared {
                shared.set_state(tina::CallHandleState::Cancelled);
            }
            // Record the cause so any late callee reply that races
            // shutdown surfaces as `RuntimeStopped` rather than the
            // generic `NoPendingCall` / `CallerClosed`.
            self.record_cancelled_call(call.call_id, tina::CancelCause::RuntimeStopped);
            self.push_event(
                call.requester.isolate,
                Some(call.cause),
                RuntimeEventKind::CallCancelled {
                    call_id: call.call_id,
                    cause: tina::CancelCause::RuntimeStopped,
                },
            );
        }
        driver_result
    }

    fn notify_signal(&mut self, name: &str) {
        let mut completed = std::mem::take(&mut self.driver_completions);
        completed.clear();
        self.driver.notify_signal(name, &mut completed);
        for op in completed.drain(..) {
            self.deliver_completion(op.call_id, op.result);
        }
        self.driver_completions = completed;
    }

    fn cancel_driver_calls_for_requester(&mut self, requester: RegisteredAddress) {
        let mut index = 0;
        while index < self.in_flight_calls.len() {
            if self.in_flight_calls[index].requester != requester {
                index += 1;
                continue;
            }

            let call = self.in_flight_calls.remove(index);
            self.driver.cancel(call.call_id);
            self.remove_translator(call.call_id);
            self.push_event(
                call.requester.isolate,
                Some(call.cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id: call.call_id,
                    call_kind: call.call_kind,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                },
            );
        }
    }

    fn remove_translator(&mut self, call_id: CallId) {
        let translator_index = self
            .translators
            .iter()
            .position(|entry| entry.call_id == call_id)
            .unwrap_or_else(|| panic!("missing translator for call {call_id:?}"));
        self.translators.remove(translator_index);
    }

    /// Registers one isolate and returns its typed address.
    ///
    /// Isolate identifiers are assigned in registration order, starting at `1`.
    #[allow(private_bounds)]
    pub fn register<I, M, Outbound>(
        &mut self,
        isolate: I,
        mailbox: M,
    ) -> Address<I::Message, I::Reply>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: IntoRuntimeFact + 'static,
        Outbound: 'static,
        M: Mailbox<I::Message> + 'static,
    {
        let address = self.register_entry::<I, Outbound>(
            isolate,
            None,
            Box::new(MailboxAdapter::<M, I::Message> {
                mailbox,
                marker: PhantomData,
            }),
        );

        Address::new_with_generation(address.shard, address.isolate, address.generation)
    }

    /// Registers one isolate and lets the runtime allocate the mailbox.
    #[allow(private_bounds)]
    pub fn register_with_capacity<I, Outbound>(
        &mut self,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Address<I::Message, I::Reply>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        let address = self.register_entry::<I, Outbound>(
            isolate,
            None,
            Box::new(AnyMailboxAdapter {
                mailbox: self
                    .mailbox_factory
                    .create::<Box<dyn Any>>(mailbox_capacity),
            }),
        );

        Address::new_with_generation(address.shard, address.isolate, address.generation)
    }

    /// Registers one isolate as a callable service and returns capability-typed
    /// handles for the `.send` and `.call` lanes.
    ///
    /// The returned [`ServiceHandle`] exposes a [`SendAddress`](tina::SendAddress)
    /// for ordinary send/continuation traffic and a
    /// [`CallAddress`](tina::CallAddress) for callable traffic. Mixing the two
    /// becomes a compile error at the call boundary instead of a runtime
    /// `CallRejectedReason::UnsupportedMessage`.
    ///
    /// `I` must implement [`tina::CallableIsolate`]. The
    /// `#[tina::isolate]` / `#[tina_runtime::isolate]` macros emit that impl
    /// automatically when the impl block defines `fn handle_call(...)`. An
    /// isolate without `handle_call` is not callable; registering it through
    /// `register_service` is a compile error rather than a service whose every
    /// caller silently sees `UnsupportedMessage`. Use
    /// [`register_service_send_only`](Self::register_service_send_only) for the
    /// send-only shape.
    ///
    /// Negative fixture: an isolate without `handle_call` is not a callable
    /// service.
    ///
    /// ```compile_fail
    /// use std::convert::Infallible;
    /// use tina::prelude::*;
    /// use tina_runtime::{DefaultMailboxFactory, Runtime};
    ///
    /// #[derive(Debug)]
    /// enum Msg { Tick }
    ///
    /// struct NoCallHandler;
    ///
    /// #[tina_runtime::isolate(message = Msg)]
    /// impl NoCallHandler {
    ///     fn handle(
    ///         &mut self,
    ///         _msg: Msg,
    ///         _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ///     ) -> Effect<Self> {
    ///         noop()
    ///     }
    /// }
    ///
    /// let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    /// // `NoCallHandler` is not a callable service.
    /// let _ = runtime.register_service::<NoCallHandler, Infallible>(NoCallHandler, 4);
    /// ```
    ///
    /// Internally this is exactly [`register_with_capacity`](Self::register_with_capacity);
    /// the raw [`Address`] stays available through
    /// [`CallAddress::address`](tina::CallAddress::address) for low-level
    /// interop.
    #[allow(private_bounds)]
    pub fn register_service<I, Outbound>(
        &mut self,
        isolate: I,
        mailbox_capacity: usize,
    ) -> ServiceHandle<I::Message, I::Reply>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + tina::CallableIsolate + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        let address = self.register_with_capacity::<I, Outbound>(isolate, mailbox_capacity);
        ServiceHandle::from_address(address)
    }

    /// Registers one isolate as a send-only service.
    ///
    /// Returned [`SendOnlyServiceHandle`] exposes only the `.send` lane. The
    /// isolate must declare `type Reply = ()` (or wrap its reply as
    /// [`std::convert::Infallible`]) so callers cannot construct a
    /// [`tina::CallAddress`] in the first place.
    #[allow(private_bounds)]
    pub fn register_service_send_only<I, Outbound>(
        &mut self,
        isolate: I,
        mailbox_capacity: usize,
    ) -> SendOnlyServiceHandle<I::Message>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>, Reply = ()> + 'static,
        I::Message: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        let address = self.register_with_capacity::<I, Outbound>(isolate, mailbox_capacity);
        SendOnlyServiceHandle {
            send: address.send_only(),
        }
    }

    /// Registers one split event/request service.
    ///
    /// The isolate's message type must be [`tina::ServiceMessage<Event,
    /// Request>`], which the `#[tina_runtime::isolate(event = ..., request =
    /// ..., reply = ...)]` macro emits. The returned handle exposes separate
    /// event and request capabilities so ordinary code cannot send requests as
    /// events or call events as requests.
    #[allow(private_bounds)]
    pub fn register_split_service<I, Event, Request, Outbound>(
        &mut self,
        isolate: I,
        mailbox_capacity: usize,
    ) -> SplitServiceHandle<Event, Request, I::Reply>
    where
        I: Isolate<
                Shard = S,
                Message = tina::ServiceMessage<Event, Request>,
                Send = TinaOutbound<Outbound>,
            > + tina::CallableIsolate
            + 'static,
        Event: 'static,
        Request: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        let address = self.register_with_capacity::<I, Outbound>(isolate, mailbox_capacity);
        SplitServiceHandle::from_address(address)
    }

    /// Registers one isolate and prefills its mailbox with `bootstrap` so the
    /// first delivered message is always `bootstrap`.
    ///
    /// The mailbox is allocated, `bootstrap` is admitted via the mailbox's
    /// own `try_send`, and only then is the isolate entry inserted into the
    /// registry. If the prefill fails, no entry is created and no address is
    /// returned. There is no cleanup-after-registration path.
    ///
    /// Honesty:
    /// - the returned address may have a full mailbox until `bootstrap` is
    ///   delivered. Sending immediately after this call can see `Full`.
    /// - bootstrap delivery is an ordinary trace-visible mailbox event.
    /// - there is no special lifecycle callback.
    #[allow(private_bounds, clippy::type_complexity)]
    pub fn register_with_capacity_and_bootstrap<I, Outbound>(
        &mut self,
        isolate: I,
        mailbox_capacity: usize,
        bootstrap: I::Message,
    ) -> Result<Address<I::Message, I::Reply>, RegisterBootstrapError<I::Message>>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        let mailbox = self
            .mailbox_factory
            .create::<Box<dyn Any>>(mailbox_capacity);
        let adapter = AnyMailboxAdapter { mailbox };
        let boxed: Box<dyn Any> = Box::new(bootstrap);
        if let Err(err) = adapter.try_send_boxed(boxed) {
            let recover = |b: Box<dyn Any>| {
                *b.downcast::<I::Message>()
                    .expect("bootstrap message type recovered from boxed Any")
            };
            return Err(match err {
                TrySendError::Full(b) => RegisterBootstrapError::Full(recover(b)),
                TrySendError::Closed(b) => RegisterBootstrapError::Closed(recover(b)),
            });
        }
        let address = self.register_entry::<I, Outbound>(isolate, None, Box::new(adapter));
        Ok(Address::new_with_generation(
            address.shard,
            address.isolate,
            address.generation,
        ))
    }

    /// Registers one isolate; constructor receives its own typed address.
    ///
    /// Replaces the `Begin { self_addr }` / `Bind { self_addr }` bootstrap
    /// variant: the closure gets the final `Address` and returns the isolate.
    /// Generation matches plain
    /// [`register_with_capacity`](Self::register_with_capacity) (initial 0).
    ///
    /// Honesty:
    /// - No message delivers before `construct` returns. The entry
    ///   lands in the registry only after the closure produces the
    ///   isolate value.
    /// - The closure has no runtime handle, so an address can only
    ///   escape through user-captured shared state.
    /// - Constructor panic *or* mailbox-create panic leaves the
    ///   allocated id with no entry. The id is never reused. A
    ///   `try_send` to a leaked address panics like any send to an
    ///   unknown id.
    /// - `construct` runs synchronously. Heavy work blocks the
    ///   caller; on `ThreadedRuntime` it blocks the worker thread
    ///   and starves every other isolate on that shard — build the
    ///   value before calling.
    #[allow(private_bounds)]
    pub fn register_with_capacity_using<I, Outbound, Ctor>(
        &mut self,
        mailbox_capacity: usize,
        construct: Ctor,
    ) -> Address<I::Message, I::Reply>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: IntoRuntimeFact + 'static,
        Outbound: 'static,
        Ctor: FnOnce(Address<I::Message, I::Reply>) -> I,
    {
        let isolate_id = IsolateId::new(self.next_isolate_id);
        self.next_isolate_id += 1;
        let generation = AddressGeneration::new(0);
        let self_addr = Address::<I::Message, I::Reply>::new_with_generation(
            self.shard.id(),
            isolate_id,
            generation,
        );

        let mailbox = self
            .mailbox_factory
            .create::<Box<dyn Any>>(mailbox_capacity);

        // Constructor panic leaves an allocated id with no entry. id
        // allocator is monotonic so leaking one is harmless.
        let isolate = construct(self_addr);

        self.entries.push(RegisteredEntry {
            id: isolate_id,
            generation,
            parent: None,
            stopped: Cell::new(false),
            stopped_event: Cell::new(None),
            mailbox: Box::new(AnyMailboxAdapter { mailbox }),
            call_contexts: RefCell::new(VecDeque::new()),
            handler: RefCell::new(Box::new(HandlerAdapter::<I, Outbound> {
                isolate,
                marker: PhantomData,
            })),
        });

        self_addr
    }

    /// Attempts to enqueue a typed message into one registered isolate.
    ///
    /// This is the runtime-side ingress surface for tests and later drivers.
    /// It preserves the mailbox's typed `Full` and `Closed` outcomes. Unknown,
    /// stale, or stopped destinations all return `Closed`: direct ingress has
    /// no caller/cause to attach a trace event to.
    pub fn try_send<M: 'static, R>(
        &self,
        address: Address<M, R>,
        message: M,
    ) -> Result<(), TrySendError<M>> {
        if address.shard() != self.shard.id() {
            panic!(
                "cross-shard runtime ingress is out of scope in this slice: target shard {} != runtime shard {}",
                address.shard().get(),
                self.shard.id().get(),
            );
        }

        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.id == address.isolate())
        else {
            return Err(TrySendError::Closed(message));
        };

        if entry.generation != address.generation() {
            return Err(TrySendError::Closed(message));
        }

        let entry_index = self
            .entries
            .iter()
            .position(|entry| entry.id == address.isolate())
            .unwrap_or_else(|| panic!("runtime ingress found entry then lost it"));

        match self.enqueue_entry_message(entry_index, Box::new(message), None) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(message)) => Err(TrySendError::Full(
                *message.downcast::<M>().unwrap_or_else(|_| {
                    panic!("runtime ingress attempted to deliver a message to a mailbox with the wrong type")
                }),
            )),
            Err(TrySendError::Closed(message)) => Err(TrySendError::Closed(
                *message.downcast::<M>().unwrap_or_else(|_| {
                    panic!("runtime ingress attempted to deliver a message to a mailbox with the wrong type")
                }),
            )),
        }
    }

    /// Attempts to enqueue one public event through a split-service event
    /// capability.
    ///
    /// This is the host/runtime companion to [`tina::send_event`]. It keeps
    /// tests and setup code on the capability-typed path instead of unwrapping
    /// the raw `ServiceMessage<Event, Request>` address.
    pub fn try_send_event<Event: 'static, Request: 'static>(
        &self,
        address: tina::ServiceEventAddress<Event, Request>,
        event: Event,
    ) -> Result<(), TrySendError<tina::ServiceMessage<Event, Request>>> {
        self.try_send(
            address.address().address(),
            tina::ServiceMessage::Event(event),
        )
    }

    /// Configures a registered isolate as supervisor for its direct children.
    ///
    /// This is a setup-time runtime API. Unknown, stale, or cross-shard parent
    /// addresses are programmer errors and panic. Reconfiguring the same parent
    /// replaces the config and resets the runtime-lifetime budget tracker.
    pub fn supervise<M: 'static, R>(&mut self, parent: Address<M, R>, config: SupervisorConfig) {
        // Keep the panicking surface for callers who want a setup-time
        // assertion, but route the actual work through the fallible
        // `try_supervise` so the panic message stays in one place.
        if self.try_supervise(parent, config).is_err() {
            panic!(
                "supervise expected an address registered with this runtime, got an unknown or stale address",
            );
        }
    }

    /// Configures one registered isolate as supervisor without panicking on
    /// unknown parents.
    ///
    /// The panicking [`supervise`](Self::supervise) variant remains
    /// available for setup code that wants the unknown-parent case to
    /// be a hard programmer error. `try_supervise` is the fallible
    /// variant that [`ThreadedRuntime`] uses internally so an
    /// unknown-parent registration does not crash the worker thread.
    pub fn try_supervise<M: 'static, R>(
        &mut self,
        parent: Address<M, R>,
        config: SupervisorConfig,
    ) -> Result<(), SuperviseError> {
        let Some(parent) = self.try_registered_address(parent) else {
            return Err(SuperviseError::UnknownParent);
        };
        let budget_state = config.budget().tracker();

        if let Some(record) = self
            .supervisors
            .iter_mut()
            .find(|record| record.parent == parent)
        {
            record.config = config;
            record.budget_state = budget_state;
            return Ok(());
        }

        self.supervisors.push(SupervisorRecord {
            parent,
            config,
            budget_state,
        });
        Ok(())
    }

    /// Runs one deterministic round over all registered isolates.
    ///
    /// The runtime first advances its driver so any pending
    /// runtime-owned calls that finished since the previous step can be
    /// delivered as ordinary later-turn messages. Then each registered
    /// isolate gets at most one delivery chance, in registration order.
    ///
    /// The return value is the number of handlers that ran in this round.
    pub fn step(&mut self) -> usize {
        let shard_id = self.shard.id();
        self.step_with_remote(&mut |_source_shard, envelope| {
            let target_shard = envelope.target_shard();
            match envelope {
                QueuedRemoteEnvelope::Send(queued) => {
                panic!(
                    "cross-shard send is out of scope in this slice: target shard {} != runtime shard {}",
                    queued.send.target_shard.get(),
                    shard_id.get(),
                );
                }
                QueuedRemoteEnvelope::CallReply(_) => {
                panic!(
                    "cross-shard call reply is out of scope in this slice: requester shard {} != runtime shard {}",
                    target_shard.get(),
                    shard_id.get(),
                );
                }
            }
        })
    }

    fn step_with_remote<FR>(&mut self, route_remote: &mut FR) -> usize
    where
        FR: FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    {
        let now = self.clock.now();
        self.advance_driver(now);
        self.harvest_isolate_call_timeouts(now);

        let mut round_messages = std::mem::take(&mut self.round_messages);
        round_messages.clear();
        reserve_round_message_scratch(&mut round_messages, self.entries.len());
        for index in 0..self.entries.len() {
            let message = if self.entries[index].stopped.get() {
                None
            } else {
                self.recv_entry_message(index)
            };
            round_messages.push(message);
        }

        let mut delivered = 0;

        for index in 0..round_messages.len() {
            let Some(message) = round_messages[index].take() else {
                continue;
            };

            if self.entries[index].stopped.get() {
                if let Some(stopped) = self.entries[index].stopped_event.get() {
                    self.push_event(
                        self.entries[index].id,
                        Some(stopped.into()),
                        RuntimeEventKind::MessageAbandoned,
                    );
                }
                continue;
            }

            delivered += 1;

            let isolate_id = self.entries[index].id;
            let mailbox_accepted =
                self.push_event(isolate_id, None, RuntimeEventKind::MailboxAccepted);
            let handler_started = self.push_event(
                isolate_id,
                Some(mailbox_accepted.into()),
                RuntimeEventKind::HandlerStarted,
            );

            let incoming_call_context = message.call_context;
            let caller = self.build_message_caller(incoming_call_context, isolate_id);
            let now = self.clock.now();

            let effect = {
                let mut handler = self.entries[index].handler.borrow_mut();
                catch_unwind(AssertUnwindSafe(|| match caller {
                    Some(caller) => handler.handle_call_boxed(
                        message.message,
                        &mut self.shard,
                        isolate_id,
                        self.entries[index].generation,
                        caller,
                        now,
                    ),
                    None => handler.handle_boxed(
                        message.message,
                        &mut self.shard,
                        isolate_id,
                        self.entries[index].generation,
                        None,
                        now,
                    ),
                }))
            };

            let effect = match effect {
                Ok(effect) => effect,
                Err(_) => {
                    let handler_panicked = self.push_event(
                        isolate_id,
                        Some(handler_started.into()),
                        RuntimeEventKind::HandlerPanicked,
                    );
                    let captured_any = self.drop_pending_deferred_captures(handler_panicked.into());
                    if !captured_any {
                        if let Some(context) = incoming_call_context {
                            self.reject_call_context(
                                isolate_id,
                                handler_panicked.into(),
                                context,
                                CallRejectedReason::HandlerPanicked,
                                route_remote,
                            );
                        }
                    }
                    self.stop_entry(index, isolate_id, handler_panicked.into());
                    self.supervise_panic(
                        RegisteredAddress {
                            shard: self.shard.id(),
                            isolate: isolate_id,
                            generation: self.entries[index].generation,
                        },
                        handler_panicked.into(),
                        &mut round_messages,
                    );
                    continue;
                }
            };

            let effect_kind = effect.kind();
            let handler_finished = self.push_event(
                isolate_id,
                Some(handler_started.into()),
                RuntimeEventKind::HandlerFinished {
                    effect: effect_kind,
                },
            );

            // If the handler captured the caller, promote the new slot
            // record(s) and zero out the message's call_context so a
            // stray Effect::Reply does not also fire on the captured
            // call.
            let captured_any = self.promote_captures(isolate_id, handler_finished.into());

            let consumed_by_effect = effect.consumes_call_context();
            let abandoned_context = if !captured_any && !consumed_by_effect {
                message.call_context
            } else {
                None
            };
            if let Some(context) = abandoned_context {
                self.reject_call_context(
                    isolate_id,
                    handler_finished.into(),
                    context,
                    CallRejectedReason::ReplyAbandoned,
                    route_remote,
                );
            }

            let effective_context = if captured_any || abandoned_context.is_some() {
                None
            } else {
                message.call_context
            };

            self.execute_effect(
                index,
                handler_finished.into(),
                effect,
                effective_context,
                &mut round_messages,
                route_remote,
            );
        }

        round_messages.clear();
        self.round_messages = round_messages;

        self.sweep_dropped_deferred_slots();
        self.gc_stopped_entries();

        delivered
    }

    fn build_message_caller(
        &self,
        call_context: Option<MessageCallContext>,
        isolate_id: IsolateId,
    ) -> Option<MessageCaller> {
        let ctx = call_context?;
        let (call_id, routing, remote_expected_reply_type_id) = match ctx {
            MessageCallContext::Local { call_id } => (call_id, CallRouting::Local, None),
            MessageCallContext::Remote {
                call_id,
                requester,
                cause,
                expected_reply_type_id,
            } => (
                call_id,
                CallRouting::Remote {
                    requester_shard: requester.shard,
                    requester_isolate: requester.isolate,
                    requester_generation: requester.generation,
                    cause: cause.event().get(),
                },
                Some(expected_reply_type_id),
            ),
        };
        let expected_reply_type_id = match routing {
            CallRouting::Local => self
                .pending_isolate_calls
                .iter()
                .find(|p| p.call_id == call_id)
                .map(|p| p.expected_reply_type_id)
                .unwrap_or_else(std::any::TypeId::of::<()>),
            CallRouting::Remote { .. } => remote_expected_reply_type_id
                .expect("remote call context carries the expected reply type"),
        };
        Some(MessageCaller::new(
            Rc::clone(&self.deferred_registry),
            call_id.get(),
            isolate_id,
            routing,
            expected_reply_type_id,
        ))
    }

    fn promote_captures(&mut self, isolate_id: IsolateId, cause: CauseId) -> bool {
        let captures = self.deferred_registry.drain_pending();
        if captures.is_empty() {
            return false;
        }
        for capture in captures {
            let slot_id = DeferredSlotId::new(capture.slot_id);
            let call_id = CallId::new(capture.call_id);
            let routing = match capture.routing {
                CallRouting::Local => deferred::DeferredRouting::Local,
                CallRouting::Remote {
                    requester_shard,
                    requester_isolate,
                    requester_generation,
                    cause: remote_cause,
                } => deferred::DeferredRouting::Remote {
                    requester: RegisteredAddress {
                        shard: requester_shard,
                        isolate: requester_isolate,
                        generation: requester_generation,
                    },
                    cause: CauseId::new(EventId::new(remote_cause)),
                },
            };
            self.push_event(
                isolate_id,
                Some(cause),
                RuntimeEventKind::DeferredReplyCaptured { slot_id, call_id },
            );
            self.promoted_slots.push(deferred::DeferredSlotRecord {
                slot_id,
                call_id,
                capturing_isolate: capture.capturing_isolate,
                shared: capture.shared,
                routing,
            });
        }
        true
    }

    fn sweep_dropped_deferred_slots(&mut self) {
        // Single pass: independent Rcs cannot cascade.
        let dropped = self.promoted_slots.sweep_dropped();
        for record in dropped {
            self.drop_promoted_deferred_slot(record, None);
        }
    }

    fn drop_pending_deferred_captures(&mut self, cause: CauseId) -> bool {
        let captures = self.deferred_registry.drain_pending();
        let captured_any = !captures.is_empty();
        for capture in captures {
            capture.shared.set_state(DeferredSlotState::Closed);
            let slot_id = DeferredSlotId::new(capture.slot_id);
            let call_id = CallId::new(capture.call_id);
            let captured = self.push_event(
                capture.capturing_isolate,
                Some(cause),
                RuntimeEventKind::DeferredReplyCaptured { slot_id, call_id },
            );
            let dropped = self.push_event(
                capture.capturing_isolate,
                Some(captured.into()),
                RuntimeEventKind::DeferredReplyDropped { slot_id, call_id },
            );
            self.complete_isolate_call(call_id, dropped.into(), CallOutcome::Closed);
        }
        captured_any
    }

    fn drop_promoted_deferred_slot(
        &mut self,
        record: deferred::DeferredSlotRecord,
        cause: Option<CauseId>,
    ) {
        if record.shared.state() != DeferredSlotState::Open {
            return;
        }
        record.shared.set_state(DeferredSlotState::Closed);
        let dropped = self.push_event(
            record.capturing_isolate,
            cause,
            RuntimeEventKind::DeferredReplyDropped {
                slot_id: record.slot_id,
                call_id: record.call_id,
            },
        );
        self.complete_isolate_call(record.call_id, dropped.into(), CallOutcome::Closed);
    }

    fn execute_effect(
        &mut self,
        index: usize,
        cause: CauseId,
        effect: ErasedEffect<S, F>,
        call_context: Option<MessageCallContext>,
        round_messages: &mut [Option<DeliveredMessage>],
        route_remote: &mut impl FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    ) -> bool {
        let isolate_id = self.entries[index].id;
        match effect {
            ErasedEffect::Stop => {
                self.stop_entry(index, isolate_id, cause);
                true
            }
            ErasedEffect::StopWith(result) => {
                self.stop_entry_with_result(index, isolate_id, cause, result);
                true
            }
            ErasedEffect::Send(send) => {
                let target_shard = send.target_shard;
                let target_isolate = send.target_isolate;
                let target_generation = send.target_generation;
                let attempted = self.push_event(
                    isolate_id,
                    Some(cause),
                    RuntimeEventKind::SendDispatchAttempted {
                        target_shard,
                        target_isolate,
                        target_generation,
                    },
                );

                let delivery = if target_shard == self.shard.id() {
                    self.dispatch_local_send(send)
                } else {
                    route_remote(
                        self.shard.id(),
                        QueuedRemoteEnvelope::Send(QueuedRemoteSend {
                            send,
                            call_context: None,
                            cause: attempted.into(),
                        }),
                    )
                };

                match delivery {
                    Ok(()) => {
                        self.push_event(
                            isolate_id,
                            Some(attempted.into()),
                            RuntimeEventKind::SendAccepted {
                                target_shard,
                                target_isolate,
                                target_generation,
                            },
                        );
                    }
                    Err(reason) => {
                        self.push_event(
                            isolate_id,
                            Some(attempted.into()),
                            RuntimeEventKind::SendRejected {
                                target_shard,
                                target_isolate,
                                target_generation,
                                reason,
                            },
                        );
                    }
                }
                false
            }
            ErasedEffect::Spawn(spawn) => {
                let mut outcome = spawn.spawn(self, isolate_id);
                let child_isolate = outcome.child.isolate;
                let child = outcome.child;
                let bootstrap_message = outcome.bootstrap_message.take();
                self.record_child(isolate_id, outcome);
                let spawned = self.push_event(
                    isolate_id,
                    Some(cause),
                    RuntimeEventKind::Spawned { child_isolate },
                );
                if let Some(message) = bootstrap_message {
                    self.enqueue_bootstrap_message(child, message, spawned.into());
                }
                false
            }
            ErasedEffect::SpawnObserved(spawn) => {
                let mut outcome = spawn.spawn_observed(self, isolate_id);
                let continuation = outcome.continuation.take();
                let continuation_cause = if let Some(mut spawn_outcome) = outcome.spawn.take() {
                    let child_isolate = spawn_outcome.child.isolate;
                    let child = spawn_outcome.child;
                    let bootstrap_message = spawn_outcome.bootstrap_message.take();
                    self.record_child(isolate_id, spawn_outcome);
                    let spawned = self.push_event(
                        isolate_id,
                        Some(cause),
                        RuntimeEventKind::Spawned { child_isolate },
                    );
                    if let Some(message) = bootstrap_message {
                        self.enqueue_bootstrap_message(child, message, spawned.into());
                    }
                    spawned.into()
                } else {
                    cause
                };
                if let Some(message) = continuation {
                    let send = ErasedSend {
                        target_shard: self.shard.id(),
                        target_isolate: isolate_id,
                        target_generation: self.entries[index].generation,
                        message,
                    };
                    let attempted = self.push_event(
                        isolate_id,
                        Some(continuation_cause),
                        RuntimeEventKind::SendDispatchAttempted {
                            target_shard: send.target_shard,
                            target_isolate: send.target_isolate,
                            target_generation: send.target_generation,
                        },
                    );
                    match self.dispatch_local_send(send) {
                        Ok(()) => {
                            self.push_event(
                                isolate_id,
                                Some(attempted.into()),
                                RuntimeEventKind::SendAccepted {
                                    target_shard: self.shard.id(),
                                    target_isolate: isolate_id,
                                    target_generation: self.entries[index].generation,
                                },
                            );
                        }
                        Err(reason) => {
                            self.push_event(
                                isolate_id,
                                Some(attempted.into()),
                                RuntimeEventKind::SendRejected {
                                    target_shard: self.shard.id(),
                                    target_isolate: isolate_id,
                                    target_generation: self.entries[index].generation,
                                    reason,
                                },
                            );
                        }
                    }
                }
                false
            }
            ErasedEffect::RestartChildren => {
                self.restart_children(isolate_id, cause, round_messages);
                false
            }
            ErasedEffect::Call(call) => {
                let requester = RegisteredAddress {
                    shard: self.shard.id(),
                    isolate: isolate_id,
                    generation: self.entries[index].generation,
                };
                self.dispatch_call(call, requester, cause, call_context, route_remote);
                false
            }
            ErasedEffect::Noop => {
                self.push_event(
                    isolate_id,
                    Some(cause),
                    RuntimeEventKind::EffectObserved {
                        effect: EffectKind::Noop,
                    },
                );
                false
            }
            ErasedEffect::Reply(reply) => {
                if let Some(context) = call_context {
                    match context {
                        MessageCallContext::Local { call_id } => {
                            if !self.complete_isolate_call(
                                call_id,
                                cause,
                                CallOutcome::Replied(reply.into_any()),
                            ) {
                                let reason = match self.recently_cancelled_cause(call_id) {
                                    Some(c) => call_reply_reason_for_cause(c),
                                    None => CallReplyRejectedReason::NoPendingCall,
                                };
                                self.push_event(
                                    isolate_id,
                                    Some(cause),
                                    RuntimeEventKind::CallReplyRejected { call_id, reason },
                                );
                            }
                        }
                        MessageCallContext::Remote {
                            call_id,
                            requester,
                            cause: request_cause,
                            ..
                        } => {
                            let reply = RemoteCallReply {
                                call_id,
                                requester,
                                cause: request_cause,
                                outcome: RemoteCallOutcome::Replied(reply),
                            };
                            if let Err(reason) = route_remote(
                                self.shard.id(),
                                QueuedRemoteEnvelope::CallReply(reply),
                            ) {
                                let reason = match reason {
                                    SendRejectedReason::Full => {
                                        CallReplyRejectedReason::ReplyPathFull
                                    }
                                    SendRejectedReason::Closed => {
                                        CallReplyRejectedReason::RequesterShardClosed
                                    }
                                };
                                self.push_event(
                                    isolate_id,
                                    Some(cause),
                                    RuntimeEventKind::CallReplyRejected { call_id, reason },
                                );
                            }
                        }
                    }
                } else {
                    self.push_event(
                        isolate_id,
                        Some(cause),
                        RuntimeEventKind::EffectObserved {
                            effect: EffectKind::Reply,
                        },
                    );
                }
                false
            }
            ErasedEffect::Reject(reason) => {
                if let Some(context) = call_context {
                    self.reject_call_context(isolate_id, cause, context, reason, route_remote);
                } else {
                    self.push_event(
                        isolate_id,
                        Some(cause),
                        RuntimeEventKind::EffectObserved {
                            effect: EffectKind::Reject,
                        },
                    );
                }
                false
            }
            ErasedEffect::Batch(effects) => {
                let mut batch_context = call_context;
                for subeffect in effects {
                    let consumes_context = subeffect.consumes_call_context();
                    if self.execute_effect(
                        index,
                        cause,
                        subeffect,
                        batch_context,
                        round_messages,
                        route_remote,
                    ) {
                        return true;
                    }
                    if consumes_context {
                        batch_context = None;
                    }
                }
                false
            }
            ErasedEffect::ReplyTo { handle, message } => {
                self.execute_reply_to(isolate_id, cause, handle, message, route_remote);
                false
            }
            ErasedEffect::Fact(fact) => {
                self.push_event(
                    isolate_id,
                    Some(cause),
                    RuntimeEventKind::FactObserved { fact },
                );
                false
            }
        }
    }

    fn reject_call_context(
        &mut self,
        isolate_id: IsolateId,
        cause: CauseId,
        context: MessageCallContext,
        reason: CallRejectedReason,
        route_remote: &mut impl FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    ) {
        match context {
            MessageCallContext::Local { call_id } => {
                self.push_call_rejected_event(isolate_id, cause, call_id, reason);
                if !self.complete_isolate_call(call_id, cause, CallOutcome::Rejected(reason)) {
                    let reason = match self.recently_cancelled_cause(call_id) {
                        Some(c) => call_reply_reason_for_cause(c),
                        None => CallReplyRejectedReason::NoPendingCall,
                    };
                    self.push_event(
                        isolate_id,
                        Some(cause),
                        RuntimeEventKind::CallReplyRejected { call_id, reason },
                    );
                }
            }
            MessageCallContext::Remote {
                call_id,
                requester,
                cause: request_cause,
                ..
            } => {
                self.push_call_rejected_event(isolate_id, cause, call_id, reason);
                let reply = RemoteCallReply {
                    call_id,
                    requester,
                    cause: request_cause,
                    outcome: RemoteCallOutcome::Rejected(reason),
                };
                if let Err(rejected) =
                    route_remote(self.shard.id(), QueuedRemoteEnvelope::CallReply(reply))
                {
                    let reason = match rejected {
                        SendRejectedReason::Full => CallReplyRejectedReason::ReplyPathFull,
                        SendRejectedReason::Closed => CallReplyRejectedReason::RequesterShardClosed,
                    };
                    self.push_event(
                        isolate_id,
                        Some(cause),
                        RuntimeEventKind::CallReplyRejected { call_id, reason },
                    );
                }
            }
        }
    }

    fn push_call_rejected_event(
        &mut self,
        isolate_id: IsolateId,
        cause: CauseId,
        call_id: CallId,
        reason: CallRejectedReason,
    ) {
        let kind = match reason {
            CallRejectedReason::ReplyAbandoned => RuntimeEventKind::CallReplyAbandoned { call_id },
            CallRejectedReason::HandlerPanicked | CallRejectedReason::UnsupportedMessage => {
                RuntimeEventKind::CallRejected { call_id, reason }
            }
        };
        self.push_event(isolate_id, Some(cause), kind);
    }

    fn execute_reply_to(
        &mut self,
        isolate_id: IsolateId,
        cause: CauseId,
        handle: DeferredReplyHandle,
        message: ErasedMessage,
        route_remote: &mut impl FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    ) {
        // Locate by handle identity. If the slot is not in the registry,
        // the caller already closed (and we already emitted the
        // terminal Rejected event). User reply is silently consumed.
        let shared = tina::runtime_internal::handle_shared(&handle).clone();
        let Some(record) = self.promoted_slots.take_by_handle(&shared) else {
            // Already terminal: caller closed and we already emitted
            // Rejected{CallerClosed}. The payload is consumed without
            // re-emitting a terminal fact.
            return;
        };

        // Typecheck the payload before invoking the original caller's
        // translator. The translator's downcast would panic on a wrong
        // type; this surfaces as a typed Rejected event instead.
        let payload_type_id = message.payload_type_id();
        if payload_type_id != record.shared.expected_reply_type_id() {
            record.shared.set_state(DeferredSlotState::Closed);
            self.push_event(
                isolate_id,
                Some(cause),
                RuntimeEventKind::DeferredReplyRejected {
                    slot_id: record.slot_id,
                    call_id: record.call_id,
                    reason: DeferredReplyRejectedReason::TypeMismatch,
                },
            );
            return;
        }

        // Slot is live. Reply through it.
        match record.routing {
            deferred::DeferredRouting::Local => {
                if self.complete_isolate_call(
                    record.call_id,
                    cause,
                    CallOutcome::Replied(message.into_any()),
                ) {
                    record.shared.set_state(DeferredSlotState::Replied);
                    self.push_event(
                        isolate_id,
                        Some(cause),
                        RuntimeEventKind::DeferredReplySent {
                            slot_id: record.slot_id,
                            call_id: record.call_id,
                        },
                    );
                } else {
                    // Pending call gone — name the cause from the
                    // recently-cancelled ring when we have it; fall
                    // through to the generic `CallerClosed` only if
                    // the entry has been evicted.
                    let reason = match self.recently_cancelled_cause(record.call_id) {
                        Some(c) => deferred_reply_reason_for_cause(c),
                        None => DeferredReplyRejectedReason::CallerClosed,
                    };
                    record.shared.set_state(DeferredSlotState::Closed);
                    self.push_event(
                        isolate_id,
                        Some(cause),
                        RuntimeEventKind::DeferredReplyRejected {
                            slot_id: record.slot_id,
                            call_id: record.call_id,
                            reason,
                        },
                    );
                }
            }
            deferred::DeferredRouting::Remote {
                requester,
                cause: request_cause,
            } => {
                let envelope = QueuedRemoteEnvelope::CallReply(RemoteCallReply {
                    call_id: record.call_id,
                    requester,
                    cause: request_cause,
                    outcome: RemoteCallOutcome::Replied(message),
                });
                match route_remote(self.shard.id(), envelope) {
                    Ok(()) => {
                        record.shared.set_state(DeferredSlotState::Replied);
                        self.push_event(
                            isolate_id,
                            Some(cause),
                            RuntimeEventKind::DeferredReplySent {
                                slot_id: record.slot_id,
                                call_id: record.call_id,
                            },
                        );
                    }
                    Err(reason) => {
                        let reject_reason = match reason {
                            SendRejectedReason::Full => DeferredReplyRejectedReason::ReplyPathFull,
                            SendRejectedReason::Closed => {
                                DeferredReplyRejectedReason::RequesterShardClosed
                            }
                        };
                        record.shared.set_state(DeferredSlotState::Closed);
                        self.push_event(
                            isolate_id,
                            Some(cause),
                            RuntimeEventKind::DeferredReplyRejected {
                                slot_id: record.slot_id,
                                call_id: record.call_id,
                                reason: reject_reason,
                            },
                        );
                    }
                }
            }
        }
    }

    fn dispatch_call(
        &mut self,
        call: ErasedCall,
        requester: RegisteredAddress,
        cause: CauseId,
        continuation_context: Option<MessageCallContext>,
        route_remote: &mut impl FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    ) {
        let call_id = self.ids.next_call_id();
        let call_kind = match &call.kind {
            call::ErasedCallKind::Backend { request, .. } => request.kind(),
            call::ErasedCallKind::ObservedSend { .. } => CallKind::ObservedSend,
            call::ErasedCallKind::IsolateCall { .. } => CallKind::IsolateCall,
            call::ErasedCallKind::CancelCall { .. } => CallKind::CancelCall,
        };

        let attempted = self.push_event(
            requester.isolate,
            Some(cause),
            RuntimeEventKind::CallDispatchAttempted { call_id, call_kind },
        );
        let dispatch_context = CallDispatchContext {
            call_id,
            requester,
            cause: attempted.into(),
            continuation_context,
        };

        match call.kind {
            call::ErasedCallKind::Backend {
                request,
                translator,
            } => {
                self.dispatch_driver_call(dispatch_context, call_kind, request, translator);
            }
            call::ErasedCallKind::ObservedSend { send, translator } => {
                self.dispatch_observed_send(dispatch_context, send, translator, route_remote);
            }
            call::ErasedCallKind::IsolateCall {
                send,
                timeout,
                translator,
                expected_reply_type_id,
                handle_shared,
            } => {
                self.dispatch_isolate_call(
                    dispatch_context,
                    send,
                    timeout,
                    translator,
                    expected_reply_type_id,
                    handle_shared,
                    route_remote,
                );
            }
            call::ErasedCallKind::CancelCall {
                handle_shared,
                translator,
            } => {
                self.dispatch_cancel_call(dispatch_context, handle_shared, translator);
            }
        }
    }

    fn dispatch_driver_call(
        &mut self,
        context: CallDispatchContext,
        call_kind: CallKind,
        request: CallInput,
        translator: Box<dyn FnOnce(CallOutput) -> Box<dyn Any>>,
    ) {
        let persistence = request.persistence_trace_info();
        if persistence == Some(call::PersistenceTraceInfo::Recovery) {
            self.push_event(
                context.requester.isolate,
                Some(context.cause),
                RuntimeEventKind::RecoveryStarted,
            );
        }
        // Register the translator and in-flight tracking before submission
        // so a synchronous completion (bind / close on Betelgeuse) can be
        // delivered through the same path as async completions.
        self.in_flight_calls.push(InFlightCall {
            call_id: context.call_id,
            call_kind,
            requester: context.requester,
            cause: context.cause,
            persistence,
            continuation_context: context.continuation_context,
        });
        self.translators.push(StoredTranslator {
            call_id: context.call_id,
            translator: Some(translator),
        });

        if let Some(immediate) = self
            .driver
            .submit(context.call_id, request, self.clock.now())
        {
            self.deliver_completion(immediate.call_id, immediate.result);
        }

        // Driver cancelled some pending calls because their resource
        // closed. Drop matching runtime state, or `has_in_flight_calls`
        // stays true forever.
        for cancelled in self.driver.take_cancelled_by_close() {
            self.cancel_in_flight_call_for_resource_close(cancelled);
        }
    }

    /// Drops runtime state for a call cancelled by resource close.
    /// Translator is not run; caller's continuation does not fire.
    /// Trace records `ResourceClosed`.
    fn cancel_in_flight_call_for_resource_close(&mut self, call_id: CallId) {
        let Some(in_flight_index) = self
            .in_flight_calls
            .iter()
            .position(|entry| entry.call_id == call_id)
        else {
            return;
        };
        let in_flight = self.in_flight_calls.remove(in_flight_index);

        if let Some(translator_index) = self
            .translators
            .iter()
            .position(|entry| entry.call_id == call_id)
        {
            self.translators.remove(translator_index);
        }

        self.push_event(
            in_flight.requester.isolate,
            Some(in_flight.cause),
            RuntimeEventKind::CallCompletionRejected {
                call_id,
                call_kind: in_flight.call_kind,
                reason: CallCompletionRejectedReason::ResourceClosed,
            },
        );
    }

    fn dispatch_observed_send(
        &mut self,
        context: CallDispatchContext,
        send: ErasedSend,
        translator: Box<dyn FnOnce(SendOutcome) -> Box<dyn Any>>,
        route_remote: &mut impl FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    ) {
        let target_shard = send.target_shard;
        let target_isolate = send.target_isolate;
        let target_generation = send.target_generation;
        let send_attempted = self.push_event(
            context.requester.isolate,
            Some(context.cause),
            RuntimeEventKind::SendDispatchAttempted {
                target_shard,
                target_isolate,
                target_generation,
            },
        );

        let delivery = if target_shard == self.shard.id() {
            self.dispatch_local_send(send)
        } else {
            route_remote(
                self.shard.id(),
                QueuedRemoteEnvelope::Send(QueuedRemoteSend {
                    send,
                    call_context: None,
                    cause: send_attempted.into(),
                }),
            )
        };

        let outcome = match delivery {
            Ok(()) => {
                self.push_event(
                    context.requester.isolate,
                    Some(send_attempted.into()),
                    RuntimeEventKind::SendAccepted {
                        target_shard,
                        target_isolate,
                        target_generation,
                    },
                );
                SendOutcome::Accepted
            }
            Err(reason) => {
                self.push_event(
                    context.requester.isolate,
                    Some(send_attempted.into()),
                    RuntimeEventKind::SendRejected {
                        target_shard,
                        target_isolate,
                        target_generation,
                        reason,
                    },
                );
                SendOutcome::from_rejected(reason)
            }
        };

        self.deliver_observed_send_outcome(
            context.call_id,
            context.requester,
            context.cause,
            outcome,
            translator,
            context.continuation_context,
        );
    }

    fn deliver_observed_send_outcome(
        &mut self,
        call_id: CallId,
        requester: RegisteredAddress,
        cause: CauseId,
        outcome: SendOutcome,
        translator: Box<dyn FnOnce(SendOutcome) -> Box<dyn Any>>,
        continuation_context: Option<MessageCallContext>,
    ) {
        let call_kind = CallKind::ObservedSend;
        let message = translator(outcome);

        let entry_index = self.entries.iter().position(|entry| {
            entry.id == requester.isolate && entry.generation == requester.generation
        });
        let Some(entry_index) = entry_index else {
            self.push_event(
                requester.isolate,
                Some(cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id,
                    call_kind,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                },
            );
            return;
        };

        if self.entries[entry_index].stopped.get() {
            self.push_event(
                requester.isolate,
                Some(cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id,
                    call_kind,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                },
            );
            return;
        }

        match self.enqueue_entry_message(entry_index, message, continuation_context) {
            Ok(()) => {
                self.push_event(
                    requester.isolate,
                    Some(cause),
                    RuntimeEventKind::CallCompleted { call_id, call_kind },
                );
            }
            Err(TrySendError::Full(_)) => {
                self.push_event(
                    requester.isolate,
                    Some(cause),
                    RuntimeEventKind::CallCompletionRejected {
                        call_id,
                        call_kind,
                        reason: CallCompletionRejectedReason::MailboxFull,
                    },
                );
            }
            Err(TrySendError::Closed(_)) => {
                self.push_event(
                    requester.isolate,
                    Some(cause),
                    RuntimeEventKind::CallCompletionRejected {
                        call_id,
                        call_kind,
                        reason: CallCompletionRejectedReason::RequesterClosed,
                    },
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn dispatch_isolate_call(
        &mut self,
        context: CallDispatchContext,
        send: ErasedSend,
        timeout: Duration,
        translator: ErasedIsolateCallTranslator,
        expected_reply_type_id: std::any::TypeId,
        handle_shared: Option<std::sync::Arc<tina::CallHandleShared>>,
        route_remote: &mut impl FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    ) {
        let target_shard = send.target_shard;
        let target_isolate = send.target_isolate;
        let target_generation = send.target_generation;
        let send_attempted = self.push_event(
            context.requester.isolate,
            Some(context.cause),
            RuntimeEventKind::SendDispatchAttempted {
                target_shard,
                target_isolate,
                target_generation,
            },
        );

        let call_context = if target_shard == self.shard.id() {
            MessageCallContext::Local {
                call_id: context.call_id,
            }
        } else {
            MessageCallContext::Remote {
                call_id: context.call_id,
                requester: context.requester,
                cause: context.cause,
                expected_reply_type_id,
            }
        };

        let delivery = if target_shard == self.shard.id() {
            self.dispatch_local_send_with_context(send, Some(call_context))
        } else {
            route_remote(
                self.shard.id(),
                QueuedRemoteEnvelope::Send(QueuedRemoteSend {
                    send,
                    call_context: Some(call_context),
                    cause: send_attempted.into(),
                }),
            )
        };

        match delivery {
            Ok(()) => {
                self.push_event(
                    context.requester.isolate,
                    Some(send_attempted.into()),
                    RuntimeEventKind::SendAccepted {
                        target_shard,
                        target_isolate,
                        target_generation,
                    },
                );
                let insertion_order = self.next_isolate_call_ordinal;
                self.next_isolate_call_ordinal += 1;
                if let Some(shared) = &handle_shared {
                    shared.set_call_id(context.call_id.get());
                    shared.set_shard_id(self.shard.id().get());
                }
                self.pending_isolate_calls.push(PendingIsolateCall {
                    call_id: context.call_id,
                    requester: context.requester,
                    cause: context.cause,
                    deadline: tina::Deadline::from_instant(self.clock.now(), timeout).instant(),
                    insertion_order,
                    continuation_context: context.continuation_context,
                    translator: Some(translator),
                    expected_reply_type_id,
                    handle_shared,
                });
            }
            Err(reason) => {
                self.push_event(
                    context.requester.isolate,
                    Some(send_attempted.into()),
                    RuntimeEventKind::SendRejected {
                        target_shard,
                        target_isolate,
                        target_generation,
                        reason,
                    },
                );
                let outcome = match reason {
                    SendRejectedReason::Full => CallOutcome::Full,
                    SendRejectedReason::Closed => CallOutcome::Closed,
                };
                if let Some(shared) = &handle_shared {
                    shared.set_call_id(context.call_id.get());
                    shared.set_shard_id(self.shard.id().get());
                    shared.set_state(tina::CallHandleState::Settled);
                }
                self.deliver_isolate_call_outcome(
                    context.call_id,
                    context.requester,
                    context.cause,
                    outcome,
                    translator,
                    context.continuation_context,
                );
            }
        }
    }

    fn dispatch_cancel_call(
        &mut self,
        context: CallDispatchContext,
        handle_shared: std::sync::Arc<tina::CallHandleShared>,
        translator: Box<dyn FnOnce(tina::CancelOutcome) -> Box<dyn Any>>,
    ) {
        // Cancel is single-writer on this shard. We still verify the
        // handle was minted on this shard; cross-shard handles would
        // miss the pending-table lookup and silently fall through to
        // `AlreadyCompleted` without us.
        let outcome = match handle_shared.state() {
            tina::CallHandleState::Settled => tina::CancelOutcome::AlreadyCompleted,
            tina::CallHandleState::Cancelled => tina::CancelOutcome::AlreadyCancelled,
            tina::CallHandleState::Pending => {
                match handle_shared.call_id() {
                    None => tina::CancelOutcome::NotAdmitted,
                    Some(raw_call_id) => {
                        let stamped_shard = handle_shared.shard_id().expect(
                            "shard_id is stamped together with call_id — call_id was Some but \
                             shard_id was None, which violates set_call_id / set_shard_id pairing.",
                        );
                        if stamped_shard != self.shard.id().get() {
                            // Cross-shard cancel: the pending-call entry lives
                            // on the originating shard. Reject with a typed
                            // outcome instead of silently no-op'ing into
                            // `AlreadyCompleted`.
                            tina::CancelOutcome::WrongShard
                        } else {
                            let call_id = CallId::new(raw_call_id);
                            match self
                                .pending_isolate_calls
                                .iter()
                                .position(|entry| entry.call_id == call_id)
                            {
                                Some(index) => {
                                    let mut entry = self.pending_isolate_calls.remove(index);
                                    // CallCancelled's trace cause chains back
                                    // to the original CallDispatchAttempted so
                                    // every CallDispatchAttempted has exactly
                                    // one settlement event downstream of it.
                                    let original_cause = entry.cause;
                                    let _ = entry.translator.take();
                                    handle_shared.set_state(tina::CallHandleState::Cancelled);
                                    self.record_cancelled_call(
                                        call_id,
                                        tina::CancelCause::CallerCancelled,
                                    );
                                    self.close_deferred_slot_for_call_with_reason(
                                        call_id,
                                        trace::DeferredReplyRejectedReason::CallerCancelled,
                                    );
                                    self.push_event(
                                        context.requester.isolate,
                                        Some(original_cause),
                                        RuntimeEventKind::CallCancelled {
                                            call_id,
                                            cause: tina::CancelCause::CallerCancelled,
                                        },
                                    );
                                    tina::CancelOutcome::Cancelled
                                }
                                None => tina::CancelOutcome::AlreadyCompleted,
                            }
                        }
                    }
                }
            }
        };

        let message_any = translator(outcome);
        let Some(entry_index) = self.entries.iter().position(|entry| {
            entry.id == context.requester.isolate
                && entry.generation == context.requester.generation
        }) else {
            self.push_event(
                context.requester.isolate,
                Some(context.cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id: context.call_id,
                    call_kind: trace::CallKind::CancelCall,
                    reason: trace::CallCompletionRejectedReason::RequesterClosed,
                },
            );
            return;
        };
        if self.entries[entry_index].stopped.get() {
            self.push_event(
                context.requester.isolate,
                Some(context.cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id: context.call_id,
                    call_kind: trace::CallKind::CancelCall,
                    reason: trace::CallCompletionRejectedReason::RequesterClosed,
                },
            );
            return;
        }
        match self.enqueue_entry_message(entry_index, message_any, context.continuation_context) {
            Ok(()) => {
                self.push_event(
                    context.requester.isolate,
                    Some(context.cause),
                    RuntimeEventKind::CallCompleted {
                        call_id: context.call_id,
                        call_kind: trace::CallKind::CancelCall,
                    },
                );
            }
            Err(TrySendError::Full(_)) => {
                self.push_event(
                    context.requester.isolate,
                    Some(context.cause),
                    RuntimeEventKind::CallCompletionRejected {
                        call_id: context.call_id,
                        call_kind: trace::CallKind::CancelCall,
                        reason: trace::CallCompletionRejectedReason::MailboxFull,
                    },
                );
            }
            Err(TrySendError::Closed(_)) => {
                self.push_event(
                    context.requester.isolate,
                    Some(context.cause),
                    RuntimeEventKind::CallCompletionRejected {
                        call_id: context.call_id,
                        call_kind: trace::CallKind::CancelCall,
                        reason: trace::CallCompletionRejectedReason::RequesterClosed,
                    },
                );
            }
        }
    }

    fn harvest_isolate_call_timeouts(&mut self, now: Instant) {
        while let Some(index) = self
            .pending_isolate_calls
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.deadline <= now)
            .min_by(|(_, left), (_, right)| {
                left.deadline
                    .cmp(&right.deadline)
                    .then_with(|| left.insertion_order.cmp(&right.insertion_order))
            })
            .map(|(index, _)| index)
        {
            let mut entry = self.pending_isolate_calls.remove(index);
            let translator = entry.translator.take().unwrap_or_else(|| {
                panic!("translator for call {:?} already consumed", entry.call_id)
            });
            // Timeout shares cancel's cleanup path; cause stays
            // distinct in the trace via `CallFailed { Timeout }` vs
            // `CallCancelled { CallerCancelled }`. Late callee replies
            // for this call_id surface as `CallerTimedOut` rejection
            // reasons (recorded in the ring with that cause) instead
            // of the generic `NoPendingCall` / `CallerClosed`.
            if let Some(shared) = &entry.handle_shared {
                shared.set_state(tina::CallHandleState::Settled);
            }
            self.record_cancelled_call(entry.call_id, tina::CancelCause::CallerTimedOut);
            self.close_deferred_slot_for_call_with_reason(
                entry.call_id,
                trace::DeferredReplyRejectedReason::CallerTimedOut,
            );
            self.deliver_isolate_call_outcome(
                entry.call_id,
                entry.requester,
                entry.cause,
                CallOutcome::Timeout,
                translator,
                entry.continuation_context,
            );
        }
    }

    /// Cancels every pending isolate call whose caller is the stopping
    /// isolate. Emits `CallCancelled { OwnerStopped }`, marks each
    /// shared cell `Cancelled`, and closes any captured deferred slot
    /// with `CallerCancelled`. The callee may still finish; its later
    /// reply hits the same rejection path as explicit cancel.
    fn cancel_pending_isolate_calls_for_owner(
        &mut self,
        owner_isolate: IsolateId,
        owner_generation: AddressGeneration,
        _stopped_cause: CauseId,
    ) {
        // Single-pass partition: O(n) over pending calls instead of
        // O(n²) from repeated `Vec::remove`. Important for routers
        // with many in-flight calls.
        let original = std::mem::take(&mut self.pending_isolate_calls);
        let (mut owned, kept): (Vec<_>, Vec<_>) = original.into_iter().partition(|entry| {
            entry.requester.isolate == owner_isolate
                && entry.requester.generation == owner_generation
        });
        self.pending_isolate_calls = kept;
        for entry in owned.iter_mut() {
            self.record_cancelled_call(entry.call_id, tina::CancelCause::OwnerStopped);
        }
        for mut entry in owned {
            let original_cause = entry.cause;
            let _ = entry.translator.take();
            if let Some(shared) = &entry.handle_shared {
                shared.set_state(tina::CallHandleState::Cancelled);
            }
            // Owner stopped: distinct from `CallerCancelled` in the
            // late-reply rejection reason as well as the settlement
            // event.
            self.close_deferred_slot_for_call_with_reason(
                entry.call_id,
                trace::DeferredReplyRejectedReason::OwnerStopped,
            );
            self.push_event(
                owner_isolate,
                Some(original_cause),
                RuntimeEventKind::CallCancelled {
                    call_id: entry.call_id,
                    cause: tina::CancelCause::OwnerStopped,
                },
            );
        }
    }

    /// Records `call_id` in the bounded recently-cancelled ring with
    /// the cause that closed it. Late replies for these surface as a
    /// rejection reason that mirrors the cause, instead of the
    /// generic `NoPendingCall` / `CallerClosed`.
    fn record_cancelled_call(&mut self, call_id: CallId, cause: tina::CancelCause) {
        if self.cancelled_calls.len() == CANCELLED_CALL_RING_CAPACITY {
            self.cancelled_calls.pop_front();
            self.cancelled_call_cause_evictions =
                self.cancelled_call_cause_evictions.saturating_add(1);
        }
        self.cancelled_calls.push_back((call_id, cause));
    }

    fn recently_cancelled_cause(&self, call_id: CallId) -> Option<tina::CancelCause> {
        self.cancelled_calls
            .iter()
            .find_map(|(id, cause)| (*id == call_id).then_some(*cause))
    }

    fn close_deferred_slot_for_call_with_reason(
        &mut self,
        call_id: CallId,
        reason: DeferredReplyRejectedReason,
    ) {
        // Local-only: caller-liveness sweep here is driven by this
        // shard's pending isolate calls. First form refuses cross-shard
        // captures so every promoted slot is local — see
        // `PromotedSlots::take_by_local_call_id`.
        if let Some(record) = self.promoted_slots.take_by_local_call_id(call_id) {
            record.shared.set_state(DeferredSlotState::Closed);
            self.push_event(
                record.capturing_isolate,
                None,
                RuntimeEventKind::DeferredReplyRejected {
                    slot_id: record.slot_id,
                    call_id: record.call_id,
                    reason,
                },
            );
        }
    }

    fn complete_isolate_call(
        &mut self,
        call_id: CallId,
        cause: CauseId,
        outcome: CallOutcome<Box<dyn Any>>,
    ) -> bool {
        let Some(index) = self
            .pending_isolate_calls
            .iter()
            .position(|entry| entry.call_id == call_id)
        else {
            return false;
        };
        let mut pending = self.pending_isolate_calls.remove(index);
        let translator = pending
            .translator
            .take()
            .unwrap_or_else(|| panic!("translator for call {call_id:?} already consumed"));
        if let Some(shared) = &pending.handle_shared {
            shared.set_state(tina::CallHandleState::Settled);
        }
        self.deliver_isolate_call_outcome(
            call_id,
            pending.requester,
            cause,
            outcome,
            translator,
            pending.continuation_context,
        );
        true
    }

    fn deliver_isolate_call_outcome(
        &mut self,
        call_id: CallId,
        requester: RegisteredAddress,
        cause: CauseId,
        outcome: CallOutcome<Box<dyn Any>>,
        translator: ErasedIsolateCallTranslator,
        continuation_context: Option<MessageCallContext>,
    ) {
        let failure_reason = match &outcome {
            CallOutcome::Replied(_) => None,
            CallOutcome::Full => Some(CallError::TargetFull),
            CallOutcome::Closed => Some(CallError::TargetClosed),
            CallOutcome::Timeout => Some(CallError::Timeout),
            CallOutcome::Rejected(reason) => Some(CallError::Rejected(*reason)),
        };

        if let Some(reason) = failure_reason {
            self.push_event(
                requester.isolate,
                Some(cause),
                RuntimeEventKind::CallFailed {
                    call_id,
                    call_kind: CallKind::IsolateCall,
                    reason,
                },
            );
        }

        let message = translator(outcome);
        let Some(entry_index) = self.entries.iter().position(|entry| {
            entry.id == requester.isolate && entry.generation == requester.generation
        }) else {
            self.push_event(
                requester.isolate,
                Some(cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id,
                    call_kind: CallKind::IsolateCall,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                },
            );
            return;
        };

        if self.entries[entry_index].stopped.get() {
            self.push_event(
                requester.isolate,
                Some(cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id,
                    call_kind: CallKind::IsolateCall,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                },
            );
            return;
        }

        match self.enqueue_entry_message(entry_index, message, continuation_context) {
            Ok(()) => {
                if failure_reason.is_none() {
                    self.push_event(
                        requester.isolate,
                        Some(cause),
                        RuntimeEventKind::CallCompleted {
                            call_id,
                            call_kind: CallKind::IsolateCall,
                        },
                    );
                }
            }
            Err(TrySendError::Full(_)) => {
                self.push_event(
                    requester.isolate,
                    Some(cause),
                    RuntimeEventKind::CallCompletionRejected {
                        call_id,
                        call_kind: CallKind::IsolateCall,
                        reason: CallCompletionRejectedReason::MailboxFull,
                    },
                );
            }
            Err(TrySendError::Closed(_)) => {
                self.push_event(
                    requester.isolate,
                    Some(cause),
                    RuntimeEventKind::CallCompletionRejected {
                        call_id,
                        call_kind: CallKind::IsolateCall,
                        reason: CallCompletionRejectedReason::RequesterClosed,
                    },
                );
            }
        }
    }

    fn advance_driver(&mut self, now: Instant) {
        let mut completed = std::mem::take(&mut self.driver_completions);
        completed.clear();
        self.driver.advance(now, &mut completed);
        for op in completed.drain(..) {
            self.deliver_completion(op.call_id, op.result);
        }
        self.driver_completions = completed;
    }

    fn deliver_completion(&mut self, call_id: CallId, result: CallOutput) {
        let in_flight_index = self
            .in_flight_calls
            .iter()
            .position(|entry| entry.call_id == call_id)
            .unwrap_or_else(|| panic!("driver produced completion for unknown call {call_id:?}"));
        let in_flight = self.in_flight_calls.remove(in_flight_index);

        let translator_index = self
            .translators
            .iter()
            .position(|entry| entry.call_id == call_id)
            .unwrap_or_else(|| panic!("missing translator for call {call_id:?}"));
        let mut stored = self.translators.remove(translator_index);
        let translator = stored
            .translator
            .take()
            .unwrap_or_else(|| panic!("translator for call {call_id:?} already consumed"));

        // Trace semantics: `CallFailed` records that the runtime
        // observed a failure result for this call. `CallCompleted`
        // records that a *successful* result's translated message
        // reached the requester's mailbox. `CallCompletionRejected`
        // records that the translator's message could not reach the
        // mailbox (regardless of whether the underlying result was a
        // success or a failure). A failed call therefore emits at most
        // `CallFailed` plus, if delivery also fails, one
        // `CallCompletionRejected` — never `CallCompleted`.
        let failure_reason = match &result {
            CallOutput::Failed(reason) => Some(*reason),
            _ => None,
        };
        if let Some(reason) = failure_reason {
            self.push_event(
                in_flight.requester.isolate,
                Some(in_flight.cause),
                RuntimeEventKind::CallFailed {
                    call_id,
                    call_kind: in_flight.call_kind,
                    reason,
                },
            );
        }
        self.push_persistence_completion_events(&in_flight, &result, failure_reason);

        if matches!(in_flight.call_kind, CallKind::TcpBind) {
            match (&result, failure_reason) {
                (CallOutput::TcpBound { local_addr, .. }, _) => {
                    self.observation
                        .notify_bound(observation::BoundAddressOutcome::Bound(*local_addr));
                }
                (_, Some(reason)) => {
                    self.observation
                        .notify_bound(observation::BoundAddressOutcome::Failed(reason));
                }
                _ => {}
            }
        }
        if matches!(in_flight.call_kind, CallKind::TlsBind) {
            match (&result, failure_reason) {
                (CallOutput::TlsBound { local_addr, .. }, _) => {
                    self.observation
                        .notify_tls_bound(observation::BoundAddressOutcome::Bound(*local_addr));
                }
                (_, Some(reason)) => {
                    self.observation
                        .notify_tls_bound(observation::BoundAddressOutcome::Failed(reason));
                }
                _ => {}
            }
        }

        match failure_reason {
            None => self.observation.notify_operation_completed(
                in_flight.requester.isolate,
                in_flight.call_kind,
                call_id,
            ),
            Some(error) => self.observation.notify_operation_failed(
                in_flight.requester.isolate,
                in_flight.call_kind,
                call_id,
                error,
            ),
        }

        let message = translator(result);

        let entry_index = self.entries.iter().position(|entry| {
            entry.id == in_flight.requester.isolate
                && entry.generation == in_flight.requester.generation
        });
        let Some(entry_index) = entry_index else {
            self.push_event(
                in_flight.requester.isolate,
                Some(in_flight.cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id,
                    call_kind: in_flight.call_kind,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                },
            );
            return;
        };

        if self.entries[entry_index].stopped.get() {
            self.push_event(
                in_flight.requester.isolate,
                Some(in_flight.cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id,
                    call_kind: in_flight.call_kind,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                },
            );
            return;
        }

        match self.enqueue_entry_message(entry_index, message, in_flight.continuation_context) {
            Ok(()) => {
                if failure_reason.is_none() {
                    self.push_event(
                        in_flight.requester.isolate,
                        Some(in_flight.cause),
                        RuntimeEventKind::CallCompleted {
                            call_id,
                            call_kind: in_flight.call_kind,
                        },
                    );
                }
                // For failed results we already emitted `CallFailed`
                // above; the translator's message reaching the mailbox
                // is the expected behavior and does not need a second
                // event.
            }
            Err(TrySendError::Full(_)) => {
                self.push_event(
                    in_flight.requester.isolate,
                    Some(in_flight.cause),
                    RuntimeEventKind::CallCompletionRejected {
                        call_id,
                        call_kind: in_flight.call_kind,
                        reason: CallCompletionRejectedReason::MailboxFull,
                    },
                );
            }
            Err(TrySendError::Closed(_)) => {
                self.push_event(
                    in_flight.requester.isolate,
                    Some(in_flight.cause),
                    RuntimeEventKind::CallCompletionRejected {
                        call_id,
                        call_kind: in_flight.call_kind,
                        reason: CallCompletionRejectedReason::RequesterClosed,
                    },
                );
            }
        }
    }

    fn push_persistence_completion_events(
        &mut self,
        in_flight: &InFlightCall,
        result: &CallOutput,
        failure_reason: Option<CallError>,
    ) {
        let Some(persistence) = in_flight.persistence else {
            return;
        };
        match (persistence, failure_reason, result) {
            (call::PersistenceTraceInfo::SnapshotCommit, None, _) => {
                self.push_event(
                    in_flight.requester.isolate,
                    Some(in_flight.cause),
                    RuntimeEventKind::SnapshotCommitted,
                );
            }
            (call::PersistenceTraceInfo::SnapshotCommit, Some(reason), _) => {
                self.push_event(
                    in_flight.requester.isolate,
                    Some(in_flight.cause),
                    RuntimeEventKind::SnapshotCommitFailed { reason },
                );
            }
            (call::PersistenceTraceInfo::JournalAppend { record_index }, None, _) => {
                self.push_event(
                    in_flight.requester.isolate,
                    Some(in_flight.cause),
                    RuntimeEventKind::JournalAppended { record_index },
                );
            }
            (call::PersistenceTraceInfo::JournalAppend { record_index }, Some(reason), _) => {
                self.push_event(
                    in_flight.requester.isolate,
                    Some(in_flight.cause),
                    RuntimeEventKind::JournalAppendFailed {
                        record_index,
                        reason,
                    },
                );
            }
            (call::PersistenceTraceInfo::Recovery, None, _) => {
                self.push_event(
                    in_flight.requester.isolate,
                    Some(in_flight.cause),
                    RuntimeEventKind::RecoveryFinished,
                );
            }
            (call::PersistenceTraceInfo::Recovery, Some(reason), _) => {
                self.push_event(
                    in_flight.requester.isolate,
                    Some(in_flight.cause),
                    RuntimeEventKind::RecoveryFailed { reason },
                );
            }
        }
    }

    fn enqueue_bootstrap_message(
        &mut self,
        child: RegisteredAddress,
        message: Box<dyn Any>,
        cause: CauseId,
    ) {
        let entry_index = self
            .entries
            .iter()
            .position(|entry| entry.id == child.isolate && entry.generation == child.generation)
            .unwrap_or_else(|| panic!("bootstrap referenced unknown child {:?}", child.isolate));
        self.enqueue_entry_message(entry_index, message, None)
            .unwrap_or_else(|_| {
                panic!(
                    "runtime failed to enqueue bootstrap message for child {:?}",
                    child.isolate
                )
            });
        self.push_event(
            child.isolate,
            Some(cause),
            RuntimeEventKind::MailboxAccepted,
        );
    }

    fn stop_entry(&mut self, index: usize, isolate_id: IsolateId, cause: CauseId) -> EventId {
        self.stop_entry_full(index, isolate_id, cause, None, None)
    }

    fn stop_entry_with_precollected(
        &mut self,
        index: usize,
        isolate_id: IsolateId,
        cause: CauseId,
        precollected: Option<DeliveredMessage>,
    ) -> EventId {
        self.stop_entry_full(index, isolate_id, cause, precollected, None)
    }

    fn stop_entry_with_result(
        &mut self,
        index: usize,
        isolate_id: IsolateId,
        cause: CauseId,
        result: StopResult,
    ) -> EventId {
        self.stop_entry_full(index, isolate_id, cause, None, Some(result))
    }

    fn stop_entry_full(
        &mut self,
        index: usize,
        isolate_id: IsolateId,
        cause: CauseId,
        precollected: Option<DeliveredMessage>,
        result: Option<StopResult>,
    ) -> EventId {
        if self.entries[index].stopped.get() {
            let stopped = self.entries[index]
                .stopped_event
                .get()
                .unwrap_or_else(|| panic!("stopped isolate has no stopped event"));
            if precollected.is_some() {
                self.push_event(
                    isolate_id,
                    Some(stopped.into()),
                    RuntimeEventKind::MessageAbandoned,
                );
            }
            // Late StopWith: isolate already stopped, drop the value.
            drop(result);
            return stopped;
        }

        self.entries[index].stopped.set(true);
        self.entries[index].mailbox.close();
        let stopped = self.push_event(isolate_id, Some(cause), RuntimeEventKind::IsolateStopped);
        self.entries[index].stopped_event.set(Some(stopped));
        let generation = self.entries[index].generation;
        self.observation
            .notify_isolate_stopped(isolate_id, generation);

        // Drain any deferred reply slots this isolate captured. The
        // isolate's state (and its DeferredReply Rcs) is not freed
        // until the entry record is dropped, so sweep would not
        // notice. Walk the registry directly.
        for record in self.promoted_slots.take_by_isolate(isolate_id) {
            self.drop_promoted_deferred_slot(record, Some(stopped.into()));
        }
        self.cancel_driver_calls_for_requester(RegisteredAddress {
            shard: self.shard.id(),
            isolate: isolate_id,
            generation: self.entries[index].generation,
        });
        self.cancel_pending_isolate_calls_for_owner(
            isolate_id,
            self.entries[index].generation,
            stopped.into(),
        );
        if precollected.is_some() {
            self.push_event(
                isolate_id,
                Some(stopped.into()),
                RuntimeEventKind::MessageAbandoned,
            );
        }
        while self.recv_entry_message(index).is_some() {
            self.push_event(
                isolate_id,
                Some(stopped.into()),
                RuntimeEventKind::MessageAbandoned,
            );
        }
        // Result delivery happens last so the host only wakes after every
        // lifecycle/trace fact is recorded. With no value, drain any
        // pending result waiter as `StoppedWithoutResult`.
        match result {
            Some(value) => self
                .observation
                .notify_isolate_result(isolate_id, generation, value),
            None => self
                .observation
                .notify_isolate_stopped_without_result(isolate_id, generation),
        }
        stopped
    }

    fn restart_children(
        &mut self,
        parent: IsolateId,
        cause: CauseId,
        round_messages: &mut [Option<DeliveredMessage>],
    ) {
        for child_record_index in 0..self.child_records.len() {
            if self.child_records[child_record_index].parent == parent {
                self.restart_child_record(parent, child_record_index, cause, round_messages);
            }
        }
    }

    fn supervise_panic(
        &mut self,
        failed_child: RegisteredAddress,
        cause: CauseId,
        round_messages: &mut [Option<DeliveredMessage>],
    ) {
        let Some(failed_record_index) = self.child_record_index_by_child(failed_child) else {
            return;
        };

        let parent = self.child_records[failed_record_index].parent;
        let failed_ordinal = self.child_records[failed_record_index].child_ordinal;
        let Some(supervisor_index) = self.supervisor_index(parent) else {
            return;
        };

        if self
            .entry_by_isolate(parent)
            .is_some_and(|entry| entry.stopped.get())
        {
            self.push_event(
                parent,
                Some(cause),
                RuntimeEventKind::SupervisorRestartRejected {
                    failed_child: failed_child.isolate,
                    failed_ordinal,
                    reason: SupervisionRejectedReason::SupervisorStopped,
                },
            );
            return;
        }

        let config = self.supervisors[supervisor_index].config;
        let policy = config.policy();
        let budget_state = self.supervisors[supervisor_index].budget_state;
        let budget_state = match budget_state.record_restart_at(self.clock.now()) {
            Ok(next) => next,
            Err(error) => {
                self.push_event(
                    parent,
                    Some(cause),
                    RuntimeEventKind::SupervisorRestartRejected {
                        failed_child: failed_child.isolate,
                        failed_ordinal,
                        reason: SupervisionRejectedReason::BudgetExceeded {
                            attempted_restart: error.attempted_restart(),
                            max_restarts: error.max_restarts(),
                        },
                    },
                );
                return;
            }
        };
        self.supervisors[supervisor_index].budget_state = budget_state;

        let triggered = self.push_event(
            parent,
            Some(cause),
            RuntimeEventKind::SupervisorRestartTriggered {
                policy,
                failed_child: failed_child.isolate,
                failed_ordinal,
            },
        );

        for child_record_index in 0..self.child_records.len() {
            if self.child_records[child_record_index].parent != parent {
                continue;
            }

            let relation = ChildRelation::from_ordinals(
                self.child_records[child_record_index].child_ordinal,
                failed_ordinal,
            );
            if policy.restarts(relation) {
                self.restart_child_record(
                    parent,
                    child_record_index,
                    triggered.into(),
                    round_messages,
                );
            }
        }
    }

    fn restart_child_record(
        &mut self,
        parent: IsolateId,
        child_record_index: usize,
        cause: CauseId,
        round_messages: &mut [Option<DeliveredMessage>],
    ) {
        let child_ordinal = self.child_records[child_record_index].child_ordinal;
        let old_child = self.child_records[child_record_index].child;
        let attempted = self.push_event(
            parent,
            Some(cause),
            RuntimeEventKind::RestartChildAttempted {
                child_ordinal,
                old_isolate: old_child.isolate,
                old_generation: old_child.generation,
            },
        );

        // Preserve the recipe across restarts while calling back into the
        // runtime mutably to construct the replacement child.
        let Some(recipe) = self.child_records[child_record_index]
            .restart_recipe
            .clone()
        else {
            self.push_event(
                parent,
                Some(attempted.into()),
                RuntimeEventKind::RestartChildSkipped {
                    child_ordinal,
                    old_isolate: old_child.isolate,
                    old_generation: old_child.generation,
                    reason: RestartSkippedReason::NotRestartable,
                },
            );
            return;
        };

        if let Some(old_entry_index) = self.entry_index(old_child) {
            if !self.entries[old_entry_index].stopped.get() {
                let precollected = round_messages
                    .get_mut(old_entry_index)
                    .and_then(Option::take);
                self.stop_entry_with_precollected(
                    old_entry_index,
                    old_child.isolate,
                    attempted.into(),
                    precollected,
                );
            }
        }

        let outcome = recipe.create(self, parent);
        let new_child = outcome.child;
        let bootstrap_message = outcome.bootstrap_message;
        self.child_records[child_record_index].child = new_child;
        self.child_records[child_record_index].mailbox_capacity = outcome.mailbox_capacity;
        // Rebind the same restart recipe so this child slot remains
        // restartable after the first replacement.
        self.child_records[child_record_index].restart_recipe = Some(recipe);

        let restarted = self.push_event(
            parent,
            Some(attempted.into()),
            RuntimeEventKind::RestartChildCompleted {
                child_ordinal,
                old_isolate: old_child.isolate,
                old_generation: old_child.generation,
                new_isolate: new_child.isolate,
                new_generation: new_child.generation,
            },
        );
        if let Some(message) = bootstrap_message {
            self.enqueue_bootstrap_message(new_child, message, restarted.into());
        }
        // Notify *after* the bootstrap message has been enqueued so a host
        // that wakes from `wait()` cannot race a `try_send` ahead of the
        // bootstrap delivery.
        self.observation.notify_child_restarted(
            parent,
            observation::ChildRestarted {
                child_ordinal,
                new_isolate: new_child.isolate,
                new_generation: new_child.generation,
            },
        );
    }

    fn push_event(
        &mut self,
        isolate: IsolateId,
        cause: Option<CauseId>,
        kind: RuntimeEventKind,
    ) -> EventId {
        let id = self.ids.next_event_id();
        let event = RuntimeEvent::new(id, cause, self.shard.id(), isolate, kind);
        // Observer first. Retention::Off does not silence it.
        // A panic here kills the recording thread by design.
        if let Some(obs) = &self.trace_observer {
            obs.on_event(&event);
        }
        match self.trace_retention {
            TraceRetention::Full => {
                self.compact_trace_prefix();
                self.trace.push(event);
            }
            TraceRetention::Bounded(capacity) if capacity > 0 => {
                if self.active_trace_len() == capacity {
                    self.trace_start += 1;
                    self.trace_dropped += 1;
                    if self.trace_start >= capacity {
                        self.compact_trace_prefix();
                    }
                }
                self.trace.push(event);
            }
            TraceRetention::Bounded(_) | TraceRetention::Off => {
                self.trace_dropped += 1;
            }
        }
        id
    }

    fn enforce_trace_retention(&mut self) {
        match self.trace_retention {
            TraceRetention::Full => {
                self.compact_trace_prefix();
            }
            TraceRetention::Bounded(capacity) => {
                let active = self.active_trace_len();
                if active > capacity {
                    let excess = active - capacity;
                    self.trace_start += excess;
                    self.trace_dropped += excess as u64;
                }
                self.compact_trace_prefix_if_empty_or_large(capacity.max(1));
            }
            TraceRetention::Off => {
                self.trace_dropped += self.active_trace_len() as u64;
                self.trace.clear();
                self.trace_start = 0;
            }
        }
    }

    fn active_trace_len(&self) -> usize {
        self.trace.len().saturating_sub(self.trace_start)
    }

    fn compact_trace_prefix(&mut self) {
        if self.trace_start == 0 {
            return;
        }
        self.trace.drain(0..self.trace_start);
        self.trace_start = 0;
    }

    fn compact_trace_prefix_if_empty_or_large(&mut self, threshold: usize) {
        if self.trace_start == 0 {
            return;
        }
        if self.trace_start >= self.trace.len() || self.trace_start >= threshold {
            self.compact_trace_prefix();
        }
    }

    fn gc_stopped_entries(&mut self) {
        let mut index = 0;
        while index < self.entries.len() {
            if self.can_gc_stopped_entry(index) {
                self.entries.remove(index);
            } else {
                index += 1;
            }
        }
    }

    fn can_gc_stopped_entry(&self, index: usize) -> bool {
        let entry = &self.entries[index];
        if !entry.stopped.get() {
            return false;
        }
        let address = RegisteredAddress {
            shard: self.shard.id(),
            isolate: entry.id,
            generation: entry.generation,
        };
        if self
            .child_records
            .iter()
            .any(|record| record.parent == entry.id || record.child == address)
        {
            return false;
        }
        if self
            .supervisors
            .iter()
            .any(|record| record.parent == address)
        {
            return false;
        }
        if self
            .in_flight_calls
            .iter()
            .any(|call| call.requester == address)
        {
            return false;
        }
        if self
            .pending_isolate_calls
            .iter()
            .any(|call| call.requester == address)
        {
            return false;
        }
        true
    }

    fn enqueue_entry_message(
        &self,
        entry_index: usize,
        message: Box<dyn Any>,
        call_context: Option<MessageCallContext>,
    ) -> Result<(), TrySendError<Box<dyn Any>>> {
        match self.entries[entry_index].mailbox.try_send_boxed(message) {
            Ok(()) => {
                self.entries[entry_index]
                    .call_contexts
                    .borrow_mut()
                    .push_back(call_context);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn recv_entry_message(&self, entry_index: usize) -> Option<DeliveredMessage> {
        let message = self.entries[entry_index].mailbox.recv_boxed()?;
        let call_context = self.entries[entry_index]
            .call_contexts
            .borrow_mut()
            .pop_front()
            .unwrap_or(None);
        Some(DeliveredMessage {
            message,
            call_context,
        })
    }

    fn dispatch_local_send(&self, send: ErasedSend) -> Result<(), SendRejectedReason> {
        self.dispatch_local_send_with_context(send, None)
    }

    fn dispatch_local_send_with_context(
        &self,
        send: ErasedSend,
        call_context: Option<MessageCallContext>,
    ) -> Result<(), SendRejectedReason> {
        if send.target_shard != self.shard.id() {
            panic!(
                "cross-shard send is out of scope in this slice: target shard {} != runtime shard {}",
                send.target_shard.get(),
                self.shard.id().get(),
            );
        }

        let Some(entry_index) = self
            .entries
            .iter()
            .position(|entry| entry.id == send.target_isolate)
        else {
            return Err(SendRejectedReason::Closed);
        };
        let entry = &self.entries[entry_index];

        if entry.generation != send.target_generation {
            return Err(SendRejectedReason::Closed);
        }

        self.enqueue_entry_message(entry_index, send.message.into_any(), call_context)
            .map_err(|reason| match reason {
                TrySendError::Full(_) => SendRejectedReason::Full,
                TrySendError::Closed(_) => SendRejectedReason::Closed,
            })
    }

    fn harvest_remote_envelope(
        &mut self,
        queued: QueuedRemoteEnvelope,
    ) -> Option<QueuedRemoteEnvelope> {
        match queued {
            QueuedRemoteEnvelope::Send(send) => self.harvest_remote_send(send),
            QueuedRemoteEnvelope::CallReply(reply) => {
                self.harvest_remote_call_reply(reply);
                None
            }
        }
    }

    fn harvest_remote_send(&mut self, queued: QueuedRemoteSend) -> Option<QueuedRemoteEnvelope> {
        // Cross-shard transport admission already happened on the source shard.
        // What we record here is destination-local harvest outcome, not a
        // retroactive change to the source-side send result.
        let send = queued.send;
        let Some(entry_index) = self
            .entries
            .iter()
            .position(|entry| entry.id == send.target_isolate)
        else {
            self.push_event(
                send.target_isolate,
                Some(queued.cause),
                RuntimeEventKind::SendRejected {
                    target_shard: send.target_shard,
                    target_isolate: send.target_isolate,
                    target_generation: send.target_generation,
                    reason: SendRejectedReason::Closed,
                },
            );
            return remote_call_outcome_envelope(queued.call_context, RemoteCallOutcome::Closed);
        };
        let entry = &self.entries[entry_index];

        if entry.generation != send.target_generation {
            self.push_event(
                send.target_isolate,
                Some(queued.cause),
                RuntimeEventKind::SendRejected {
                    target_shard: send.target_shard,
                    target_isolate: send.target_isolate,
                    target_generation: send.target_generation,
                    reason: SendRejectedReason::Closed,
                },
            );
            return remote_call_outcome_envelope(queued.call_context, RemoteCallOutcome::Closed);
        }

        match self.enqueue_entry_message(entry_index, send.message.into_any(), queued.call_context)
        {
            Ok(()) => {
                self.push_event(
                    send.target_isolate,
                    Some(queued.cause),
                    RuntimeEventKind::MailboxAccepted,
                );
                None
            }
            Err(TrySendError::Full(_)) => {
                self.push_event(
                    send.target_isolate,
                    Some(queued.cause),
                    RuntimeEventKind::SendRejected {
                        target_shard: send.target_shard,
                        target_isolate: send.target_isolate,
                        target_generation: send.target_generation,
                        reason: SendRejectedReason::Full,
                    },
                );
                remote_call_outcome_envelope(queued.call_context, RemoteCallOutcome::Full)
            }
            Err(TrySendError::Closed(_)) => {
                self.push_event(
                    send.target_isolate,
                    Some(queued.cause),
                    RuntimeEventKind::SendRejected {
                        target_shard: send.target_shard,
                        target_isolate: send.target_isolate,
                        target_generation: send.target_generation,
                        reason: SendRejectedReason::Closed,
                    },
                );
                remote_call_outcome_envelope(queued.call_context, RemoteCallOutcome::Closed)
            }
        }
    }

    fn harvest_remote_call_reply(&mut self, reply: RemoteCallReply) {
        match reply.outcome {
            RemoteCallOutcome::Replied(message) => {
                if !self.complete_isolate_call(
                    reply.call_id,
                    reply.cause,
                    CallOutcome::Replied(message.into_any()),
                ) {
                    // Cross-shard late reply path: same cause-aware
                    // classification as the local-reply path. Without
                    // this, a cancelled or timed-out cross-shard call
                    // loses the new `CallerCancelled` / `CallerTimedOut`
                    // / `OwnerStopped` / `RuntimeStopped` truth and
                    // surfaces as the generic `NoPendingCall`.
                    let reason = match self.recently_cancelled_cause(reply.call_id) {
                        Some(c) => call_reply_reason_for_cause(c),
                        None => CallReplyRejectedReason::NoPendingCall,
                    };
                    self.push_event(
                        reply.requester.isolate,
                        Some(reply.cause),
                        RuntimeEventKind::CallReplyRejected {
                            call_id: reply.call_id,
                            reason,
                        },
                    );
                }
            }
            RemoteCallOutcome::Full => {
                self.complete_remote_isolate_call(reply, CallOutcome::Full);
            }
            RemoteCallOutcome::Closed => {
                self.complete_remote_isolate_call(reply, CallOutcome::Closed);
            }
            RemoteCallOutcome::Rejected(reason) => {
                self.complete_remote_isolate_call(reply, CallOutcome::Rejected(reason));
            }
        }
    }

    fn complete_remote_isolate_call(
        &mut self,
        reply: RemoteCallReply,
        outcome: CallOutcome<Box<dyn Any>>,
    ) {
        if !self.complete_isolate_call(reply.call_id, reply.cause, outcome) {
            self.push_event(
                reply.requester.isolate,
                Some(reply.cause),
                RuntimeEventKind::CallReplyRejected {
                    call_id: reply.call_id,
                    reason: CallReplyRejectedReason::NoPendingCall,
                },
            );
        }
    }

    fn entry_index(&self, address: RegisteredAddress) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.id == address.isolate && entry.generation == address.generation)
    }

    fn entry_by_isolate(&self, isolate: IsolateId) -> Option<&RegisteredEntry<S, F>> {
        self.entries.iter().find(|entry| entry.id == isolate)
    }

    fn child_record_index_by_child(&self, child: RegisteredAddress) -> Option<usize> {
        self.child_records
            .iter()
            .position(|record| record.child == child)
    }

    fn supervisor_index(&self, parent: IsolateId) -> Option<usize> {
        self.supervisors
            .iter()
            .position(|record| record.parent.isolate == parent)
    }

    fn try_registered_address<M: 'static, R>(
        &self,
        address: Address<M, R>,
    ) -> Option<RegisteredAddress> {
        if address.shard() != self.shard.id() {
            return None;
        }

        let entry = self
            .entries
            .iter()
            .find(|entry| entry.id == address.isolate())?;

        if entry.generation != address.generation() {
            return None;
        }

        Some(RegisteredAddress {
            shard: address.shard(),
            isolate: address.isolate(),
            generation: address.generation(),
        })
    }

    fn register_entry<I, Outbound>(
        &mut self,
        isolate: I,
        parent: Option<IsolateId>,
        mailbox: Box<dyn ErasedMailbox>,
    ) -> RegisteredAddress
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        let isolate_id = IsolateId::new(self.next_isolate_id);
        self.next_isolate_id += 1;
        let generation = AddressGeneration::new(0);

        self.entries.push(RegisteredEntry {
            id: isolate_id,
            generation,
            parent,
            stopped: Cell::new(false),
            stopped_event: Cell::new(None),
            mailbox,
            call_contexts: RefCell::new(VecDeque::new()),
            handler: RefCell::new(Box::new(HandlerAdapter::<I, Outbound> {
                isolate,
                marker: PhantomData,
            })),
        });

        RegisteredAddress {
            shard: self.shard.id(),
            isolate: isolate_id,
            generation,
        }
    }

    fn register_sendable_with_capacity<I, Outbound>(
        &mut self,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Address<I::Message, I::Reply>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Reply: Send + 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: IntoRuntimeFact + 'static,
        Outbound: Send + 'static,
    {
        let address = self.register_sendable_entry::<I, Outbound>(
            isolate,
            None,
            Box::new(AnyMailboxAdapter {
                mailbox: self
                    .mailbox_factory
                    .create::<Box<dyn Any>>(mailbox_capacity),
            }),
        );

        Address::new_with_generation(address.shard, address.isolate, address.generation)
    }

    #[allow(clippy::type_complexity)]
    pub(crate) fn register_sendable_with_capacity_and_bootstrap<I, Outbound>(
        &mut self,
        isolate: I,
        mailbox_capacity: usize,
        bootstrap: I::Message,
    ) -> Result<Address<I::Message, I::Reply>, RegisterBootstrapError<I::Message>>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Reply: Send + 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: IntoRuntimeFact + 'static,
        Outbound: Send + 'static,
    {
        let mailbox = self
            .mailbox_factory
            .create::<Box<dyn Any>>(mailbox_capacity);
        let adapter = AnyMailboxAdapter { mailbox };
        let boxed: Box<dyn Any> = Box::new(bootstrap);
        if let Err(err) = adapter.try_send_boxed(boxed) {
            let recover = |b: Box<dyn Any>| {
                *b.downcast::<I::Message>()
                    .expect("bootstrap message type recovered from boxed Any")
            };
            return Err(match err {
                TrySendError::Full(b) => RegisterBootstrapError::Full(recover(b)),
                TrySendError::Closed(b) => RegisterBootstrapError::Closed(recover(b)),
            });
        }
        let address = self.register_sendable_entry::<I, Outbound>(isolate, None, Box::new(adapter));
        Ok(Address::new_with_generation(
            address.shard,
            address.isolate,
            address.generation,
        ))
    }

    fn register_sendable_entry<I, Outbound>(
        &mut self,
        isolate: I,
        parent: Option<IsolateId>,
        mailbox: Box<dyn ErasedMailbox>,
    ) -> RegisteredAddress
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Reply: Send + 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: IntoRuntimeFact + 'static,
        Outbound: Send + 'static,
    {
        let isolate_id = IsolateId::new(self.next_isolate_id);
        self.next_isolate_id += 1;
        let generation = AddressGeneration::new(0);

        self.entries.push(RegisteredEntry {
            id: isolate_id,
            generation,
            parent,
            stopped: Cell::new(false),
            stopped_event: Cell::new(None),
            mailbox,
            call_contexts: RefCell::new(VecDeque::new()),
            handler: RefCell::new(Box::new(SendableHandlerAdapter::<I, Outbound> {
                isolate,
                marker: PhantomData,
            })),
        });

        RegisteredAddress {
            shard: self.shard.id(),
            isolate: isolate_id,
            generation,
        }
    }

    fn spawn_isolate<I, Outbound>(
        &mut self,
        parent: IsolateId,
        isolate: I,
        mailbox_capacity: usize,
        bootstrap_message: Option<I::Message>,
    ) -> SpawnOutcome<S, F>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        if mailbox_capacity == 0 {
            panic!("spawn requested mailbox capacity 0, which is out of scope for this slice");
        }

        let child = self.register_entry::<I, Outbound>(
            isolate,
            Some(parent),
            Box::new(AnyMailboxAdapter {
                mailbox: self
                    .mailbox_factory
                    .create::<Box<dyn Any>>(mailbox_capacity),
            }),
        );

        SpawnOutcome {
            child,
            mailbox_capacity,
            restart_recipe: None,
            bootstrap_message: bootstrap_message.map(|message| Box::new(message) as Box<dyn Any>),
        }
    }

    fn record_child(&mut self, parent: IsolateId, outcome: SpawnOutcome<S, F>) {
        let child_ordinal = self
            .child_records
            .iter()
            .filter(|record| record.parent == parent)
            .count();

        self.child_records.push(ChildRecord {
            parent,
            child: outcome.child,
            child_ordinal,
            mailbox_capacity: outcome.mailbox_capacity,
            restart_recipe: outcome.restart_recipe,
        });
    }

    /// Returns the stored direct-parent lineage in registration order.
    #[cfg(test)]
    pub(crate) fn lineage_snapshot(&self) -> Vec<(IsolateId, Option<IsolateId>)> {
        self.entries
            .iter()
            .map(|entry| (entry.id, entry.parent))
            .collect()
    }

    /// Returns the stored child records in spawn order.
    #[cfg(test)]
    pub(crate) fn child_record_snapshot(&self) -> Vec<ChildRecordSnapshot> {
        self.child_records
            .iter()
            .map(|record| ChildRecordSnapshot {
                parent: record.parent,
                child_shard: record.child.shard,
                child_isolate: record.child.isolate,
                child_generation: record.child.generation,
                child_ordinal: record.child_ordinal,
                mailbox_capacity: record.mailbox_capacity,
                restartable: record.restart_recipe.is_some(),
            })
            .collect()
    }

    /// Returns the stored supervisor records in configuration order.
    #[cfg(test)]
    pub(crate) fn supervisor_snapshot(&self) -> Vec<SupervisorRecordSnapshot> {
        self.supervisors
            .iter()
            .map(|record| SupervisorRecordSnapshot {
                parent: record.parent,
                config: record.config,
                budget_state: record.budget_state,
            })
            .collect()
    }
}

trait ErasedMailbox {
    fn recv_boxed(&self) -> Option<Box<dyn Any>>;
    fn try_send_boxed(&self, message: Box<dyn Any>) -> Result<(), TrySendError<Box<dyn Any>>>;
    fn close(&self);
}

struct MailboxAdapter<M, Msg>
where
    M: Mailbox<Msg>,
{
    mailbox: M,
    marker: PhantomData<fn(Msg) -> Msg>,
}

impl<M, Msg> ErasedMailbox for MailboxAdapter<M, Msg>
where
    M: Mailbox<Msg>,
    Msg: 'static,
{
    fn recv_boxed(&self) -> Option<Box<dyn Any>> {
        self.mailbox
            .recv()
            .map(|message| Box::new(message) as Box<dyn Any>)
    }

    fn try_send_boxed(&self, message: Box<dyn Any>) -> Result<(), TrySendError<Box<dyn Any>>> {
        let message = message.downcast::<Msg>().unwrap_or_else(|_| {
            panic!("runtime attempted to deliver a message to a mailbox with the wrong type")
        });

        match self.mailbox.try_send(*message) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(message)) => {
                Err(TrySendError::Full(Box::new(message) as Box<dyn Any>))
            }
            Err(TrySendError::Closed(message)) => {
                Err(TrySendError::Closed(Box::new(message) as Box<dyn Any>))
            }
        }
    }

    fn close(&self) {
        self.mailbox.close();
    }
}

struct AnyMailboxAdapter {
    mailbox: Box<dyn Mailbox<Box<dyn Any>>>,
}

impl ErasedMailbox for AnyMailboxAdapter {
    fn recv_boxed(&self) -> Option<Box<dyn Any>> {
        self.mailbox.recv()
    }

    fn try_send_boxed(&self, message: Box<dyn Any>) -> Result<(), TrySendError<Box<dyn Any>>> {
        self.mailbox.try_send(message)
    }

    fn close(&self) {
        self.mailbox.close();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RegisteredAddress {
    shard: ShardId,
    isolate: IsolateId,
    generation: AddressGeneration,
}

struct SpawnOutcome<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    child: RegisteredAddress,
    mailbox_capacity: usize,
    restart_recipe: Option<Rc<dyn ErasedRestartRecipe<S, F>>>,
    bootstrap_message: Option<Box<dyn Any>>,
}

struct SpawnObservedOutcome<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    spawn: Option<SpawnOutcome<S, F>>,
    continuation: Option<ErasedMessage>,
}

#[cfg_attr(not(test), allow(dead_code))]
struct ChildRecord<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    parent: IsolateId,
    child: RegisteredAddress,
    child_ordinal: usize,
    mailbox_capacity: usize,
    restart_recipe: Option<Rc<dyn ErasedRestartRecipe<S, F>>>,
}

struct SupervisorRecord {
    parent: RegisteredAddress,
    config: SupervisorConfig,
    budget_state: RestartBudgetState,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChildRecordSnapshot {
    parent: IsolateId,
    child_shard: ShardId,
    child_isolate: IsolateId,
    child_generation: AddressGeneration,
    child_ordinal: usize,
    mailbox_capacity: usize,
    restartable: bool,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SupervisorRecordSnapshot {
    parent: RegisteredAddress,
    config: SupervisorConfig,
    budget_state: RestartBudgetState,
}

trait ErasedHandler<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    fn handle_boxed(
        &mut self,
        message: Box<dyn Any>,
        shard: &mut S,
        isolate_id: IsolateId,
        generation: AddressGeneration,
        caller: Option<MessageCaller>,
        now: std::time::Instant,
    ) -> ErasedEffect<S, F>;

    fn handle_call_boxed(
        &mut self,
        message: Box<dyn Any>,
        shard: &mut S,
        isolate_id: IsolateId,
        generation: AddressGeneration,
        caller: MessageCaller,
        now: std::time::Instant,
    ) -> ErasedEffect<S, F>;
}

trait ErasedSpawn<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    fn spawn(self: Box<Self>, runtime: &mut Runtime<S, F>, parent: IsolateId)
    -> SpawnOutcome<S, F>;

    fn try_spawn_observed(
        self: Box<Self>,
        runtime: &mut Runtime<S, F>,
        parent: IsolateId,
    ) -> Result<SpawnOutcome<S, F>, SpawnObservedError> {
        Ok(self.spawn(runtime, parent))
    }
}

trait ErasedRestartRecipe<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    fn create(&self, runtime: &mut Runtime<S, F>, parent: IsolateId) -> SpawnOutcome<S, F>;
}

trait IntoErasedSpawn<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    fn into_erased_spawn(self) -> Box<dyn ErasedSpawn<S, F>>;
}

trait ErasedSpawnObserved<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    fn spawn_observed(
        self: Box<Self>,
        runtime: &mut Runtime<S, F>,
        parent: IsolateId,
    ) -> SpawnObservedOutcome<S, F>;
}

trait IntoErasedSpawnObserved<S, F, ParentMessage>
where
    S: Shard,
    F: MailboxFactory,
{
    fn into_erased_spawn_observed(self) -> Box<dyn ErasedSpawnObserved<S, F>>;
}

struct HandlerAdapter<I, Outbound>
where
    I: Isolate,
{
    isolate: I,
    marker: PhantomData<fn(Outbound) -> Outbound>,
}

impl<I, S, F, Outbound> ErasedHandler<S, F> for HandlerAdapter<I, Outbound>
where
    I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
    I::Message: 'static,
    I::Reply: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
    I::Call: IntoErasedCall<I::Message> + 'static,
    I::Fact: IntoRuntimeFact + 'static,
    Outbound: 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn handle_boxed(
        &mut self,
        message: Box<dyn Any>,
        shard: &mut S,
        isolate_id: IsolateId,
        generation: AddressGeneration,
        caller: Option<MessageCaller>,
        now: std::time::Instant,
    ) -> ErasedEffect<S, F> {
        let message = message.downcast::<I::Message>().unwrap_or_else(|_| {
            panic!("runtime attempted to deliver a handler message with the wrong type")
        });

        let effect = {
            let mut ctx = Context::<_, I::Reply>::new_typed(shard, isolate_id)
                .with_current_generation(generation)
                .with_now(now);
            if let Some(caller) = caller {
                ctx = ctx.with_caller(caller);
            }
            self.isolate.handle(*message, &mut ctx)
        };

        erase_effect::<I, S, F, Outbound>(effect)
    }

    fn handle_call_boxed(
        &mut self,
        message: Box<dyn Any>,
        shard: &mut S,
        isolate_id: IsolateId,
        generation: AddressGeneration,
        caller: MessageCaller,
        now: std::time::Instant,
    ) -> ErasedEffect<S, F> {
        let message = message.downcast::<I::Message>().unwrap_or_else(|_| {
            panic!("runtime attempted to deliver a call handler message with the wrong type")
        });

        let effect = {
            let ctx = Context::<_, I::Reply>::new_typed(shard, isolate_id)
                .with_current_generation(generation)
                .with_now(now)
                .with_caller(caller);
            self.isolate.handle_call(*message, CallContext::new(ctx))
        };

        erase_effect::<I, S, F, Outbound>(effect)
    }
}

struct SendableHandlerAdapter<I, Outbound>
where
    I: Isolate,
{
    isolate: I,
    marker: PhantomData<fn(Outbound) -> Outbound>,
}

impl<I, S, F, Outbound> ErasedHandler<S, F> for SendableHandlerAdapter<I, Outbound>
where
    I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
    I::Message: 'static,
    I::Reply: Send + 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
    I::Call: IntoErasedCall<I::Message> + 'static,
    I::Fact: IntoRuntimeFact + 'static,
    Outbound: Send + 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn handle_boxed(
        &mut self,
        message: Box<dyn Any>,
        shard: &mut S,
        isolate_id: IsolateId,
        generation: AddressGeneration,
        caller: Option<MessageCaller>,
        now: std::time::Instant,
    ) -> ErasedEffect<S, F> {
        let message = message.downcast::<I::Message>().unwrap_or_else(|_| {
            panic!("runtime attempted to deliver a handler message with the wrong type")
        });

        let effect = {
            let mut ctx = Context::<_, I::Reply>::new_typed(shard, isolate_id)
                .with_current_generation(generation)
                .with_now(now);
            if let Some(caller) = caller {
                ctx = ctx.with_caller(caller);
            }
            self.isolate.handle(*message, &mut ctx)
        };

        erase_effect_sendable::<I, S, F, Outbound>(effect)
    }

    fn handle_call_boxed(
        &mut self,
        message: Box<dyn Any>,
        shard: &mut S,
        isolate_id: IsolateId,
        generation: AddressGeneration,
        caller: MessageCaller,
        now: std::time::Instant,
    ) -> ErasedEffect<S, F> {
        let message = message.downcast::<I::Message>().unwrap_or_else(|_| {
            panic!("runtime attempted to deliver a call handler message with the wrong type")
        });

        let effect = {
            let ctx = Context::<_, I::Reply>::new_typed(shard, isolate_id)
                .with_current_generation(generation)
                .with_now(now)
                .with_caller(caller);
            self.isolate.handle_call(*message, CallContext::new(ctx))
        };

        erase_effect_sendable::<I, S, F, Outbound>(effect)
    }
}

fn erase_effect<I, S, F, Outbound>(effect: Effect<I>) -> ErasedEffect<S, F>
where
    I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
    I::Message: 'static,
    I::Reply: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
    I::Call: IntoErasedCall<I::Message> + 'static,
    I::Fact: IntoRuntimeFact + 'static,
    Outbound: 'static,
    S: Shard,
    F: MailboxFactory,
{
    match effect {
        Effect::Noop => ErasedEffect::Noop,
        Effect::Reply(reply) => ErasedEffect::Reply(ErasedMessage::Local(Box::new(reply))),
        Effect::Reject(reason) => ErasedEffect::Reject(reason),
        Effect::Send(send) => {
            let (destination, message) = send.into_parts();
            ErasedEffect::Send(ErasedSend {
                target_shard: destination.shard(),
                target_isolate: destination.isolate(),
                target_generation: destination.generation(),
                message: ErasedMessage::Local(Box::new(message)),
            })
        }
        Effect::Spawn(spawn) => ErasedEffect::Spawn(spawn.into_erased_spawn()),
        Effect::SpawnObserved(spawn) => {
            ErasedEffect::SpawnObserved(spawn.into_erased_spawn_observed())
        }
        Effect::Stop => ErasedEffect::Stop,
        Effect::StopWith(result) => ErasedEffect::StopWith(result),
        Effect::RestartChildren => ErasedEffect::RestartChildren,
        Effect::Call(call) => ErasedEffect::Call(call.into_erased_call()),
        Effect::Batch(effects) => ErasedEffect::Batch(
            effects
                .into_iter()
                .map(erase_effect::<I, S, F, Outbound>)
                .collect(),
        ),
        Effect::ReplyTo(slot, reply) => ErasedEffect::ReplyTo {
            handle: tina::runtime_internal::deferred_into_handle(slot),
            message: ErasedMessage::Local(Box::new(reply)),
        },
        Effect::Fact(fact) => ErasedEffect::Fact(fact.into_runtime_fact()),
    }
}

fn erase_effect_sendable<I, S, F, Outbound>(effect: Effect<I>) -> ErasedEffect<S, F>
where
    I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
    I::Message: 'static,
    I::Reply: Send + 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
    I::Call: IntoErasedCall<I::Message> + 'static,
    I::Fact: IntoRuntimeFact + 'static,
    Outbound: Send + 'static,
    S: Shard,
    F: MailboxFactory,
{
    match effect {
        Effect::Noop => ErasedEffect::Noop,
        Effect::Reply(reply) => ErasedEffect::Reply(ErasedMessage::Sendable(Box::new(reply))),
        Effect::Reject(reason) => ErasedEffect::Reject(reason),
        Effect::Send(send) => {
            let (destination, message) = send.into_parts();
            ErasedEffect::Send(ErasedSend {
                target_shard: destination.shard(),
                target_isolate: destination.isolate(),
                target_generation: destination.generation(),
                message: ErasedMessage::Sendable(Box::new(message)),
            })
        }
        Effect::Spawn(spawn) => ErasedEffect::Spawn(spawn.into_erased_spawn()),
        Effect::SpawnObserved(spawn) => {
            ErasedEffect::SpawnObserved(spawn.into_erased_spawn_observed())
        }
        Effect::Stop => ErasedEffect::Stop,
        Effect::StopWith(result) => ErasedEffect::StopWith(result),
        Effect::RestartChildren => ErasedEffect::RestartChildren,
        Effect::Call(call) => ErasedEffect::Call(call.into_erased_call()),
        Effect::Batch(effects) => ErasedEffect::Batch(
            effects
                .into_iter()
                .map(erase_effect_sendable::<I, S, F, Outbound>)
                .collect(),
        ),
        Effect::ReplyTo(slot, reply) => ErasedEffect::ReplyTo {
            handle: tina::runtime_internal::deferred_into_handle(slot),
            message: ErasedMessage::Sendable(Box::new(reply)),
        },
        Effect::Fact(fact) => ErasedEffect::Fact(fact.into_runtime_fact()),
    }
}

struct RegisteredEntry<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    id: IsolateId,
    generation: AddressGeneration,
    #[cfg_attr(not(test), allow(dead_code))]
    parent: Option<IsolateId>,
    stopped: Cell<bool>,
    stopped_event: Cell<Option<EventId>>,
    mailbox: Box<dyn ErasedMailbox>,
    call_contexts: RefCell<VecDeque<Option<MessageCallContext>>>,
    handler: RefCell<Box<dyn ErasedHandler<S, F>>>,
}

enum ErasedEffect<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    Noop,
    Reply(ErasedMessage),
    Reject(CallRejectedReason),
    Send(ErasedSend),
    Spawn(Box<dyn ErasedSpawn<S, F>>),
    SpawnObserved(Box<dyn ErasedSpawnObserved<S, F>>),
    Stop,
    StopWith(StopResult),
    RestartChildren,
    Call(ErasedCall),
    Batch(Vec<ErasedEffect<S, F>>),
    ReplyTo {
        handle: DeferredReplyHandle,
        message: ErasedMessage,
    },
    Fact(RuntimeFact),
}

impl<S, F> ErasedEffect<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    fn kind(&self) -> EffectKind {
        match self {
            Self::Noop => EffectKind::Noop,
            Self::Reply(_) => EffectKind::Reply,
            Self::Reject(_) => EffectKind::Reject,
            Self::Send(_) => EffectKind::Send,
            Self::Spawn(_) => EffectKind::Spawn,
            Self::SpawnObserved(_) => EffectKind::SpawnObserved,
            Self::Stop => EffectKind::Stop,
            Self::StopWith(_) => EffectKind::StopWith,
            Self::RestartChildren => EffectKind::RestartChildren,
            Self::Call(_) => EffectKind::Call,
            Self::Batch(_) => EffectKind::Batch,
            Self::ReplyTo { .. } => EffectKind::ReplyTo,
            Self::Fact(_) => EffectKind::Fact,
        }
    }

    fn consumes_call_context(&self) -> bool {
        match self {
            Self::Reply(_) | Self::Reject(_) => true,
            Self::Batch(effects) => {
                for effect in effects {
                    if effect.consumes_call_context() {
                        return true;
                    }
                    if effect.stops_before_consuming_call_context() {
                        return false;
                    }
                }
                false
            }
            _ => false,
        }
    }

    fn stops_before_consuming_call_context(&self) -> bool {
        match self {
            Self::Stop | Self::StopWith(_) => true,
            Self::Reply(_) | Self::Reject(_) => false,
            Self::Batch(effects) => {
                for effect in effects {
                    if effect.consumes_call_context() {
                        return false;
                    }
                    if effect.stops_before_consuming_call_context() {
                        return true;
                    }
                }
                false
            }
            _ => false,
        }
    }
}

pub(crate) struct ErasedSend {
    pub(crate) target_shard: ShardId,
    pub(crate) target_isolate: IsolateId,
    pub(crate) target_generation: AddressGeneration,
    pub(crate) message: ErasedMessage,
}

enum QueuedRemoteEnvelope {
    Send(QueuedRemoteSend),
    CallReply(RemoteCallReply),
}

impl QueuedRemoteEnvelope {
    fn target_shard(&self) -> ShardId {
        match self {
            Self::Send(send) => send.send.target_shard,
            Self::CallReply(reply) => reply.requester.shard,
        }
    }
}

fn remote_call_outcome_envelope(
    context: Option<MessageCallContext>,
    outcome: RemoteCallOutcome,
) -> Option<QueuedRemoteEnvelope> {
    let Some(MessageCallContext::Remote {
        call_id,
        requester,
        cause,
        ..
    }) = context
    else {
        return None;
    };
    Some(QueuedRemoteEnvelope::CallReply(RemoteCallReply {
        call_id,
        requester,
        cause,
        outcome,
    }))
}

struct QueuedRemoteSend {
    send: ErasedSend,
    call_context: Option<MessageCallContext>,
    cause: CauseId,
}

struct SendableQueuedRemoteSend {
    target_shard: ShardId,
    target_isolate: IsolateId,
    target_generation: AddressGeneration,
    message: Box<dyn Any + Send>,
    call_context: Option<MessageCallContext>,
    cause: CauseId,
}

impl SendableQueuedRemoteSend {
    fn new(send: ErasedSend, call_context: Option<MessageCallContext>, cause: CauseId) -> Self {
        Self {
            target_shard: send.target_shard,
            target_isolate: send.target_isolate,
            target_generation: send.target_generation,
            message: send.message.into_sendable(),
            call_context,
            cause,
        }
    }

    fn into_queued_remote_send(self) -> QueuedRemoteSend {
        QueuedRemoteSend {
            send: ErasedSend {
                target_shard: self.target_shard,
                target_isolate: self.target_isolate,
                target_generation: self.target_generation,
                message: ErasedMessage::Sendable(self.message),
            },
            call_context: self.call_context,
            cause: self.cause,
        }
    }
}

enum SendableQueuedRemoteEnvelope {
    Send(SendableQueuedRemoteSend),
    CallReply(SendableRemoteCallReply),
}

impl SendableQueuedRemoteEnvelope {
    fn new(envelope: QueuedRemoteEnvelope) -> Self {
        match envelope {
            QueuedRemoteEnvelope::Send(send) => Self::Send(SendableQueuedRemoteSend::new(
                send.send,
                send.call_context,
                send.cause,
            )),
            QueuedRemoteEnvelope::CallReply(reply) => {
                Self::CallReply(SendableRemoteCallReply::new(reply))
            }
        }
    }

    fn into_queued_remote_envelope(self) -> QueuedRemoteEnvelope {
        match self {
            Self::Send(send) => QueuedRemoteEnvelope::Send(send.into_queued_remote_send()),
            Self::CallReply(reply) => {
                QueuedRemoteEnvelope::CallReply(reply.into_remote_call_reply())
            }
        }
    }
}

struct RemoteCallReply {
    call_id: CallId,
    requester: RegisteredAddress,
    cause: CauseId,
    outcome: RemoteCallOutcome,
}

enum RemoteCallOutcome {
    Replied(ErasedMessage),
    Full,
    Closed,
    Rejected(CallRejectedReason),
}

struct SendableRemoteCallReply {
    call_id: CallId,
    requester: RegisteredAddress,
    cause: CauseId,
    outcome: SendableRemoteCallOutcome,
}

impl SendableRemoteCallReply {
    fn new(reply: RemoteCallReply) -> Self {
        match reply.outcome {
            RemoteCallOutcome::Replied(message) => Self {
                call_id: reply.call_id,
                requester: reply.requester,
                cause: reply.cause,
                outcome: SendableRemoteCallOutcome::Replied(message.into_sendable()),
            },
            RemoteCallOutcome::Full => Self {
                call_id: reply.call_id,
                requester: reply.requester,
                cause: reply.cause,
                outcome: SendableRemoteCallOutcome::Full,
            },
            RemoteCallOutcome::Closed => Self {
                call_id: reply.call_id,
                requester: reply.requester,
                cause: reply.cause,
                outcome: SendableRemoteCallOutcome::Closed,
            },
            RemoteCallOutcome::Rejected(reason) => Self {
                call_id: reply.call_id,
                requester: reply.requester,
                cause: reply.cause,
                outcome: SendableRemoteCallOutcome::Rejected(reason),
            },
        }
    }

    fn into_remote_call_reply(self) -> RemoteCallReply {
        let outcome = match self.outcome {
            SendableRemoteCallOutcome::Replied(reply) => {
                RemoteCallOutcome::Replied(ErasedMessage::Sendable(reply))
            }
            SendableRemoteCallOutcome::Full => RemoteCallOutcome::Full,
            SendableRemoteCallOutcome::Closed => RemoteCallOutcome::Closed,
            SendableRemoteCallOutcome::Rejected(reason) => RemoteCallOutcome::Rejected(reason),
        };
        RemoteCallReply {
            call_id: self.call_id,
            requester: self.requester,
            cause: self.cause,
            outcome,
        }
    }
}

enum SendableRemoteCallOutcome {
    Replied(Box<dyn Any + Send>),
    Full,
    Closed,
    Rejected(CallRejectedReason),
}

pub(crate) enum ErasedMessage {
    Local(Box<dyn Any>),
    Sendable(Box<dyn Any + Send>),
}

impl ErasedMessage {
    fn into_any(self) -> Box<dyn Any> {
        match self {
            Self::Local(message) => message,
            Self::Sendable(message) => message,
        }
    }

    fn payload_type_id(&self) -> std::any::TypeId {
        match self {
            Self::Local(message) => (**message).type_id(),
            Self::Sendable(message) => (**message).type_id(),
        }
    }

    fn into_sendable(self) -> Box<dyn Any + Send> {
        match self {
            Self::Local(_) => {
                panic!("live cross-shard send attempted to move a non-Send message")
            }
            Self::Sendable(message) => message,
        }
    }
}

impl<S, F> IntoErasedSpawn<S, F> for std::convert::Infallible
where
    S: Shard,
    F: MailboxFactory,
{
    fn into_erased_spawn(self) -> Box<dyn ErasedSpawn<S, F>> {
        match self {}
    }
}

impl<S, F, ParentMessage> IntoErasedSpawnObserved<S, F, ParentMessage> for std::convert::Infallible
where
    S: Shard,
    F: MailboxFactory,
{
    fn into_erased_spawn_observed(self) -> Box<dyn ErasedSpawnObserved<S, F>> {
        match self {}
    }
}

struct SpawnObservedAdapter<Spawn, ParentMessage, ChildMessage, ChildReply> {
    inner: tina::SpawnObserved<Spawn, ParentMessage, ChildMessage, ChildReply>,
}

impl<Spawn, ParentMessage, ChildMessage, ChildReply, S, F> ErasedSpawnObserved<S, F>
    for SpawnObservedAdapter<Spawn, ParentMessage, ChildMessage, ChildReply>
where
    Spawn: IntoErasedSpawn<S, F> + 'static,
    ParentMessage: 'static,
    ChildMessage: 'static,
    ChildReply: 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn spawn_observed(
        self: Box<Self>,
        runtime: &mut Runtime<S, F>,
        parent: IsolateId,
    ) -> SpawnObservedOutcome<S, F> {
        let (spawn, continuation) = self.inner.into_parts();
        match spawn
            .into_erased_spawn()
            .try_spawn_observed(runtime, parent)
        {
            Ok(outcome) => {
                let child_address = Address::<ChildMessage, ChildReply>::new_with_generation(
                    outcome.child.shard,
                    outcome.child.isolate,
                    outcome.child.generation,
                );
                let child_ref = ChildRef::new(child_address);
                let message = continuation(Ok(child_ref));
                SpawnObservedOutcome {
                    spawn: Some(outcome),
                    continuation: Some(ErasedMessage::Local(Box::new(message))),
                }
            }
            Err(error) => {
                let message = continuation(Err(error));
                SpawnObservedOutcome {
                    spawn: None,
                    continuation: Some(ErasedMessage::Local(Box::new(message))),
                }
            }
        }
    }
}

impl<Spawn, ParentMessage, ChildMessage, ChildReply, S, F>
    IntoErasedSpawnObserved<S, F, ParentMessage>
    for tina::SpawnObserved<Spawn, ParentMessage, ChildMessage, ChildReply>
where
    Spawn: IntoErasedSpawn<S, F> + 'static,
    ParentMessage: 'static,
    ChildMessage: 'static,
    ChildReply: 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn into_erased_spawn_observed(self) -> Box<dyn ErasedSpawnObserved<S, F>> {
        Box::new(SpawnObservedAdapter { inner: self })
    }
}

struct SpawnAdapter<I, Outbound>
where
    I: Isolate,
{
    isolate: I,
    mailbox_capacity: usize,
    bootstrap_message: Option<I::Message>,
    marker: PhantomData<fn(Outbound) -> Outbound>,
}

impl<I, S, F, Outbound> ErasedSpawn<S, F> for SpawnAdapter<I, Outbound>
where
    I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
    I::Message: 'static,
    I::Reply: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
    I::Call: IntoErasedCall<I::Message> + 'static,
    I::Fact: IntoRuntimeFact + 'static,
    Outbound: 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn spawn(
        self: Box<Self>,
        runtime: &mut Runtime<S, F>,
        parent: IsolateId,
    ) -> SpawnOutcome<S, F> {
        runtime.spawn_isolate::<I, Outbound>(
            parent,
            self.isolate,
            self.mailbox_capacity,
            self.bootstrap_message,
        )
    }

    fn try_spawn_observed(
        self: Box<Self>,
        runtime: &mut Runtime<S, F>,
        parent: IsolateId,
    ) -> Result<SpawnOutcome<S, F>, SpawnObservedError> {
        if self.mailbox_capacity == 0 {
            return Err(SpawnObservedError::ZeroMailboxCapacity);
        }
        Ok(self.spawn(runtime, parent))
    }
}

impl<I, S, F, OutboundMsg> IntoErasedSpawn<S, F> for tina::ChildDefinition<I>
where
    I: Isolate<Shard = S, Send = TinaOutbound<OutboundMsg>> + 'static,
    I::Message: 'static,
    I::Reply: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
    I::Call: IntoErasedCall<I::Message> + 'static,
    I::Fact: IntoRuntimeFact + 'static,
    OutboundMsg: 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn into_erased_spawn(self) -> Box<dyn ErasedSpawn<S, F>> {
        let (isolate, mailbox_capacity, bootstrap_message) = self.into_parts();
        Box::new(SpawnAdapter::<I, OutboundMsg> {
            isolate,
            mailbox_capacity,
            bootstrap_message,
            marker: PhantomData,
        })
    }
}

struct RestartableSpawnAdapter<I, Outbound>
where
    I: Isolate,
{
    factory: Box<dyn Fn() -> I>,
    mailbox_capacity: usize,
    bootstrap_factory: Option<Box<dyn Fn() -> I::Message>>,
    marker: PhantomData<fn(Outbound) -> Outbound>,
}

impl<I, S, F, Outbound> ErasedSpawn<S, F> for RestartableSpawnAdapter<I, Outbound>
where
    I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
    I::Message: 'static,
    I::Reply: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
    I::Call: IntoErasedCall<I::Message> + 'static,
    I::Fact: IntoRuntimeFact + 'static,
    Outbound: 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn spawn(
        self: Box<Self>,
        runtime: &mut Runtime<S, F>,
        parent: IsolateId,
    ) -> SpawnOutcome<S, F> {
        let adapter = Rc::new(*self);
        let isolate = (adapter.factory)();
        let mailbox_capacity = adapter.mailbox_capacity;
        let bootstrap_message = adapter.bootstrap_factory.as_ref().map(|f| f());
        let mut outcome = runtime.spawn_isolate::<I, Outbound>(
            parent,
            isolate,
            mailbox_capacity,
            bootstrap_message,
        );
        outcome.restart_recipe = Some(adapter);
        outcome
    }

    fn try_spawn_observed(
        self: Box<Self>,
        runtime: &mut Runtime<S, F>,
        parent: IsolateId,
    ) -> Result<SpawnOutcome<S, F>, SpawnObservedError> {
        if self.mailbox_capacity == 0 {
            return Err(SpawnObservedError::ZeroMailboxCapacity);
        }
        Ok(self.spawn(runtime, parent))
    }
}

impl<I, S, F, Outbound> ErasedRestartRecipe<S, F> for RestartableSpawnAdapter<I, Outbound>
where
    I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
    I::Message: 'static,
    I::Reply: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
    I::Call: IntoErasedCall<I::Message> + 'static,
    I::Fact: IntoRuntimeFact + 'static,
    Outbound: 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn create(&self, runtime: &mut Runtime<S, F>, parent: IsolateId) -> SpawnOutcome<S, F> {
        let isolate = (self.factory)();
        let bootstrap_message = self.bootstrap_factory.as_ref().map(|f| f());
        runtime.spawn_isolate::<I, Outbound>(
            parent,
            isolate,
            self.mailbox_capacity,
            bootstrap_message,
        )
    }
}

impl<I, S, F, OutboundMsg> IntoErasedSpawn<S, F> for tina::RestartableChildDefinition<I>
where
    I: Isolate<Shard = S, Send = TinaOutbound<OutboundMsg>> + 'static,
    I::Message: 'static,
    I::Reply: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
    I::Call: IntoErasedCall<I::Message> + 'static,
    I::Fact: IntoRuntimeFact + 'static,
    OutboundMsg: 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn into_erased_spawn(self) -> Box<dyn ErasedSpawn<S, F>> {
        let (factory, mailbox_capacity, bootstrap_factory) = self.into_parts();
        Box::new(RestartableSpawnAdapter::<I, OutboundMsg> {
            factory,
            mailbox_capacity,
            bootstrap_factory,
            marker: PhantomData,
        })
    }
}

#[cfg(test)]
mod tests;
