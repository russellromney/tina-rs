#![forbid(unsafe_code)]
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
use std::collections::{BTreeMap, VecDeque};
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tina::{
    Address, AddressGeneration, ChildRelation, Context, Effect, Isolate, IsolateId, Mailbox,
    Outbound as TinaOutbound, RestartBudgetState, Shard, ShardId, TrySendError,
};
use tina_supervisor::SupervisorConfig;

use betelgeuse::{IOLoopHandle, io_loop};

mod call;
mod driver;
pub mod persistence;
mod trace;

pub use crate::persistence::{
    LOCAL_PERSISTENCE_SUPPORT, LocalPersistenceSupport, PersistenceSupportLevel,
};
pub use call::{
    CallError, CallId, CallInput, CallOutcome, CallOutput, ErasedCall, FileId, FileOpenOptions,
    IntoErasedCall, IsolateCall, JournalRecord, JournalReplay, JournalReplayWarning, ListenerId,
    PathKind, PathMetadata, PersistenceTraceInfo, ProcessRunResult, ProcessStatus, RuntimeCall,
    RuntimeCallParts, SendOutcome, SnapshotImage, StreamId, TlsStreamId, TypedCall, UdpSocketId,
    call, dns_lookup, file_close, file_create, file_fsync, file_open, file_read, file_read_at,
    file_size, file_write, file_write_at, journal_append, journal_replay, mkdir, path_metadata,
    process_run, read_dir, remove_file, rename_replace, send_observed, signal_wait, sleep,
    sleep_then, snapshot_commit, snapshot_load, sync_parent, tcp_accept, tcp_bind,
    tcp_close_listener, tcp_close_stream, tcp_connect, tcp_read, tcp_write, tls_close, tls_connect,
    tls_read, tls_write, udp_bind, udp_close_socket, udp_recv_from, udp_send_to,
};
use driver::DriverCompletion;
/// Declares a Tina isolate whose call channel defaults to [`RuntimeCall<Message>`](RuntimeCall).
///
/// This is the preferred runtime authoring path. It keeps the handler as normal
/// Rust code and only fills the repetitive [`tina::Isolate`] associated types.
pub use tina_macros::runtime_isolate as isolate;
pub use trace::{
    CallCompletionRejectedReason, CallKind, CallReplyRejectedReason, CauseId, EffectKind, EventId,
    RestartSkippedReason, RuntimeEvent, RuntimeEventKind, SendRejectedReason,
    SupervisionRejectedReason,
};

use driver::{BetelgeuseDriver, DriverShutdownError, RuntimeDriver};

/// Runtime-owned mailbox factory for registered and spawned isolate mailboxes.
///
/// The factory lives in `tina-runtime`, not in `tina`, because child
/// mailbox allocation is a runtime concern rather than a trait-crate concern.
pub trait MailboxFactory {
    /// Creates one mailbox with the requested capacity.
    ///
    /// The type parameter is the runtime's storage type for that mailbox.
    /// Runtime-created isolate mailboxes may store erased message boxes
    /// internally even when user code handles a concrete message type.
    /// User-provided mailboxes passed to [`Runtime::register`] remain typed
    /// by the isolate message type.
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>>;
}

/// Requirement level for Tina's driver-runtime contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverRuntimeRequirement {
    /// A Tina-shaped driver runtime must provide this.
    Required,
    /// A Tina-shaped driver runtime must not provide this behind Tina's back.
    Forbidden,
    /// This is useful for some backend implementations but not part of the
    /// core Tina contract.
    BackendSpecific,
    /// Tina makes no claim for this capability.
    NotClaimed,
}

/// The substrate contract Tina wants underneath a shard-local runner.
///
/// This is not a claim that Tina is a general Rust async runtime. It names the
/// smaller thing Tina needs: completion-shaped I/O that the Tina runner owns,
/// advances, cancels, and can simulate deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TinaDriverRuntimeContract {
    /// I/O completes by writing into caller-owned completion state.
    pub completion_based_io: DriverRuntimeRequirement,
    /// Runtime ingress and cross-thread commands stay bounded.
    pub bounded_runtime_commands: DriverRuntimeRequirement,
    /// Pending work can be explicitly canceled by runtime-owned call id.
    pub explicit_cancellation: DriverRuntimeRequirement,
    /// Shutdown owns the driver lifecycle and proves completion storage release.
    pub owned_shutdown: DriverRuntimeRequirement,
    /// Driver progress is advanced by the Tina runner, not hidden tasks.
    pub explicit_progress: DriverRuntimeRequirement,
    /// A deterministic simulated backend exists for DST and replay.
    pub deterministic_simulation: DriverRuntimeRequirement,
    /// Hidden executor tasks inside the driver are forbidden.
    pub hidden_executor_tasks: DriverRuntimeRequirement,
    /// A general async/futures executor is outside this contract.
    pub general_async_executor: DriverRuntimeRequirement,
}

/// Tina's current driver-runtime contract target.
pub const TINA_DRIVER_RUNTIME_CONTRACT: TinaDriverRuntimeContract = TinaDriverRuntimeContract {
    completion_based_io: DriverRuntimeRequirement::Required,
    bounded_runtime_commands: DriverRuntimeRequirement::Required,
    explicit_cancellation: DriverRuntimeRequirement::Required,
    owned_shutdown: DriverRuntimeRequirement::Required,
    explicit_progress: DriverRuntimeRequirement::Required,
    deterministic_simulation: DriverRuntimeRequirement::Required,
    hidden_executor_tasks: DriverRuntimeRequirement::Forbidden,
    general_async_executor: DriverRuntimeRequirement::NotClaimed,
};

/// Support status for one runtime-owned resource family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceSupport {
    /// Tina can execute this resource family on the current live runtime.
    Supported,
    /// Tina cannot honestly execute this resource family on the current live runtime.
    Unsupported,
    /// Tina can model this resource family in the deterministic simulator only.
    SimulatedOnly,
    /// Tina expects an explicit user adapter or service isolate for this family.
    AdapterOnly,
}

/// Execution shape for one resource family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceExecutionShape {
    /// Small runtime bookkeeping completes inline.
    Inline,
    /// Work completes through caller-owned completion slots.
    CompletionBacked,
    /// Nonblocking resource work is polled by the Tina driver step.
    PollBacked,
    /// Blocking work runs on a bounded named lane, away from shard handlers.
    LaneBackedBlocking,
    /// Work is delegated to an explicit user adapter or service isolate.
    ExternalAdapter,
    /// No execution shape exists because the resource is unsupported.
    NotApplicable,
}

/// Cancellation shape for one resource family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancellationSupport {
    /// Accepted work can be canceled before it starts.
    CancelableBeforeStart,
    /// Started work may finish, but its completion is tombstoned.
    TombstonedAfterStart,
    /// Cancellation requires closing the owned resource.
    ResourceCloseOnly,
    /// Tina cannot cancel this resource family on this runtime.
    NotCancelable,
    /// No cancellation shape exists because the resource is unsupported.
    NotApplicable,
}

/// Shutdown shape for one resource family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownSupport {
    /// Shutdown drains pending work to a settled terminal state.
    Drained,
    /// Shutdown cancels pending work.
    Canceled,
    /// Shutdown tombstones late completions.
    Tombstoned,
    /// Shutdown cannot manage this resource family.
    Unsupported,
    /// No shutdown shape exists because the resource is unsupported.
    NotApplicable,
}

/// Capability report for one runtime-owned resource family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceCapability {
    support: ResourceSupport,
    execution: ResourceExecutionShape,
    cancellation: CancellationSupport,
    shutdown: ShutdownSupport,
    capacity: Option<usize>,
}

impl ResourceCapability {
    /// Creates one capability row.
    pub const fn new(
        support: ResourceSupport,
        execution: ResourceExecutionShape,
        cancellation: CancellationSupport,
        shutdown: ShutdownSupport,
        capacity: Option<usize>,
    ) -> Self {
        Self {
            support,
            execution,
            cancellation,
            shutdown,
            capacity,
        }
    }

    /// Resource support status.
    pub const fn support(&self) -> ResourceSupport {
        self.support
    }

    /// Resource execution shape.
    pub const fn execution(&self) -> ResourceExecutionShape {
        self.execution
    }

    /// Resource cancellation shape.
    pub const fn cancellation(&self) -> CancellationSupport {
        self.cancellation
    }

    /// Resource shutdown shape.
    pub const fn shutdown(&self) -> ShutdownSupport {
        self.shutdown
    }

    /// Configured bounded capacity, when this resource owns a bounded lane.
    pub const fn capacity(&self) -> Option<usize> {
        self.capacity
    }
}

/// Durability capability details for local persistence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurabilityCapability {
    /// Whether snapshot commits write a temp file before rename.
    pub temp_write_before_rename: PersistenceSupportLevel,
    /// Whether snapshot commit rename replacement is claimed on this platform.
    pub rename_commit: PersistenceSupportLevel,
    /// Whether data file fsync is claimed on this platform.
    pub file_fsync: PersistenceSupportLevel,
    /// Whether parent-directory fsync after rename is claimed on this platform.
    pub directory_fsync_after_rename: PersistenceSupportLevel,
    /// Whether commit-uncertain is a possible visible outcome.
    pub commit_uncertain_possible: bool,
    /// Whether journal replay validates checksums.
    pub checksum_validation: PersistenceSupportLevel,
    /// Whether truncated journal tails are visible warnings.
    pub truncated_tail_warning: PersistenceSupportLevel,
}

impl DurabilityCapability {
    const fn local() -> Self {
        Self {
            temp_write_before_rename: LOCAL_PERSISTENCE_SUPPORT.temp_write_before_rename,
            rename_commit: LOCAL_PERSISTENCE_SUPPORT.rename_commit,
            file_fsync: LOCAL_PERSISTENCE_SUPPORT.file_fsync,
            directory_fsync_after_rename: LOCAL_PERSISTENCE_SUPPORT.directory_fsync_after_rename,
            commit_uncertain_possible: true,
            checksum_validation: LOCAL_PERSISTENCE_SUPPORT.checksum_validation,
            truncated_tail_warning: LOCAL_PERSISTENCE_SUPPORT.truncated_tail_warning,
        }
    }
}

/// Structured live-runtime capability table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeCapabilities {
    /// Runtime-owned timer support.
    pub timers: ResourceCapability,
    /// Runtime-owned TCP support.
    pub tcp: ResourceCapability,
    /// Runtime-owned local file support.
    pub local_file: ResourceCapability,
    /// Runtime-owned local persistence support.
    pub local_persistence: ResourceCapability,
    /// Shared storage lane used by blocking storage and persistence work.
    pub storage_lane: ResourceCapability,
    /// Runtime-owned DNS support.
    pub dns: ResourceCapability,
    /// Runtime-owned UDP support.
    pub udp: ResourceCapability,
    /// Runtime-owned TLS support.
    pub tls: ResourceCapability,
    /// Runtime-owned process support.
    pub process: ResourceCapability,
    /// Runtime-owned signal support.
    pub signal: ResourceCapability,
    /// Platform durability support details.
    pub durability: DurabilityCapability,
}

