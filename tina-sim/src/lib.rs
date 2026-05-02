#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

//! Single-shard virtual-time simulator for `tina-rs`.
//!
//! Phase 016 started Voyager with the narrowest honest simulator slice:
//!
//! - one shard
//! - virtual monotonic time
//! - the shipped `Sleep { after }` / `TimerFired` contract
//! - deterministic replay artifacts
//!
//! Phase 017 widens that slice just enough to make replayed divergence useful:
//!
//! - seeded perturbation over local-send and timer-wake delivery
//! - a small checker surface that can halt a run with a structured reason
//! - replay artifacts that preserve checker failures alongside the event record
//!
//! Phase 018 adds single-shard spawn and supervision replay:
//!
//! - public `ChildDefinition` / `RestartableChildDefinition` execution
//! - runtime-owned direct parent-child lineage and restartable child records
//! - direct-child `RestartChildren` and supervised panic restart
//! - bootstrap re-delivery, stale identity rejection, and budget exhaustion
//!   through the live runtime event vocabulary
//!
//! Phase 019 adds scripted single-shard TCP simulation:
//!
//! - bind, accept, read, write, listener close, and stream close
//! - replayable peer-visible output
//! - checker-backed TCP completion perturbation replay
//!
//! `tina-sim` intentionally reuses the live runtime's event vocabulary from
//! `tina-runtime` instead of inventing a second user-visible meaning model.
//! The simulator is still narrower than the live runtime: it supports local
//! sends, stop, reply/noop observation, ordered `Batch`, runtime-owned
//! `Sleep`, single-shard spawn/supervision, and scripted single-shard TCP
//! simulation.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::time::Duration;

use tina::{
    Address, AddressGeneration, ChildDefinition, ChildRelation, Context, Effect, Isolate,
    IsolateId, Outbound as TinaOutbound, RestartBudgetState, RestartableChildDefinition, Shard,
    ShardId, TrySendError,
};
use tina_runtime::{
    CallCompletionRejectedReason, CallError, CallId, CallInput, CallKind, CallOutput, EffectKind,
    ListenerId, RestartSkippedReason, RuntimeCall, RuntimeEvent, RuntimeEventKind,
    SendRejectedReason, SupervisionRejectedReason,
};
use tina_supervisor::SupervisorConfig;

/// Deterministic simulator configuration for one run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SimulatorConfig {
    /// Replay seed that drives the simulator's narrow seeded perturbation
    /// surface.
    pub seed: u64,

    /// Seeded fault configuration for this run.
    pub faults: FaultConfig,

    /// Scripted TCP configuration for this run.
    pub tcp: ScriptedTcpConfig,
}

/// Captured replay data for one deterministic simulator run.
///
/// Replay means rerunning the same workload under the same simulator config
/// and reproducing the same event record, final virtual time, and checker
/// failure (when one occurred).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayArtifact {
    config: SimulatorConfig,
    final_time: Duration,
    event_record: Vec<RuntimeEvent>,
    checker_failure: Option<CheckerFailure>,
    observed_peer_output: Vec<ObservedPeerOutput>,
}

impl ReplayArtifact {
    /// Returns the simulator config used for the original run.
    pub fn config(&self) -> &SimulatorConfig {
        &self.config
    }

    /// Returns the final virtual time reached by the run.
    pub const fn final_time(&self) -> Duration {
        self.final_time
    }

    /// Returns the deterministic event record captured for the run.
    pub fn event_record(&self) -> &[RuntimeEvent] {
        &self.event_record
    }

    /// Returns the optional checker failure captured for the run.
    pub fn checker_failure(&self) -> Option<&CheckerFailure> {
        self.checker_failure.as_ref()
    }

    /// Returns the peer-visible output captured during the run.
    pub fn observed_peer_output(&self) -> &[ObservedPeerOutput] {
        &self.observed_peer_output
    }
}

/// Captured replay data for one deterministic multi-shard simulator run.
///
/// Like [`ReplayArtifact`], this records one whole-run event record rather
/// than splitting replay state into per-shard artifacts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiShardReplayArtifact {
    simulator_config: SimulatorConfig,
    multishard_config: MultiShardSimulatorConfig,
    final_time: Duration,
    event_record: Vec<RuntimeEvent>,
    checker_failure: Option<CheckerFailure>,
    observed_peer_output: Vec<ObservedPeerOutput>,
}

impl MultiShardReplayArtifact {
    /// Returns the single-shard simulator config cloned onto each shard.
    pub fn simulator_config(&self) -> &SimulatorConfig {
        &self.simulator_config
    }

    /// Returns the multi-shard coordinator config used for the run.
    pub const fn multishard_config(&self) -> MultiShardSimulatorConfig {
        self.multishard_config
    }

    /// Returns the final shared virtual time reached by the run.
    pub const fn final_time(&self) -> Duration {
        self.final_time
    }

    /// Returns the deterministic whole-run event record captured for the run.
    pub fn event_record(&self) -> &[RuntimeEvent] {
        &self.event_record
    }

    /// Returns the optional checker failure captured for the run.
    pub fn checker_failure(&self) -> Option<&CheckerFailure> {
        self.checker_failure.as_ref()
    }

    /// Returns the peer-visible output captured during the run.
    pub fn observed_peer_output(&self) -> &[ObservedPeerOutput] {
        &self.observed_peer_output
    }
}

/// Seeded fault configuration for one simulator run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct FaultConfig {
    /// Perturbation applied to local `Send` delivery.
    pub local_send: LocalSendFaultMode,
    /// Perturbation applied to timer wake delivery.
    pub timer_wake: FaultMode,
    /// Perturbation applied to ready TCP completions.
    pub tcp_completion: TcpCompletionFaultMode,
}

/// One narrow seeded local-send fault mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LocalSendFaultMode {
    /// No perturbation.
    #[default]
    None,

    /// Delay matching sends by a deterministic number of extra delivery
    /// rounds.
    DelayByRounds {
        /// One out of every `one_in` candidate sends is delayed.
        one_in: u64,

        /// Extra delivery rounds skipped before the message becomes visible.
        rounds: u64,
    },
}

/// One narrow seeded fault mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FaultMode {
    /// No perturbation.
    #[default]
    None,

    /// Delay matching events by a deterministic amount.
    DelayBy {
        /// One out of every `one_in` candidate events is delayed.
        one_in: u64,

        /// Extra virtual delay applied when the fault fires.
        by: Duration,
    },
}

/// One narrow seeded TCP-completion fault mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TcpCompletionFaultMode {
    /// No perturbation.
    #[default]
    None,

    /// Delay matching TCP completions by a deterministic number of extra
    /// simulator steps.
    DelayBySteps {
        /// One out of every `one_in` candidate completions is delayed.
        one_in: u64,

        /// Extra simulator steps skipped before the completion becomes visible.
        steps: u64,
    },

    /// Reorder ready TCP completions that tie in the same simulator step while
    /// still preserving per-resource FIFO.
    ReorderReady {
        /// One out of every `one_in` ready batches is reordered.
        one_in: u64,
    },
}

/// Scripted TCP configuration for one simulator run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedTcpConfig {
    /// Maximum number of async TCP completions that may be pending inside the
    /// simulator at once.
    pub pending_completion_capacity: usize,

    /// Listener scripts available to `TcpBind`.
    pub listeners: Vec<ScriptedListenerConfig>,
}

impl Default for ScriptedTcpConfig {
    fn default() -> Self {
        Self {
            pending_completion_capacity: usize::MAX,
            listeners: Vec::new(),
        }
    }
}

/// Scripted listener configuration for one bindable address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedListenerConfig {
    /// The address isolates request in `CallInput::TcpBind`.
    pub bind_addr: SocketAddr,

    /// The local address returned through `CallOutput::TcpBound`.
    pub local_addr: SocketAddr,

    /// Maximum number of accepted-but-not-yet-consumed peers that may wait in
    /// the listener's ready queue.
    pub backlog_capacity: usize,

    /// Scripted peers that may arrive on this listener.
    pub peers: Vec<ScriptedPeerConfig>,
}

/// Scripted peer configuration for one simulated accepted connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedPeerConfig {
    /// The first simulator step on which this peer may become visible to a
    /// pending `TcpAccept`.
    pub accept_after_step: u64,

    /// The remote address returned through `CallOutput::TcpAccepted`.
    pub peer_addr: SocketAddr,

    /// Inbound byte chunks the peer will make readable to the isolate.
    pub inbound_chunks: Vec<Vec<u8>>,

    /// Explicit bound for the peer's readable-buffer bytes.
    pub inbound_capacity: usize,

    /// Optional extra cap applied to each read result after `max_len`.
    pub read_chunk_cap: Option<usize>,

    /// Maximum number of bytes one write completion may accept.
    pub write_cap: usize,

    /// Explicit bound for the bytes the peer will accept from the isolate.
    pub output_capacity: usize,
}

/// One captured peer-visible output stream from a simulated run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedPeerOutput {
    peer_addr: SocketAddr,
    bytes: Vec<u8>,
}

impl ObservedPeerOutput {
    /// Returns the peer address associated with this captured output.
    pub const fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    /// Returns the bytes written to this simulated peer.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Decision returned by a simulator checker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckerDecision {
    /// Keep running.
    Continue,

    /// Halt the run with a structured reason.
    Fail(String),
}

/// Small user-facing checker surface for `tina-sim`.
pub trait Checker {
    /// Stable checker name recorded into replay artifacts.
    fn name(&self) -> &'static str;

    /// Observes one emitted runtime event and decides whether the run should
    /// continue.
    fn on_event(&mut self, event: &RuntimeEvent) -> CheckerDecision;
}

/// Structured checker failure captured by the simulator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckerFailure {
    checker_name: &'static str,
    event_id: tina_runtime::EventId,
    reason: String,
}

impl CheckerFailure {
    /// Returns the checker name.
    pub const fn checker_name(&self) -> &'static str {
        self.checker_name
    }

    /// Returns the event that triggered the failure.
    pub const fn event_id(&self) -> tina_runtime::EventId {
        self.event_id
    }

    /// Returns the failure reason.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RegisteredAddress {
    shard: ShardId,
    isolate: IsolateId,
    generation: AddressGeneration,
}

#[derive(Debug)]
struct InFlightCall {
    call_id: CallId,
    call_kind: CallKind,
    requester: RegisteredAddress,
    cause: tina_runtime::CauseId,
}

type ErasedTranslator = Box<dyn FnOnce(CallOutput) -> Box<dyn Any>>;

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

#[derive(Debug)]
struct TimerEntry {
    call_id: CallId,
    deadline: Duration,
    insertion_order: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TcpResourceKey {
    Listener(ListenerId),
    Stream(tina_runtime::StreamId),
}

#[derive(Debug)]
struct PendingTcpCompletion {
    call_id: CallId,
    ready_at_step: u64,
    insertion_order: u64,
    resource: TcpResourceKey,
    result: CallOutput,
}

#[derive(Debug)]
struct PendingAccept {
    call_id: CallId,
    listener: ListenerId,
    insertion_order: u64,
}

#[derive(Debug)]
struct ScriptedPeerState {
    accept_after_step: u64,
    peer_addr: SocketAddr,
    inbound_chunks: VecDeque<Vec<u8>>,
    read_chunk_cap: Option<usize>,
    write_cap: usize,
    output_capacity: usize,
    output: Vec<u8>,
}

#[derive(Debug)]
struct ListenerState {
    id: ListenerId,
    bind_addr: SocketAddr,
    local_addr: SocketAddr,
    backlog_capacity: usize,
    future_peers: VecDeque<ScriptedPeerState>,
    ready_peers: VecDeque<ScriptedPeerState>,
    closed: bool,
}

#[derive(Debug)]
struct StreamState {
    id: tina_runtime::StreamId,
    peer_addr: SocketAddr,
    inbound_chunks: VecDeque<Vec<u8>>,
    read_chunk_cap: Option<usize>,
    write_cap: usize,
    output_capacity: usize,
    output: Vec<u8>,
    closed: bool,
}

struct QueuedMessage {
    message: Box<dyn Any>,
    visible_at_step: u64,
}

struct LocalInbox {
    capacity: usize,
    queue: RefCell<VecDeque<QueuedMessage>>,
    closed: Cell<bool>,
}

impl std::fmt::Debug for LocalInbox {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalInbox")
            .field("capacity", &self.capacity)
            .field("closed", &self.closed.get())
            .field("len", &self.queue.borrow().len())
            .finish()
    }
}

impl LocalInbox {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: RefCell::new(VecDeque::new()),
            closed: Cell::new(false),
        }
    }

    fn push(
        &self,
        message: Box<dyn Any>,
        visible_at_step: u64,
    ) -> Result<(), TrySendError<Box<dyn Any>>> {
        if self.closed.get() {
            return Err(TrySendError::Closed(message));
        }
        let mut queue = self.queue.borrow_mut();
        if queue.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        queue.push_back(QueuedMessage {
            message,
            visible_at_step,
        });
        Ok(())
    }

    fn pop_visible(&self, current_step: u64) -> Option<Box<dyn Any>> {
        let mut queue = self.queue.borrow_mut();
        let visible = queue
            .front()
            .map(|entry| entry.visible_at_step <= current_step)
            .unwrap_or(false);
        if visible {
            queue.pop_front().map(|entry| entry.message)
        } else {
            None
        }
    }

    fn close(&self) {
        self.closed.set(true);
    }

    fn has_pending(&self) -> bool {
        !self.queue.borrow().is_empty()
    }
}

