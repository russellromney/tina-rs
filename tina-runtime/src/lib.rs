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
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tina::{
    Address, AddressGeneration, ChildRelation, Context, Effect, Isolate, IsolateId, Mailbox,
    Outbound as TinaOutbound, RestartBudgetState, Shard, ShardId, TrySendError,
};
use tina_supervisor::SupervisorConfig;

use betelgeuse::{IOLoopHandle, io_loop};

mod call;
mod capabilities;
mod clock;
mod driver;
mod errors;
mod live_report;
mod local_system;
mod mailbox;
mod multi_shard;
mod observation;
pub mod persistence;
mod trace;

pub use errors::{
    SuperviseError, ThreadedRuntimeError, ThreadedSendObservedError, ThreadedTrySendError,
};
pub use local_system::{
    LocalMultiShardSystem, LocalMultiShardSystemShutdown, LocalSystem, LocalSystemConfig,
    LocalSystemConfigError, LocalSystemMultiShardBuilder, LocalSystemShutdown,
    LocalSystemShutdownReport, LocalSystemSingleShardBuilder, LocalSystemState,
    LocalSystemTerminalReport, LocalSystemTerminalSummary, ShutdownUncleanReason, TraceSnapshot,
};
use local_system::{
    ThreadedCommandFn, ThreadedIoLoopFactory, ThreadedWorkerExit, ThreadedWorkerJoin,
};
pub use multi_shard::{MultiShardRuntime, MultiShardRuntimeConfig};