impl RuntimeCapabilities {
    /// Returns the current live [`ThreadedRuntime`] capability table.
    pub const fn threaded(storage_lane_capacity: usize) -> Self {
        Self {
            timers: ResourceCapability::new(
                ResourceSupport::Supported,
                ResourceExecutionShape::Inline,
                CancellationSupport::CancelableBeforeStart,
                ShutdownSupport::Canceled,
                None,
            ),
            tcp: ResourceCapability::new(
                ResourceSupport::Supported,
                ResourceExecutionShape::CompletionBacked,
                CancellationSupport::TombstonedAfterStart,
                ShutdownSupport::Tombstoned,
                None,
            ),
            local_file: ResourceCapability::new(
                ResourceSupport::Supported,
                ResourceExecutionShape::CompletionBacked,
                CancellationSupport::TombstonedAfterStart,
                ShutdownSupport::Tombstoned,
                None,
            ),
            local_persistence: ResourceCapability::new(
                ResourceSupport::Supported,
                ResourceExecutionShape::LaneBackedBlocking,
                CancellationSupport::TombstonedAfterStart,
                ShutdownSupport::Tombstoned,
                Some(storage_lane_capacity),
            ),
            storage_lane: ResourceCapability::new(
                ResourceSupport::Supported,
                ResourceExecutionShape::LaneBackedBlocking,
                CancellationSupport::TombstonedAfterStart,
                ShutdownSupport::Tombstoned,
                Some(storage_lane_capacity),
            ),
            dns: ResourceCapability::new(
                ResourceSupport::Supported,
                ResourceExecutionShape::LaneBackedBlocking,
                CancellationSupport::TombstonedAfterStart,
                ShutdownSupport::Tombstoned,
                Some(driver::DEFAULT_DNS_LANE_CAPACITY),
            ),
            udp: ResourceCapability::new(
                ResourceSupport::Supported,
                ResourceExecutionShape::PollBacked,
                CancellationSupport::CancelableBeforeStart,
                ShutdownSupport::Canceled,
                None,
            ),
            tls: ResourceCapability::new(
                ResourceSupport::Supported,
                ResourceExecutionShape::LaneBackedBlocking,
                CancellationSupport::TombstonedAfterStart,
                ShutdownSupport::Tombstoned,
                Some(driver::DEFAULT_TLS_LANE_CAPACITY),
            ),
            process: ResourceCapability::new(
                ResourceSupport::Supported,
                ResourceExecutionShape::LaneBackedBlocking,
                CancellationSupport::TombstonedAfterStart,
                ShutdownSupport::Tombstoned,
                Some(driver::DEFAULT_PROCESS_LANE_CAPACITY),
            ),
            signal: ResourceCapability::new(
                ResourceSupport::Supported,
                ResourceExecutionShape::PollBacked,
                CancellationSupport::CancelableBeforeStart,
                ShutdownSupport::Drained,
                Some(driver::DEFAULT_SIGNAL_CAPACITY),
            ),
            durability: DurabilityCapability::local(),
        }
    }
}

/// Runtime-owned clock abstraction.
///
/// Production uses a monotonic wall-clock. Tests may inject a manual clock
/// so timer behavior can be driven deterministically without real sleeps.
trait Clock: std::fmt::Debug {
    fn now(&self) -> Instant;
}

#[derive(Debug)]
struct MonotonicClock;

impl Clock for MonotonicClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[cfg(test)]
#[derive(Debug)]
struct ManualClock {
    base: Instant,
    offset: Cell<Duration>,
}

#[cfg(test)]
impl ManualClock {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            offset: Cell::new(Duration::ZERO),
        }
    }

    fn advance(&self, by: Duration) {
        self.offset.set(self.offset.get() + by);
    }
}

#[cfg(test)]
impl Clock for ManualClock {
    fn now(&self) -> Instant {
        self.base + self.offset.get()
    }
}

#[cfg(test)]
impl Clock for Rc<ManualClock> {
    fn now(&self) -> Instant {
        self.base + self.offset.get()
    }
}

#[derive(Debug, Clone, Copy)]
struct MessageCallContext {
    call_id: CallId,
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
}

#[derive(Debug)]
struct InFlightCall {
    call_id: CallId,
    call_kind: CallKind,
    requester: RegisteredAddress,
    cause: CauseId,
    persistence: Option<call::PersistenceTraceInfo>,
}

type ErasedTranslator = Box<dyn FnOnce(CallOutput) -> Box<dyn Any>>;
type ErasedIsolateCallTranslator = Box<dyn FnOnce(CallOutcome<Box<dyn Any>>) -> Box<dyn Any>>;