trait ErasedHandler<S>
where
    S: Shard,
{
    fn handle_boxed(
        &mut self,
        message: Box<dyn Any>,
        shard: &mut S,
        isolate_id: IsolateId,
    ) -> ErasedEffect<S>;
}

trait ErasedSpawn<S>
where
    S: Shard,
{
    fn spawn(self: Box<Self>, sim: &mut Simulator<S>, parent: IsolateId) -> SpawnOutcome<S>;
}

trait ErasedRestartRecipe<S>
where
    S: Shard,
{
    fn create(&self, sim: &mut Simulator<S>, parent: IsolateId) -> SpawnOutcome<S>;
}

trait IntoErasedSpawn<S>
where
    S: Shard,
{
    fn into_erased_spawn(self) -> Box<dyn ErasedSpawn<S>>;
}

struct HandlerAdapter<I, Outbound>
where
    I: Isolate,
{
    isolate: I,
    marker: PhantomData<fn(Outbound) -> Outbound>,
}

impl<I, S, Msg, Outbound> ErasedHandler<S> for HandlerAdapter<I, Outbound>
where
    I: Isolate<Message = Msg, Shard = S, Send = TinaOutbound<Outbound>, Call = RuntimeCall<Msg>>
        + 'static,
    I::Spawn: IntoErasedSpawn<S> + 'static,
    Msg: 'static,
    Outbound: 'static,
    S: Shard,
{
    fn handle_boxed(
        &mut self,
        message: Box<dyn Any>,
        shard: &mut S,
        isolate_id: IsolateId,
    ) -> ErasedEffect<S> {
        let message = message.downcast::<Msg>().unwrap_or_else(|_| {
            panic!("simulator attempted to deliver a handler message with the wrong type")
        });

        let effect = {
            let mut ctx = Context::new(shard, isolate_id);
            self.isolate.handle(*message, &mut ctx)
        };

        erase_effect::<I, S, Msg, Outbound>(effect)
    }
}

fn erase_effect<I, S, Msg, Outbound>(effect: Effect<I>) -> ErasedEffect<S>
where
    I: Isolate<Message = Msg, Shard = S, Send = TinaOutbound<Outbound>, Call = RuntimeCall<Msg>>
        + 'static,
    I::Spawn: IntoErasedSpawn<S> + 'static,
    Msg: 'static,
    Outbound: 'static,
    S: Shard,
{
    match effect {
        Effect::Noop => ErasedEffect::Noop,
        Effect::Reply(_) => ErasedEffect::Reply,
        Effect::Send(send) => {
            let (destination, message) = send.into_parts();
            ErasedEffect::Send(ErasedSend {
                target_shard: destination.shard(),
                target_isolate: destination.isolate(),
                target_generation: destination.generation(),
                message: Box::new(message),
            })
        }
        Effect::Spawn(spawn) => ErasedEffect::Spawn(spawn.into_erased_spawn()),
        Effect::Stop => ErasedEffect::Stop,
        Effect::RestartChildren => ErasedEffect::RestartChildren,
        Effect::Call(call) => {
            let (request, translator) = call.into_parts();
            ErasedEffect::Call(ErasedCall {
                request,
                translator: Box::new(move |result| Box::new(translator(result)) as Box<dyn Any>),
            })
        }
        Effect::Batch(effects) => ErasedEffect::Batch(
            effects
                .into_iter()
                .map(erase_effect::<I, S, Msg, Outbound>)
                .collect(),
        ),
    }
}

struct RegisteredEntry<S>
where
    S: Shard,
{
    id: IsolateId,
    generation: AddressGeneration,
    parent: Option<IsolateId>,
    stopped: Cell<bool>,
    stopped_event: Cell<Option<tina_runtime::EventId>>,
    inbox: LocalInbox,
    handler: RefCell<Box<dyn ErasedHandler<S>>>,
}

impl<S> std::fmt::Debug for RegisteredEntry<S>
where
    S: Shard,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RegisteredEntry")
            .field("id", &self.id)
            .field("generation", &self.generation)
            .field("parent", &self.parent)
            .field("stopped", &self.stopped.get())
            .field("stopped_event", &self.stopped_event.get())
            .finish_non_exhaustive()
    }
}

enum ErasedEffect<S>
where
    S: Shard,
{
    Noop,
    Reply,
    Send(ErasedSend),
    Spawn(Box<dyn ErasedSpawn<S>>),
    Stop,
    RestartChildren,
    Call(ErasedCall),
    Batch(Vec<ErasedEffect<S>>),
}

impl<S> ErasedEffect<S>
where
    S: Shard,
{
    fn kind(&self) -> EffectKind {
        match self {
            Self::Noop => EffectKind::Noop,
            Self::Reply => EffectKind::Reply,
            Self::Send(_) => EffectKind::Send,
            Self::Spawn(_) => EffectKind::Spawn,
            Self::Stop => EffectKind::Stop,
            Self::RestartChildren => EffectKind::RestartChildren,
            Self::Call(_) => EffectKind::Call,
            Self::Batch(_) => EffectKind::Batch,
        }
    }
}

struct ErasedSend {
    target_shard: ShardId,
    target_isolate: IsolateId,
    target_generation: AddressGeneration,
    message: Box<dyn Any>,
}

struct QueuedRemoteSend {
    send: ErasedSend,
    cause: tina_runtime::CauseId,
}

struct ErasedCall {
    request: CallInput,
    translator: ErasedTranslator,
}

struct SpawnOutcome<S>
where
    S: Shard,
{
    child: RegisteredAddress,
    mailbox_capacity: usize,
    restart_recipe: Option<Rc<dyn ErasedRestartRecipe<S>>>,
    bootstrap_message: Option<Box<dyn Any>>,
}

