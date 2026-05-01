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
    next_event_id: u64,
    next_call_id: u64,
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
        Self {
            shard,
            config,
            entries: Vec::new(),
            child_records: Vec::new(),
            supervisors: Vec::new(),
            next_isolate_id: 1,
            next_listener_id: 1,
            next_stream_id: 1,
            next_event_id: 1,
            next_call_id: 1,
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
                match self.dispatch_local_send(send) {
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
                    Err((target_shard, target_isolate, target_generation, reason)) => {
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
                    if self.execute_effect(index, isolate_id, cause, subeffect, round_messages) {
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
        let call_id = CallId::new(self.next_call_id);
        self.next_call_id += 1;
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

    fn dispatch_local_send(
        &mut self,
        send: ErasedSend,
    ) -> Result<(), (ShardId, IsolateId, AddressGeneration, SendRejectedReason)> {
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
            return Err((
                send.target_shard,
                send.target_isolate,
                send.target_generation,
                SendRejectedReason::Closed,
            ));
        };

        if entry.generation != send.target_generation || entry.stopped.get() {
            return Err((
                send.target_shard,
                send.target_isolate,
                send.target_generation,
                SendRejectedReason::Closed,
            ));
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
                TrySendError::Full(_) => (
                    send.target_shard,
                    send.target_isolate,
                    send.target_generation,
                    SendRejectedReason::Full,
                ),
                TrySendError::Closed(_) => (
                    send.target_shard,
                    send.target_isolate,
                    send.target_generation,
                    SendRejectedReason::Closed,
                ),
            })
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
        let id = tina_runtime::EventId::new(self.next_event_id);
        self.next_event_id += 1;
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