const INITIAL_ENTRY_CAPACITY: usize = 8;
const INITIAL_CHILD_RECORD_CAPACITY: usize = 8;
const INITIAL_SUPERVISOR_CAPACITY: usize = 4;
const INITIAL_TRACE_CAPACITY: usize = 256;
const INITIAL_CALL_CAPACITY: usize = 8;

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
    translator: Option<ErasedIsolateCallTranslator>,
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
        Self {
            shard,
            mailbox_factory,
            entries: Vec::with_capacity(INITIAL_ENTRY_CAPACITY),
            child_records: Vec::with_capacity(INITIAL_CHILD_RECORD_CAPACITY),
            supervisors: Vec::with_capacity(INITIAL_SUPERVISOR_CAPACITY),
            next_isolate_id: 1,
            ids,
            trace: Vec::with_capacity(INITIAL_TRACE_CAPACITY),
            trace_retention: TraceRetention::Full,
            trace_dropped: 0,
            driver,
            in_flight_calls: Vec::with_capacity(INITIAL_CALL_CAPACITY),
            translators: Vec::with_capacity(INITIAL_CALL_CAPACITY),
            clock,
            pending_isolate_calls: Vec::with_capacity(INITIAL_CALL_CAPACITY),
            round_messages: Vec::with_capacity(INITIAL_ENTRY_CAPACITY),
            driver_completions: Vec::with_capacity(INITIAL_CALL_CAPACITY),
            next_isolate_call_ordinal: 0,
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

    /// Returns a shared reference to the shard.
    pub const fn shard(&self) -> &S {
        &self.shard
    }

    /// Returns the accumulated runtime trace.
    pub fn trace(&self) -> &[RuntimeEvent] {
        &self.trace
    }

    /// Returns the active trace retention policy.
    pub const fn trace_retention(&self) -> TraceRetention {
        self.trace_retention
    }

    /// Returns the number of trace events dropped by the retention policy.
    pub const fn trace_dropped(&self) -> u64 {
        self.trace_dropped
    }

    /// Sets the trace retention policy for future events.
    ///
    /// Lowering retention trims the current trace immediately so callers can
    /// rely on the memory bound after this returns.
    pub fn set_trace_retention(&mut self, retention: TraceRetention) {
        self.trace_retention = retention;
        self.enforce_trace_retention();
    }

    fn cancel_in_flight_calls_for_shutdown(&mut self) -> Result<(), DriverShutdownError> {
        self.driver.cancel_pending()?;
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
            self.push_event(
                call.requester.isolate,
                Some(call.cause),
                RuntimeEventKind::CallCompletionRejected {
                    call_id: call.call_id,
                    call_kind: CallKind::IsolateCall,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                },
            );
        }
        Ok(())
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
        I::Call: IntoErasedCall<I::Message> + 'static,
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
        I::Call: IntoErasedCall<I::Message> + 'static,
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

    /// Attempts to enqueue a typed message into one registered isolate.
    ///
    /// This is the runtime-side ingress surface for tests and later drivers.
    /// It preserves the mailbox's typed `Full` and `Closed` outcomes, while
    /// still treating unknown isolate IDs as programmer error.
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
            panic!(
                "runtime ingress targeted unknown isolate {} on shard {}",
                address.isolate().get(),
                address.shard().get(),
            );
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

    /// Configures a registered isolate as supervisor for its direct children.
    ///
    /// This is a setup-time runtime API. Unknown, stale, or cross-shard parent
    /// addresses are programmer errors and panic. Reconfiguring the same parent
    /// replaces the config and resets the runtime-lifetime budget tracker.
    pub fn supervise<M: 'static, R>(&mut self, parent: Address<M, R>, config: SupervisorConfig) {
        let parent = self.checked_registered_address(parent, "supervise");
        let budget_state = config.budget().tracker();

        if let Some(record) = self
            .supervisors
            .iter_mut()
            .find(|record| record.parent == parent)
        {
            record.config = config;
            record.budget_state = budget_state;
            return;
        }

        self.supervisors.push(SupervisorRecord {
            parent,
            config,
            budget_state,
        });
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
        self.step_with_remote(&mut |_source_shard, send, _cause| {
            panic!(
                "cross-shard send is out of scope in this slice: target shard {} != runtime shard {}",
                send.target_shard.get(),
                shard_id.get(),
            );
        })
    }

    fn step_with_remote<FR>(&mut self, route_remote: &mut FR) -> usize
    where
        FR: FnMut(ShardId, ErasedSend, CauseId) -> Result<(), SendRejectedReason>,
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

            let effect = {
                let mut handler = self.entries[index].handler.borrow_mut();
                catch_unwind(AssertUnwindSafe(|| {
                    handler.handle_boxed(message.message, &mut self.shard, isolate_id)
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

            self.execute_effect(
                index,
                handler_finished.into(),
                effect,
                message.call_context,
                &mut round_messages,
                route_remote,
            );
        }

        round_messages.clear();
        self.round_messages = round_messages;
        delivered
    }

    fn execute_effect(
        &mut self,
        index: usize,
        cause: CauseId,
        effect: ErasedEffect<S, F>,
        call_context: Option<MessageCallContext>,
        round_messages: &mut [Option<DeliveredMessage>],
        route_remote: &mut impl FnMut(ShardId, ErasedSend, CauseId) -> Result<(), SendRejectedReason>,
    ) -> bool {
        let isolate_id = self.entries[index].id;
        match effect {
            ErasedEffect::Stop => {
                self.stop_entry(index, isolate_id, cause);
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
                    route_remote(self.shard.id(), send, attempted.into())
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
                self.dispatch_call(call, requester, cause, route_remote);
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
                    if !self.complete_isolate_call(
                        context.call_id,
                        cause,
                        CallOutcome::Replied(reply.into_any()),
                    ) {
                        self.push_event(
                            isolate_id,
                            Some(cause),
                            RuntimeEventKind::CallReplyRejected {
                                call_id: context.call_id,
                                reason: CallReplyRejectedReason::NoPendingCall,
                            },
                        );
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
            ErasedEffect::Batch(effects) => {
                for subeffect in effects {
                    if self.execute_effect(
                        index,
                        cause,
                        subeffect,
                        call_context,
                        round_messages,
                        route_remote,
                    ) {
                        return true;
                    }
                }
                false
            }
        }
    }

    fn dispatch_call(
        &mut self,
        call: ErasedCall,
        requester: RegisteredAddress,
        cause: CauseId,
        route_remote: &mut impl FnMut(ShardId, ErasedSend, CauseId) -> Result<(), SendRejectedReason>,
    ) {
        let call_id = self.ids.next_call_id();
        let call_kind = match &call.kind {
            call::ErasedCallKind::Backend { request, .. } => request.kind(),
            call::ErasedCallKind::ObservedSend { .. } => CallKind::ObservedSend,
            call::ErasedCallKind::IsolateCall { .. } => CallKind::IsolateCall,
        };

        let attempted = self.push_event(
            requester.isolate,
            Some(cause),
            RuntimeEventKind::CallDispatchAttempted { call_id, call_kind },
        );

        match call.kind {
            call::ErasedCallKind::Backend {
                request,
                translator,
            } => {
                self.dispatch_driver_call(
                    call_id,
                    call_kind,
                    requester,
                    attempted.into(),
                    request,
                    translator,
                );
            }
            call::ErasedCallKind::ObservedSend { send, translator } => {
                self.dispatch_observed_send(
                    call_id,
                    requester,
                    attempted.into(),
                    send,
                    translator,
                    route_remote,
                );
            }
            call::ErasedCallKind::IsolateCall {
                send,
                timeout,
                translator,
            } => {
                self.dispatch_isolate_call(
                    call_id,
                    requester,
                    attempted.into(),
                    send,
                    timeout,
                    translator,
                );
            }
        }
    }

    fn dispatch_driver_call(
        &mut self,
        call_id: CallId,
        call_kind: CallKind,
        requester: RegisteredAddress,
        cause: CauseId,
        request: CallInput,
        translator: Box<dyn FnOnce(CallOutput) -> Box<dyn Any>>,
    ) {
        let persistence = request.persistence_trace_info();
        if persistence == Some(call::PersistenceTraceInfo::Recovery) {
            self.push_event(
                requester.isolate,
                Some(cause),
                RuntimeEventKind::RecoveryStarted,
            );
        }
        // Register the translator and in-flight tracking before submission
        // so a synchronous completion (bind / close on Betelgeuse) can be
        // delivered through the same path as async completions.
        self.in_flight_calls.push(InFlightCall {
            call_id,
            call_kind,
            requester,
            cause,
            persistence,
        });
        self.translators.push(StoredTranslator {
            call_id,
            translator: Some(translator),
        });

        if let Some(immediate) = self.driver.submit(call_id, request, self.clock.now()) {
            self.deliver_completion(immediate.call_id, immediate.result);
        }
    }

    fn dispatch_observed_send(
        &mut self,
        call_id: CallId,
        requester: RegisteredAddress,
        cause: CauseId,
        send: ErasedSend,
        translator: Box<dyn FnOnce(SendOutcome) -> Box<dyn Any>>,
        route_remote: &mut impl FnMut(ShardId, ErasedSend, CauseId) -> Result<(), SendRejectedReason>,
    ) {
        let target_shard = send.target_shard;
        let target_isolate = send.target_isolate;
        let target_generation = send.target_generation;
        let send_attempted = self.push_event(
            requester.isolate,
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
            route_remote(self.shard.id(), send, send_attempted.into())
        };

        let outcome = match delivery {
            Ok(()) => {
                self.push_event(
                    requester.isolate,
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
                    requester.isolate,
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

        self.deliver_observed_send_outcome(call_id, requester, cause, outcome, translator);
    }

    fn deliver_observed_send_outcome(
        &mut self,
        call_id: CallId,
        requester: RegisteredAddress,
        cause: CauseId,
        outcome: SendOutcome,
        translator: Box<dyn FnOnce(SendOutcome) -> Box<dyn Any>>,
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

        match self.enqueue_entry_message(entry_index, message, None) {
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

    fn dispatch_isolate_call(
        &mut self,
        call_id: CallId,
        requester: RegisteredAddress,
        cause: CauseId,
        send: ErasedSend,
        timeout: Duration,
        translator: ErasedIsolateCallTranslator,
    ) {
        let target_shard = send.target_shard;
        let target_isolate = send.target_isolate;
        let target_generation = send.target_generation;
        let send_attempted = self.push_event(
            requester.isolate,
            Some(cause),
            RuntimeEventKind::SendDispatchAttempted {
                target_shard,
                target_isolate,
                target_generation,
            },
        );

        if target_shard != self.shard.id() {
            self.push_event(
                requester.isolate,
                Some(send_attempted.into()),
                RuntimeEventKind::SendRejected {
                    target_shard,
                    target_isolate,
                    target_generation,
                    reason: SendRejectedReason::Closed,
                },
            );
            self.deliver_isolate_call_outcome(
                call_id,
                requester,
                cause,
                CallOutcome::Closed,
                translator,
            );
            return;
        }

        let delivery =
            self.dispatch_local_send_with_context(send, Some(MessageCallContext { call_id }));

        match delivery {
            Ok(()) => {
                self.push_event(
                    requester.isolate,
                    Some(send_attempted.into()),
                    RuntimeEventKind::SendAccepted {
                        target_shard,
                        target_isolate,
                        target_generation,
                    },
                );
                let insertion_order = self.next_isolate_call_ordinal;
                self.next_isolate_call_ordinal += 1;
                self.pending_isolate_calls.push(PendingIsolateCall {
                    call_id,
                    requester,
                    cause,
                    deadline: self.clock.now() + timeout,
                    insertion_order,
                    translator: Some(translator),
                });
            }
            Err(reason) => {
                self.push_event(
                    requester.isolate,
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
                self.deliver_isolate_call_outcome(call_id, requester, cause, outcome, translator);
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
            self.deliver_isolate_call_outcome(
                entry.call_id,
                entry.requester,
                entry.cause,
                CallOutcome::Timeout,
                translator,
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
        self.deliver_isolate_call_outcome(call_id, pending.requester, cause, outcome, translator);
        true
    }

    fn deliver_isolate_call_outcome(
        &mut self,
        call_id: CallId,
        requester: RegisteredAddress,
        cause: CauseId,
        outcome: CallOutcome<Box<dyn Any>>,
        translator: ErasedIsolateCallTranslator,
    ) {
        let failure_reason = match &outcome {
            CallOutcome::Replied(_) => None,
            CallOutcome::Full => Some(CallError::TargetFull),
            CallOutcome::Closed => Some(CallError::TargetClosed),
            CallOutcome::Timeout => Some(CallError::Timeout),
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

        match self.enqueue_entry_message(entry_index, message, None) {
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

        match self.enqueue_entry_message(entry_index, message, None) {
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
        self.stop_entry_with_precollected(index, isolate_id, cause, None)
    }

    fn stop_entry_with_precollected(
        &mut self,
        index: usize,
        isolate_id: IsolateId,
        cause: CauseId,
        precollected: Option<DeliveredMessage>,
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
            return stopped;
        }

        self.entries[index].stopped.set(true);
        self.entries[index].mailbox.close();
        let stopped = self.push_event(isolate_id, Some(cause), RuntimeEventKind::IsolateStopped);
        self.entries[index].stopped_event.set(Some(stopped));
        self.cancel_driver_calls_for_requester(RegisteredAddress {
            shard: self.shard.id(),
            isolate: isolate_id,
            generation: self.entries[index].generation,
        });
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
        let budget_state = match budget_state.record_restart() {
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
    }

    fn push_event(
        &mut self,
        isolate: IsolateId,
        cause: Option<CauseId>,
        kind: RuntimeEventKind,
    ) -> EventId {
        let id = self.ids.next_event_id();
        match self.trace_retention {
            TraceRetention::Full => {
                self.trace
                    .push(RuntimeEvent::new(id, cause, self.shard.id(), isolate, kind));
            }
            TraceRetention::Bounded(capacity) if capacity > 0 => {
                if self.trace.len() == capacity {
                    self.trace.remove(0);
                    self.trace_dropped += 1;
                }
                self.trace
                    .push(RuntimeEvent::new(id, cause, self.shard.id(), isolate, kind));
            }
            TraceRetention::Bounded(_) | TraceRetention::Off => {
                self.trace_dropped += 1;
            }
        }
        id
    }

    fn enforce_trace_retention(&mut self) {
        match self.trace_retention {
            TraceRetention::Full => {}
            TraceRetention::Bounded(capacity) => {
                if self.trace.len() > capacity {
                    let excess = self.trace.len() - capacity;
                    self.trace.drain(0..excess);
                    self.trace_dropped += excess as u64;
                }
            }
            TraceRetention::Off => {
                self.trace_dropped += self.trace.len() as u64;
                self.trace.clear();
            }
        }
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
            panic!(
                "send targeted unknown isolate {} on shard {}",
                send.target_isolate.get(),
                send.target_shard.get(),
            );
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

    fn harvest_remote_send(&mut self, queued: QueuedRemoteSend) {
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
            return;
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
            return;
        }

        match self.enqueue_entry_message(entry_index, send.message.into_any(), None) {
            Ok(()) => {
                self.push_event(
                    send.target_isolate,
                    Some(queued.cause),
                    RuntimeEventKind::MailboxAccepted,
                );
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
            }
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

    fn checked_registered_address<M: 'static, R>(
        &self,
        address: Address<M, R>,
        operation: &str,
    ) -> RegisteredAddress {
        if address.shard() != self.shard.id() {
            panic!(
                "{operation} targeted a parent on another shard: target shard {} != runtime shard {}",
                address.shard().get(),
                self.shard.id().get(),
            );
        }

        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.id == address.isolate())
        else {
            panic!(
                "{operation} targeted unknown parent isolate {} on shard {}",
                address.isolate().get(),
                address.shard().get(),
            );
        };

        if entry.generation != address.generation() {
            panic!(
                "{operation} targeted stale parent isolate {} generation {} on shard {}",
                address.isolate().get(),
                address.generation().get(),
                address.shard().get(),
            );
        }

        RegisteredAddress {
            shard: address.shard(),
            isolate: address.isolate(),
            generation: address.generation(),
        }
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
        I::Call: IntoErasedCall<I::Message> + 'static,
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
        I::Call: IntoErasedCall<I::Message> + 'static,
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
        I::Call: IntoErasedCall<I::Message> + 'static,
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
        I::Call: IntoErasedCall<I::Message> + 'static,
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

type RemoteQueueIndexes = BTreeMap<(ShardId, ShardId), usize>;
type RemoteQueues = Vec<VecDeque<QueuedRemoteSend>>;

/// Deterministic explicit-step coordinator over a fixed set of shard runtimes.
///
/// This additive shell preserves the existing single-shard [`Runtime`] API
/// while giving Galileo one honest place to define global ingress, global
/// stepping order, and explicit root placement by shard.
pub struct MultiShardRuntime<S, F>
where
    S: Shard + 'static,
    F: MailboxFactory + 'static,
{
    runtimes: Vec<Runtime<S, F>>,
    shard_ids: Vec<ShardId>,
    shard_indexes: BTreeMap<ShardId, usize>,
    remote_queue_indexes: RemoteQueueIndexes,
    config: MultiShardRuntimeConfig,
    remote_queues: RemoteQueues,
    next_remote_queues: RemoteQueues,
}

/// Bounded coordinator config for additive multi-shard runtime shells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MultiShardRuntimeConfig {
    /// Capacity of each source-shard -> destination-shard queue.
    pub shard_pair_capacity: usize,
}

impl Default for MultiShardRuntimeConfig {
    fn default() -> Self {
        Self {
            shard_pair_capacity: 64,
        }
    }
}

impl<S, F> MultiShardRuntime<S, F>
where
    S: Shard + 'static,
    F: MailboxFactory + Clone + 'static,
{
    /// Creates one additive multi-shard coordinator over the provided shards.
    ///
    /// Shards are stepped in ascending [`ShardId`] order, regardless of input
    /// order. Empty shard sets and duplicate shard ids are programmer errors
    /// and panic.
    pub fn new<I>(shards: I, mailbox_factory: F) -> Self
    where
        I: IntoIterator<Item = S>,
    {
        Self::with_config(shards, mailbox_factory, MultiShardRuntimeConfig::default())
    }

    /// Creates one additive multi-shard coordinator with explicit shard-pair
    /// queue boundedness.
    pub fn with_config<I>(shards: I, mailbox_factory: F, config: MultiShardRuntimeConfig) -> Self
    where
        I: IntoIterator<Item = S>,
    {
        let mut shards: Vec<S> = shards.into_iter().collect();
        if shards.is_empty() {
            panic!("multi-shard runtime requires at least one shard");
        }
        if config.shard_pair_capacity == 0 {
            panic!("multi-shard runtime requires shard-pair capacity > 0");
        }

        shards.sort_by_key(Shard::id);
        for pair in shards.windows(2) {
            if pair[0].id() == pair[1].id() {
                panic!(
                    "multi-shard runtime received duplicate shard id {}",
                    pair[0].id().get()
                );
            }
        }

        let ids = IdSource::new();
        let mut runtimes = Vec::with_capacity(shards.len());
        let mut shard_ids = Vec::with_capacity(shards.len());
        let mut shard_indexes = BTreeMap::new();
        for shard in shards {
            let shard_id = shard.id();
            shard_indexes.insert(shard_id, runtimes.len());
            shard_ids.push(shard_id);
            runtimes.push(Runtime::with_clock_and_ids(
                shard,
                mailbox_factory.clone(),
                Box::new(MonotonicClock),
                ids.clone(),
            ));
        }
        let (remote_queue_indexes, remote_queues) =
            build_remote_queues(&shard_ids, config.shard_pair_capacity);
        let next_remote_queues = build_remote_queue_storage(&shard_ids, config.shard_pair_capacity);

        Self {
            runtimes,
            shard_ids,
            shard_indexes,
            remote_queue_indexes,
            config,
            remote_queues,
            next_remote_queues,
        }
    }

    /// Returns the shard ids owned by this coordinator in global step order.
    pub fn shard_ids(&self) -> Vec<ShardId> {
        self.shard_ids.clone()
    }

    /// Returns the merged deterministic event record in global event-id order.
    pub fn trace(&self) -> Vec<RuntimeEvent> {
        let mut events: Vec<_> = self
            .runtimes
            .iter()
            .flat_map(|runtime| runtime.trace().iter().copied())
            .collect();
        events.sort_by_key(|event| event.id());
        events
    }

    /// Returns whether any owned shard still has in-flight runtime-owned work.
    pub fn has_in_flight_calls(&self) -> bool {
        self.runtimes.iter().any(Runtime::has_in_flight_calls)
    }

    /// Registers one root isolate on the requested owning shard.
    #[allow(private_bounds)]
    pub fn register_on<I, M, Outbound>(
        &mut self,
        shard: ShardId,
        isolate: I,
        mailbox: M,
    ) -> Address<I::Message, I::Reply>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        Outbound: 'static,
        M: Mailbox<I::Message> + 'static,
    {
        self.runtime_mut(shard)
            .register::<I, M, Outbound>(isolate, mailbox)
    }

    /// Registers one root isolate on the requested shard and lets that shard
    /// runtime allocate the mailbox.
    #[allow(private_bounds)]
    pub fn register_with_capacity_on<I, Outbound>(
        &mut self,
        shard: ShardId,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Address<I::Message, I::Reply>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        Outbound: 'static,
    {
        self.runtime_mut(shard)
            .register_with_capacity::<I, Outbound>(isolate, mailbox_capacity)
    }

    /// Configures a registered isolate as supervisor on its owning shard.
    pub fn supervise<M: 'static, R>(&mut self, parent: Address<M, R>, config: SupervisorConfig) {
        self.runtime_mut(parent.shard()).supervise(parent, config);
    }

    /// Attempts one typed global ingress send routed strictly by target shard.
    pub fn try_send<M: 'static, R>(
        &self,
        address: Address<M, R>,
        message: M,
    ) -> Result<(), TrySendError<M>> {
        self.runtime(address.shard()).try_send(address, message)
    }

    /// Runs one global deterministic round in ascending shard-id order.
    pub fn step(&mut self) -> usize {
        std::mem::swap(&mut self.remote_queues, &mut self.next_remote_queues);
        let mut delivered = 0;
        let config = self.config;
        let shard_ids = &self.shard_ids;
        let shard_indexes = &self.shard_indexes;
        let remote_queue_indexes = &self.remote_queue_indexes;
        let remote_queues = &mut self.remote_queues;
        let next_remote_queues = &mut self.next_remote_queues;
        let runtimes = &mut self.runtimes;

        for destination in shard_ids.iter().copied() {
            let index = shard_indexes.get(&destination).copied().unwrap_or_else(|| {
                panic!(
                    "multi-shard runtime targeted unknown shard {}",
                    destination.get()
                )
            });
            for source in shard_ids.iter().copied() {
                if source == destination {
                    continue;
                }
                let key = (source, destination);
                let queue_index = remote_queue_indexes.get(&key).copied().unwrap_or_else(|| {
                    panic!(
                        "multi-shard runtime missing queue from shard {} to shard {}",
                        source.get(),
                        destination.get()
                    )
                });
                while let Some(queued) = remote_queues[queue_index].pop_front() {
                    runtimes[index].harvest_remote_send(queued);
                }
            }
            delivered += runtimes[index].step_with_remote(&mut |source_shard, send, cause| {
                if !shard_indexes.contains_key(&send.target_shard) {
                    panic!(
                        "multi-shard runtime targeted unknown destination shard {}",
                        send.target_shard.get()
                    );
                }

                let key = (source_shard, send.target_shard);
                let queue_index = remote_queue_indexes.get(&key).copied().unwrap_or_else(|| {
                    panic!(
                        "multi-shard runtime missing queue from shard {} to shard {}",
                        source_shard.get(),
                        send.target_shard.get()
                    )
                });
                let queue = &mut next_remote_queues[queue_index];
                if queue.len() >= config.shard_pair_capacity {
                    return Err(SendRejectedReason::Full);
                }
                queue.push_back(QueuedRemoteSend { send, cause });
                Ok(())
            });
        }

        delivered
    }

    fn runtime(&self, shard: ShardId) -> &Runtime<S, F> {
        &self.runtimes[self.checked_shard_index(shard)]
    }

    fn runtime_mut(&mut self, shard: ShardId) -> &mut Runtime<S, F> {
        let index = self.checked_shard_index(shard);
        &mut self.runtimes[index]
    }

    fn checked_shard_index(&self, shard: ShardId) -> usize {
        self.shard_indexes
            .get(&shard)
            .copied()
            .unwrap_or_else(|| panic!("multi-shard runtime targeted unknown shard {}", shard.get()))
    }
}

fn build_remote_queues(
    shard_ids: &[ShardId],
    shard_pair_capacity: usize,
) -> (RemoteQueueIndexes, RemoteQueues) {
    let queue_count = shard_ids
        .len()
        .saturating_mul(shard_ids.len().saturating_sub(1));
    let mut indexes = BTreeMap::new();
    let mut queues = Vec::with_capacity(queue_count);
    for source in shard_ids.iter().copied() {
        for destination in shard_ids.iter().copied() {
            if source == destination {
                continue;
            }
            indexes.insert((source, destination), queues.len());
            queues.push(VecDeque::with_capacity(shard_pair_capacity));
        }
    }
    (indexes, queues)
}

fn build_remote_queue_storage(shard_ids: &[ShardId], shard_pair_capacity: usize) -> RemoteQueues {
    let queue_count = shard_ids
        .len()
        .saturating_mul(shard_ids.len().saturating_sub(1));
    let mut queues = Vec::with_capacity(queue_count);
    for _ in 0..queue_count {
        queues.push(VecDeque::with_capacity(shard_pair_capacity));
    }
    queues
}

/// Configuration for [`ThreadedRuntime`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadedRuntimeConfig {
    /// Capacity of the bounded control/ingress queue feeding the shard worker.
    pub command_capacity: usize,

    /// Capacity of the bounded storage lane used for local persistence work.
    pub storage_lane_capacity: usize,

    /// Trace retention for the worker-owned runtime.
    pub trace_retention: TraceRetention,

    /// How long an idle worker may park before checking runtime-owned work
    /// again.
    pub idle_wait: Duration,
}

impl Default for ThreadedRuntimeConfig {
    fn default() -> Self {
        Self {
            command_capacity: 64,
            storage_lane_capacity: driver::DEFAULT_STORAGE_LANE_CAPACITY,
            trace_retention: TraceRetention::Full,
            idle_wait: Duration::from_millis(1),
        }
    }
}

/// Error returned by setup/control operations on [`ThreadedRuntime`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadedRuntimeError {
    /// The worker thread stopped before it could accept or answer the command.
    WorkerStopped,
    /// The worker could not prove backend completion-slot ownership was
    /// released during shutdown.
    DriverShutdownFailed,
}

/// Error returned by [`ThreadedRuntime::try_send`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadedTrySendError {
    /// The bounded worker ingress queue is full.
    IngressFull,

    /// The worker thread stopped before it could accept the ingress command.
    WorkerStopped,
}

/// Error returned by [`ThreadedRuntime::send_and_observe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadedSendObservedError {
    /// The bounded worker ingress queue is full.
    IngressFull,

    /// The target isolate mailbox is full.
    MailboxFull,

    /// The target isolate is closed or stale.
    MailboxClosed,

    /// The worker thread stopped before the send could be observed.
    WorkerStopped,
}

/// Observable lifecycle state for one live shard worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveShardState {
    /// The shard worker is running and accepts bounded ingress.
    Running,

    /// The shard worker stopped after graceful shutdown.
    Stopped,

    /// The shard worker failed or became unreachable before clean shutdown.
    Failed,
}

impl LiveShardState {
    const RUNNING: u8 = 0;
    const STOPPED: u8 = 1;
    const FAILED: u8 = 2;

    const fn from_raw(raw: u8) -> Self {
        match raw {
            Self::RUNNING => Self::Running,
            Self::STOPPED => Self::Stopped,
            _ => Self::Failed,
        }
    }
}

/// Snapshot of one bounded queue's visible pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveQueueReport {
    capacity: usize,
    depth: Option<usize>,
    accepted: Option<usize>,
    rejected_full: Option<usize>,
    rejected_closed: Option<usize>,
}

impl LiveQueueReport {
    const fn new(
        capacity: usize,
        depth: Option<usize>,
        accepted: Option<usize>,
        rejected_full: Option<usize>,
        rejected_closed: Option<usize>,
    ) -> Self {
        Self {
            capacity,
            depth,
            accepted,
            rejected_full,
            rejected_closed,
        }
    }

    const fn unmeasured(capacity: usize) -> Self {
        Self::new(capacity, None, None, None, None)
    }

    /// Stable configured queue capacity.
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Exact or sampled queue depth when the runtime can report it honestly.
    pub const fn depth(&self) -> Option<usize> {
        self.depth
    }

    /// Count of visible accepted handoffs for this queue, when measured.
    pub const fn accepted(&self) -> Option<usize> {
        self.accepted
    }

    /// Count of visible rejections caused by full bounded capacity, when measured.
    pub const fn rejected_full(&self) -> Option<usize> {
        self.rejected_full
    }

    /// Count of visible rejections caused by a stopped/closed destination, when measured.
    pub const fn rejected_closed(&self) -> Option<usize> {
        self.rejected_closed
    }
}

#[derive(Debug)]
struct LiveQueueMetrics {
    capacity: usize,
    accepted: AtomicUsize,
    rejected_full: AtomicUsize,
    rejected_closed: AtomicUsize,
}

impl LiveQueueMetrics {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            accepted: AtomicUsize::new(0),
            rejected_full: AtomicUsize::new(0),
            rejected_closed: AtomicUsize::new(0),
        }
    }

    fn accepted(&self) {
        self.accepted.fetch_add(1, Ordering::Relaxed);
    }

    fn rejected_full(&self) {
        self.rejected_full.fetch_add(1, Ordering::Relaxed);
    }

    fn rejected_closed(&self) {
        self.rejected_closed.fetch_add(1, Ordering::Relaxed);
    }

    fn report(&self) -> LiveQueueReport {
        LiveQueueReport::new(
            self.capacity,
            None,
            Some(self.accepted.load(Ordering::Relaxed)),
            Some(self.rejected_full.load(Ordering::Relaxed)),
            Some(self.rejected_closed.load(Ordering::Relaxed)),
        )
    }
}

#[derive(Debug)]
struct LiveShardMetrics {
    shard: ShardId,
    worker_name: Option<String>,
    state: AtomicU8,
    ingress: LiveQueueMetrics,
    storage_lane: LiveQueueMetrics,
    trace_retention: TraceRetention,
}

impl LiveShardMetrics {
    fn new(shard: ShardId, worker_name: Option<String>, config: ThreadedRuntimeConfig) -> Self {
        Self {
            shard,
            worker_name,
            state: AtomicU8::new(LiveShardState::RUNNING),
            ingress: LiveQueueMetrics::new(config.command_capacity),
            storage_lane: LiveQueueMetrics::new(config.storage_lane_capacity),
            trace_retention: config.trace_retention,
        }
    }

    fn state(&self) -> LiveShardState {
        LiveShardState::from_raw(self.state.load(Ordering::Acquire))
    }

    fn set_state(&self, state: LiveShardState) {
        let raw = match state {
            LiveShardState::Running => LiveShardState::RUNNING,
            LiveShardState::Stopped => LiveShardState::STOPPED,
            LiveShardState::Failed => LiveShardState::FAILED,
        };
        self.state.store(raw, Ordering::Release);
    }

    fn report(&self) -> LiveShardReport {
        LiveShardReport {
            shard: self.shard,
            worker_name: self.worker_name.clone(),
            state: self.state(),
            ingress: self.ingress.report(),
            storage_lane: LiveQueueReport::unmeasured(self.storage_lane.capacity),
            trace_retention: self.trace_retention,
            trace_dropped: None,
        }
    }
}

/// Snapshot of one live shard worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveShardReport {
    shard: ShardId,
    worker_name: Option<String>,
    state: LiveShardState,
    ingress: LiveQueueReport,
    storage_lane: LiveQueueReport,
    trace_retention: TraceRetention,
    trace_dropped: Option<u64>,
}

impl LiveShardReport {
    /// Shard owned by this worker.
    pub const fn shard(&self) -> ShardId {
        self.shard
    }

    /// Worker thread name when the live substrate can name it.
    pub fn worker_name(&self) -> Option<&str> {
        self.worker_name.as_deref()
    }

    /// Observable lifecycle state.
    pub const fn state(&self) -> LiveShardState {
        self.state
    }

    /// Visible bounded ingress queue pressure.
    pub const fn ingress(&self) -> &LiveQueueReport {
        &self.ingress
    }

    /// Visible bounded storage lane pressure.
    pub const fn storage_lane(&self) -> &LiveQueueReport {
        &self.storage_lane
    }

    /// Configured trace retention.
    pub const fn trace_retention(&self) -> TraceRetention {
        self.trace_retention
    }

    /// Dropped trace count when available without probing the worker.
    pub const fn trace_dropped(&self) -> Option<u64> {
        self.trace_dropped
    }
}

/// Snapshot of one live source-to-target shard transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveRemoteQueueReport {
    source: ShardId,
    target: ShardId,
    queue: LiveQueueReport,
}

impl LiveRemoteQueueReport {
    /// Source shard for this local transport.
    pub const fn source(&self) -> ShardId {
        self.source
    }

    /// Target shard for this local transport.
    pub const fn target(&self) -> ShardId {
        self.target
    }

    /// Visible bounded transport pressure.
    pub const fn queue(&self) -> &LiveQueueReport {
        &self.queue
    }
}

/// Snapshot of one local Tina live topology.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveTopologyReport {
    shards: Vec<LiveShardReport>,
    remote_queues: Vec<LiveRemoteQueueReport>,
}

impl LiveTopologyReport {
    fn single(shard: LiveShardReport) -> Self {
        Self {
            shards: vec![shard],
            remote_queues: Vec::new(),
        }
    }

    fn new(shards: Vec<LiveShardReport>, remote_queues: Vec<LiveRemoteQueueReport>) -> Self {
        Self {
            shards,
            remote_queues,
        }
    }

    /// Shard reports in stable shard order.
    pub fn shards(&self) -> &[LiveShardReport] {
        &self.shards
    }

    /// Remote queue reports in stable source/target order.
    pub fn remote_queues(&self) -> &[LiveRemoteQueueReport] {
        &self.remote_queues
    }

    /// Finds one shard report.
    pub fn shard(&self, shard: ShardId) -> Option<&LiveShardReport> {
        self.shards.iter().find(|report| report.shard == shard)
    }
}

type ThreadedCommandFn<S, F> = Box<dyn FnOnce(&mut Runtime<S, F>) + Send>;
type ThreadedIoLoopFactory = Box<dyn FnOnce() -> IOLoopHandle<Global> + Send>;
type ThreadedWorkerJoin = JoinHandle<Result<Vec<RuntimeEvent>, ThreadedRuntimeError>>;

/// Lifecycle state for the canonical local Tina app owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalSystemState {
    /// Reserved state for app owners that expose asynchronous startup.
    Starting,
    /// The app owner has a live worker handle and accepts bounded ingress.
    Accepting,
    /// Reserved state for app owners that expose an observable shutdown start.
    Closing,
    /// Reserved state for app owners that expose an observable drain window.
    Draining,
    /// The app has closed cleanly.
    Closed,
    /// The app failed during shutdown or worker execution.
    Failed,
}

/// Terminal report returned by [`LocalSystem`] and [`LocalMultiShardSystem`] shutdown.
#[derive(Debug)]
pub struct LocalSystemTerminalReport {
    state: LocalSystemState,
    trace: Vec<RuntimeEvent>,
    error: Option<ThreadedRuntimeError>,
    topology: Option<LiveTopologyReport>,
}

/// Counted terminal work visible in a [`LocalSystemTerminalReport`] trace.
///
/// This is accounting from Tina's public trace, not a hidden runtime metrics
/// channel. It is meant to answer the shutdown question: what completed,
/// failed, was rejected, or was abandoned in the work Tina can see?
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LocalSystemTerminalSummary {
    /// Successful runtime-owned call completions delivered to requesters.
    pub call_completed: usize,
    /// Runtime-owned calls that produced typed failure outcomes.
    pub call_failed: usize,
    /// Runtime-owned completions that could not be delivered.
    pub call_completion_rejected: usize,
    /// Late or stale isolate-call replies rejected by the runtime.
    pub call_reply_rejected: usize,
    /// Sends rejected by mailbox, address, or transport pressure.
    pub send_rejected: usize,
    /// Buffered messages abandoned because an isolate stopped.
    pub message_abandoned: usize,
    /// Successful journal append trace events.
    pub journal_appended: usize,
    /// Failed persistence trace events.
    pub persistence_failed: usize,
    /// Finished recovery trace events.
    pub recovery_finished: usize,
    /// Failed recovery trace events.
    pub recovery_failed: usize,
}

impl LocalSystemTerminalReport {
    /// Creates a terminal report from final state and trace.
    pub fn new(state: LocalSystemState, trace: Vec<RuntimeEvent>) -> Self {
        Self {
            state,
            trace,
            error: None,
            topology: None,
        }
    }

    /// Creates a failed terminal report.
    pub fn failed(error: ThreadedRuntimeError) -> Self {
        Self {
            state: LocalSystemState::Failed,
            trace: Vec::new(),
            error: Some(error),
            topology: None,
        }
    }

    /// Creates a terminal report with the final live topology snapshot.
    pub fn new_with_topology(
        state: LocalSystemState,
        trace: Vec<RuntimeEvent>,
        topology: LiveTopologyReport,
    ) -> Self {
        Self {
            state,
            trace,
            error: None,
            topology: Some(topology),
        }
    }

    /// Creates a failed terminal report with the final live topology snapshot.
    pub fn failed_with_topology(error: ThreadedRuntimeError, topology: LiveTopologyReport) -> Self {
        Self {
            state: LocalSystemState::Failed,
            trace: Vec::new(),
            error: Some(error),
            topology: Some(topology),
        }
    }

    /// Final lifecycle state.
    pub const fn state(&self) -> LocalSystemState {
        self.state
    }

    /// Final trace returned by the live worker.
    pub fn trace(&self) -> &[RuntimeEvent] {
        &self.trace
    }

    /// Summarizes terminal work visible in the final trace.
    pub fn summary(&self) -> LocalSystemTerminalSummary {
        terminal_summary(&self.trace)
    }

    /// Terminal failure, if shutdown or worker execution failed.
    pub const fn error(&self) -> Option<ThreadedRuntimeError> {
        self.error
    }

    /// Final topology snapshot if the owner could still report it.
    pub fn topology(&self) -> Option<&LiveTopologyReport> {
        self.topology.as_ref()
    }

    /// Consumes the report and returns the final trace.
    pub fn into_trace(self) -> Vec<RuntimeEvent> {
        self.trace
    }
}

fn terminal_summary(trace: &[RuntimeEvent]) -> LocalSystemTerminalSummary {
    let mut summary = LocalSystemTerminalSummary::default();
    for event in trace {
        match event.kind() {
            RuntimeEventKind::CallCompleted { .. } => summary.call_completed += 1,
            RuntimeEventKind::CallFailed { .. } => summary.call_failed += 1,
            RuntimeEventKind::CallCompletionRejected { .. } => {
                summary.call_completion_rejected += 1;
            }
            RuntimeEventKind::CallReplyRejected { .. } => summary.call_reply_rejected += 1,
            RuntimeEventKind::SendRejected { .. } => summary.send_rejected += 1,
            RuntimeEventKind::MessageAbandoned => summary.message_abandoned += 1,
            RuntimeEventKind::JournalAppended { .. } => summary.journal_appended += 1,
            RuntimeEventKind::SnapshotCommitFailed { .. }
            | RuntimeEventKind::JournalAppendFailed { .. } => {
                summary.persistence_failed += 1;
            }
            RuntimeEventKind::RecoveryFinished => summary.recovery_finished += 1,
            RuntimeEventKind::RecoveryFailed { .. } => summary.recovery_failed += 1,
            _ => {}
        }
    }
    summary
}

/// Canonical live app owner for one local Tina shard.
///
/// `LocalSystem` is the preferred user-facing owner for local live services.
/// [`ThreadedRuntime`] remains the lower-level backend-honest runner
/// underneath it.
pub struct LocalSystem<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    runtime: Option<ThreadedRuntime<S, F>>,
}

impl<S, F> LocalSystem<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    /// Starts configuring one single-shard local app.
    pub fn single_shard(shard: S, mailbox_factory: F) -> LocalSystemSingleShardBuilder<S, F> {
        LocalSystemSingleShardBuilder {
            shard,
            mailbox_factory,
            config: ThreadedRuntimeConfig::default(),
        }
    }

    /// Starts configuring one multi-shard local app.
    pub fn multi_shard(mailbox_factory: F) -> LocalSystemMultiShardBuilder<S, F>
    where
        F: Clone,
    {
        LocalSystemMultiShardBuilder {
            shards: Vec::new(),
            mailbox_factory,
            config: ThreadedRuntimeConfig::default(),
        }
    }

    /// Returns the owner-local lifecycle state.
    ///
    /// This does not synchronously probe worker health. Operations such as
    /// registration, sending, tracing, or shutdown report worker failure with a
    /// typed error.
    pub fn state(&self) -> LocalSystemState {
        if self.runtime.is_some() {
            LocalSystemState::Accepting
        } else {
            LocalSystemState::Closed
        }
    }

    /// Returns a live topology snapshot for this app.
    pub fn topology(&self) -> LiveTopologyReport {
        self.runtime().topology()
    }

    /// Returns the live runtime capability table for this app.
    pub fn capabilities(&self) -> RuntimeCapabilities {
        self.runtime().capabilities()
    }

    /// Registers one root isolate with a runtime-allocated mailbox.
    #[allow(private_bounds)]
    pub fn register_root<I, Outbound>(
        &self,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Result<Address<I::Message, I::Reply>, ThreadedRuntimeError>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + Send + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        Outbound: 'static,
    {
        self.runtime()
            .register_with_capacity::<I, Outbound>(isolate, mailbox_capacity)
    }

    /// Configures a registered root as a supervisor.
    pub fn supervise<M: 'static, R: 'static>(
        &self,
        parent: Address<M, R>,
        config: SupervisorConfig,
    ) -> Result<(), ThreadedRuntimeError> {
        self.runtime().supervise(parent, config)
    }

    /// Attempts one bounded ingress handoff.
    pub fn try_send<M: Send + 'static, R: 'static>(
        &self,
        address: Address<M, R>,
        message: M,
    ) -> Result<(), ThreadedTrySendError> {
        self.runtime().try_send(address, message)
    }

    /// Attempts one ingress send and observes the mailbox outcome.
    pub fn send_and_observe<M: Send + 'static, R: 'static>(
        &self,
        address: Address<M, R>,
        message: M,
    ) -> Result<(), ThreadedSendObservedError> {
        self.runtime().send_and_observe(address, message)
    }

    /// Returns a trace snapshot.
    pub fn trace(&self) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError> {
        self.runtime().trace()
    }

    /// Begins graceful shutdown.
    pub fn shutdown(self) -> LocalSystemShutdown<S, F> {
        LocalSystemShutdown {
            runtime: self.runtime,
        }
    }

    /// Consumes the app and returns its lower-level ThreadedRuntime.
    ///
    /// This is for bridge/adapters that need to share the backend runner behind
    /// an `Arc` while preserving `LocalSystem` as the preferred construction path.
    pub fn into_threaded_runtime(mut self) -> ThreadedRuntime<S, F> {
        self.runtime
            .take()
            .expect("local app runtime is unavailable after shutdown")
    }

    fn runtime(&self) -> &ThreadedRuntime<S, F> {
        self.runtime
            .as_ref()
            .expect("local app runtime is unavailable after shutdown")
    }
}

/// Builder for a single-shard [`LocalSystem`].
pub struct LocalSystemSingleShardBuilder<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    shard: S,
    mailbox_factory: F,
    config: ThreadedRuntimeConfig,
}

impl<S, F> LocalSystemSingleShardBuilder<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    /// Sets bounded ingress command capacity.
    pub const fn ingress_capacity(mut self, capacity: usize) -> Self {
        self.config.command_capacity = capacity;
        self
    }

    /// Sets trace retention for the worker-owned runtime.
    pub const fn trace_retention(mut self, retention: TraceRetention) -> Self {
        self.config.trace_retention = retention;
        self
    }

    /// Sets bounded storage-lane capacity for local persistence work.
    pub const fn storage_lane_capacity(mut self, capacity: usize) -> Self {
        self.config.storage_lane_capacity = capacity;
        self
    }

    /// Sets idle wait duration for the live worker.
    pub const fn idle_wait(mut self, idle_wait: Duration) -> Self {
        self.config.idle_wait = idle_wait;
        self
    }

    /// Builds the local app and starts its worker.
    pub fn build(self) -> LocalSystem<S, F> {
        LocalSystem {
            runtime: Some(ThreadedRuntime::with_config(
                self.shard,
                self.mailbox_factory,
                self.config,
            )),
        }
    }
}