struct ChildRecord<S>
where
    S: Shard,
{
    parent: IsolateId,
    child: RegisteredAddress,
    child_ordinal: usize,
    mailbox_capacity: usize,
    restart_recipe: Option<Rc<dyn ErasedRestartRecipe<S>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SupervisorRecord {
    parent: RegisteredAddress,
    config: SupervisorConfig,
    budget_state: RestartBudgetState,
}

#[derive(Debug, Clone)]
struct IdSource {
    next_event_id: Rc<Cell<u64>>,
    next_call_id: Rc<Cell<u64>>,
}

impl IdSource {
    fn new() -> Self {
        Self {
            next_event_id: Rc::new(Cell::new(1)),
            next_call_id: Rc::new(Cell::new(1)),
        }
    }

    fn next_event_id(&self) -> tina_runtime::EventId {
        let raw = self.next_event_id.get();
        self.next_event_id.set(raw + 1);
        tina_runtime::EventId::new(raw)
    }

    fn next_call_id(&self) -> CallId {
        let raw = self.next_call_id.get();
        self.next_call_id.set(raw + 1);
        CallId::new(raw)
    }
}

/// Narrow single-shard simulator for timer-driven `tina-rs` workloads.
pub struct Simulator<S>
where
    S: Shard,
{
    shard: S,
    config: SimulatorConfig,
    entries: Vec<RegisteredEntry<S>>,
    child_records: Vec<ChildRecord<S>>,
    supervisors: Vec<SupervisorRecord>,
    next_isolate_id: u64,
    next_listener_id: u64,
    next_stream_id: u64,
    ids: IdSource,
    trace: Vec<RuntimeEvent>,
    virtual_now: Duration,
    step_ordinal: u64,
    timers: Vec<TimerEntry>,
    next_timer_ordinal: u64,
    next_send_ordinal: u64,
    next_tcp_completion_ordinal: u64,
    listeners: Vec<ListenerState>,
    streams: Vec<StreamState>,
    pending_accepts: Vec<PendingAccept>,
    pending_tcp_completions: Vec<PendingTcpCompletion>,
    in_flight_calls: Vec<InFlightCall>,
    translators: Vec<StoredTranslator>,
    last_checker_failure: Option<CheckerFailure>,
}

impl<S> Simulator<S>
where
    S: Shard,
{
    /// Creates a new simulator with virtual time starting at zero.
    pub fn new(shard: S, config: SimulatorConfig) -> Self {
        Self::with_ids(shard, config, IdSource::new())
    }

    fn with_ids(shard: S, config: SimulatorConfig, ids: IdSource) -> Self {
        Self {
            shard,
            config,
            entries: Vec::new(),
            child_records: Vec::new(),
            supervisors: Vec::new(),
            next_isolate_id: 1,
            next_listener_id: 1,
            next_stream_id: 1,
            ids,
            trace: Vec::new(),
            virtual_now: Duration::ZERO,
            step_ordinal: 0,
            timers: Vec::new(),
            next_timer_ordinal: 0,
            next_send_ordinal: 0,
            next_tcp_completion_ordinal: 0,
            listeners: Vec::new(),
            streams: Vec::new(),
            pending_accepts: Vec::new(),
            pending_tcp_completions: Vec::new(),
            in_flight_calls: Vec::new(),
            translators: Vec::new(),
            last_checker_failure: None,
        }
    }

    /// Returns the immutable simulator config for this run.
    pub fn config(&self) -> &SimulatorConfig {
        &self.config
    }

    /// Returns the current virtual monotonic time.
    pub const fn now(&self) -> Duration {
        self.virtual_now
    }

    /// Returns the accumulated deterministic event record.
    pub fn trace(&self) -> &[RuntimeEvent] {
        &self.trace
    }

    /// Returns whether the simulator still has pending timers or undelivered
    /// call completions.
    pub fn has_in_flight_calls(&self) -> bool {
        !self.in_flight_calls.is_empty()
            || !self.timers.is_empty()
            || !self.pending_tcp_completions.is_empty()
            || !self.pending_accepts.is_empty()
    }

    /// Registers one isolate and returns its typed address.
    ///
    /// 016 intentionally requires spawnless isolates that use the current
    /// runtime's timer-aware call vocabulary and local `Outbound` sends.
    #[allow(private_bounds)]
    pub fn register<I, Msg, Outbound>(&mut self, isolate: I) -> Address<Msg>
    where
        I: Isolate<
                Message = Msg,
                Shard = S,
                Send = TinaOutbound<Outbound>,
                Call = RuntimeCall<Msg>,
            > + 'static,
        I::Spawn: IntoErasedSpawn<S> + 'static,
        Msg: 'static,
        Outbound: 'static,
    {
        let address = self.register_entry::<I, Msg, Outbound>(isolate, None, usize::MAX);
        Address::new_with_generation(address.shard, address.isolate, address.generation)
    }

    /// Registers one isolate with an explicit simulator mailbox capacity.
    #[allow(private_bounds)]
    pub fn register_with_mailbox_capacity<I, Msg, Outbound>(
        &mut self,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Address<Msg>
    where
        I: Isolate<
                Message = Msg,
                Shard = S,
                Send = TinaOutbound<Outbound>,
                Call = RuntimeCall<Msg>,
            > + 'static,
        I::Spawn: IntoErasedSpawn<S> + 'static,
        Msg: 'static,
        Outbound: 'static,
    {
        let address = self.register_entry::<I, Msg, Outbound>(isolate, None, mailbox_capacity);
        Address::new_with_generation(address.shard, address.isolate, address.generation)
    }

    /// Configures a registered isolate as supervisor for its direct children.
    ///
    /// The simulator mirrors `tina-runtime`: supervision applies to
    /// direct children only, and reconfiguring a parent resets the
    /// runtime-lifetime restart budget tracker.
    pub fn supervise<M: 'static>(&mut self, parent: Address<M>, config: SupervisorConfig) {
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

    /// Attempts to inject one typed message into a registered isolate.
    pub fn try_send<M: 'static>(
        &self,
        address: Address<M>,
        message: M,
    ) -> Result<(), TrySendError<M>> {
        if address.shard() != self.shard.id() {
            panic!(
                "cross-shard simulator ingress is out of scope in this slice: target shard {} != simulator shard {}",
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
                "simulator ingress targeted unknown isolate {} on shard {}",
                address.isolate().get(),
                address.shard().get(),
            );
        };

        if entry.generation != address.generation() || entry.stopped.get() {
            return Err(TrySendError::Closed(message));
        }

        match entry.inbox.push(Box::new(message), self.step_ordinal) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(message)) => {
                Err(TrySendError::Full(*message.downcast::<M>().unwrap_or_else(
                    |_| panic!("simulator ingress hit wrong mailbox type"),
                )))
            }
            Err(TrySendError::Closed(message)) => Err(TrySendError::Closed(
                *message
                    .downcast::<M>()
                    .unwrap_or_else(|_| panic!("simulator ingress hit wrong mailbox type")),
            )),
        }
    }

    /// Advances virtual monotonic time by `by`.
    pub fn advance_time(&mut self, by: Duration) {
        self.virtual_now += by;
    }

    /// Advances virtual time directly to the next due timer, if any.
    pub fn advance_to_next_timer(&mut self) -> bool {
        let Some(next_deadline) = self.timers.iter().map(|entry| entry.deadline).min() else {
            return false;
        };
        if next_deadline > self.virtual_now {
            self.virtual_now = next_deadline;
        }
        true
    }

    /// Runs one deterministic simulator round.
    ///
    /// The simulator samples virtual time once at the start of the round,
    /// harvests due timers, and then gives each registered isolate at most one
    /// delivery chance in registration order.
    pub fn step(&mut self) -> usize {
        let shard_id = self.shard.id();
        self.step_with_remote(&mut |_source_shard, send, _cause| {
            panic!(
                "cross-shard simulation is out of scope in this slice: target shard {} != simulator shard {}",
                send.target_shard.get(),
                shard_id.get(),
            );
        })
    }

    fn step_with_remote<FR>(&mut self, route_remote: &mut FR) -> usize
    where
        FR: FnMut(ShardId, ErasedSend, tina_runtime::CauseId) -> Result<(), SendRejectedReason>,
    {
        self.step_ordinal += 1;
        let now = self.virtual_now;
        self.harvest_timers(now);
        self.harvest_tcp();

        let mut round_messages: Vec<Option<Box<dyn Any>>> = self
            .entries
            .iter()
            .map(|entry| {
                if entry.stopped.get() {
                    None
                } else {
                    entry.inbox.pop_visible(self.step_ordinal)
                }
            })
            .collect();

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
                    handler.handle_boxed(message, &mut self.shard, isolate_id)
                }))
            };

            let effect = match effect {
                Ok(effect) => effect,
                Err(_) => {
                    let panicked = self.push_event(
                        isolate_id,
                        Some(handler_started.into()),
                        RuntimeEventKind::HandlerPanicked,
                    );
                    self.stop_entry(index, isolate_id, panicked.into());
                    self.supervise_panic(
                        RegisteredAddress {
                            shard: self.shard.id(),
                            isolate: isolate_id,
                            generation: self.entries[index].generation,
                        },
                        panicked.into(),
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
                isolate_id,
                handler_finished.into(),
                effect,
                &mut round_messages,
                route_remote,
            );
        }

        delivered
    }

    /// Continues running until the simulator becomes quiescent.
    ///
    /// When no messages are ready but timers remain pending, the simulator
    /// advances virtual time to the earliest pending deadline and keeps
    /// stepping. This is the simplest honest driver for the first replayable
    /// Voyager slice.
    pub fn run_until_quiescent(&mut self) -> usize {
        let mut total = 0;
        loop {
            let delivered = self.step();
            total += delivered;
            if delivered > 0 {
                continue;
            }
            if self.advance_to_next_timer() {
                continue;
            }
            if self.has_in_flight_calls() {
                continue;
            }
            if self.has_pending_messages() {
                continue;
            }
            break total;
        }
    }

    /// Runs until quiescent or until a checker halts the run.
    pub fn run_until_quiescent_checked<C: Checker>(
        &mut self,
        checker: &mut C,
    ) -> Option<CheckerFailure> {
        self.last_checker_failure = None;
        let mut observed_len = 0;
        loop {
            let delivered = self.step();
            if let Some(failure) = self.observe_new_events(checker, &mut observed_len) {
                self.last_checker_failure = Some(failure.clone());
                return Some(failure);
            }
            if delivered > 0 {
                continue;
            }
            if self.advance_to_next_timer() {
                continue;
            }
            if self.has_in_flight_calls() {
                continue;
            }
            if self.has_pending_messages() {
                continue;
            }
            return None;
        }
    }

    /// Captures a deterministic replay artifact for the current run.
    pub fn replay_artifact(&self) -> ReplayArtifact {
        ReplayArtifact {
            config: self.config.clone(),
            final_time: self.virtual_now,
            event_record: self.trace.clone(),
            checker_failure: self.last_checker_failure.clone(),
            observed_peer_output: self.observed_peer_output(),
        }
    }

    fn observe_new_events<C: Checker>(
        &self,
        checker: &mut C,
        observed_len: &mut usize,
    ) -> Option<CheckerFailure> {
        while *observed_len < self.trace.len() {
            let event = self.trace[*observed_len];
            *observed_len += 1;
            match checker.on_event(&event) {
                CheckerDecision::Continue => {}
                CheckerDecision::Fail(reason) => {
                    return Some(CheckerFailure {
                        checker_name: checker.name(),
                        event_id: event.id(),
                        reason,
                    });
                }
            }
        }
        None
    }

    fn execute_effect(
        &mut self,
        index: usize,
        isolate_id: IsolateId,
        cause: tina_runtime::CauseId,
        effect: ErasedEffect<S>,
        round_messages: &mut [Option<Box<dyn Any>>],
        route_remote: &mut impl FnMut(
            ShardId,
            ErasedSend,
            tina_runtime::CauseId,
        ) -> Result<(), SendRejectedReason>,
    ) -> bool {
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
            ErasedEffect::Call(call) => {
                let requester = RegisteredAddress {
                    shard: self.shard.id(),
                    isolate: isolate_id,
                    generation: self.entries[index].generation,
                };
                self.dispatch_call(call, requester, cause);
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
            ErasedEffect::Reply => {
                self.push_event(
                    isolate_id,
                    Some(cause),
                    RuntimeEventKind::EffectObserved {
                        effect: EffectKind::Reply,
                    },
                );
                false
            }
            ErasedEffect::RestartChildren => {
                self.restart_children(isolate_id, cause, round_messages);
                false
            }
            ErasedEffect::Batch(effects) => {
                for subeffect in effects {
                    if self.execute_effect(
                        index,
                        isolate_id,
                        cause,
                        subeffect,
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

    fn register_entry<I, Msg, Outbound>(
        &mut self,
        isolate: I,
        parent: Option<IsolateId>,
        mailbox_capacity: usize,
    ) -> RegisteredAddress
    where
        I: Isolate<
                Message = Msg,
                Shard = S,
                Send = TinaOutbound<Outbound>,
                Call = RuntimeCall<Msg>,
            > + 'static,
        I::Spawn: IntoErasedSpawn<S> + 'static,
        Msg: 'static,
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
            inbox: LocalInbox::new(mailbox_capacity),
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

    fn spawn_isolate<I, Msg, Outbound>(
        &mut self,
        parent: IsolateId,
        isolate: I,
        mailbox_capacity: usize,
        bootstrap_message: Option<Msg>,
    ) -> SpawnOutcome<S>
    where
        I: Isolate<
                Message = Msg,
                Shard = S,
                Send = TinaOutbound<Outbound>,
                Call = RuntimeCall<Msg>,
            > + 'static,
        I::Spawn: IntoErasedSpawn<S> + 'static,
        Msg: 'static,
        Outbound: 'static,
    {
        if mailbox_capacity == 0 {
            panic!("spawn requested mailbox capacity 0, which is out of scope for this slice");
        }

        let child =
            self.register_entry::<I, Msg, Outbound>(isolate, Some(parent), mailbox_capacity);

        SpawnOutcome {
            child,
            mailbox_capacity,
            restart_recipe: None,
            bootstrap_message: bootstrap_message.map(|message| Box::new(message) as Box<dyn Any>),
        }
    }

    fn record_child(&mut self, parent: IsolateId, outcome: SpawnOutcome<S>) {
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

    fn enqueue_bootstrap_message(
        &mut self,
        child: RegisteredAddress,
        message: Box<dyn Any>,
        cause: tina_runtime::CauseId,
    ) {
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.id == child.isolate && entry.generation == child.generation)
            .unwrap_or_else(|| panic!("bootstrap referenced unknown child {:?}", child.isolate));
        entry
            .inbox
            .push(message, self.step_ordinal)
            .unwrap_or_else(|_| {
                panic!(
                    "simulator failed to enqueue bootstrap message for child {:?}",
                    child.isolate
                )
            });
        self.push_event(
            child.isolate,
            Some(cause),
            RuntimeEventKind::MailboxAccepted,
        );
    }

    fn restart_children(
        &mut self,
        parent: IsolateId,
        cause: tina_runtime::CauseId,
        round_messages: &mut [Option<Box<dyn Any>>],
    ) {
        let child_record_indices: Vec<usize> = self
            .child_records
            .iter()
            .enumerate()
            .filter_map(|(index, record)| (record.parent == parent).then_some(index))
            .collect();

        for child_record_index in child_record_indices {
            self.restart_child_record(parent, child_record_index, cause, round_messages);
        }
    }

    fn supervise_panic(
        &mut self,
        failed_child: RegisteredAddress,
        cause: tina_runtime::CauseId,
        round_messages: &mut [Option<Box<dyn Any>>],
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

        let selected: Vec<usize> = self
            .child_records
            .iter()
            .enumerate()
            .filter_map(|(index, record)| {
                if record.parent != parent {
                    return None;
                }

                let relation = ChildRelation::from_ordinals(record.child_ordinal, failed_ordinal);
                policy.restarts(relation).then_some(index)
            })
            .collect();

        for child_record_index in selected {
            self.restart_child_record(parent, child_record_index, triggered.into(), round_messages);
        }
    }

    fn restart_child_record(
        &mut self,
        parent: IsolateId,
        child_record_index: usize,
        cause: tina_runtime::CauseId,
        round_messages: &mut [Option<Box<dyn Any>>],
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

    fn checked_registered_address<M: 'static>(
        &self,
        address: Address<M>,
        operation: &str,
    ) -> RegisteredAddress {
        if address.shard() != self.shard.id() {
            panic!(
                "{operation} targeted a parent on another shard: target shard {} != simulator shard {}",
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

    fn entry_index(&self, address: RegisteredAddress) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.id == address.isolate && entry.generation == address.generation)
    }

    fn entry_by_isolate(&self, isolate: IsolateId) -> Option<&RegisteredEntry<S>> {
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

    fn dispatch_call(
        &mut self,
        call: ErasedCall,
        requester: RegisteredAddress,
        cause: tina_runtime::CauseId,
    ) {
        let call_id = self.ids.next_call_id();
        let call_kind = call_kind(&call.request);
        self.push_event(
            requester.isolate,
            Some(cause),
            RuntimeEventKind::CallDispatchAttempted { call_id, call_kind },
        );

        let ErasedCall {
            request,
            translator,
        } = call;
        self.in_flight_calls.push(InFlightCall {
            call_id,
            call_kind,
            requester,
            cause,
        });
        self.translators.push(StoredTranslator {
            call_id,
            translator: Some(translator),
        });

        match request {
            CallInput::Sleep { after } => {
                let insertion_order = self.next_timer_ordinal;
                let deadline = self.virtual_now
                    + after
                    + self.fault_delay(self.config.faults.timer_wake, insertion_order, 0x5449_4d45);
                self.next_timer_ordinal += 1;
                self.timers.push(TimerEntry {
                    call_id,
                    deadline,
                    insertion_order,
                });
            }
            CallInput::TcpBind { addr } => self.handle_tcp_bind(call_id, addr),
            CallInput::TcpAccept { listener } => self.handle_tcp_accept(call_id, listener),
            CallInput::TcpRead { stream, max_len } => {
                self.handle_tcp_read(call_id, stream, max_len)
            }
            CallInput::TcpWrite { stream, bytes } => self.handle_tcp_write(call_id, stream, bytes),
            CallInput::TcpListenerClose { listener } => {
                self.handle_tcp_listener_close(call_id, listener)
            }
            CallInput::TcpStreamClose { stream } => self.handle_tcp_stream_close(call_id, stream),
        }
    }

    fn handle_tcp_bind(&mut self, call_id: CallId, addr: SocketAddr) {
        if self
            .listeners
            .iter()
            .any(|listener| listener.bind_addr == addr && !listener.closed)
        {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        }

        let Some(script) = self
            .config
            .tcp
            .listeners
            .iter()
            .find(|listener| listener.bind_addr == addr)
            .cloned()
        else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        };

        let listener = ListenerState {
            id: ListenerId::new(self.next_listener_id),
            bind_addr: script.bind_addr,
            local_addr: script.local_addr,
            backlog_capacity: script.backlog_capacity,
            future_peers: script
                .peers
                .into_iter()
                .map(|peer| {
                    let inbound_len = peer.inbound_chunks.iter().map(Vec::len).sum::<usize>();
                    if inbound_len > peer.inbound_capacity {
                        panic!(
                            "scripted peer {} exceeds inbound_capacity {} with {} bytes",
                            peer.peer_addr, peer.inbound_capacity, inbound_len
                        );
                    }
                    if peer.write_cap == 0 {
                        panic!("scripted peer {} configured write_cap 0", peer.peer_addr);
                    }
                    ScriptedPeerState {
                        accept_after_step: peer.accept_after_step,
                        peer_addr: peer.peer_addr,
                        inbound_chunks: VecDeque::from(peer.inbound_chunks),
                        read_chunk_cap: peer.read_chunk_cap,
                        write_cap: peer.write_cap,
                        output_capacity: peer.output_capacity,
                        output: Vec::new(),
                    }
                })
                .collect(),
            ready_peers: VecDeque::new(),
            closed: false,
        };
        self.next_listener_id += 1;
        let listener_id = listener.id;
        let local_addr = listener.local_addr;
        self.listeners.push(listener);
        self.deliver_completion(
            call_id,
            CallOutput::TcpBound {
                listener: listener_id,
                local_addr,
            },
        );
    }

    fn handle_tcp_accept(&mut self, call_id: CallId, listener: ListenerId) {
        let Some(listener_index) = self.listener_index(listener) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };

        if self.listeners[listener_index].closed {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        }

        self.promote_ready_peers(listener_index);
        if let Some(peer) = self.listeners[listener_index].ready_peers.pop_front() {
            let (stream, peer_addr) = self.open_stream_from_peer(peer);
            self.schedule_tcp_completion(
                call_id,
                TcpResourceKey::Listener(listener),
                CallOutput::TcpAccepted { stream, peer_addr },
            );
            return;
        }

        let insertion_order = self.next_tcp_completion_ordinal;
        self.next_tcp_completion_ordinal += 1;
        self.pending_accepts.push(PendingAccept {
            call_id,
            listener,
            insertion_order,
        });
    }

    fn handle_tcp_read(&mut self, call_id: CallId, stream: tina_runtime::StreamId, max_len: usize) {
        let Some(stream_index) = self.stream_index(stream) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };

        let Some(result) = self.stream_read_result(stream_index, max_len) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };

        self.schedule_tcp_completion(call_id, TcpResourceKey::Stream(stream), result);
    }

    fn handle_tcp_write(
        &mut self,
        call_id: CallId,
        stream: tina_runtime::StreamId,
        bytes: Vec<u8>,
    ) {
        let Some(stream_index) = self.stream_index(stream) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };

        let Some(result) = self.stream_write_result(stream_index, &bytes) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };

        self.schedule_tcp_completion(call_id, TcpResourceKey::Stream(stream), result);
    }

    fn handle_tcp_listener_close(&mut self, call_id: CallId, listener: ListenerId) {
        let Some(listener_index) = self.listener_index(listener) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };

        if self.listeners[listener_index].closed {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        }

        self.listeners[listener_index].closed = true;
        self.fail_pending_accepts(listener);
        self.fail_pending_tcp_completions(TcpResourceKey::Listener(listener));
        self.deliver_completion(call_id, CallOutput::TcpListenerClosed);
    }

    fn handle_tcp_stream_close(&mut self, call_id: CallId, stream: tina_runtime::StreamId) {
        let Some(stream_index) = self.stream_index(stream) else {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        };

        if self.streams[stream_index].closed {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::InvalidResource));
            return;
        }

        self.streams[stream_index].closed = true;
        self.fail_pending_tcp_completions(TcpResourceKey::Stream(stream));
        self.deliver_completion(call_id, CallOutput::TcpStreamClosed);
    }

    fn schedule_tcp_completion(
        &mut self,
        call_id: CallId,
        resource: TcpResourceKey,
        result: CallOutput,
    ) {
        if self.pending_tcp_completions.len() >= self.config.tcp.pending_completion_capacity {
            self.deliver_completion(call_id, CallOutput::Failed(CallError::Io));
            return;
        }

        let insertion_order = self.next_tcp_completion_ordinal;
        self.next_tcp_completion_ordinal += 1;
        let mut ready_at_step = self.step_ordinal + 1 + self.tcp_delay_steps(insertion_order);
        if let Some(previous_ready) = self
            .pending_tcp_completions
            .iter()
            .filter(|pending| pending.resource == resource)
            .map(|pending| pending.ready_at_step)
            .max()
        {
            ready_at_step = ready_at_step.max(previous_ready);
        }
        self.pending_tcp_completions.push(PendingTcpCompletion {
            call_id,
            ready_at_step,
            insertion_order,
            resource,
            result,
        });
    }

    fn listener_index(&self, listener: ListenerId) -> Option<usize> {
        self.listeners.iter().position(|state| state.id == listener)
    }

    fn stream_index(&self, stream: tina_runtime::StreamId) -> Option<usize> {
        self.streams.iter().position(|state| state.id == stream)
    }

    fn promote_ready_peers(&mut self, listener_index: usize) {
        let current_step = self.step_ordinal;
        while self.listeners[listener_index]
            .future_peers
            .front()
            .is_some_and(|peer| peer.accept_after_step <= current_step)
            && self.listeners[listener_index].ready_peers.len()
                < self.listeners[listener_index].backlog_capacity
        {
            let peer = self.listeners[listener_index]
                .future_peers
                .pop_front()
                .unwrap_or_else(|| panic!("listener future peer disappeared"));
            self.listeners[listener_index].ready_peers.push_back(peer);
        }
    }

    fn open_stream_from_peer(
        &mut self,
        peer: ScriptedPeerState,
    ) -> (tina_runtime::StreamId, SocketAddr) {
        let stream = tina_runtime::StreamId::new(self.next_stream_id);
        self.next_stream_id += 1;
        let peer_addr = peer.peer_addr;
        self.streams.push(StreamState {
            id: stream,
            peer_addr,
            inbound_chunks: peer.inbound_chunks,
            read_chunk_cap: peer.read_chunk_cap,
            write_cap: peer.write_cap,
            output_capacity: peer.output_capacity,
            output: peer.output,
            closed: false,
        });
        (stream, peer_addr)
    }

    fn stream_read_result(&mut self, stream_index: usize, max_len: usize) -> Option<CallOutput> {
        let stream = self.streams.get_mut(stream_index)?;
        if stream.closed {
            return None;
        }

        let mut bytes = Vec::new();
        let read_cap = stream.read_chunk_cap.unwrap_or(max_len);
        let target_len = max_len.min(read_cap);
        if target_len == 0 {
            return Some(CallOutput::TcpRead { bytes });
        }

        while bytes.len() < target_len {
            let Some(mut chunk) = stream.inbound_chunks.pop_front() else {
                break;
            };
            let remaining = target_len - bytes.len();
            if chunk.len() <= remaining {
                bytes.extend_from_slice(&chunk);
            } else {
                bytes.extend_from_slice(&chunk[..remaining]);
                chunk.drain(..remaining);
                stream.inbound_chunks.push_front(chunk);
            }
        }

        Some(CallOutput::TcpRead { bytes })
    }

    fn stream_write_result(&mut self, stream_index: usize, bytes: &[u8]) -> Option<CallOutput> {
        let stream = self.streams.get_mut(stream_index)?;
        if stream.closed {
            return None;
        }

        let remaining_capacity = stream.output_capacity.saturating_sub(stream.output.len());
        let count = bytes.len().min(stream.write_cap).min(remaining_capacity);
        stream.output.extend_from_slice(&bytes[..count]);
        Some(CallOutput::TcpWrote { count })
    }

    fn fail_pending_accepts(&mut self, listener: ListenerId) {
        let mut survivors = Vec::new();
        for pending in std::mem::take(&mut self.pending_accepts) {
            if pending.listener == listener {
                self.deliver_completion(
                    pending.call_id,
                    CallOutput::Failed(CallError::InvalidResource),
                );
            } else {
                survivors.push(pending);
            }
        }
        self.pending_accepts = survivors;
    }

    fn fail_pending_tcp_completions(&mut self, resource: TcpResourceKey) {
        let mut survivors = Vec::new();
        for pending in std::mem::take(&mut self.pending_tcp_completions) {
            if pending.resource == resource {
                self.deliver_completion(
                    pending.call_id,
                    CallOutput::Failed(CallError::InvalidResource),
                );
            } else {
                survivors.push(pending);
            }
        }
        self.pending_tcp_completions = survivors;
    }

    fn harvest_tcp(&mut self) {
        self.activate_pending_accepts();

        let mut ready = Vec::new();
        let mut still_pending = Vec::new();
        for completion in std::mem::take(&mut self.pending_tcp_completions) {
            if completion.ready_at_step <= self.step_ordinal {
                ready.push(completion);
            } else {
                still_pending.push(completion);
            }
        }
        self.pending_tcp_completions = still_pending;
        self.order_ready_tcp_completions(&mut ready);

        let mut batch_offset = 0;
        let mut last_registration_index = None;
        for completion in ready {
            let registration_index = self.requester_registration_index(completion.call_id);
            if let Some(previous) = last_registration_index {
                if registration_index <= previous {
                    batch_offset += 1;
                }
            }
            self.deliver_completion_at(
                completion.call_id,
                completion.result,
                self.step_ordinal + batch_offset,
            );
            last_registration_index = Some(registration_index);
        }
    }

    fn activate_pending_accepts(&mut self) {
        let mut survivors = Vec::new();
        let mut pending = std::mem::take(&mut self.pending_accepts);
        pending.sort_by_key(|entry| entry.insertion_order);
        for pending_accept in pending {
            let Some(listener_index) = self.listener_index(pending_accept.listener) else {
                self.deliver_completion(
                    pending_accept.call_id,
                    CallOutput::Failed(CallError::InvalidResource),
                );
                continue;
            };
            if self.listeners[listener_index].closed {
                self.deliver_completion(
                    pending_accept.call_id,
                    CallOutput::Failed(CallError::InvalidResource),
                );
                continue;
            }

            self.promote_ready_peers(listener_index);
            if let Some(peer) = self.listeners[listener_index].ready_peers.pop_front() {
                let (stream, peer_addr) = self.open_stream_from_peer(peer);
                self.schedule_tcp_completion(
                    pending_accept.call_id,
                    TcpResourceKey::Listener(pending_accept.listener),
                    CallOutput::TcpAccepted { stream, peer_addr },
                );
            } else {
                survivors.push(pending_accept);
            }
        }
        self.pending_accepts = survivors;
    }

    fn order_ready_tcp_completions(&self, ready: &mut [PendingTcpCompletion]) {
        ready.sort_by(|left, right| {
            left.ready_at_step
                .cmp(&right.ready_at_step)
                .then_with(|| left.insertion_order.cmp(&right.insertion_order))
        });

        if let TcpCompletionFaultMode::ReorderReady { one_in } = self.config.faults.tcp_completion {
            if one_in == 0 {
                return;
            }

            let mut start = 0;
            let mut batch_index = 0_u64;
            while start < ready.len() {
                let ready_at_step = ready[start].ready_at_step;
                let mut end = start + 1;
                while end < ready.len() && ready[end].ready_at_step == ready_at_step {
                    end += 1;
                }

                let selector = self
                    .config
                    .seed
                    .wrapping_add(0x5443_5052)
                    .wrapping_add(batch_index)
                    % one_in;
                if selector == 0 && end - start > 1 {
                    ready[start..end].sort_by(|left, right| {
                        right
                            .resource
                            .cmp(&left.resource)
                            .then_with(|| left.insertion_order.cmp(&right.insertion_order))
                    });
                }

                start = end;
                batch_index += 1;
            }
        }
    }

    fn tcp_delay_steps(&self, insertion_order: u64) -> u64 {
        match self.config.faults.tcp_completion {
            TcpCompletionFaultMode::None | TcpCompletionFaultMode::ReorderReady { .. } => 0,
            TcpCompletionFaultMode::DelayBySteps { one_in, steps } => {
                if one_in == 0 || steps == 0 {
                    return 0;
                }
                let selector = self
                    .config
                    .seed
                    .wrapping_add(0x5443_5044)
                    .wrapping_add(insertion_order)
                    % one_in;
                if selector == 0 { steps } else { 0 }
            }
        }
    }

    fn harvest_timers(&mut self, now: Duration) {
        let mut due = Vec::new();
        let mut still_pending = Vec::new();
        for entry in std::mem::take(&mut self.timers) {
            if entry.deadline <= now {
                due.push(entry);
            } else {
                still_pending.push(entry);
            }
        }
        self.timers = still_pending;
        due.sort_by(|a, b| {
            a.deadline
                .cmp(&b.deadline)
                .then_with(|| a.insertion_order.cmp(&b.insertion_order))
        });

        let mut batch_offset = 0;
        let mut last_registration_index = None;
        for entry in due {
            let registration_index = self.requester_registration_index(entry.call_id);
            if let Some(previous) = last_registration_index {
                if registration_index <= previous {
                    batch_offset += 1;
                }
            }
            self.deliver_completion_at(
                entry.call_id,
                CallOutput::TimerFired,
                self.step_ordinal + batch_offset,
            );
            last_registration_index = Some(registration_index);
        }
    }

    fn deliver_completion(&mut self, call_id: CallId, result: CallOutput) {
        self.deliver_completion_at(call_id, result, self.step_ordinal);
    }

    fn deliver_completion_at(&mut self, call_id: CallId, result: CallOutput, visible_at_step: u64) {
        let in_flight_index = self
            .in_flight_calls
            .iter()
            .position(|entry| entry.call_id == call_id)
            .unwrap_or_else(|| {
                panic!("simulator produced completion for unknown call {call_id:?}")
            });
        let in_flight = self.in_flight_calls.remove(in_flight_index);

        let translator_index = self
            .translators
            .iter()
            .position(|entry| entry.call_id == call_id)
            .unwrap_or_else(|| panic!("missing simulator translator for call {call_id:?}"));
        let mut stored = self.translators.remove(translator_index);
        let translator = stored
            .translator
            .take()
            .unwrap_or_else(|| panic!("translator for call {call_id:?} already consumed"));

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

        match self.entries[entry_index]
            .inbox
            .push(message, visible_at_step)
        {
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

    fn dispatch_local_send(&mut self, send: ErasedSend) -> Result<(), SendRejectedReason> {
        if send.target_shard != self.shard.id() {
            panic!(
                "cross-shard simulation is out of scope in this slice: target shard {} != simulator shard {}",
                send.target_shard.get(),
                self.shard.id().get(),
            );
        }

        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.id == send.target_isolate)
        else {
            return Err(SendRejectedReason::Closed);
        };

        if entry.generation != send.target_generation || entry.stopped.get() {
            return Err(SendRejectedReason::Closed);
        }

        let send_ordinal = self.next_send_ordinal;
        self.next_send_ordinal += 1;
        let visible_at_step = self.step_ordinal
            + 1
            + self.local_send_delay_rounds(self.config.faults.local_send, send_ordinal);

        entry
            .inbox
            .push(send.message, visible_at_step)
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
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.id == send.target_isolate)
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

        if entry.generation != send.target_generation || entry.stopped.get() {
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

        match entry.inbox.push(send.message, self.step_ordinal + 1) {
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

    fn fault_delay(&self, mode: FaultMode, ordinal: u64, tag: u64) -> Duration {
        match mode {
            FaultMode::None => Duration::ZERO,
            FaultMode::DelayBy { one_in, by } => {
                if one_in == 0 {
                    return Duration::ZERO;
                }
                let selector = self.config.seed.wrapping_add(tag).wrapping_add(ordinal) % one_in;
                if selector == 0 { by } else { Duration::ZERO }
            }
        }
    }

    fn local_send_delay_rounds(&self, mode: LocalSendFaultMode, ordinal: u64) -> u64 {
        match mode {
            LocalSendFaultMode::None => 0,
            LocalSendFaultMode::DelayByRounds { one_in, rounds } => {
                if one_in == 0 || rounds == 0 {
                    return 0;
                }
                let selector = self
                    .config
                    .seed
                    .wrapping_add(0x5345_4e44)
                    .wrapping_add(ordinal)
                    % one_in;
                if selector == 0 { rounds } else { 0 }
            }
        }
    }

    fn stop_entry(
        &mut self,
        index: usize,
        isolate_id: IsolateId,
        cause: tina_runtime::CauseId,
    ) -> tina_runtime::EventId {
        self.stop_entry_with_precollected(index, isolate_id, cause, None)
    }

    fn stop_entry_with_precollected(
        &mut self,
        index: usize,
        isolate_id: IsolateId,
        cause: tina_runtime::CauseId,
        precollected: Option<Box<dyn Any>>,
    ) -> tina_runtime::EventId {
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
        self.entries[index].inbox.close();
        let stopped = self.push_event(isolate_id, Some(cause), RuntimeEventKind::IsolateStopped);
        self.entries[index].stopped_event.set(Some(stopped));
        if precollected.is_some() {
            self.push_event(
                isolate_id,
                Some(stopped.into()),
                RuntimeEventKind::MessageAbandoned,
            );
        }
        while self.entries[index].inbox.pop_visible(u64::MAX).is_some() {
            self.push_event(
                isolate_id,
                Some(stopped.into()),
                RuntimeEventKind::MessageAbandoned,
            );
        }
        stopped
    }

    fn push_event(
        &mut self,
        isolate: IsolateId,
        cause: Option<tina_runtime::CauseId>,
        kind: RuntimeEventKind,
    ) -> tina_runtime::EventId {
        let id = self.ids.next_event_id();
        self.trace
            .push(RuntimeEvent::new(id, cause, self.shard.id(), isolate, kind));
        id
    }

    fn has_pending_messages(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| !entry.stopped.get() && entry.inbox.has_pending())
    }

    fn observed_peer_output(&self) -> Vec<ObservedPeerOutput> {
        self.streams
            .iter()
            .map(|stream| ObservedPeerOutput {
                peer_addr: stream.peer_addr,
                bytes: stream.output.clone(),
            })
            .collect()
    }

    fn requester_registration_index(&self, call_id: CallId) -> usize {
        let requester = self
            .in_flight_calls
            .iter()
            .find(|entry| entry.call_id == call_id)
            .map(|entry| entry.requester)
            .unwrap_or_else(|| panic!("missing in-flight requester for call {call_id:?}"));
        self.entries
            .iter()
            .position(|entry| {
                entry.id == requester.isolate && entry.generation == requester.generation
            })
            .unwrap_or_else(|| panic!("missing registered requester for timer call {call_id:?}"))
    }
}

fn call_kind(request: &CallInput) -> CallKind {
    match request {
        CallInput::TcpBind { .. } => CallKind::TcpBind,
        CallInput::TcpAccept { .. } => CallKind::TcpAccept,
        CallInput::TcpRead { .. } => CallKind::TcpRead,
        CallInput::TcpWrite { .. } => CallKind::TcpWrite,
        CallInput::TcpListenerClose { .. } => CallKind::TcpListenerClose,
        CallInput::TcpStreamClose { .. } => CallKind::TcpStreamClose,
        CallInput::Sleep { .. } => CallKind::Sleep,
    }
}

impl<S> IntoErasedSpawn<S> for Infallible
where
    S: Shard,
{
    fn into_erased_spawn(self) -> Box<dyn ErasedSpawn<S>> {
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

impl<I, S, Msg, Outbound> ErasedSpawn<S> for SpawnAdapter<I, Outbound>
where
    I: Isolate<Message = Msg, Shard = S, Send = TinaOutbound<Outbound>, Call = RuntimeCall<Msg>>
        + 'static,
    I::Spawn: IntoErasedSpawn<S> + 'static,
    Msg: 'static,
    Outbound: 'static,
    S: Shard,
{
    fn spawn(self: Box<Self>, sim: &mut Simulator<S>, parent: IsolateId) -> SpawnOutcome<S> {
        sim.spawn_isolate::<I, Msg, Outbound>(
            parent,
            self.isolate,
            self.mailbox_capacity,
            self.bootstrap_message,
        )
    }
}

impl<I, S, Msg, Outbound> IntoErasedSpawn<S> for ChildDefinition<I>
where
    I: Isolate<Message = Msg, Shard = S, Send = TinaOutbound<Outbound>, Call = RuntimeCall<Msg>>
        + 'static,
    I::Spawn: IntoErasedSpawn<S> + 'static,
    Msg: 'static,
    Outbound: 'static,
    S: Shard,
{
    fn into_erased_spawn(self) -> Box<dyn ErasedSpawn<S>> {
        let (isolate, mailbox_capacity, bootstrap_message) = self.into_parts();
        Box::new(SpawnAdapter::<I, Outbound> {
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

impl<I, S, Msg, Outbound> ErasedSpawn<S> for RestartableSpawnAdapter<I, Outbound>
where
    I: Isolate<Message = Msg, Shard = S, Send = TinaOutbound<Outbound>, Call = RuntimeCall<Msg>>
        + 'static,
    I::Spawn: IntoErasedSpawn<S> + 'static,
    Msg: 'static,
    Outbound: 'static,
    S: Shard,
{
    fn spawn(self: Box<Self>, sim: &mut Simulator<S>, parent: IsolateId) -> SpawnOutcome<S> {
        let adapter = Rc::new(*self);
        let isolate = (adapter.factory)();
        let mailbox_capacity = adapter.mailbox_capacity;
        let bootstrap_message = adapter.bootstrap_factory.as_ref().map(|factory| factory());
        let mut outcome = sim.spawn_isolate::<I, Msg, Outbound>(
            parent,
            isolate,
            mailbox_capacity,
            bootstrap_message,
        );
        outcome.restart_recipe = Some(adapter);
        outcome
    }
}

impl<I, S, Msg, Outbound> ErasedRestartRecipe<S> for RestartableSpawnAdapter<I, Outbound>
where
    I: Isolate<Message = Msg, Shard = S, Send = TinaOutbound<Outbound>, Call = RuntimeCall<Msg>>
        + 'static,
    I::Spawn: IntoErasedSpawn<S> + 'static,
    Msg: 'static,
    Outbound: 'static,
    S: Shard,
{
    fn create(&self, sim: &mut Simulator<S>, parent: IsolateId) -> SpawnOutcome<S> {
        let isolate = (self.factory)();
        let bootstrap_message = self.bootstrap_factory.as_ref().map(|factory| factory());
        sim.spawn_isolate::<I, Msg, Outbound>(
            parent,
            isolate,
            self.mailbox_capacity,
            bootstrap_message,
        )
    }
}

impl<I, S, Msg, Outbound> IntoErasedSpawn<S> for RestartableChildDefinition<I>
where
    I: Isolate<Message = Msg, Shard = S, Send = TinaOutbound<Outbound>, Call = RuntimeCall<Msg>>
        + 'static,
    I::Spawn: IntoErasedSpawn<S> + 'static,
    Msg: 'static,
    Outbound: 'static,
    S: Shard,
{
    fn into_erased_spawn(self) -> Box<dyn ErasedSpawn<S>> {
        let (factory, mailbox_capacity, bootstrap_factory) = self.into_parts();
        Box::new(RestartableSpawnAdapter::<I, Outbound> {
            factory,
            mailbox_capacity,
            bootstrap_factory,
            marker: PhantomData,
        })
    }
}

/// Deterministic explicit-step coordinator over a fixed set of shard simulators.
///
/// This additive shell preserves the existing single-shard [`Simulator`] API
/// while giving Galileo one honest place to define global ingress, global
/// stepping order, and explicit root placement by shard in virtual time.
pub struct MultiShardSimulator<S>
where
    S: Shard + 'static,
{
    simulators: Vec<Simulator<S>>,
    shard_indexes: BTreeMap<ShardId, usize>,
    config: MultiShardSimulatorConfig,
    remote_queues: BTreeMap<(ShardId, ShardId), VecDeque<QueuedRemoteSend>>,
    last_checker_failure: Option<CheckerFailure>,
}

/// Bounded coordinator config for additive multi-shard simulator shells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MultiShardSimulatorConfig {
    /// Capacity of each source-shard -> destination-shard queue.
    pub shard_pair_capacity: usize,
}

impl Default for MultiShardSimulatorConfig {
    fn default() -> Self {
        Self {
            shard_pair_capacity: 64,
        }
    }
}

impl<S> MultiShardSimulator<S>
where
    S: Shard + 'static,
{
    /// Creates one additive multi-shard simulator over the provided shards.
    ///
    /// Shards are stepped in ascending [`ShardId`] order, regardless of input
    /// order. Empty shard sets and duplicate shard ids are programmer errors
    /// and panic.
    pub fn new<I>(shards: I, config: SimulatorConfig) -> Self
    where
        I: IntoIterator<Item = S>,
    {
        Self::with_config(shards, config, MultiShardSimulatorConfig::default())
    }

    /// Creates one additive multi-shard simulator with explicit shard-pair
    /// queue boundedness.
    pub fn with_config<I>(
        shards: I,
        config: SimulatorConfig,
        multishard: MultiShardSimulatorConfig,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
    {
        let mut shards: Vec<S> = shards.into_iter().collect();
        if shards.is_empty() {
            panic!("multi-shard simulator requires at least one shard");
        }
        if multishard.shard_pair_capacity == 0 {
            panic!("multi-shard simulator requires shard-pair capacity > 0");
        }

        shards.sort_by_key(Shard::id);
        for pair in shards.windows(2) {
            if pair[0].id() == pair[1].id() {
                panic!(
                    "multi-shard simulator received duplicate shard id {}",
                    pair[0].id().get()
                );
            }
        }

        let ids = IdSource::new();
        let mut simulators = Vec::with_capacity(shards.len());
        let mut shard_indexes = BTreeMap::new();
        for shard in shards {
            let shard_id = shard.id();
            shard_indexes.insert(shard_id, simulators.len());
            simulators.push(Simulator::with_ids(shard, config.clone(), ids.clone()));
        }

        Self {
            simulators,
            shard_indexes,
            config: multishard,
            remote_queues: BTreeMap::new(),
            last_checker_failure: None,
        }
    }

    /// Returns the shard ids owned by this coordinator in global step order.
    pub fn shard_ids(&self) -> Vec<ShardId> {
        self.simulators
            .iter()
            .map(|simulator| simulator.shard.id())
            .collect()
    }

    /// Returns the current shared virtual time.
    pub fn now(&self) -> Duration {
        self.simulators
            .first()
            .map(|simulator| simulator.virtual_now)
            .unwrap_or(Duration::ZERO)
    }

    /// Returns the merged deterministic event record in global event-id order.
    pub fn trace(&self) -> Vec<RuntimeEvent> {
        let mut events: Vec<_> = self
            .simulators
            .iter()
            .flat_map(|simulator| simulator.trace().iter().copied())
            .collect();
        events.sort_by_key(|event| event.id());
        events
    }

    /// Returns whether any owned shard still has pending timers or undelivered
    /// runtime-owned completions.
    pub fn has_in_flight_calls(&self) -> bool {
        self.simulators.iter().any(Simulator::has_in_flight_calls)
    }

    /// Registers one root isolate on the requested owning shard.
    #[allow(private_bounds)]
    pub fn register_on<I, Msg, Outbound>(&mut self, shard: ShardId, isolate: I) -> Address<Msg>
    where
        I: Isolate<
                Message = Msg,
                Shard = S,
                Send = TinaOutbound<Outbound>,
                Call = RuntimeCall<Msg>,
            > + 'static,
        I::Spawn: IntoErasedSpawn<S> + 'static,
        Msg: 'static,
        Outbound: 'static,
    {
        self.simulator_mut(shard)
            .register::<I, Msg, Outbound>(isolate)
    }

    /// Registers one root isolate on the requested shard with an explicit
    /// mailbox capacity.
    #[allow(private_bounds)]
    pub fn register_with_capacity_on<I, Msg, Outbound>(
        &mut self,
        shard: ShardId,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Address<Msg>
    where
        I: Isolate<
                Message = Msg,
                Shard = S,
                Send = TinaOutbound<Outbound>,
                Call = RuntimeCall<Msg>,
            > + 'static,
        I::Spawn: IntoErasedSpawn<S> + 'static,
        Msg: 'static,
        Outbound: 'static,
    {
        self.simulator_mut(shard)
            .register_with_mailbox_capacity::<I, Msg, Outbound>(isolate, mailbox_capacity)
    }

    /// Configures a registered isolate as supervisor on its owning shard.
    pub fn supervise<M: 'static>(&mut self, parent: Address<M>, config: SupervisorConfig) {
        self.simulator_mut(parent.shard()).supervise(parent, config);
    }

    /// Attempts one typed global ingress send routed strictly by target shard.
    pub fn try_send<M: 'static>(
        &self,
        address: Address<M>,
        message: M,
    ) -> Result<(), TrySendError<M>> {
        self.simulator(address.shard()).try_send(address, message)
    }

    /// Advances the shared virtual monotonic time by `by`.
    pub fn advance_time(&mut self, by: Duration) {
        for simulator in &mut self.simulators {
            simulator.advance_time(by);
        }
    }

    /// Advances shared virtual time to the next due timer on any shard.
    pub fn advance_to_next_timer(&mut self) -> bool {
        let Some(next_deadline) = self
            .simulators
            .iter()
            .flat_map(|simulator| simulator.timers.iter().map(|entry| entry.deadline))
            .min()
        else {
            return false;
        };

        for simulator in &mut self.simulators {
            if next_deadline > simulator.virtual_now {
                simulator.virtual_now = next_deadline;
            }
        }

        true
    }

    /// Runs one global deterministic round in ascending shard-id order.
    pub fn step(&mut self) -> usize {
        let mut ready = std::mem::take(&mut self.remote_queues);
        let shard_ids = self.shard_ids();
        let mut delivered = 0;

        for destination in &shard_ids {
            self.harvest_for_destination(*destination, &shard_ids, &mut ready);
            let index = self.checked_shard_index(*destination);
            let config = self.config;
            let shard_indexes = self.shard_indexes.clone();
            let remote_queues = &mut self.remote_queues;
            delivered +=
                self.simulators[index].step_with_remote(&mut |source_shard, send, cause| {
                    if !shard_indexes.contains_key(&send.target_shard) {
                        panic!(
                            "multi-shard simulator targeted unknown destination shard {}",
                            send.target_shard.get()
                        );
                    }

                    let key = (source_shard, send.target_shard);
                    let queue = remote_queues.entry(key).or_default();
                    if queue.len() >= config.shard_pair_capacity {
                        return Err(SendRejectedReason::Full);
                    }
                    queue.push_back(QueuedRemoteSend { send, cause });
                    Ok(())
                });
        }

        delivered
    }

    /// Continues running until every shard and every cross-shard queue is
    /// quiescent.
    pub fn run_until_quiescent(&mut self) -> usize {
        let mut total = 0;
        loop {
            let delivered = self.step();
            total += delivered;
            if delivered > 0 {
                continue;
            }
            if self.advance_to_next_timer() {
                continue;
            }
            if self.has_in_flight_calls() {
                continue;
            }
            if !self.remote_queues.is_empty() {
                continue;
            }
            if self.simulators.iter().any(Simulator::has_pending_messages) {
                continue;
            }
            break total;
        }
    }

    /// Runs until every shard and every cross-shard queue is quiescent, or
    /// until a checker halts the whole multi-shard run.
    pub fn run_until_quiescent_checked<C: Checker>(
        &mut self,
        checker: &mut C,
    ) -> Option<CheckerFailure> {
        self.last_checker_failure = None;
        let mut observed_len = 0;
        loop {
            let delivered = self.step();
            if let Some(failure) = self.observe_new_events(checker, &mut observed_len) {
                self.last_checker_failure = Some(failure.clone());
                return Some(failure);
            }
            if delivered > 0 {
                continue;
            }
            if self.advance_to_next_timer() {
                continue;
            }
            if self.has_in_flight_calls() {
                continue;
            }
            if !self.remote_queues.is_empty() {
                continue;
            }
            if self.simulators.iter().any(Simulator::has_pending_messages) {
                continue;
            }
            return None;
        }
    }

    /// Captures a deterministic replay artifact for the current whole-run
    /// multi-shard state.
    pub fn replay_artifact(&self) -> MultiShardReplayArtifact {
        MultiShardReplayArtifact {
            simulator_config: self
                .simulators
                .first()
                .map(|simulator| simulator.config().clone())
                .unwrap_or_default(),
            multishard_config: self.config,
            final_time: self.now(),
            event_record: self.trace(),
            checker_failure: self.last_checker_failure.clone(),
            observed_peer_output: self
                .simulators
                .iter()
                .flat_map(Simulator::observed_peer_output)
                .collect(),
        }
    }

    fn simulator(&self, shard: ShardId) -> &Simulator<S> {
        &self.simulators[self.checked_shard_index(shard)]
    }

    fn simulator_mut(&mut self, shard: ShardId) -> &mut Simulator<S> {
        let index = self.checked_shard_index(shard);
        &mut self.simulators[index]
    }

    fn checked_shard_index(&self, shard: ShardId) -> usize {
        self.shard_indexes.get(&shard).copied().unwrap_or_else(|| {
            panic!(
                "multi-shard simulator targeted unknown shard {}",
                shard.get()
            )
        })
    }

    fn observe_new_events<C: Checker>(
        &self,
        checker: &mut C,
        observed_len: &mut usize,
    ) -> Option<CheckerFailure> {
        let trace = self.trace();
        while *observed_len < trace.len() {
            let event = trace[*observed_len];
            *observed_len += 1;
            match checker.on_event(&event) {
                CheckerDecision::Continue => {}
                CheckerDecision::Fail(reason) => {
                    return Some(CheckerFailure {
                        checker_name: checker.name(),
                        event_id: event.id(),
                        reason,
                    });
                }
            }
        }
        None
    }

    fn harvest_for_destination(
        &mut self,
        destination: ShardId,
        shard_ids: &[ShardId],
        ready: &mut BTreeMap<(ShardId, ShardId), VecDeque<QueuedRemoteSend>>,
    ) {
        let index = self.checked_shard_index(destination);
        for source in shard_ids {
            if *source == destination {
                continue;
            }
            let key = (*source, destination);
            let Some(queue) = ready.get_mut(&key) else {
                continue;
            };
            while let Some(queued) = queue.pop_front() {
                self.simulators[index].harvest_remote_send(queued);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use tina::{Outbound, batch, noop, send, spawn, stop};

    #[derive(Debug, Clone, Copy)]
    struct NumberedShard(u32);

    impl Shard for NumberedShard {
        fn id(&self) -> ShardId {
            ShardId::new(self.0)
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SimTimerMsg {
        Start,
        Fired,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SimStepEvent {
        Tick,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SimRemoteEvent {
        Kick,
        KickTwice,
        KickThrice,
        Arrived,
        StopNow,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SimShardLocalSupervisionEvent {
        SpawnChild,
        Panic,
        Noop,
    }

    #[derive(Debug)]
    struct SimSleeper<S> {
        delay: Duration,
        marker: PhantomData<S>,
    }

    #[derive(Debug)]
    struct SimRecorder<S> {
        marker: PhantomData<S>,
    }

    #[derive(Debug)]
    struct SimRemoteSender<S> {
        target: Address<SimRemoteEvent>,
        marker: PhantomData<S>,
    }

    #[derive(Debug)]
    struct SimRemoteSink<S> {
        marker: PhantomData<S>,
    }

    #[derive(Debug)]
    struct SimShardLocalParent<S> {
        marker: PhantomData<S>,
    }

    #[derive(Debug)]
    struct SimShardLocalChild<S> {
        marker: PhantomData<S>,
    }

    impl<S> Isolate for SimSleeper<S>
    where
        S: Shard + 'static,
    {
        type Message = SimTimerMsg;
        type Reply = ();
        type Send = Outbound<Infallible>;
        type Spawn = Infallible;
        type Call = RuntimeCall<SimTimerMsg>;
        type Shard = S;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard>,
        ) -> Effect<Self> {
            match msg {
                SimTimerMsg::Start => Effect::Call(RuntimeCall::new(
                    CallInput::Sleep { after: self.delay },
                    |result| match result {
                        CallOutput::TimerFired => SimTimerMsg::Fired,
                        other => panic!("expected TimerFired, got {other:?}"),
                    },
                )),
                SimTimerMsg::Fired => Effect::Noop,
            }
        }
    }

    impl<S> Isolate for SimRecorder<S>
    where
        S: Shard + 'static,
    {
        type Message = SimStepEvent;
        type Reply = ();
        type Send = Outbound<Infallible>;
        type Spawn = Infallible;
        type Call = RuntimeCall<SimStepEvent>;
        type Shard = S;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard>,
        ) -> Effect<Self> {
            match msg {
                SimStepEvent::Tick => Effect::Noop,
            }
        }
    }

    impl<S> Isolate for SimRemoteSender<S>
    where
        S: Shard + 'static,
    {
        type Message = SimRemoteEvent;
        type Reply = ();
        type Send = Outbound<SimRemoteEvent>;
        type Spawn = Infallible;
        type Call = RuntimeCall<SimRemoteEvent>;
        type Shard = S;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard>,
        ) -> Effect<Self> {
            match msg {
                SimRemoteEvent::Kick => send(self.target, SimRemoteEvent::Arrived),
                SimRemoteEvent::KickTwice => batch([
                    send(self.target, SimRemoteEvent::Arrived),
                    send(self.target, SimRemoteEvent::Arrived),
                ]),
                SimRemoteEvent::KickThrice => batch([
                    send(self.target, SimRemoteEvent::Arrived),
                    send(self.target, SimRemoteEvent::Arrived),
                    send(self.target, SimRemoteEvent::Arrived),
                ]),
                SimRemoteEvent::Arrived => noop(),
                SimRemoteEvent::StopNow => stop(),
            }
        }
    }

    impl<S> Isolate for SimRemoteSink<S>
    where
        S: Shard + 'static,
    {
        type Message = SimRemoteEvent;
        type Reply = ();
        type Send = Outbound<Infallible>;
        type Spawn = Infallible;
        type Call = RuntimeCall<SimRemoteEvent>;
        type Shard = S;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard>,
        ) -> Effect<Self> {
            match msg {
                SimRemoteEvent::Arrived
                | SimRemoteEvent::Kick
                | SimRemoteEvent::KickTwice
                | SimRemoteEvent::KickThrice => noop(),
                SimRemoteEvent::StopNow => stop(),
            }
        }
    }

    impl<S> Isolate for SimShardLocalParent<S>
    where
        S: Shard + 'static,
    {
        type Message = SimShardLocalSupervisionEvent;
        type Reply = ();
        type Send = Outbound<Infallible>;
        type Spawn = tina::RestartableChildDefinition<SimShardLocalChild<S>>;
        type Call = RuntimeCall<SimShardLocalSupervisionEvent>;
        type Shard = S;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard>,
        ) -> Effect<Self> {
            match msg {
                SimShardLocalSupervisionEvent::SpawnChild => {
                    spawn(tina::RestartableChildDefinition::new(
                        || SimShardLocalChild {
                            marker: PhantomData,
                        },
                        4,
                    ))
                }
                SimShardLocalSupervisionEvent::Panic => {
                    panic!("parent should not panic in this test")
                }
                SimShardLocalSupervisionEvent::Noop => noop(),
            }
        }
    }

    impl<S> Isolate for SimShardLocalChild<S>
    where
        S: Shard + 'static,
    {
        type Message = SimShardLocalSupervisionEvent;
        type Reply = ();
        type Send = Outbound<Infallible>;
        type Spawn = Infallible;
        type Call = RuntimeCall<SimShardLocalSupervisionEvent>;
        type Shard = S;

        fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut Context<'_, Self::Shard>,
        ) -> Effect<Self> {
            match msg {
                SimShardLocalSupervisionEvent::Panic => {
                    panic!("child panicked for restart test")
                }
                SimShardLocalSupervisionEvent::SpawnChild | SimShardLocalSupervisionEvent::Noop => {
                    noop()
                }
            }
        }
    }

    struct AddressLocalRemoteFailureRun {
        artifact: MultiShardReplayArtifact,
        failure: Option<CheckerFailure>,
    }

    struct AddressLocalRemoteFailureChecker {
        destination: ShardId,
        rejected: IsolateId,
        live: IsolateId,
        marker_shard: ShardId,
        marker: IsolateId,
        saw_address_rejection: bool,
        saw_live_accept_after_rejection: bool,
    }

    impl Checker for AddressLocalRemoteFailureChecker {
        fn name(&self) -> &'static str {
            "address_local_remote_failure"
        }

        fn on_event(&mut self, event: &RuntimeEvent) -> CheckerDecision {
            match event.kind() {
                RuntimeEventKind::SendRejected {
                    target_isolate,
                    reason: SendRejectedReason::Closed,
                    ..
                } if event.shard() == self.destination && target_isolate == self.rejected => {
                    self.saw_address_rejection = true;
                }
                RuntimeEventKind::MailboxAccepted
                    if self.saw_address_rejection
                        && event.shard() == self.destination
                        && event.isolate() == self.live =>
                {
                    self.saw_live_accept_after_rejection = true;
                }
                RuntimeEventKind::HandlerStarted
                    if self.saw_address_rejection
                        && event.shard() == self.marker_shard
                        && event.isolate() == self.marker
                        && !self.saw_live_accept_after_rejection =>
                {
                    return CheckerDecision::Fail(
                        "destination shard did not accept later live traffic after address-local remote failure"
                            .into(),
                    );
                }
                _ => {}
            }

            CheckerDecision::Continue
        }
    }

    fn run_address_local_remote_failure_checker_workload(
        include_live_send: bool,
        config: SimulatorConfig,
        multishard: MultiShardSimulatorConfig,
    ) -> AddressLocalRemoteFailureRun {
        let mut sim = MultiShardSimulator::with_config(
            [NumberedShard(11), NumberedShard(22)],
            config,
            multishard,
        );

        let unknown = Address::new_with_generation(
            ShardId::new(22),
            IsolateId::new(999),
            AddressGeneration::new(0),
        );
        let live_sink = sim.register_with_capacity_on::<SimRemoteSink<NumberedShard>, _, _>(
            ShardId::new(22),
            SimRemoteSink {
                marker: PhantomData,
            },
            4,
        );
        let marker = sim.register_with_capacity_on::<SimRemoteSink<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSink {
                marker: PhantomData,
            },
            4,
        );
        let bad_sender = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: unknown,
                marker: PhantomData,
            },
            4,
        );
        let good_sender = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: live_sink,
                marker: PhantomData,
            },
            4,
        );

        sim.try_send(bad_sender, SimRemoteEvent::Kick).unwrap();
        if include_live_send {
            sim.try_send(good_sender, SimRemoteEvent::Kick).unwrap();
        }

        sim.step();
        sim.step();
        sim.try_send(marker, SimRemoteEvent::Kick).unwrap();

        let mut checker = AddressLocalRemoteFailureChecker {
            destination: ShardId::new(22),
            rejected: unknown.isolate(),
            live: live_sink.isolate(),
            marker_shard: ShardId::new(11),
            marker: marker.isolate(),
            saw_address_rejection: false,
            saw_live_accept_after_rejection: false,
        };
        let failure = sim.run_until_quiescent_checked(&mut checker);

        AddressLocalRemoteFailureRun {
            artifact: sim.replay_artifact(),
            failure,
        }
    }

    #[test]
    fn sibling_simulators_can_share_global_event_and_call_id_sources() {
        let ids = IdSource::new();
        let mut first =
            Simulator::with_ids(NumberedShard(11), SimulatorConfig::default(), ids.clone());
        let mut second = Simulator::with_ids(NumberedShard(22), SimulatorConfig::default(), ids);

        let first_sleeper = first.register(SimSleeper::<NumberedShard> {
            delay: Duration::from_millis(5),
            marker: PhantomData,
        });
        let second_sleeper = second.register(SimSleeper::<NumberedShard> {
            delay: Duration::from_millis(7),
            marker: PhantomData,
        });

        first.try_send(first_sleeper, SimTimerMsg::Start).unwrap();
        second.try_send(second_sleeper, SimTimerMsg::Start).unwrap();

        assert_eq!(first.step(), 1);
        assert_eq!(second.step(), 1);

        let first_event_ids: Vec<_> = first.trace().iter().map(|event| event.id().get()).collect();
        let second_event_ids: Vec<_> = second
            .trace()
            .iter()
            .map(|event| event.id().get())
            .collect();
        assert_eq!(first_event_ids, vec![1, 2, 3, 4]);
        assert_eq!(second_event_ids, vec![5, 6, 7, 8]);

        let first_call_ids: Vec<_> = first
            .trace()
            .iter()
            .filter_map(|event| match event.kind() {
                RuntimeEventKind::CallDispatchAttempted { call_id, .. } => Some(call_id.get()),
                _ => None,
            })
            .collect();
        let second_call_ids: Vec<_> = second
            .trace()
            .iter()
            .filter_map(|event| match event.kind() {
                RuntimeEventKind::CallDispatchAttempted { call_id, .. } => Some(call_id.get()),
                _ => None,
            })
            .collect();
        assert_eq!(first_call_ids, vec![1]);
        assert_eq!(second_call_ids, vec![2]);
    }

    #[test]
    fn multishard_simulator_routes_ingress_and_steps_in_ascending_shard_order() {
        let mut sim = MultiShardSimulator::new(
            [NumberedShard(22), NumberedShard(11)],
            SimulatorConfig::default(),
        );

        assert_eq!(sim.shard_ids(), vec![ShardId::new(11), ShardId::new(22)]);

        let shard_twenty_two = sim.register_with_capacity_on::<SimRecorder<NumberedShard>, _, _>(
            ShardId::new(22),
            SimRecorder {
                marker: PhantomData,
            },
            4,
        );
        let shard_eleven = sim.register_with_capacity_on::<SimRecorder<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRecorder {
                marker: PhantomData,
            },
            4,
        );

        assert_eq!(shard_twenty_two.shard(), ShardId::new(22));
        assert_eq!(shard_eleven.shard(), ShardId::new(11));

        sim.try_send(shard_twenty_two, SimStepEvent::Tick).unwrap();
        sim.try_send(shard_eleven, SimStepEvent::Tick).unwrap();

        assert_eq!(sim.step(), 2);

        let handler_shards: Vec<_> = sim
            .trace()
            .into_iter()
            .filter_map(|event| match event.kind() {
                RuntimeEventKind::HandlerStarted => Some(event.shard().get()),
                _ => None,
            })
            .collect();
        assert_eq!(handler_shards, vec![11, 22]);
    }

    #[test]
    fn cross_shard_simulation_becomes_visible_on_the_next_global_step() {
        let mut sim = MultiShardSimulator::new(
            [NumberedShard(11), NumberedShard(22)],
            SimulatorConfig::default(),
        );

        let sink = sim.register_with_capacity_on::<SimRemoteSink<NumberedShard>, _, _>(
            ShardId::new(22),
            SimRemoteSink {
                marker: PhantomData,
            },
            4,
        );
        let sender = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: sink,
                marker: PhantomData,
            },
            4,
        );

        sim.try_send(sender, SimRemoteEvent::Kick).unwrap();

        assert_eq!(sim.step(), 1);
        let step_one_trace = sim.trace();
        let step_one_sink_handlers = step_one_trace
            .iter()
            .filter(|event| {
                event.shard() == ShardId::new(22)
                    && matches!(event.kind(), RuntimeEventKind::HandlerStarted)
            })
            .count();
        assert_eq!(step_one_sink_handlers, 0);

        assert_eq!(sim.step(), 1);
        let trace = sim.trace();
        assert!(trace.iter().any(|event| {
            event.shard() == ShardId::new(11)
                && matches!(event.kind(), RuntimeEventKind::SendAccepted { .. })
        }));
        assert!(trace.iter().any(|event| {
            event.shard() == ShardId::new(22)
                && matches!(event.kind(), RuntimeEventKind::MailboxAccepted)
        }));
    }

    #[test]
    fn cross_shard_simulation_queue_overflow_rejects_at_source_time() {
        let mut sim = MultiShardSimulator::with_config(
            [NumberedShard(11), NumberedShard(22)],
            SimulatorConfig::default(),
            MultiShardSimulatorConfig {
                shard_pair_capacity: 1,
            },
        );

        let sink = sim.register_with_capacity_on::<SimRemoteSink<NumberedShard>, _, _>(
            ShardId::new(22),
            SimRemoteSink {
                marker: PhantomData,
            },
            4,
        );
        let sender = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: sink,
                marker: PhantomData,
            },
            4,
        );

        sim.try_send(sender, SimRemoteEvent::KickTwice).unwrap();

        assert_eq!(sim.step(), 1);

        let trace = sim.trace();
        let full_rejections = trace
            .iter()
            .filter(|event| {
                event.shard() == ShardId::new(11)
                    && matches!(
                        event.kind(),
                        RuntimeEventKind::SendRejected {
                            reason: SendRejectedReason::Full,
                            ..
                        }
                    )
            })
            .count();
        assert_eq!(full_rejections, 1);
    }

    #[test]
    fn cross_shard_simulation_closed_target_rejects_on_destination_harvest() {
        let mut sim = MultiShardSimulator::new(
            [NumberedShard(11), NumberedShard(22)],
            SimulatorConfig::default(),
        );

        let sink = sim.register_with_capacity_on::<SimRemoteSink<NumberedShard>, _, _>(
            ShardId::new(22),
            SimRemoteSink {
                marker: PhantomData,
            },
            4,
        );
        let sender = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: sink,
                marker: PhantomData,
            },
            4,
        );

        sim.try_send(sender, SimRemoteEvent::Kick).unwrap();
        sim.try_send(sink, SimRemoteEvent::StopNow).unwrap();

        assert_eq!(sim.step(), 2);
        assert_eq!(sim.step(), 0);

        let trace = sim.trace();
        let closed_rejections = trace
            .iter()
            .filter(|event| {
                event.shard() == ShardId::new(22)
                    && matches!(
                        event.kind(),
                        RuntimeEventKind::SendRejected {
                            reason: SendRejectedReason::Closed,
                            ..
                        }
                    )
            })
            .count();
        assert_eq!(closed_rejections, 1);
    }

    #[test]
    fn cross_shard_simulation_unknown_isolate_rejects_on_destination_harvest() {
        let mut sim = MultiShardSimulator::new(
            [NumberedShard(11), NumberedShard(22)],
            SimulatorConfig::default(),
        );

        let unknown = Address::new_with_generation(
            ShardId::new(22),
            IsolateId::new(999),
            AddressGeneration::new(0),
        );
        let sender = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: unknown,
                marker: PhantomData,
            },
            4,
        );

        sim.try_send(sender, SimRemoteEvent::Kick).unwrap();

        assert_eq!(sim.step(), 1);
        assert_eq!(sim.step(), 0);

        let trace = sim.trace();
        let destination_closed_rejections = trace
            .iter()
            .filter(|event| {
                event.shard() == ShardId::new(22)
                    && matches!(
                        event.kind(),
                        RuntimeEventKind::SendRejected {
                            target_isolate,
                            reason: SendRejectedReason::Closed,
                            ..
                        } if target_isolate == unknown.isolate()
                    )
            })
            .count();
        assert_eq!(destination_closed_rejections, 1);
    }

    #[test]
    fn cross_shard_simulation_unknown_isolate_does_not_poison_destination_shard() {
        let mut sim = MultiShardSimulator::new(
            [NumberedShard(11), NumberedShard(22)],
            SimulatorConfig::default(),
        );

        let unknown = Address::new_with_generation(
            ShardId::new(22),
            IsolateId::new(999),
            AddressGeneration::new(0),
        );
        let live_sink = sim.register_with_capacity_on::<SimRemoteSink<NumberedShard>, _, _>(
            ShardId::new(22),
            SimRemoteSink {
                marker: PhantomData,
            },
            4,
        );
        let bad_sender = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: unknown,
                marker: PhantomData,
            },
            4,
        );
        let good_sender = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: live_sink,
                marker: PhantomData,
            },
            4,
        );

        sim.try_send(bad_sender, SimRemoteEvent::Kick).unwrap();
        sim.try_send(good_sender, SimRemoteEvent::Kick).unwrap();

        assert_eq!(sim.step(), 2);
        assert_eq!(sim.step(), 1);

        let trace = sim.trace();
        let unknown_rejection = trace
            .iter()
            .find(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::SendRejected {
                        target_isolate,
                        reason: SendRejectedReason::Closed,
                        ..
                    } if event.shard() == ShardId::new(22)
                        && target_isolate == unknown.isolate()
                )
            })
            .expect("unknown remote target should reject on destination shard");
        let live_accept = trace
            .iter()
            .find(|event| {
                event.shard() == ShardId::new(22)
                    && event.isolate() == live_sink.isolate()
                    && matches!(event.kind(), RuntimeEventKind::MailboxAccepted)
            })
            .expect("later valid traffic to same shard should still be accepted");

        assert!(unknown_rejection.id() < live_accept.id());

        let live_handler_count = trace
            .iter()
            .filter(|event| {
                event.shard() == ShardId::new(22)
                    && event.isolate() == live_sink.isolate()
                    && matches!(event.kind(), RuntimeEventKind::HandlerStarted)
            })
            .count();
        assert_eq!(live_handler_count, 1);
    }

    #[test]
    fn multishard_checker_accepts_address_local_remote_failure_then_good_traffic() {
        let run = run_address_local_remote_failure_checker_workload(
            true,
            SimulatorConfig::default(),
            MultiShardSimulatorConfig::default(),
        );

        assert!(run.failure.is_none());
        assert!(run.artifact.checker_failure().is_none());
    }

    #[test]
    fn multishard_checker_failure_replays_for_address_local_liveness_bug() {
        let first = run_address_local_remote_failure_checker_workload(
            false,
            SimulatorConfig::default(),
            MultiShardSimulatorConfig::default(),
        );
        let replayed = run_address_local_remote_failure_checker_workload(
            false,
            first.artifact.simulator_config().clone(),
            first.artifact.multishard_config(),
        );

        let failure = first
            .failure
            .as_ref()
            .expect("checker should catch missing later live traffic");
        assert_eq!(failure.checker_name(), "address_local_remote_failure");
        assert_eq!(first.artifact.checker_failure(), Some(failure));
        assert_eq!(
            first.artifact.event_record(),
            replayed.artifact.event_record()
        );
        assert_eq!(first.artifact.final_time(), replayed.artifact.final_time());
        assert_eq!(
            first.artifact.checker_failure(),
            replayed.artifact.checker_failure()
        );
        assert_eq!(first.failure, replayed.failure);
    }

    #[test]
    fn cross_shard_simulation_destination_mailbox_full_rejects_on_harvest() {
        let mut sim = MultiShardSimulator::new(
            [NumberedShard(11), NumberedShard(22)],
            SimulatorConfig::default(),
        );

        let sink = sim.register_with_capacity_on::<SimRemoteSink<NumberedShard>, _, _>(
            ShardId::new(22),
            SimRemoteSink {
                marker: PhantomData,
            },
            1,
        );
        let remote_sender = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: sink,
                marker: PhantomData,
            },
            4,
        );
        let local_filler = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(22),
            SimRemoteSender {
                target: sink,
                marker: PhantomData,
            },
            4,
        );

        sim.try_send(remote_sender, SimRemoteEvent::Kick).unwrap();
        sim.try_send(local_filler, SimRemoteEvent::Kick).unwrap();

        assert_eq!(sim.step(), 2);
        assert_eq!(sim.step(), 1);

        let trace = sim.trace();
        let source_accepts = trace
            .iter()
            .filter(|event| {
                event.shard() == ShardId::new(11)
                    && matches!(event.kind(), RuntimeEventKind::SendAccepted { .. })
            })
            .count();
        assert_eq!(source_accepts, 1);

        let destination_full_rejections = trace
            .iter()
            .filter(|event| {
                event.shard() == ShardId::new(22)
                    && matches!(
                        event.kind(),
                        RuntimeEventKind::SendRejected {
                            reason: SendRejectedReason::Full,
                            ..
                        }
                    )
            })
            .count();
        assert_eq!(destination_full_rejections, 1);
    }

    #[test]
    fn cross_shard_simulation_preserves_fifo_from_one_source() {
        let mut sim = MultiShardSimulator::new(
            [NumberedShard(11), NumberedShard(22)],
            SimulatorConfig::default(),
        );

        let sink = sim.register_with_capacity_on::<SimRemoteSink<NumberedShard>, _, _>(
            ShardId::new(22),
            SimRemoteSink {
                marker: PhantomData,
            },
            8,
        );
        let sender = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: sink,
                marker: PhantomData,
            },
            4,
        );

        sim.try_send(sender, SimRemoteEvent::KickThrice).unwrap();

        assert_eq!(sim.step(), 1);
        assert_eq!(sim.step(), 1);

        let trace = sim.trace();
        let source_attempts: Vec<_> = trace
            .iter()
            .filter_map(|event| match event.kind() {
                RuntimeEventKind::SendDispatchAttempted { .. }
                    if event.shard() == ShardId::new(11) =>
                {
                    Some(event.id().get())
                }
                _ => None,
            })
            .collect();
        let destination_causes: Vec<_> = trace
            .iter()
            .filter_map(|event| match event.kind() {
                RuntimeEventKind::MailboxAccepted if event.shard() == ShardId::new(22) => {
                    event.cause().map(|cause| cause.event().get())
                }
                _ => None,
            })
            .collect();
        assert_eq!(destination_causes, source_attempts);
    }

    #[test]
    fn cross_shard_simulation_preserves_fifo_per_isolate_pair_with_multiple_sources_and_targets() {
        let mut sim = MultiShardSimulator::new(
            [NumberedShard(11), NumberedShard(44)],
            SimulatorConfig::default(),
        );

        let sink_a = sim.register_with_capacity_on::<SimRemoteSink<NumberedShard>, _, _>(
            ShardId::new(44),
            SimRemoteSink {
                marker: PhantomData,
            },
            8,
        );
        let sink_b = sim.register_with_capacity_on::<SimRemoteSink<NumberedShard>, _, _>(
            ShardId::new(44),
            SimRemoteSink {
                marker: PhantomData,
            },
            8,
        );
        let sender_a = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: sink_a,
                marker: PhantomData,
            },
            4,
        );
        let sender_b = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: sink_b,
                marker: PhantomData,
            },
            4,
        );

        sim.try_send(sender_a, SimRemoteEvent::KickThrice).unwrap();
        sim.try_send(sender_b, SimRemoteEvent::KickTwice).unwrap();

        assert_eq!(sim.step(), 2);
        assert_eq!(sim.step(), 2);

        let trace = sim.trace();
        let sender_a_attempts: Vec<_> = trace
            .iter()
            .filter_map(|event| match event.kind() {
                RuntimeEventKind::SendDispatchAttempted { .. }
                    if event.shard() == ShardId::new(11)
                        && event.isolate() == sender_a.isolate() =>
                {
                    Some(event.id().get())
                }
                _ => None,
            })
            .collect();
        let sender_b_attempts: Vec<_> = trace
            .iter()
            .filter_map(|event| match event.kind() {
                RuntimeEventKind::SendDispatchAttempted { .. }
                    if event.shard() == ShardId::new(11)
                        && event.isolate() == sender_b.isolate() =>
                {
                    Some(event.id().get())
                }
                _ => None,
            })
            .collect();
        let sink_a_causes: Vec<_> = trace
            .iter()
            .filter_map(|event| match event.kind() {
                RuntimeEventKind::MailboxAccepted
                    if event.shard() == ShardId::new(44) && event.isolate() == sink_a.isolate() =>
                {
                    event.cause().map(|cause| cause.event().get())
                }
                _ => None,
            })
            .collect();
        let sink_b_causes: Vec<_> = trace
            .iter()
            .filter_map(|event| match event.kind() {
                RuntimeEventKind::MailboxAccepted
                    if event.shard() == ShardId::new(44) && event.isolate() == sink_b.isolate() =>
                {
                    event.cause().map(|cause| cause.event().get())
                }
                _ => None,
            })
            .collect();

        assert_eq!(sink_a_causes, sender_a_attempts);
        assert_eq!(sink_b_causes, sender_b_attempts);
    }

    #[test]
    fn cross_shard_simulation_harvests_sources_in_ascending_shard_order() {
        let mut sim = MultiShardSimulator::new(
            [
                NumberedShard(33),
                NumberedShard(22),
                NumberedShard(11),
                NumberedShard(44),
            ],
            SimulatorConfig::default(),
        );

        let sink = sim.register_with_capacity_on::<SimRemoteSink<NumberedShard>, _, _>(
            ShardId::new(44),
            SimRemoteSink {
                marker: PhantomData,
            },
            8,
        );
        let sender11 = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(11),
            SimRemoteSender {
                target: sink,
                marker: PhantomData,
            },
            4,
        );
        let sender22 = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(22),
            SimRemoteSender {
                target: sink,
                marker: PhantomData,
            },
            4,
        );
        let sender33 = sim.register_with_capacity_on::<SimRemoteSender<NumberedShard>, _, _>(
            ShardId::new(33),
            SimRemoteSender {
                target: sink,
                marker: PhantomData,
            },
            4,
        );

        sim.try_send(sender33, SimRemoteEvent::Kick).unwrap();
        sim.try_send(sender22, SimRemoteEvent::Kick).unwrap();
        sim.try_send(sender11, SimRemoteEvent::Kick).unwrap();

        assert_eq!(sim.step(), 3);
        assert_eq!(sim.step(), 1);

        let trace = sim.trace();
        let source_order: Vec<_> = trace
            .iter()
            .filter_map(|event| match event.kind() {
                RuntimeEventKind::SendDispatchAttempted { .. }
                    if matches!(event.shard().get(), 11 | 22 | 33) =>
                {
                    Some((event.shard().get(), event.id().get()))
                }
                _ => None,
            })
            .collect();
        let destination_causes: Vec<_> = trace
            .iter()
            .filter_map(|event| match event.kind() {
                RuntimeEventKind::MailboxAccepted if event.shard() == ShardId::new(44) => {
                    event.cause().map(|cause| cause.event().get())
                }
                _ => None,
            })
            .collect();
        let expected: Vec<_> = source_order.into_iter().map(|(_, id)| id).collect();
        assert_eq!(destination_causes, expected);
    }

    #[test]
    fn multishard_simulation_supervision_keeps_children_on_parent_shard() {
        let mut sim = MultiShardSimulator::new(
            [NumberedShard(11), NumberedShard(22)],
            SimulatorConfig::default(),
        );

        let parent = sim
            .register_with_capacity_on::<SimShardLocalParent<NumberedShard>, _, Infallible>(
                ShardId::new(22),
                SimShardLocalParent {
                    marker: PhantomData,
                },
                4,
            );
        sim.supervise(
            parent,
            SupervisorConfig::new(tina::RestartPolicy::OneForOne, tina::RestartBudget::new(3)),
        );

        sim.try_send(parent, SimShardLocalSupervisionEvent::SpawnChild)
            .unwrap();
        assert_eq!(sim.step(), 1);

        let trace = sim.trace();
        let child = trace
            .iter()
            .find_map(|event| match event.kind() {
                RuntimeEventKind::Spawned { child_isolate }
                    if event.shard() == ShardId::new(22) =>
                {
                    Some(child_isolate)
                }
                _ => None,
            })
            .expect("child spawn should be recorded on the parent shard");
        assert!(
            trace.iter().all(|event| {
                !matches!(event.kind(), RuntimeEventKind::Spawned { .. })
                    || event.shard() == ShardId::new(22)
            }),
            "multi-shard supervision must not create children on another shard"
        );

        let child_address = Address::new(ShardId::new(22), child);
        sim.try_send(child_address, SimShardLocalSupervisionEvent::Panic)
            .unwrap();
        assert_eq!(sim.step(), 1);

        let trace = sim.trace();
        let restart = trace
            .iter()
            .find_map(|event| match event.kind() {
                RuntimeEventKind::RestartChildCompleted {
                    old_isolate,
                    new_isolate,
                    ..
                } if event.shard() == ShardId::new(22) && old_isolate == child => Some(new_isolate),
                _ => None,
            })
            .expect("supervised restart should complete on the parent shard");
        assert_ne!(restart, child);
        assert!(
            trace.iter().all(|event| {
                !matches!(
                    event.kind(),
                    RuntimeEventKind::SupervisorRestartTriggered { .. }
                        | RuntimeEventKind::RestartChildAttempted { .. }
                        | RuntimeEventKind::RestartChildCompleted { .. }
                ) || event.shard() == ShardId::new(22)
            }),
            "restart events must stay on the parent shard"
        );

        sim.try_send(
            Address::new(ShardId::new(22), restart),
            SimShardLocalSupervisionEvent::Noop,
        )
        .unwrap();
    }
}