pub use live_report::{
    AffinityStatus, LiveQueueReport, LiveRemoteQueueReport, LiveShardReport, LiveShardState,
    LiveTopologyReport,
};
use live_report::{LiveQueueMetrics, LiveShardMetrics};

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
pub use call::{
    CallError, CallId, CallInput, CallOutcome, CallOutput, ErasedCall, FileId, FileOpenOptions,
    IntoErasedCall, IsolateCall, JournalRecord, JournalReplay, JournalReplayWarning, ListenerId,
    PathKind, PathMetadata, PersistenceTraceInfo, ProcessRunResult, ProcessStatus, RuntimeCall,
    RuntimeCallParts, RuntimeCallable, SendOutcome, SnapshotImage, StreamId, TlsListenerId,
    TlsStreamId, TypedCall, UdpSocketId, call, dns_lookup, file_close, file_create, file_fsync,
    file_open, file_read, file_read_at, file_size, file_write, file_write_at, journal_append,
    journal_replay, mkdir, path_metadata, process_run, read_dir, remove_file, rename_replace,
    send_observed, signal_wait, sleep, sleep_then, snapshot_commit, snapshot_load, sync_parent,
    tcp_accept, tcp_bind, tcp_close_listener, tcp_close_stream, tcp_connect, tcp_read, tcp_write,
    tls_accept, tls_bind, tls_close, tls_close_listener, tls_connect, tls_read, tls_write,
    udp_bind, udp_close_socket, udp_recv_from, udp_send_to,
};
use driver::DriverCompletion;
pub use observation::{
    BoundAddressWaiter, ChildRestarted, ChildRestartedWaiter, IsolateCompleteWaiter,
    OperationDoneWaiter, WaitError,
};
/// Declares a Tina isolate whose call channel defaults to [`RuntimeCall<Message>`](RuntimeCall).
///
/// This is the preferred runtime authoring path. It keeps the handler as normal
/// Rust code and only fills the repetitive [`tina::Isolate`] associated types.
pub use tina_macros::runtime_isolate as isolate;
pub use trace::{
    CallCompletionRejectedReason, CallKind, CallReplyRejectedReason, CauseId, EffectKind, EventId,
    RestartSkippedReason, RuntimeEvent, RuntimeEventKind, SendRejectedReason,
    SupervisionRejectedReason, stable_trace_hash,
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

    /// Sets the trace retention policy for future events.
    ///
    /// Lowering retention trims the current trace immediately so callers can
    /// rely on the memory bound after this returns.
    pub fn set_trace_retention(&mut self, retention: TraceRetention) {
        self.trace_retention = retention;
        self.enforce_trace_retention();
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
        // Phase 047 Rock 8: keep the panicking surface for callers who want
        // a setup-time assertion, but route the actual work through the
        // fallible `try_supervise` so the panic message stays in one place.
        if self.try_supervise(parent, config).is_err() {
            panic!(
                "supervise expected an address registered with this runtime, got an unknown or stale address",
            );
        }
    }

    /// Configures one registered isolate as supervisor without panicking on
    /// unknown parents.
    ///
    /// Phase 047 Rock 8 (runtime surface alignment): the panicking
    /// [`supervise`](Self::supervise) variant remains available for setup
    /// code that wants the unknown-parent case to be a hard programmer
    /// error. `try_supervise` is the fallible variant that
    /// [`ThreadedRuntime`] uses internally so an unknown-parent registration
    /// does not crash the worker thread.
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
        route_remote: &mut impl FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
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
                                self.push_event(
                                    isolate_id,
                                    Some(cause),
                                    RuntimeEventKind::CallReplyRejected {
                                        call_id,
                                        reason: CallReplyRejectedReason::NoPendingCall,
                                    },
                                );
                            }
                        }
                        MessageCallContext::Remote {
                            call_id,
                            requester,
                            cause: request_cause,
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
        continuation_context: Option<MessageCallContext>,
        route_remote: &mut impl FnMut(ShardId, QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
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
            } => {
                self.dispatch_isolate_call(
                    dispatch_context,
                    send,
                    timeout,
                    translator,
                    route_remote,
                );
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

    fn dispatch_isolate_call(
        &mut self,
        context: CallDispatchContext,
        send: ErasedSend,
        timeout: Duration,
        translator: ErasedIsolateCallTranslator,
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
                self.pending_isolate_calls.push(PendingIsolateCall {
                    call_id: context.call_id,
                    requester: context.requester,
                    cause: context.cause,
                    deadline: self.clock.now() + timeout,
                    insertion_order,
                    continuation_context: context.continuation_context,
                    translator: Some(translator),
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
                entry.continuation_context,
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
        self.observation
            .notify_isolate_stopped(isolate_id, self.entries[index].generation);
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
            RemoteCallOutcome::Full => {
                self.complete_remote_isolate_call(reply, CallOutcome::Full);
            }
            RemoteCallOutcome::Closed => {
                self.complete_remote_isolate_call(reply, CallOutcome::Closed);
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

/// Configuration for [`ThreadedRuntime`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadedRuntimeConfig {
    /// Capacity of the bounded control/ingress queue feeding the shard worker.
    pub command_capacity: usize,

    /// Capacity of each live source-shard -> destination-shard transport.
    pub shard_pair_capacity: usize,

    /// Maximum remote envelopes one live shard worker harvests before giving
    /// its local runtime a turn.
    pub remote_inbound_drain_budget: usize,

    /// Capacity of the bounded storage lane used for local persistence work.
    pub storage_lane_capacity: usize,

    /// Capacity of the bounded DNS lane.
    pub dns_lane_capacity: usize,

    /// Capacity of the bounded TLS lane.
    pub tls_lane_capacity: usize,

    /// Capacity of the bounded process lane.
    pub process_lane_capacity: usize,

    /// Capacity of runtime-owned signal waits.
    pub signal_capacity: usize,

    /// Desired OS core for this shard worker.
    ///
    /// The current portable backend reports this as advisory intent only. It
    /// does not hard-pin the worker without a platform-specific affinity
    /// implementation.
    pub configured_core: Option<usize>,

    /// Setup-time reserves for runtime-owned metadata.
    pub preallocation: PreallocationConfig,

    /// Trace retention for the worker-owned runtime.
    pub trace_retention: TraceRetention,

    /// How long an idle worker may park before checking runtime-owned work
    /// again.
    pub idle_wait: Duration,

    /// Per-shard budget for draining lane workers after cancellation
    /// during shutdown. When the budget elapses, shutdown returns even if
    /// some lane work could not finish.
    pub shutdown_lane_drain_timeout: Duration,
}

impl Default for ThreadedRuntimeConfig {
    fn default() -> Self {
        Self {
            command_capacity: 64,
            shard_pair_capacity: 64,
            remote_inbound_drain_budget: 64,
            storage_lane_capacity: driver::DEFAULT_STORAGE_LANE_CAPACITY,
            dns_lane_capacity: driver::DEFAULT_DNS_LANE_CAPACITY,
            tls_lane_capacity: driver::DEFAULT_TLS_LANE_CAPACITY,
            process_lane_capacity: driver::DEFAULT_PROCESS_LANE_CAPACITY,
            signal_capacity: driver::DEFAULT_SIGNAL_CAPACITY,
            configured_core: None,
            preallocation: PreallocationConfig::default(),
            trace_retention: TraceRetention::Full,
            idle_wait: Duration::from_millis(1),
            shutdown_lane_drain_timeout: DEFAULT_SHUTDOWN_LANE_DRAIN_TIMEOUT,
        }
    }
}

/// Per-shard default budget for draining lane workers after cancellation.
pub const DEFAULT_SHUTDOWN_LANE_DRAIN_TIMEOUT: Duration = Duration::from_millis(100);

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
    handle: Option<ThreadedWorkerJoin>,
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
        if config.dns_lane_capacity == 0 {
            panic!("ThreadedRuntime requires DNS lane capacity > 0");
        }
        if config.tls_lane_capacity == 0 {
            panic!("ThreadedRuntime requires TLS lane capacity > 0");
        }
        if config.process_lane_capacity == 0 {
            panic!("ThreadedRuntime requires process lane capacity > 0");
        }
        if config.signal_capacity == 0 {
            panic!("ThreadedRuntime requires signal capacity > 0");
        }
        if config.remote_inbound_drain_budget == 0 {
            panic!("ThreadedRuntime requires remote inbound drain budget > 0");
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
        let worker_metrics = Arc::clone(&metrics);
        let handle = thread::Builder::new()
            .name(worker_name)
            .spawn(move || {
                threaded_worker_loop(
                    shard,
                    mailbox_factory,
                    receiver,
                    config,
                    io_loop_factory,
                    worker_metrics,
                )
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
    ///
    /// Phase 047 Rock 8: this method panics on unknown parent (consistent
    /// with the explicit-step `Runtime::supervise`). Use
    /// [`try_supervise`](Self::try_supervise) for a non-panicking variant
    /// that surfaces unknown / stale parents as a typed
    /// [`SuperviseError::UnknownParent`] without crashing the worker.
    pub fn supervise<M: 'static, R: 'static>(
        &self,
        parent: Address<M, R>,
        config: SupervisorConfig,
    ) -> Result<(), ThreadedRuntimeError> {
        self.call(move |runtime| runtime.supervise(parent, config))
    }

    /// Configures a registered isolate as supervisor on the worker shard
    /// without panicking on unknown / stale parents.
    ///
    /// `Ok(Ok(()))` — registration succeeded. `Ok(Err(SuperviseError::UnknownParent))`
    /// — the address is not currently registered or its generation is
    /// stale. `Err(ThreadedRuntimeError)` — the worker thread had already
    /// stopped or the shutdown handshake could not be observed.
    pub fn try_supervise<M: 'static, R: 'static>(
        &self,
        parent: Address<M, R>,
        config: SupervisorConfig,
    ) -> Result<Result<(), SuperviseError>, ThreadedRuntimeError> {
        self.call(move |runtime| runtime.try_supervise(parent, config))
    }

    /// Registers a typed waiter for the next `tcp_bind` completion on the
    /// worker shard.
    ///
    /// Returns a [`BoundAddressWaiter`] the host can call `.wait(timeout)`
    /// on. Each call returns a fresh waiter.
    ///
    /// **Order matters.** Register the waiter *before* you trigger the bind
    /// (typically before the `try_send` that kicks the listener isolate). The
    /// command channel is FIFO, so a registration enqueued before the bind
    /// trigger always lands in the registry before the worker processes the
    /// trigger; a registration enqueued after the bind has already completed
    /// will wait for the *next* bind, not the one that just happened.
    ///
    /// If the worker is already stopped, the returned waiter resolves
    /// immediately to [`WaitError::RuntimeStopped`] when `wait` is called —
    /// the waiter itself is the single source of truth for "did this bind
    /// happen", so no extra registration error is surfaced here.
    pub fn observe_next_bound(&self) -> BoundAddressWaiter {
        match self.call(|runtime| runtime.observe_next_bound()) {
            Ok(waiter) => waiter,
            Err(_) => observation::stopped_bound_waiter(),
        }
    }

    /// Registers a typed waiter for the targeted isolate's `IsolateStopped`
    /// event on the worker shard.
    ///
    /// See [`Runtime::observe_isolate_complete`] for semantics. If the worker
    /// is already stopped the returned waiter resolves immediately to
    /// [`WaitError::RuntimeStopped`].
    pub fn observe_isolate_complete<M: 'static, R: 'static>(
        &self,
        address: Address<M, R>,
    ) -> observation::IsolateCompleteWaiter {
        match self.call(move |runtime| runtime.observe_isolate_complete(address)) {
            Ok(waiter) => waiter,
            Err(_) => observation::stopped_isolate_complete_waiter(),
        }
    }

    /// Registers a typed waiter for the next runtime call of `call_kind`
    /// issued by `address` that completes on the worker shard.
    ///
    /// See [`Runtime::observe_operation_done`] for semantics.
    pub fn observe_operation_done<M: 'static, R: 'static>(
        &self,
        address: Address<M, R>,
        call_kind: CallKind,
    ) -> observation::OperationDoneWaiter {
        match self.call(move |runtime| runtime.observe_operation_done(address, call_kind)) {
            Ok(waiter) => waiter,
            Err(_) => observation::stopped_operation_done_waiter(),
        }
    }

    /// Registers a typed waiter for the next supervised restart of any
    /// direct child of `parent_address` on the worker shard.
    ///
    /// See [`Runtime::observe_child_restarted`] for semantics.
    pub fn observe_child_restarted<M: 'static, R: 'static>(
        &self,
        parent_address: Address<M, R>,
    ) -> observation::ChildRestartedWaiter {
        match self.call(move |runtime| runtime.observe_child_restarted(parent_address)) {
            Ok(waiter) => waiter,
            Err(_) => observation::stopped_child_restarted_waiter(),
        }
    }

    /// Attempts one typed ingress handoff through the bounded worker queue.
    ///
    /// Success means the worker accepted ownership of the message command. It
    /// does not mean the target mailbox has accepted the message yet. Mailbox
    /// `Full` / `Closed` outcomes are observed on the worker side through trace
    /// or through [`send_and_observe`](Self::send_and_observe).
    ///
    /// Phase 047 Rock 8 — porting note: this is the fast, fire-and-forget
    /// surface. Unlike [`Runtime::try_send`] (the explicit-step equivalent),
    /// `ThreadedRuntime::try_send`:
    ///
    /// - returns `ThreadedTrySendError`, not `TrySendError<M>`. The
    ///   message is consumed even on `IngressFull`; callers that need to
    ///   recover the message (or distinguish `MailboxFull` from
    ///   `MailboxClosed`) should use [`send_and_observe`](Self::send_and_observe),
    ///   which is the strict, message-recoverable equivalent.
    /// - silently drops messages addressed to a stale or unknown isolate
    ///   on the worker side once the command is accepted. Use
    ///   [`send_and_observe`](Self::send_and_observe) when the host must
    ///   learn that the target was already closed.
    pub fn try_send<M: Send + 'static, R: 'static>(
        &self,
        address: Address<M, R>,
        message: M,
    ) -> Result<(), ThreadedTrySendError> {
        // Phase 043 Rock 5: a Failed worker rejects ingress immediately
        // even before the bounded sync_channel has observed Disconnected,
        // so callers cannot enqueue work into a quarantined shard.
        if self.metrics.state() == LiveShardState::Failed {
            self.metrics.ingress.rejected_closed();
            return Err(ThreadedTrySendError::WorkerStopped);
        }
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

    /// Returns retained trace without failing the observability path.
    pub fn trace(&self) -> TraceSnapshot {
        match self.complete_trace() {
            Ok(events) => TraceSnapshot::complete(events),
            Err(ThreadedRuntimeError::WorkerStopped) => TraceSnapshot::partial(
                Vec::new(),
                self.topology()
                    .shards()
                    .iter()
                    .map(|shard| shard.shard())
                    .collect(),
            ),
            Err(_) => TraceSnapshot::partial(
                Vec::new(),
                self.topology()
                    .shards()
                    .iter()
                    .map(|shard| shard.shard())
                    .collect(),
            ),
        }
    }

    /// Returns complete trace, failing if the worker can no longer report.
    pub fn complete_trace(&self) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError> {
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
        RuntimeCapabilities::threaded_with_capacities(
            self.metrics.config.storage_lane_capacity,
            self.metrics.config.dns_lane_capacity,
            self.metrics.config.tls_lane_capacity,
            self.metrics.config.process_lane_capacity,
            self.metrics.config.signal_capacity,
        )
    }

    /// Requests shutdown and joins the worker, returning its final trace.
    pub fn shutdown(self) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError> {
        let report = self.shutdown_report();
        if let Some(error) = report.error() {
            Err(error)
        } else {
            Ok(report.into_trace())
        }
    }

    /// Requests shutdown and joins the worker, always returning terminal truth.
    pub fn shutdown_report(mut self) -> LocalSystemTerminalReport {
        let (shutdown_result, trace) = self.shutdown_inner_with_available_trace();
        match shutdown_result {
            Ok(()) => LocalSystemTerminalReport::new_with_topology(
                LocalSystemState::Closed,
                trace,
                self.topology(),
            ),
            Err(error) => LocalSystemTerminalReport::failed_with_topology_and_trace(
                error,
                self.topology(),
                trace,
            ),
        }
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
        let (result, trace) = self.shutdown_inner_with_available_trace();
        result.map(|()| trace)
    }

    fn shutdown_inner_with_available_trace(
        &mut self,
    ) -> (Result<(), ThreadedRuntimeError>, Vec<RuntimeEvent>) {
        let Some(handle) = self.handle.take() else {
            return (Ok(()), Vec::new());
        };
        let _ = self.commands.send(ThreadedCommand::Shutdown);
        match handle.join() {
            Ok(exit) => {
                if let Some(error) = exit.error {
                    self.metrics.set_state(LiveShardState::Failed);
                    (Err(error), exit.trace)
                } else {
                    self.metrics.set_state(LiveShardState::Stopped);
                    (Ok(()), exit.trace)
                }
            }
            Err(_) => {
                self.metrics.set_state(LiveShardState::Failed);
                (Err(ThreadedRuntimeError::WorkerStopped), Vec::new())
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
    metrics: Arc<LiveShardMetrics>,
) -> ThreadedWorkerExit
where
    S: Shard,
    F: MailboxFactory,
{
    metrics.set_worker_thread_id(format!("{:?}", thread::current().id()));
    let mut runtime = Runtime::with_clock_and_ids_and_driver_and_preallocation(
        shard,
        mailbox_factory,
        Box::new(MonotonicClock),
        IdSource::new(),
        Box::new(BetelgeuseDriver::with_io_loop_and_capacities(
            io_loop_factory(),
            config.storage_lane_capacity,
            config.dns_lane_capacity,
            config.tls_lane_capacity,
            config.process_lane_capacity,
            config.signal_capacity,
        )),
        config.preallocation,
    );
    runtime.set_trace_retention(config.trace_retention);

    loop {
        metrics.set_resource_counts(runtime.resource_report());
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

    let shutdown_deadline = Instant::now() + config.shutdown_lane_drain_timeout;
    let shutdown_result = runtime.cancel_in_flight_calls_for_shutdown(shutdown_deadline);
    metrics.set_resource_counts(runtime.resource_report());
    let trace = runtime.trace().to_vec();
    if shutdown_result.is_err() {
        return ThreadedWorkerExit::failed(ThreadedRuntimeError::DriverShutdownFailed, trace);
    }
    ThreadedWorkerExit::clean(trace)
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
        if config.dns_lane_capacity == 0 {
            panic!("ThreadedMultiShardRuntime requires DNS lane capacity > 0");
        }
        if config.tls_lane_capacity == 0 {
            panic!("ThreadedMultiShardRuntime requires TLS lane capacity > 0");
        }
        if config.process_lane_capacity == 0 {
            panic!("ThreadedMultiShardRuntime requires process lane capacity > 0");
        }
        if config.signal_capacity == 0 {
            panic!("ThreadedMultiShardRuntime requires signal capacity > 0");
        }
        if config.shard_pair_capacity == 0 {
            panic!("ThreadedMultiShardRuntime requires shard-pair capacity > 0");
        }
        if config.remote_inbound_drain_budget == 0 {
            panic!("ThreadedMultiShardRuntime requires remote inbound drain budget > 0");
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
        for (ordinal, shard) in shards.iter().enumerate() {
            let worker_config = ThreadedRuntimeConfig {
                configured_core: config.configured_core.map(|core| core + ordinal),
                ..config
            };
            let (sender, receiver) = std::sync::mpsc::sync_channel(config.command_capacity);
            commands.insert(shard.id(), sender);
            shard_metrics.insert(
                shard.id(),
                Arc::new(LiveShardMetrics::new(
                    shard.id(),
                    Some(format!("tina-shard-{}", shard.id().get())),
                    worker_config,
                )),
            );
            receivers.push((shard.id(), receiver));
        }
        let mut remote_metrics = BTreeMap::new();
        let mut remote_senders = BTreeMap::new();
        let mut remote_receivers: BTreeMap<
            ShardId,
            Vec<(
                ShardId,
                std::sync::mpsc::Receiver<SendableQueuedRemoteEnvelope>,
            )>,
        > = BTreeMap::new();
        for source in &shards {
            for target in &shards {
                if source.id() != target.id() {
                    let (sender, receiver) =
                        std::sync::mpsc::sync_channel(config.shard_pair_capacity);
                    remote_senders.insert((source.id(), target.id()), sender);
                    remote_receivers
                        .entry(target.id())
                        .or_default()
                        .push((source.id(), receiver));
                    remote_metrics.insert(
                        (source.id(), target.id()),
                        Arc::new(LiveQueueMetrics::new(config.shard_pair_capacity)),
                    );
                }
            }
        }

        let ids = IdSource::new();
        let mut handles = Vec::with_capacity(shards.len());
        for (ordinal, (shard, (_shard_id, receiver))) in
            shards.into_iter().zip(receivers).enumerate()
        {
            let worker_config = ThreadedRuntimeConfig {
                configured_core: config.configured_core.map(|core| core + ordinal),
                ..config
            };
            let factory = mailbox_factory.clone();
            let ids = ids.clone();
            let remote_senders = remote_senders.clone();
            let shard_id = shard.id();
            let remote_receivers = remote_receivers.remove(&shard_id).unwrap_or_default();
            let remote_metrics_for_worker = remote_metrics.clone();
            let shard_metrics_for_worker = Arc::clone(
                shard_metrics
                    .get(&shard_id)
                    .expect("shard metrics exist for worker"),
            );
            handles.push((
                shard_id,
                thread::Builder::new()
                    .name(format!("tina-shard-{}", shard_id.get()))
                    .spawn(move || {
                        let io_loop = io_loop(Global).expect(
                            "failed to initialise Betelgeuse IO loop for tina-runtime shard",
                        );
                        let runtime = Runtime::with_clock_and_ids_and_driver_and_preallocation(
                            shard,
                            factory,
                            Box::new(MonotonicClock),
                            ids,
                            Box::new(BetelgeuseDriver::with_io_loop_and_capacities(
                                io_loop,
                                worker_config.storage_lane_capacity,
                                worker_config.dns_lane_capacity,
                                worker_config.tls_lane_capacity,
                                worker_config.process_lane_capacity,
                                worker_config.signal_capacity,
                            )),
                            worker_config.preallocation,
                        );
                        let mut runtime = runtime;
                        runtime.set_trace_retention(worker_config.trace_retention);
                        threaded_worker_loop_with_remote(
                            runtime,
                            receiver,
                            worker_config,
                            remote_senders,
                            remote_receivers,
                            remote_metrics_for_worker,
                            shard_metrics_for_worker,
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
        // Phase 043 Rock 5: reject ingress to a quarantined shard
        // immediately, before the bounded sync_channel has observed
        // Disconnected. Cross-shard senders should not race with the
        // worker's natural exit window.
        if let Some(metrics) = self.shard_metrics.get(&address.shard()) {
            if metrics.state() == LiveShardState::Failed {
                metrics.ingress.rejected_closed();
                return Err(ThreadedTrySendError::WorkerStopped);
            }
        }
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

    /// Returns retained trace from shards still able to report.
    pub fn trace(&self) -> TraceSnapshot {
        let mut events = Vec::new();
        let mut missing_shards = Vec::new();
        for shard in self.commands.keys() {
            match self.call_on(*shard, |runtime| runtime.trace().to_vec()) {
                Ok(trace) => events.extend(trace),
                Err(_) => missing_shards.push(*shard),
            }
        }
        events.sort_by_key(|event| event.id());
        TraceSnapshot::partial(events, missing_shards)
    }

    /// Returns complete trace, failing if any shard can no longer report.
    pub fn complete_trace(&self) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError> {
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
        let config = self
            .shard_metrics
            .values()
            .next()
            .map(|metrics| metrics.config)
            .unwrap_or_default();
        RuntimeCapabilities::threaded_with_capacities(
            config.storage_lane_capacity,
            config.dns_lane_capacity,
            config.tls_lane_capacity,
            config.process_lane_capacity,
            config.signal_capacity,
        )
    }

    /// Requests shutdown and joins every worker.
    pub fn shutdown(self) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError> {
        let report = self.shutdown_report();
        if let Some(error) = report.error() {
            Err(error)
        } else {
            Ok(report.into_trace())
        }
    }

    /// Requests shutdown and joins every worker, always returning terminal truth.
    pub fn shutdown_report(mut self) -> LocalSystemTerminalReport {
        let (shutdown_result, trace) = self.shutdown_inner_with_available_trace();
        match shutdown_result {
            Ok(()) => LocalSystemTerminalReport::new_with_topology(
                LocalSystemState::Closed,
                trace,
                self.topology(),
            ),
            Err(error) => LocalSystemTerminalReport::failed_with_topology_and_trace(
                error,
                self.topology(),
                trace,
            ),
        }
    }

    fn call_on<R, C>(&self, shard: ShardId, command: C) -> Result<R, ThreadedRuntimeError>
    where
        R: Send + 'static,
        C: FnOnce(&mut Runtime<S, F>) -> R + Send + 'static,
    {
        let Some(sender) = self.commands.get(&shard) else {
            return Err(ThreadedRuntimeError::UnknownShard(shard));
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
        let (result, events) = self.shutdown_inner_with_available_trace();
        result.map(|()| events)
    }

    fn shutdown_inner_with_available_trace(
        &mut self,
    ) -> (Result<(), ThreadedRuntimeError>, Vec<RuntimeEvent>) {
        for sender in self.commands.values() {
            let _ = sender.send(ThreadedCommand::Shutdown);
        }

        let mut events = Vec::new();
        let mut failure = None;
        for (shard, handle) in std::mem::take(&mut self.handles) {
            match handle.join() {
                Ok(exit) => {
                    if let Some(error) = exit.error {
                        if let Some(metrics) = self.shard_metrics.get(&shard) {
                            metrics.set_state(LiveShardState::Failed);
                        }
                        failure = Some(error);
                    } else if let Some(metrics) = self.shard_metrics.get(&shard) {
                        metrics.set_state(LiveShardState::Stopped);
                    }
                    events.extend(exit.trace);
                }
                Err(_) => {
                    if let Some(metrics) = self.shard_metrics.get(&shard) {
                        metrics.set_state(LiveShardState::Failed);
                    }
                    failure = Some(ThreadedRuntimeError::WorkerStopped);
                }
            }
        }
        events.sort_by_key(|event| event.id());
        if let Some(error) = failure {
            return (Err(error), events);
        }
        (Ok(()), events)
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
    remote_senders: BTreeMap<
        (ShardId, ShardId),
        std::sync::mpsc::SyncSender<SendableQueuedRemoteEnvelope>,
    >,
    remote_receivers: Vec<(
        ShardId,
        std::sync::mpsc::Receiver<SendableQueuedRemoteEnvelope>,
    )>,
    remote_metrics: BTreeMap<(ShardId, ShardId), Arc<LiveQueueMetrics>>,
    shard_metrics: Arc<LiveShardMetrics>,
) -> ThreadedWorkerExit
where
    S: Shard,
    F: MailboxFactory,
{
    shard_metrics.set_worker_thread_id(format!("{:?}", thread::current().id()));
    let source_shard = runtime.shard().id();
    loop {
        shard_metrics.set_resource_counts(runtime.resource_report());
        let route_remote = |envelope: QueuedRemoteEnvelope| -> Result<(), SendRejectedReason> {
            let target_shard = envelope.target_shard();
            let Some(sender) = remote_senders.get(&(source_shard, target_shard)) else {
                panic!(
                    "ThreadedMultiShardRuntime targeted unknown destination shard {}",
                    target_shard.get()
                );
            };
            let envelope = SendableQueuedRemoteEnvelope::new(envelope);
            let metrics = remote_metrics.get(&(source_shard, target_shard));
            match sender.try_send(envelope) {
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
        };
        let remote_delivered = drain_remote_inbound(
            &mut runtime,
            &remote_receivers,
            &route_remote,
            config.remote_inbound_drain_budget,
        );
        if remote_delivered == 0 {
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
        }

        let delivered = runtime.step_with_remote(&mut |_, envelope| route_remote(envelope));

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

    let shutdown_deadline = Instant::now() + config.shutdown_lane_drain_timeout;
    let shutdown_result = runtime.cancel_in_flight_calls_for_shutdown(shutdown_deadline);
    shard_metrics.set_resource_counts(runtime.resource_report());
    let trace = runtime.trace().to_vec();
    if shutdown_result.is_err() {
        return ThreadedWorkerExit::failed(ThreadedRuntimeError::DriverShutdownFailed, trace);
    }
    ThreadedWorkerExit::clean(trace)
}

fn drain_remote_inbound<S, F>(
    runtime: &mut Runtime<S, F>,
    remote_receivers: &[(
        ShardId,
        std::sync::mpsc::Receiver<SendableQueuedRemoteEnvelope>,
    )],
    route_remote: &impl Fn(QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    budget: usize,
) -> usize
where
    S: Shard,
    F: MailboxFactory,
{
    let mut delivered = 0;
    for (_, receiver) in remote_receivers {
        loop {
            if delivered >= budget {
                return delivered;
            }
            match receiver.try_recv() {
                Ok(envelope) => {
                    delivered += 1;
                    if let Some(outbound) =
                        runtime.harvest_remote_envelope(envelope.into_queued_remote_envelope())
                    {
                        let _ = route_remote(outbound);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        }
    }
    delivered
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
        }
    }

    fn into_remote_call_reply(self) -> RemoteCallReply {
        let outcome = match self.outcome {
            SendableRemoteCallOutcome::Replied(reply) => {
                RemoteCallOutcome::Replied(ErasedMessage::Sendable(reply))
            }
            SendableRemoteCallOutcome::Full => RemoteCallOutcome::Full,
            SendableRemoteCallOutcome::Closed => RemoteCallOutcome::Closed,
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