/// Graceful shutdown handle for [`LocalSystem`].
pub struct LocalSystemShutdown<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    runtime: Option<ThreadedRuntime<S, F>>,
}

impl<S, F> LocalSystemShutdown<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    /// Marks the shutdown as draining runtime-owned work.
    ///
    /// Current Betelgeuse-backed shutdown drains/cancels inside worker
    /// shutdown. The method exists to keep the user-facing lifecycle explicit.
    pub fn drain(self) -> Self {
        self
    }

    /// Joins the worker and returns its terminal report.
    pub fn join(self) -> Result<LocalSystemTerminalReport, ThreadedRuntimeError> {
        let report = self.join_report();
        if let Some(error) = report.error() {
            Err(error)
        } else {
            Ok(report)
        }
    }

    /// Joins the worker and always returns the terminal lifecycle report.
    pub fn join_report(mut self) -> LocalSystemTerminalReport {
        let Some(mut runtime) = self.runtime.take() else {
            return LocalSystemTerminalReport::new(LocalSystemState::Closed, Vec::new());
        };
        match runtime.shutdown_inner() {
            Ok(trace) => LocalSystemTerminalReport::new_with_topology(
                LocalSystemState::Closed,
                trace,
                runtime.topology(),
            ),
            Err(error) => {
                LocalSystemTerminalReport::failed_with_topology(error, runtime.topology())
            }
        }
    }
}

/// Builder for a multi-shard local app.
pub struct LocalSystemMultiShardBuilder<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + Clone + 'static,
{
    shards: Vec<S>,
    mailbox_factory: F,
    config: ThreadedRuntimeConfig,
}

impl<S, F> LocalSystemMultiShardBuilder<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + Clone + 'static,
{
    /// Adds one shard to the local app topology.
    pub fn shard(mut self, shard: S) -> Self {
        self.shards.push(shard);
        self
    }

    /// Sets bounded ingress command capacity.
    pub const fn ingress_capacity(mut self, capacity: usize) -> Self {
        self.config.command_capacity = capacity;
        self
    }

    /// Names desired shard-pair capacity for live remote sends.
    ///
    /// The current live substrate routes remote sends through the target
    /// worker's bounded command queue, so this sets the same underlying
    /// capacity as [`ingress_capacity`](Self::ingress_capacity) until a
    /// dedicated live shard-pair queue exists.
    pub const fn shard_pair_capacity(mut self, capacity: usize) -> Self {
        self.config.command_capacity = capacity;
        self
    }

    /// Sets trace retention for worker-owned runtimes.
    pub const fn trace_retention(mut self, retention: TraceRetention) -> Self {
        self.config.trace_retention = retention;
        self
    }

    /// Sets bounded storage-lane capacity for local persistence work.
    pub const fn storage_lane_capacity(mut self, capacity: usize) -> Self {
        self.config.storage_lane_capacity = capacity;
        self
    }

    /// Builds the multi-shard local app and starts one worker per shard.
    pub fn build(self) -> LocalMultiShardSystem<S, F> {
        LocalMultiShardSystem {
            runtime: Some(ThreadedMultiShardRuntime::with_config(
                self.shards,
                self.mailbox_factory,
                self.config,
            )),
        }
    }
}

/// Canonical live app owner for a local multi-shard Tina service.
pub struct LocalMultiShardSystem<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + Clone + 'static,
{
    runtime: Option<ThreadedMultiShardRuntime<S, F>>,
}

impl<S, F> LocalMultiShardSystem<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + Clone + 'static,
{
    /// Registers one root isolate on the chosen shard.
    #[allow(private_bounds)]
    pub fn register_root_on<I, Outbound>(
        &self,
        shard: ShardId,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Result<Address<I::Message, I::Reply>, ThreadedRuntimeError>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + Send + 'static,
        I::Message: 'static,
        I::Reply: Send + 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        Outbound: Send + 'static,
    {
        self.runtime()
            .register_with_capacity_on::<I, Outbound>(shard, isolate, mailbox_capacity)
    }

    /// Configures a registered root as a supervisor.
    pub fn supervise<M: 'static, R: 'static>(
        &self,
        parent: Address<M, R>,
        config: SupervisorConfig,
    ) -> Result<(), ThreadedRuntimeError> {
        self.runtime().supervise(parent, config)
    }

    /// Attempts one bounded ingress handoff to the owning worker shard.
    pub fn try_send<M: Send + 'static, R: 'static>(
        &self,
        address: Address<M, R>,
        message: M,
    ) -> Result<(), ThreadedTrySendError> {
        self.runtime().try_send(address, message)
    }

    /// Returns a globally sorted trace snapshot.
    pub fn trace(&self) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError> {
        self.runtime().trace()
    }

    /// Returns a live topology snapshot for this app.
    pub fn topology(&self) -> LiveTopologyReport {
        self.runtime().topology()
    }

    /// Returns the live runtime capability table for this app.
    pub fn capabilities(&self) -> RuntimeCapabilities {
        self.runtime().capabilities()
    }

    /// Begins graceful shutdown.
    pub fn shutdown(self) -> LocalMultiShardSystemShutdown<S, F> {
        LocalMultiShardSystemShutdown {
            runtime: self.runtime,
        }
    }

    fn runtime(&self) -> &ThreadedMultiShardRuntime<S, F> {
        self.runtime
            .as_ref()
            .expect("local multi-shard app runtime is unavailable after shutdown")
    }
}

/// Graceful shutdown handle for [`LocalMultiShardSystem`].
pub struct LocalMultiShardSystemShutdown<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + Clone + 'static,
{
    runtime: Option<ThreadedMultiShardRuntime<S, F>>,
}

impl<S, F> LocalMultiShardSystemShutdown<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + Clone + 'static,
{
    /// Marks the shutdown as draining runtime-owned work.
    pub fn drain(self) -> Self {
        self
    }

    /// Joins all workers and returns a terminal report.
    pub fn join(self) -> Result<LocalSystemTerminalReport, ThreadedRuntimeError> {
        let report = self.join_report();
        if let Some(error) = report.error() {
            Err(error)
        } else {
            Ok(report)
        }
    }

    /// Joins all workers and always returns the terminal lifecycle report.
    pub fn join_report(mut self) -> LocalSystemTerminalReport {
        let Some(mut runtime) = self.runtime.take() else {
            return LocalSystemTerminalReport::new(LocalSystemState::Closed, Vec::new());
        };
        match runtime.shutdown_inner() {
            Ok(trace) => LocalSystemTerminalReport::new_with_topology(
                LocalSystemState::Closed,
                trace,
                runtime.topology(),
            ),
            Err(error) => {
                LocalSystemTerminalReport::failed_with_topology(error, runtime.topology())
            }
        }
    }
}

enum ThreadedCommand<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    Run(ThreadedCommandFn<S, F>),
    Shutdown,
}

/// One live shard-owned runtime worker.
///
/// The worker constructs and owns a single [`Runtime`] on its OS thread. The
/// handle only communicates through a bounded command queue, so ingress
/// pressure remains visible instead of falling into an unbounded executor
/// backlog. This is the Betelgeuse live substrate shape; the
/// explicit-step [`Runtime`] and [`MultiShardRuntime`] remain the semantic
/// oracle.
pub struct ThreadedRuntime<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    commands: std::sync::mpsc::SyncSender<ThreadedCommand<S, F>>,
    handle: Option<JoinHandle<Result<Vec<RuntimeEvent>, ThreadedRuntimeError>>>,
    metrics: Arc<LiveShardMetrics>,
}

impl<S, F> ThreadedRuntime<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    /// Starts one worker thread for one shard runtime.
    pub fn new(shard: S, mailbox_factory: F) -> Self {
        Self::with_config(shard, mailbox_factory, ThreadedRuntimeConfig::default())
    }

    /// Starts one worker thread with explicit bounded-command configuration.
    pub fn with_config(shard: S, mailbox_factory: F, config: ThreadedRuntimeConfig) -> Self {
        Self::with_config_and_io_loop_factory(shard, mailbox_factory, config, || {
            io_loop(Global).expect("failed to initialise Betelgeuse IO loop for tina-runtime")
        })
    }

    /// Starts one worker thread with an explicit Betelgeuse I/O loop factory.
    ///
    /// The factory runs on the worker thread so loop implementations that own
    /// thread-local state can still be used without making the runtime itself
    /// shared across threads.
    pub fn with_config_and_io_loop_factory<G>(
        shard: S,
        mailbox_factory: F,
        config: ThreadedRuntimeConfig,
        io_loop_factory: G,
    ) -> Self
    where
        G: FnOnce() -> IOLoopHandle<Global> + Send + 'static,
    {
        if config.command_capacity == 0 {
            panic!("ThreadedRuntime requires command capacity > 0");
        }
        if config.storage_lane_capacity == 0 {
            panic!("ThreadedRuntime requires storage lane capacity > 0");
        }

        let (commands, receiver) = std::sync::mpsc::sync_channel(config.command_capacity);
        let shard_id = shard.id();
        let worker_name = format!("tina-shard-{}", shard_id.get());
        let metrics = Arc::new(LiveShardMetrics::new(
            shard_id,
            Some(worker_name.clone()),
            config,
        ));
        let io_loop_factory: ThreadedIoLoopFactory = Box::new(io_loop_factory);
        let handle = thread::Builder::new()
            .name(worker_name)
            .spawn(move || {
                threaded_worker_loop(shard, mailbox_factory, receiver, config, io_loop_factory)
            })
            .expect("failed to spawn Tina threaded worker");

        Self {
            commands,
            handle: Some(handle),
            metrics,
        }
    }

    /// Registers one root isolate and lets the worker allocate its mailbox.
    #[allow(private_bounds)]
    pub fn register_with_capacity<I, Outbound>(
        &self,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Result<Address<I::Message, I::Reply>, ThreadedRuntimeError>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + Send + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        Outbound: 'static,
    {
        self.call(move |runtime| {
            runtime.register_with_capacity::<I, Outbound>(isolate, mailbox_capacity)
        })
    }

    /// Configures a registered isolate as supervisor on the worker shard.
    pub fn supervise<M: 'static, R: 'static>(
        &self,
        parent: Address<M, R>,
        config: SupervisorConfig,
    ) -> Result<(), ThreadedRuntimeError> {
        self.call(move |runtime| runtime.supervise(parent, config))
    }

    /// Attempts one typed ingress handoff through the bounded worker queue.
    ///
    /// Success means the worker accepted ownership of the message command. It
    /// does not mean the target mailbox has accepted the message yet. Mailbox
    /// `Full` / `Closed` outcomes are observed on the worker side through trace
    /// or through [`send_and_observe`](Self::send_and_observe).
    pub fn try_send<M: Send + 'static, R: 'static>(
        &self,
        address: Address<M, R>,
        message: M,
    ) -> Result<(), ThreadedTrySendError> {
        let command = ThreadedCommand::Run(Box::new(move |runtime| {
            let _ = runtime.try_send(address, message);
        }));

        match self.commands.try_send(command) {
            Ok(()) => {
                self.metrics.ingress.accepted();
                Ok(())
            }
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.metrics.ingress.rejected_full();
                Err(ThreadedTrySendError::IngressFull)
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                self.metrics.ingress.rejected_closed();
                self.metrics.set_state(LiveShardState::Failed);
                Err(ThreadedTrySendError::WorkerStopped)
            }
        }
    }

    /// Attempts one typed ingress send and waits for the worker to observe the
    /// target mailbox outcome.
    ///
    /// This is a synchronous control path for tests and setup code that need to
    /// distinguish mailbox `Full` from `Closed`. Ordinary ingress should prefer
    /// [`try_send`](Self::try_send), which only proves bounded handoff.
    pub fn send_and_observe<M: Send + 'static, R: 'static>(
        &self,
        address: Address<M, R>,
        message: M,
    ) -> Result<(), ThreadedSendObservedError> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let command = ThreadedCommand::Run(Box::new(move |runtime| {
            let result = runtime
                .try_send(address, message)
                .map_err(|error| match error {
                    TrySendError::Full(_) => ThreadedSendObservedError::MailboxFull,
                    TrySendError::Closed(_) => ThreadedSendObservedError::MailboxClosed,
                });
            let _ = reply_tx.send(result);
        }));

        match self.commands.try_send(command) {
            Ok(()) => {
                self.metrics.ingress.accepted();
                reply_rx
                    .recv()
                    .unwrap_or(Err(ThreadedSendObservedError::WorkerStopped))
            }
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.metrics.ingress.rejected_full();
                Err(ThreadedSendObservedError::IngressFull)
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                self.metrics.ingress.rejected_closed();
                self.metrics.set_state(LiveShardState::Failed);
                Err(ThreadedSendObservedError::WorkerStopped)
            }
        }
    }

    /// Attempts one typed ingress send and reports the target mailbox outcome
    /// later from the worker thread.
    ///
    /// This preserves the nonblocking bounded-handoff behavior of
    /// [`try_send`](Self::try_send) while still letting bridge code surface
    /// target `Full` / `Closed` instead of degrading those failures into
    /// timeouts.
    ///
    /// The observer runs on the worker thread and must stay nonblocking.
    pub fn try_send_and_observe_with<M, R, O>(
        &self,
        address: Address<M, R>,
        message: M,
        observer: O,
    ) -> Result<(), ThreadedTrySendError>
    where
        M: Send + 'static,
        R: 'static,
        O: FnOnce(Result<(), ThreadedSendObservedError>) + Send + 'static,
    {
        self.try_send_and_observe_with_preflight(address, message, |_| None, observer)
    }

    /// Attempts one typed ingress send with a worker-side preflight check.
    ///
    /// The preflight runs on the worker thread immediately before mailbox
    /// admission. It is for already-queued commands that may have become stale
    /// before the worker could observe them; it must stay nonblocking.
    pub fn try_send_and_observe_with_preflight<M, R, P, O>(
        &self,
        address: Address<M, R>,
        message: M,
        preflight: P,
        observer: O,
    ) -> Result<(), ThreadedTrySendError>
    where
        M: Send + 'static,
        R: 'static,
        P: FnOnce(&M) -> Option<ThreadedSendObservedError> + Send + 'static,
        O: FnOnce(Result<(), ThreadedSendObservedError>) + Send + 'static,
    {
        let command = ThreadedCommand::Run(Box::new(move |runtime| {
            if let Some(error) = preflight(&message) {
                observer(Err(error));
                return;
            }

            observer(
                runtime
                    .try_send(address, message)
                    .map_err(|error| match error {
                        TrySendError::Full(_) => ThreadedSendObservedError::MailboxFull,
                        TrySendError::Closed(_) => ThreadedSendObservedError::MailboxClosed,
                    }),
            );
        }));

        match self.commands.try_send(command) {
            Ok(()) => {
                self.metrics.ingress.accepted();
                Ok(())
            }
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.metrics.ingress.rejected_full();
                Err(ThreadedTrySendError::IngressFull)
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                self.metrics.ingress.rejected_closed();
                self.metrics.set_state(LiveShardState::Failed);
                Err(ThreadedTrySendError::WorkerStopped)
            }
        }
    }

    /// Returns a snapshot of the worker trace.
    pub fn trace(&self) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError> {
        self.call(|runtime| runtime.trace().to_vec())
    }

    /// Returns whether the worker still has runtime-owned work pending.
    pub fn has_in_flight_calls(&self) -> Result<bool, ThreadedRuntimeError> {
        self.call(|runtime| runtime.has_in_flight_calls())
    }

    /// Returns a handle-owned topology snapshot without probing the worker.
    pub fn topology(&self) -> LiveTopologyReport {
        LiveTopologyReport::single(self.metrics.report())
    }

    /// Returns the live runtime capability table for this worker.
    pub fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities::threaded(self.metrics.storage_lane.capacity)
    }

    /// Requests shutdown and joins the worker, returning its final trace.
    pub fn shutdown(mut self) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError> {
        self.shutdown_inner()
    }

    fn call<R, C>(&self, command: C) -> Result<R, ThreadedRuntimeError>
    where
        R: Send + 'static,
        C: FnOnce(&mut Runtime<S, F>) -> R + Send + 'static,
    {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.commands
            .send(ThreadedCommand::Run(Box::new(move |runtime| {
                let _ = reply_tx.send(command(runtime));
            })))
            .map_err(|_| {
                self.metrics.set_state(LiveShardState::Failed);
                ThreadedRuntimeError::WorkerStopped
            })?;
        reply_rx.recv().map_err(|_| {
            self.metrics.set_state(LiveShardState::Failed);
            ThreadedRuntimeError::WorkerStopped
        })
    }

    fn shutdown_inner(&mut self) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError> {
        let Some(handle) = self.handle.take() else {
            return Ok(Vec::new());
        };
        let _ = self.commands.send(ThreadedCommand::Shutdown);
        match handle.join() {
            Ok(Ok(trace)) => {
                self.metrics.set_state(LiveShardState::Stopped);
                Ok(trace)
            }
            Ok(Err(error)) => {
                self.metrics.set_state(LiveShardState::Failed);
                Err(error)
            }
            Err(_) => {
                self.metrics.set_state(LiveShardState::Failed);
                Err(ThreadedRuntimeError::WorkerStopped)
            }
        }
    }
}

impl<S, F> Drop for ThreadedRuntime<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    fn drop(&mut self) {
        let _ = self.shutdown_inner();
    }
}

fn threaded_worker_loop<S, F>(
    shard: S,
    mailbox_factory: F,
    receiver: std::sync::mpsc::Receiver<ThreadedCommand<S, F>>,
    config: ThreadedRuntimeConfig,
    io_loop_factory: ThreadedIoLoopFactory,
) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError>
where
    S: Shard,
    F: MailboxFactory,
{
    let mut runtime = Runtime::with_clock_and_ids_and_driver(
        shard,
        mailbox_factory,
        Box::new(MonotonicClock),
        IdSource::new(),
        Box::new(BetelgeuseDriver::with_io_loop_and_storage_capacity(
            io_loop_factory(),
            config.storage_lane_capacity,
        )),
    );
    runtime.set_trace_retention(config.trace_retention);

    loop {
        match receiver.try_recv() {
            Ok(ThreadedCommand::Run(command)) => {
                command(&mut runtime);
                continue;
            }
            Ok(ThreadedCommand::Shutdown) => {
                deliver_shutdown_signal_and_drain(&mut runtime);
                break;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }

        let delivered = runtime.step();
        if delivered == 0 && !runtime.has_in_flight_calls() {
            match receiver.recv_timeout(config.idle_wait) {
                Ok(ThreadedCommand::Run(command)) => command(&mut runtime),
                Ok(ThreadedCommand::Shutdown) => {
                    deliver_shutdown_signal_and_drain(&mut runtime);
                    break;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
        } else {
            thread::yield_now();
        }
    }

    runtime
        .cancel_in_flight_calls_for_shutdown()
        .map_err(|_| ThreadedRuntimeError::DriverShutdownFailed)?;
    Ok(runtime.trace().to_vec())
}

fn deliver_shutdown_signal_and_drain<S, F>(runtime: &mut Runtime<S, F>)
where
    S: Shard,
    F: MailboxFactory,
{
    runtime.notify_signal("shutdown");
    for _ in 0..1024 {
        if runtime.step() == 0 {
            break;
        }
    }
}

/// One live worker-per-shard runtime over a fixed shard set.
///
/// This is the Betelgeuse live multi-shard substrate. It keeps each shard
/// runtime owned by one OS thread, routes cross-shard effects through bounded
/// worker queues, and preserves the explicit-step runtime/simulator as the
/// semantic oracle. Live cross-shard payloads must be `Send` because they move
/// between worker threads.
pub struct ThreadedMultiShardRuntime<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + Clone + 'static,
{
    commands: BTreeMap<ShardId, std::sync::mpsc::SyncSender<ThreadedCommand<S, F>>>,
    handles: Vec<(ShardId, ThreadedWorkerJoin)>,
    shard_metrics: BTreeMap<ShardId, Arc<LiveShardMetrics>>,
    remote_metrics: BTreeMap<(ShardId, ShardId), Arc<LiveQueueMetrics>>,
}

impl<S, F> ThreadedMultiShardRuntime<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + Clone + 'static,
{
    /// Starts one live worker thread per shard.
    pub fn new<I>(shards: I, mailbox_factory: F) -> Self
    where
        I: IntoIterator<Item = S>,
    {
        Self::with_config(shards, mailbox_factory, ThreadedRuntimeConfig::default())
    }

    /// Starts one live worker thread per shard with explicit queue config.
    pub fn with_config<I>(shards: I, mailbox_factory: F, config: ThreadedRuntimeConfig) -> Self
    where
        I: IntoIterator<Item = S>,
    {
        if config.command_capacity == 0 {
            panic!("ThreadedMultiShardRuntime requires command capacity > 0");
        }
        if config.storage_lane_capacity == 0 {
            panic!("ThreadedMultiShardRuntime requires storage lane capacity > 0");
        }

        let mut shards: Vec<S> = shards.into_iter().collect();
        if shards.is_empty() {
            panic!("ThreadedMultiShardRuntime requires at least one shard");
        }
        shards.sort_by_key(Shard::id);
        for pair in shards.windows(2) {
            if pair[0].id() == pair[1].id() {
                panic!(
                    "ThreadedMultiShardRuntime received duplicate shard id {}",
                    pair[0].id().get()
                );
            }
        }

        let mut commands = BTreeMap::new();
        let mut shard_metrics = BTreeMap::new();
        let mut receivers = Vec::with_capacity(shards.len());
        for shard in &shards {
            let (sender, receiver) = std::sync::mpsc::sync_channel(config.command_capacity);
            commands.insert(shard.id(), sender);
            shard_metrics.insert(
                shard.id(),
                Arc::new(LiveShardMetrics::new(
                    shard.id(),
                    Some(format!("tina-shard-{}", shard.id().get())),
                    config,
                )),
            );
            receivers.push((shard.id(), receiver));
        }
        let mut remote_metrics = BTreeMap::new();
        for source in &shards {
            for target in &shards {
                if source.id() != target.id() {
                    remote_metrics.insert(
                        (source.id(), target.id()),
                        Arc::new(LiveQueueMetrics::new(config.command_capacity)),
                    );
                }
            }
        }

        let ids = IdSource::new();
        let mut handles = Vec::with_capacity(shards.len());
        for (shard, (_shard_id, receiver)) in shards.into_iter().zip(receivers) {
            let factory = mailbox_factory.clone();
            let ids = ids.clone();
            let remote_senders = commands.clone();
            let shard_id = shard.id();
            let remote_metrics_for_worker = remote_metrics.clone();
            handles.push((
                shard_id,
                thread::Builder::new()
                    .name(format!("tina-shard-{}", shard_id.get()))
                    .spawn(move || {
                        let io_loop = io_loop(Global).expect(
                            "failed to initialise Betelgeuse IO loop for tina-runtime shard",
                        );
                        let runtime = Runtime::with_clock_and_ids_and_driver(
                            shard,
                            factory,
                            Box::new(MonotonicClock),
                            ids,
                            Box::new(BetelgeuseDriver::with_io_loop_and_storage_capacity(
                                io_loop,
                                config.storage_lane_capacity,
                            )),
                        );
                        let mut runtime = runtime;
                        runtime.set_trace_retention(config.trace_retention);
                        threaded_worker_loop_with_remote(
                            runtime,
                            receiver,
                            config,
                            remote_senders,
                            remote_metrics_for_worker,
                        )
                    })
                    .expect("failed to spawn Tina threaded shard worker"),
            ));
        }

        Self {
            commands,
            handles,
            shard_metrics,
            remote_metrics,
        }
    }

    /// Registers one root isolate on a chosen shard.
    #[allow(private_bounds)]
    pub fn register_with_capacity_on<I, Outbound>(
        &self,
        shard: ShardId,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Result<Address<I::Message, I::Reply>, ThreadedRuntimeError>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + Send + 'static,
        I::Message: 'static,
        I::Reply: Send + 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        Outbound: Send + 'static,
    {
        self.call_on(shard, move |runtime| {
            runtime.register_sendable_with_capacity::<I, Outbound>(isolate, mailbox_capacity)
        })
    }

    /// Configures a registered root isolate as supervisor on its owning shard.
    pub fn supervise<M: 'static, R: 'static>(
        &self,
        parent: Address<M, R>,
        config: SupervisorConfig,
    ) -> Result<(), ThreadedRuntimeError> {
        self.call_on(parent.shard(), move |runtime| {
            runtime.supervise(parent, config)
        })
    }

    /// Attempts bounded ingress to the worker that owns `address`.
    pub fn try_send<M: Send + 'static, R: 'static>(
        &self,
        address: Address<M, R>,
        message: M,
    ) -> Result<(), ThreadedTrySendError> {
        let Some(sender) = self.commands.get(&address.shard()) else {
            panic!(
                "ThreadedMultiShardRuntime targeted unknown shard {}",
                address.shard().get()
            );
        };
        let command = ThreadedCommand::Run(Box::new(move |runtime| {
            let _ = runtime.try_send(address, message);
        }));
        match sender.try_send(command) {
            Ok(()) => {
                if let Some(metrics) = self.shard_metrics.get(&address.shard()) {
                    metrics.ingress.accepted();
                }
                Ok(())
            }
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                if let Some(metrics) = self.shard_metrics.get(&address.shard()) {
                    metrics.ingress.rejected_full();
                }
                Err(ThreadedTrySendError::IngressFull)
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                if let Some(metrics) = self.shard_metrics.get(&address.shard()) {
                    metrics.ingress.rejected_closed();
                    metrics.set_state(LiveShardState::Failed);
                }
                Err(ThreadedTrySendError::WorkerStopped)
            }
        }
    }

    /// Returns a globally sorted trace snapshot across all worker shards.
    pub fn trace(&self) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError> {
        let mut events = Vec::new();
        for shard in self.commands.keys() {
            events.extend(self.call_on(*shard, |runtime| runtime.trace().to_vec())?);
        }
        events.sort_by_key(|event| event.id());
        Ok(events)
    }

    /// Returns a trace snapshot from one worker shard.
    pub fn trace_on(&self, shard: ShardId) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError> {
        self.call_on(shard, |runtime| runtime.trace().to_vec())
    }

    /// Returns a handle-owned topology snapshot without probing workers.
    pub fn topology(&self) -> LiveTopologyReport {
        let shards = self
            .shard_metrics
            .values()
            .map(|metrics| metrics.report())
            .collect();
        let remote_queues = self
            .remote_metrics
            .iter()
            .map(|(&(source, target), metrics)| LiveRemoteQueueReport {
                source,
                target,
                queue: metrics.report(),
            })
            .collect();
        LiveTopologyReport::new(shards, remote_queues)
    }

    /// Returns the live runtime capability table shared by each worker.
    pub fn capabilities(&self) -> RuntimeCapabilities {
        let capacity = self
            .shard_metrics
            .values()
            .next()
            .map(|metrics| metrics.storage_lane.capacity)
            .unwrap_or(driver::DEFAULT_STORAGE_LANE_CAPACITY);
        RuntimeCapabilities::threaded(capacity)
    }

    /// Requests shutdown and joins every worker.
    pub fn shutdown(mut self) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError> {
        self.shutdown_inner()
    }

    fn call_on<R, C>(&self, shard: ShardId, command: C) -> Result<R, ThreadedRuntimeError>
    where
        R: Send + 'static,
        C: FnOnce(&mut Runtime<S, F>) -> R + Send + 'static,
    {
        let Some(sender) = self.commands.get(&shard) else {
            panic!(
                "ThreadedMultiShardRuntime targeted unknown shard {}",
                shard.get()
            );
        };
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        sender
            .send(ThreadedCommand::Run(Box::new(move |runtime| {
                let _ = reply_tx.send(command(runtime));
            })))
            .map_err(|_| {
                if let Some(metrics) = self.shard_metrics.get(&shard) {
                    metrics.set_state(LiveShardState::Failed);
                }
                ThreadedRuntimeError::WorkerStopped
            })?;
        reply_rx.recv().map_err(|_| {
            if let Some(metrics) = self.shard_metrics.get(&shard) {
                metrics.set_state(LiveShardState::Failed);
            }
            ThreadedRuntimeError::WorkerStopped
        })
    }

    fn shutdown_inner(&mut self) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError> {
        for sender in self.commands.values() {
            let _ = sender.send(ThreadedCommand::Shutdown);
        }

        let mut events = Vec::new();
        let mut failure = None;
        for (shard, handle) in std::mem::take(&mut self.handles) {
            match handle.join() {
                Ok(Ok(trace)) => {
                    if let Some(metrics) = self.shard_metrics.get(&shard) {
                        metrics.set_state(LiveShardState::Stopped);
                    }
                    events.extend(trace);
                }
                Ok(Err(error)) => {
                    if let Some(metrics) = self.shard_metrics.get(&shard) {
                        metrics.set_state(LiveShardState::Failed);
                    }
                    failure = Some(error);
                }
                Err(_) => {
                    if let Some(metrics) = self.shard_metrics.get(&shard) {
                        metrics.set_state(LiveShardState::Failed);
                    }
                    failure = Some(ThreadedRuntimeError::WorkerStopped);
                }
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        events.sort_by_key(|event| event.id());
        Ok(events)
    }
}

impl<S, F> Drop for ThreadedMultiShardRuntime<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + Clone + 'static,
{
    fn drop(&mut self) {
        let _ = self.shutdown_inner();
    }
}

fn threaded_worker_loop_with_remote<S, F>(
    mut runtime: Runtime<S, F>,
    receiver: std::sync::mpsc::Receiver<ThreadedCommand<S, F>>,
    config: ThreadedRuntimeConfig,
    remote_senders: BTreeMap<ShardId, std::sync::mpsc::SyncSender<ThreadedCommand<S, F>>>,
    remote_metrics: BTreeMap<(ShardId, ShardId), Arc<LiveQueueMetrics>>,
) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError>
where
    S: Shard,
    F: MailboxFactory,
{
    let source_shard = runtime.shard().id();
    loop {
        match receiver.try_recv() {
            Ok(ThreadedCommand::Run(command)) => {
                command(&mut runtime);
                continue;
            }
            Ok(ThreadedCommand::Shutdown) => {
                deliver_shutdown_signal_and_drain(&mut runtime);
                break;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }

        let delivered = runtime.step_with_remote(&mut |_, send, cause| {
            let target_shard = send.target_shard;
            let Some(sender) = remote_senders.get(&target_shard) else {
                panic!(
                    "ThreadedMultiShardRuntime targeted unknown destination shard {}",
                    target_shard.get()
                );
            };
            let queued = SendableQueuedRemoteSend::new(send, cause);
            let command = ThreadedCommand::Run(Box::new(move |runtime| {
                runtime.harvest_remote_send(queued.into_queued_remote_send());
            }));
            let metrics = remote_metrics.get(&(source_shard, target_shard));
            match sender.try_send(command) {
                Ok(()) => {
                    if let Some(metrics) = metrics {
                        metrics.accepted();
                    }
                    Ok(())
                }
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    if let Some(metrics) = metrics {
                        metrics.rejected_full();
                    }
                    Err(SendRejectedReason::Full)
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    if let Some(metrics) = metrics {
                        metrics.rejected_closed();
                    }
                    Err(SendRejectedReason::Closed)
                }
            }
        });

        if delivered == 0 && !runtime.has_in_flight_calls() {
            match receiver.recv_timeout(config.idle_wait) {
                Ok(ThreadedCommand::Run(command)) => command(&mut runtime),
                Ok(ThreadedCommand::Shutdown) => {
                    deliver_shutdown_signal_and_drain(&mut runtime);
                    break;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
        } else {
            thread::yield_now();
        }
    }

    runtime
        .cancel_in_flight_calls_for_shutdown()
        .map_err(|_| ThreadedRuntimeError::DriverShutdownFailed)?;
    Ok(runtime.trace().to_vec())
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
    ) -> ErasedEffect<S, F>;
}

trait ErasedSpawn<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    fn spawn(self: Box<Self>, runtime: &mut Runtime<S, F>, parent: IsolateId)
    -> SpawnOutcome<S, F>;
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
    I::Call: IntoErasedCall<I::Message> + 'static,
    Outbound: 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn handle_boxed(
        &mut self,
        message: Box<dyn Any>,
        shard: &mut S,
        isolate_id: IsolateId,
    ) -> ErasedEffect<S, F> {
        let message = message.downcast::<I::Message>().unwrap_or_else(|_| {
            panic!("runtime attempted to deliver a handler message with the wrong type")
        });

        let effect = {
            let mut ctx = Context::new(shard, isolate_id);
            self.isolate.handle(*message, &mut ctx)
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
    I::Call: IntoErasedCall<I::Message> + 'static,
    Outbound: Send + 'static,
    S: Shard,
    F: MailboxFactory,
{
    fn handle_boxed(
        &mut self,
        message: Box<dyn Any>,
        shard: &mut S,
        isolate_id: IsolateId,
    ) -> ErasedEffect<S, F> {
        let message = message.downcast::<I::Message>().unwrap_or_else(|_| {
            panic!("runtime attempted to deliver a handler message with the wrong type")
        });

        let effect = {
            let mut ctx = Context::new(shard, isolate_id);
            self.isolate.handle(*message, &mut ctx)
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
    I::Call: IntoErasedCall<I::Message> + 'static,
    Outbound: 'static,
    S: Shard,
    F: MailboxFactory,
{
    match effect {
        Effect::Noop => ErasedEffect::Noop,
        Effect::Reply(reply) => ErasedEffect::Reply(ErasedMessage::Local(Box::new(reply))),
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
        Effect::Stop => ErasedEffect::Stop,
        Effect::RestartChildren => ErasedEffect::RestartChildren,
        Effect::Call(call) => ErasedEffect::Call(call.into_erased_call()),
        Effect::Batch(effects) => ErasedEffect::Batch(
            effects
                .into_iter()
                .map(erase_effect::<I, S, F, Outbound>)
                .collect(),
        ),
    }
}

fn erase_effect_sendable<I, S, F, Outbound>(effect: Effect<I>) -> ErasedEffect<S, F>
where
    I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
    I::Message: 'static,
    I::Reply: Send + 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::Call: IntoErasedCall<I::Message> + 'static,
    Outbound: Send + 'static,
    S: Shard,
    F: MailboxFactory,
{
    match effect {
        Effect::Noop => ErasedEffect::Noop,
        Effect::Reply(reply) => ErasedEffect::Reply(ErasedMessage::Sendable(Box::new(reply))),
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
        Effect::Stop => ErasedEffect::Stop,
        Effect::RestartChildren => ErasedEffect::RestartChildren,
        Effect::Call(call) => ErasedEffect::Call(call.into_erased_call()),
        Effect::Batch(effects) => ErasedEffect::Batch(
            effects
                .into_iter()
                .map(erase_effect_sendable::<I, S, F, Outbound>)
                .collect(),
        ),
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
    Send(ErasedSend),
    Spawn(Box<dyn ErasedSpawn<S, F>>),
    Stop,
    RestartChildren,
    Call(ErasedCall),
    Batch(Vec<ErasedEffect<S, F>>),
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
            Self::Send(_) => EffectKind::Send,
            Self::Spawn(_) => EffectKind::Spawn,
            Self::Stop => EffectKind::Stop,
            Self::RestartChildren => EffectKind::RestartChildren,
            Self::Call(_) => EffectKind::Call,
            Self::Batch(_) => EffectKind::Batch,
        }
    }
}

pub(crate) struct ErasedSend {
    pub(crate) target_shard: ShardId,
    pub(crate) target_isolate: IsolateId,
    pub(crate) target_generation: AddressGeneration,
    pub(crate) message: ErasedMessage,
}

struct QueuedRemoteSend {
    send: ErasedSend,
    cause: CauseId,
}

struct SendableQueuedRemoteSend {
    target_shard: ShardId,
    target_isolate: IsolateId,
    target_generation: AddressGeneration,
    message: Box<dyn Any + Send>,
    cause: CauseId,
}

impl SendableQueuedRemoteSend {
    fn new(send: ErasedSend, cause: CauseId) -> Self {
        Self {
            target_shard: send.target_shard,
            target_isolate: send.target_isolate,
            target_generation: send.target_generation,
            message: send.message.into_sendable(),
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
            cause: self.cause,
        }
    }
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
    I::Call: IntoErasedCall<I::Message> + 'static,
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
}

impl<I, S, F, OutboundMsg> IntoErasedSpawn<S, F> for tina::ChildDefinition<I>
where
    I: Isolate<Shard = S, Send = TinaOutbound<OutboundMsg>> + 'static,
    I::Message: 'static,
    I::Reply: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::Call: IntoErasedCall<I::Message> + 'static,
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
    I::Call: IntoErasedCall<I::Message> + 'static,
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
}

impl<I, S, F, Outbound> ErasedRestartRecipe<S, F> for RestartableSpawnAdapter<I, Outbound>
where
    I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
    I::Message: 'static,
    I::Reply: 'static,
    I::Spawn: IntoErasedSpawn<S, F> + 'static,
    I::Call: IntoErasedCall<I::Message> + 'static,
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
    I::Call: IntoErasedCall<I::Message> + 'static,
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
