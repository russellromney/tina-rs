//! Isolate-shape vocabulary for the tina core.
//!
//! Owns the [`Isolate`] / [`CallableIsolate`] traits, the mailbox
//! vocabulary, restart policy/budget types, the `Shard` trait,
//! child-spawn types (`ChildDefinition`, `ChildRef`, the
//! `SpawnObserved` family, `RestartableChildDefinition`), and the
//! `StopResult` envelope. Re-exported from the crate root.
//!
//! ## Module map
//!
//! New isolate-shape vocabulary belongs here. The closed [`Effect`]
//! enum and effect constructors live in `mod effect`; address types
//! live in `mod address`.

use std::marker::PhantomData;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::{
    Address, AddressGeneration, CallContext, CallRejectedReason, Context, Effect, IsolateId,
    ServiceMessage, ShardId,
};

/// A typed state machine that consumes one message at a time and returns an
/// [`Effect`] for the runtime to execute.
///
/// Handlers are synchronous on purpose. They mutate local state, inspect the
/// inbound message, and describe the next action as data.
pub trait Isolate: Sized {
    /// The inbox message type accepted by this isolate.
    type Message;

    /// The payload produced by [`Effect::Reply`].
    ///
    /// Use `()` when the isolate does not reply to the current caller.
    type Reply;

    /// The payload produced by [`Effect::Send`].
    ///
    /// A common choice is [`crate::Outbound`] when an isolate needs to address a
    /// single typed mailbox.
    type Send;

    /// The payload produced by [`Effect::Spawn`].
    ///
    /// [`ChildDefinition`] is the simplest one-shot spawn payload.
    /// [`RestartableChildDefinition`] adds a repeatable factory for children
    /// that a runtime may restart later.
    type Spawn;

    /// The payload produced by [`Effect::SpawnObserved`].
    type SpawnObserved;

    /// The payload produced by [`Effect::SpawnObservedOn`] — an observed
    /// spawn placed on another shard via `spawn_observed(child).on_shard(...)`.
    ///
    /// Defaults to [`std::convert::Infallible`]: isolates that never spawn a
    /// child onto another shard cannot construct the effect, and the type
    /// system enforces it. To use `.on_shard(...)`, set this to
    /// `SpawnObservedRemote<Spawn, Self::Message, ChildMessage, ChildReply>` —
    /// the `#[tina::isolate]` / `#[tina_runtime::isolate]` macros accept a
    /// `spawn_observed_remote = ...` key for it, or set it directly on a
    /// hand-written impl.
    type SpawnObservedRemote = core::convert::Infallible;

    /// The payload produced by [`Effect::Io`].
    ///
    /// I/O describes one runtime-owned external operation (TCP,
    /// timers, future file I/O, child-process spawn, etc.) plus the
    /// information needed to turn the runtime's later result back into one
    /// ordinary [`Self::Message`] for this isolate. The trait crate stays
    /// substrate-neutral here: concrete request and result vocabularies
    /// belong to runtime crates, not to `tina`.
    ///
    /// Use [`std::convert::Infallible`] when an isolate never issues I/O
    /// effects.
    type Io;

    /// The payload produced by [`Effect::Fact`].
    ///
    /// A *fact* is one named, replayable observation the isolate may emit
    /// alongside ordinary effects. Ordinary isolates declare
    /// `Fact = std::convert::Infallible` and never call [`crate::fact`].
    /// Protocol isolates that need to feed replay-visible protocol events
    /// declare `Fact = tina_runtime::ProtocolFact` (or another type that
    /// implements `tina_runtime::IntoRuntimeFact`).
    ///
    /// The conversion bound lives at the runtime registration boundary, not on
    /// this trait, so the substrate-neutral `tina` crate does not depend on
    /// `tina-runtime`.
    type Fact;

    /// The shard abstraction available through [`Context`].
    type Shard: Shard + ?Sized;

    /// Handles one inbound message and returns the next runtime effect.
    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self>;

    /// Handles one inbound call message and returns the next runtime effect.
    ///
    /// Plain sends enter [`handle`](Self::handle). Calls enter this method
    /// with an explicit [`CallContext`], which must be consumed by replying,
    /// rejecting, or promoting it into a [`crate::RequestContext`]. The default
    /// rejects callable traffic so a missing implementation never leaves the
    /// caller waiting for a timeout.
    fn handle_call(&mut self, _msg: Self::Message, call: CallContext<'_, Self>) -> Effect<Self>
    where
        Self::Reply: 'static,
    {
        call.reject(CallRejectedReason::UnsupportedMessage)
    }
}

/// Marker that an [`Isolate`] exposes a meaningful `handle_call` and is
/// therefore safe to register through `tina_runtime::Runtime::register_service`.
///
/// The default `handle_call` on [`Isolate`] always rejects with
/// `CallRejectedReason::UnsupportedMessage`. Registering such an isolate
/// through the callable lane would create a service whose every call returns
/// a runtime rejection. `CallableIsolate` is the type-level "this isolate's
/// `handle_call` is intentional" stamp.
///
/// The `#[tina::isolate]` and `#[tina_runtime::isolate]` macros emit
/// `impl CallableIsolate for ...` automatically when the impl block defines
/// `fn handle_call(...)`. Hand-rolled isolates may implement this trait
/// manually after defining their own `handle_call`.
///
/// Send-only services intentionally do not implement `CallableIsolate` and
/// must be registered through `register_service_send_only`.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a callable service",
    label = "missing `fn handle_call`",
    note = "callable services must define `handle_call(&mut self, msg, call)` on the isolate impl",
    note = "send-only services must register through `register_service_send_only` instead"
)]
pub trait CallableIsolate: Isolate {}

/// `recv` takes `&self` because real SPSC implementations rely on interior
/// mutability (atomics over a ring buffer).
///
/// Concrete implementations may enforce concurrency contracts at runtime rather
/// than in the type system. For example, an SPSC mailbox may panic if more than
/// one producer or more than one consumer enters concurrently even though the
/// trait surface uses shared references.
pub trait Mailbox<T> {
    /// Returns the maximum number of messages the mailbox can hold without
    /// applying backpressure or shedding load.
    fn capacity(&self) -> usize;

    /// Attempts to enqueue a message without blocking.
    fn try_send(&self, message: T) -> Result<(), TrySendError<T>>;

    /// Attempts to dequeue the next message without blocking.
    fn recv(&self) -> Option<T>;

    /// Returns whether the mailbox holds no message right now.
    ///
    /// A cheap readiness probe: the runtime uses it to skip `recv` on quiet
    /// isolates. It must reflect real state for every ingress path (mediated
    /// sends and direct `try_send` alike), so no message is ever skipped. No
    /// default on purpose — a wrong `true` would silently drop scheduling.
    fn is_empty(&self) -> bool;

    /// Closes the mailbox so subsequent `try_send` calls return
    /// [`TrySendError::Closed`]. Idempotent. Already-buffered messages
    /// remain visible to `recv` until drained.
    fn close(&self);

    /// Whether subsequent `try_send` calls return [`TrySendError::Closed`].
    ///
    /// Default is `false`. Runtime-owned adapters call this when reserving a
    /// terminal-delivery slot so a closed shared mailbox is not treated as
    /// free capacity.
    fn is_closed(&self) -> bool {
        false
    }
}

impl<T> Mailbox<T> for Box<dyn Mailbox<T>> {
    fn capacity(&self) -> usize {
        (**self).capacity()
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        (**self).try_send(message)
    }

    fn recv(&self) -> Option<T> {
        (**self).recv()
    }

    fn is_empty(&self) -> bool {
        (**self).is_empty()
    }

    fn close(&self) {
        (**self).close()
    }

    fn is_closed(&self) -> bool {
        (**self).is_closed()
    }
}

/// Error returned by [`Mailbox::try_send`] when a bounded mailbox cannot accept
/// a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrySendError<T> {
    /// The mailbox is currently at capacity.
    Full(T),

    /// The mailbox has been closed and can never accept more messages.
    Closed(T),
}

/// Supervision restart policy for a parent isolate's children.
///
/// These policies describe *which* children participate in a restart once the
/// runtime detects a failure. They do not imply how failures are detected or
/// how restarts are executed; that mechanism belongs to later runtime crates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RestartPolicy {
    /// Restart only the child that failed.
    OneForOne,

    /// Restart the failed child and every sibling in the group.
    OneForAll,

    /// Restart the failed child plus any children started after it.
    RestForOne,
}

impl RestartPolicy {
    /// Returns the restart decision for a child with the given relation to the
    /// child that failed.
    pub const fn decision(self, relation: ChildRelation) -> RestartDecision {
        match (self, relation) {
            (Self::OneForOne, ChildRelation::Failed) => RestartDecision::Restart,
            (Self::OneForOne, ChildRelation::BeforeFailed) => RestartDecision::KeepRunning,
            (Self::OneForOne, ChildRelation::AfterFailed) => RestartDecision::KeepRunning,
            (Self::OneForAll, _) => RestartDecision::Restart,
            (Self::RestForOne, ChildRelation::BeforeFailed) => RestartDecision::KeepRunning,
            (Self::RestForOne, ChildRelation::Failed) => RestartDecision::Restart,
            (Self::RestForOne, ChildRelation::AfterFailed) => RestartDecision::Restart,
        }
    }

    /// Returns whether this policy restarts a child with the given relation to
    /// the child that failed.
    pub const fn restarts(self, relation: ChildRelation) -> bool {
        matches!(self.decision(relation), RestartDecision::Restart)
    }
}

/// Relative position of a child with respect to the child that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChildRelation {
    /// The child was started before the child that failed.
    BeforeFailed,

    /// The child is the one that failed.
    Failed,

    /// The child was started after the child that failed.
    AfterFailed,
}

impl ChildRelation {
    /// Classifies a child by ordinal position relative to the child that
    /// failed.
    pub const fn from_ordinals(child_ordinal: usize, failed_ordinal: usize) -> Self {
        if child_ordinal < failed_ordinal {
            Self::BeforeFailed
        } else if child_ordinal == failed_ordinal {
            Self::Failed
        } else {
            Self::AfterFailed
        }
    }
}

/// Whether a child should be restarted under a [`RestartPolicy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RestartDecision {
    /// The runtime should restart the child.
    Restart,

    /// The runtime should leave the child running.
    KeepRunning,
}

/// Fixed restart allowance for one contiguous budget window.
///
/// `tina` only models the accounting boundary. Later runtime crates decide
/// what starts or resets a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RestartBudget {
    max_restarts: u32,
    window: Option<Duration>,
}

impl RestartBudget {
    /// Creates a lifetime restart budget with a fixed number of allowed
    /// restarts.
    ///
    /// This keeps the pre-windowed API compatible. New code that wants the
    /// counter to reset should use [`within`](Self::within); code that wants
    /// permanent exhaustion can spell it explicitly with
    /// [`lifetime`](Self::lifetime).
    pub const fn new(max_restarts: u32) -> Self {
        Self::lifetime(max_restarts)
    }

    /// Creates a restart budget that never resets.
    pub const fn lifetime(max_restarts: u32) -> Self {
        Self {
            max_restarts,
            window: None,
        }
    }

    /// Creates a restart budget that allows `max_restarts` per `period`.
    ///
    /// The first restart opens the window. A later restart at or after
    /// `period` from that first restart starts a fresh window.
    pub const fn within(max_restarts: u32, period: Duration) -> Self {
        Self {
            max_restarts,
            window: Some(period),
        }
    }

    /// Returns the maximum number of restarts allowed in this budget window.
    pub const fn max_restarts(self) -> u32 {
        self.max_restarts
    }

    /// Returns the reset period for a windowed budget, or `None` for a
    /// lifetime budget.
    pub const fn window(self) -> Option<Duration> {
        self.window
    }

    /// Starts restart accounting at zero consumed restarts.
    pub const fn tracker(self) -> RestartBudgetState {
        RestartBudgetState {
            budget: self,
            restarts_used: 0,
            window_started: None,
        }
    }
}

/// Restart accounting state for a specific [`RestartBudget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RestartBudgetState {
    budget: RestartBudget,
    restarts_used: u32,
    window_started: Option<Instant>,
}

impl RestartBudgetState {
    /// Returns the configured restart budget.
    pub const fn budget(self) -> RestartBudget {
        self.budget
    }

    /// Returns the number of restarts already consumed.
    pub const fn restarts_used(self) -> u32 {
        self.restarts_used
    }

    /// Returns how many restarts remain before the budget is exhausted.
    pub const fn restarts_remaining(self) -> u32 {
        self.budget.max_restarts.saturating_sub(self.restarts_used)
    }

    /// Returns whether the budget is exhausted.
    pub const fn is_exhausted(self) -> bool {
        self.restarts_used >= self.budget.max_restarts
    }

    /// Records one restart attempt.
    ///
    /// Returns the updated accounting state when the restart is still allowed,
    /// or [`RestartBudgetExceeded`] once the budget has been exhausted.
    pub fn record_restart(self) -> Result<Self, RestartBudgetExceeded> {
        self.record_restart_at(Instant::now())
    }

    /// Records one restart attempt at a runtime-owned monotonic instant.
    ///
    /// Lifetime budgets never reset. Windowed budgets reset once `now` is at
    /// least one configured period after the first restart in the current
    /// window.
    pub fn record_restart_at(self, now: Instant) -> Result<Self, RestartBudgetExceeded> {
        let mut state = self;
        if let Some(period) = state.budget.window {
            state = match state.window_started {
                Some(started) if now.duration_since(started) >= period => Self {
                    budget: state.budget,
                    restarts_used: 0,
                    window_started: Some(now),
                },
                Some(_) => state,
                None => Self {
                    budget: state.budget,
                    restarts_used: state.restarts_used,
                    window_started: Some(now),
                },
            };
        }

        if state.is_exhausted() {
            return Err(RestartBudgetExceeded {
                attempted_restart: state.restarts_used.saturating_add(1),
                max_restarts: state.budget.max_restarts,
            });
        }

        Ok(Self {
            budget: state.budget,
            restarts_used: state.restarts_used + 1,
            window_started: state.window_started,
        })
    }

    /// Resets the consumed restart count to zero.
    pub const fn reset(self) -> Self {
        Self {
            budget: self.budget,
            restarts_used: 0,
            window_started: None,
        }
    }
}

/// Error returned when a restart would exceed the configured budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RestartBudgetExceeded {
    attempted_restart: u32,
    max_restarts: u32,
}

impl RestartBudgetExceeded {
    /// Returns the restart ordinal that was rejected.
    pub const fn attempted_restart(self) -> u32 {
        self.attempted_restart
    }

    /// Returns the configured maximum number of allowed restarts.
    pub const fn max_restarts(self) -> u32 {
        self.max_restarts
    }
}

#[cfg(test)]
mod restart_budget_tests {
    use super::*;

    #[test]
    fn lifetime_restart_budget_exhausts_permanently() {
        let now = Instant::now();
        let state = RestartBudget::lifetime(1).tracker();
        let state = state.record_restart_at(now).expect("first restart");
        assert!(
            state
                .record_restart_at(now + Duration::from_secs(3600))
                .is_err()
        );
    }

    #[test]
    fn windowed_restart_budget_resets_after_period() {
        let now = Instant::now();
        let state = RestartBudget::within(1, Duration::from_secs(10)).tracker();
        let state = state.record_restart_at(now).expect("first restart");
        assert!(
            state
                .record_restart_at(now + Duration::from_secs(1))
                .is_err()
        );
        let state = state
            .record_restart_at(now + Duration::from_secs(10))
            .expect("new window restart");
        assert_eq!(state.restarts_used(), 1);
        assert_eq!(state.restarts_remaining(), 0);
    }
}

/// Executor-per-core abstraction.
///
/// Runtime crates implement this trait for their shard type. A shard knows its
/// identifier and can mint typed addresses on that shard.
pub trait Shard {
    /// Returns the logical shard identifier.
    fn id(&self) -> ShardId;

    /// Constructs an [`Address`] for an isolate that lives on this shard.
    fn address<M>(&self, isolate: IsolateId) -> Address<M> {
        Address::new(self.id(), isolate)
    }
}

/// Built-in single-shard type for programs that have only one shard.
///
/// When `#[tina::isolate]` (or `#[tina_runtime::isolate]`) is invoked
/// without a `shard = ...` argument, the macro defaults to this type so
/// single-shard examples and small services do not need to define a
/// one-off shard struct just to satisfy the macro. Programs that run
/// across more than one shard continue to declare their own shard types
/// explicitly.
///
/// `SingleShard` is a real value the user constructs at runtime startup;
/// it is **not** a global mutable singleton, and registering an isolate on
/// it still goes through the runtime's normal registration path.
///
/// The shard id is fixed at `ShardId::new(0)`. If a program mixes
/// `SingleShard` with another shard at id `0`, that is a configuration
/// error and the runtime will reject the registrations as it does today
/// for any duplicate shard id.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SingleShard;

impl SingleShard {
    /// The shard id `SingleShard` always reports.
    pub const ID: ShardId = ShardId::new(0);
}

impl Shard for SingleShard {
    fn id(&self) -> ShardId {
        Self::ID
    }
}

/// A minimal spawn request.
///
/// The runtime owns what "spawn" means operationally. This type only carries
/// the state machine to construct and the requested mailbox capacity.
#[must_use = "a spawn request has no effect until a runtime executes it"]
#[derive(Debug)]
pub struct ChildDefinition<I>
where
    I: Isolate,
{
    isolate: I,
    mailbox_capacity: usize,
    bootstrap_message: Option<I::Message>,
}

impl<I> ChildDefinition<I>
where
    I: Isolate,
{
    /// Creates a spawn request without supervision metadata. Restartable
    /// children use [`RestartableChildDefinition`].
    pub fn new(isolate: I, mailbox_capacity: usize) -> Self {
        Self {
            isolate,
            mailbox_capacity,
            bootstrap_message: None,
        }
    }

    /// Adds one initial child message that the runtime should enqueue after
    /// the child is created.
    pub fn with_initial_message(mut self, message: I::Message) -> Self {
        self.bootstrap_message = Some(message);
        self
    }

    /// Returns the requested mailbox capacity for the spawned isolate.
    pub const fn mailbox_capacity(&self) -> usize {
        self.mailbox_capacity
    }

    /// Returns a shared reference to the isolate state that will be spawned.
    pub const fn isolate(&self) -> &I {
        &self.isolate
    }

    /// Consumes the request and returns its parts.
    pub fn into_parts(self) -> (I, usize, Option<I::Message>) {
        (self.isolate, self.mailbox_capacity, self.bootstrap_message)
    }
}

/// Typed reference to one child incarnation.
///
/// A `ChildRef` is not a liveness promise. It names the address and generation
/// the runtime created for one child spawn. If the child restarts, this value is
/// stale and sends through the old address close/reject like any stale address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChildRef<M, R = ()> {
    /// Typed address for this child incarnation.
    pub address: Address<M, R>,
    /// Address generation for this child incarnation.
    pub generation: AddressGeneration,
}

impl<M, R> ChildRef<M, R> {
    /// Creates a child reference from a runtime-issued address.
    pub const fn new(address: Address<M, R>) -> Self {
        Self {
            address,
            generation: address.generation(),
        }
    }
}

/// Spawn-construction error delivered to a
/// `spawn_observed(...).then(...)` continuation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SpawnObservedError {
    /// The child requested a zero-capacity mailbox.
    ///
    /// Plain [`crate::spawn`] keeps its existing panic-on-zero behavior. The observed
    /// form can report the rejection through its continuation before any child
    /// is recorded.
    ZeroMailboxCapacity,

    /// A cross-shard `spawn_observed(...).on_shard(...)` could not reach the
    /// target shard (its inbound queue was full or the shard had stopped), so
    /// no child was created. Same-shard observed spawn never produces this.
    DestinationUnavailable,

    /// A restartable child's initial isolate or bootstrap factory panicked.
    ///
    /// No child is published. The panic is contained and the initial
    /// continuation receives this error exactly once. Replacement factory
    /// panics remain restart lifecycle facts reported as
    /// `RestartSkippedReason::FactoryPanicked` by runtime owners.
    FactoryPanicked,

    /// The parent's mailbox could not reserve a slot for eventual terminal
    /// result delivery. No child was created and no reservation remains.
    ParentMailboxFull,

    /// The parent's mailbox is closed, so a terminal-delivery reservation
    /// cannot be taken. No child was created.
    ParentMailboxClosed,
}

/// Type-level child address information carried by supported spawn requests.
pub trait SpawnAddress {
    /// Child message type accepted by the spawned isolate.
    type Message;
    /// Child reply type produced by the spawned isolate.
    type Reply;
}

impl SpawnAddress for core::convert::Infallible {
    type Message = core::convert::Infallible;
    type Reply = ();
}

impl<I> SpawnAddress for ChildDefinition<I>
where
    I: Isolate,
{
    type Message = I::Message;
    type Reply = I::Reply;
}

/// Result delivered by `spawn_observed(...).then(...)`.
pub type SpawnObservedResult<M, R = ()> = Result<ChildRef<M, R>, SpawnObservedError>;

/// Continuation invoked by the runtime after an observed spawn is processed.
pub type SpawnObservedContinuation<P, M, R = ()> = Box<dyn FnOnce(SpawnObservedResult<M, R>) -> P>;

/// Repeatable continuation invoked for each successful replacement child.
pub type SpawnRestartedContinuation<P, M, R = ()> = Rc<dyn Fn(ChildRef<M, R>) -> P>;

/// Maps a child's type-erased [`crate::StopResult`] into a parent message.
///
/// Returns `None` when the payload type does not match the mapper's expected
/// type. The runtime disposes that result exactly once and does not deliver a
/// wrong-typed parent event.
pub type SpawnTerminalContinuation<P> = Rc<dyn Fn(crate::StopResult) -> Option<P>>;

/// Parts consumed by runtime adapters that understand observed spawn.
pub type SpawnObservedParts<S, P, M, R = ()> = (
    S,
    SpawnObservedContinuation<P, M, R>,
    Option<SpawnRestartedContinuation<P, M, R>>,
    Option<SpawnTerminalContinuation<P>>,
);

type SpawnObservedMarker<M, R> = PhantomData<fn() -> (M, R)>;

/// Builder returned by [`crate::spawn_observed`].
#[must_use = "a spawn_observed request has no effect until returned as an Effect"]
#[derive(Debug)]
pub struct SpawnObservedBuilder<S, M, R = ()> {
    spawn: S,
    marker: SpawnObservedMarker<M, R>,
}

impl<S, M, R> SpawnObservedBuilder<S, M, R> {
    pub(crate) const fn new(spawn: S) -> Self {
        Self {
            spawn,
            marker: PhantomData,
        }
    }

    /// Observes a typed child [`crate::stop_with`] result as a parent message.
    ///
    /// Chain with [`SpawnObservedTerminalBuilder::then`] or
    /// [`SpawnObservedTerminalBuilder::then_with_restarts`] so the parent
    /// receives initial (and optional replacement) lifecycle events plus the
    /// exact terminal payload. Admission reserves one parent mailbox slot for
    /// that generation's terminal delivery; if the parent mailbox is full or
    /// closed, spawn is not admitted.
    pub fn then_result<T, F>(self, map: F) -> SpawnObservedTerminalBuilder<S, M, R, T, F>
    where
        T: Send + 'static,
        F: 'static,
    {
        SpawnObservedTerminalBuilder {
            spawn: self.spawn,
            map,
            marker: PhantomData,
        }
    }

    /// Observes a typed child [`crate::stop_with`] result as a split-service
    /// event without exposing the service envelope.
    ///
    /// Chain with [`SpawnObservedServiceTerminalBuilder::then_service_event`]
    /// or
    /// [`SpawnObservedServiceTerminalBuilder::then_service_event_with_restarts`].
    pub fn then_service_result<T, F>(
        self,
        map: F,
    ) -> SpawnObservedServiceTerminalBuilder<S, M, R, T, F>
    where
        T: Send + 'static,
        F: 'static,
    {
        SpawnObservedServiceTerminalBuilder {
            spawn: self.spawn,
            map,
            marker: PhantomData,
        }
    }

    /// Maps the runtime's later child-start result into an ordinary parent
    /// continuation message.
    pub fn then<I, P, F>(self, continuation: F) -> Effect<I>
    where
        I: Isolate<Message = P, SpawnObserved = SpawnObserved<S, P, M, R>>,
        F: FnOnce(SpawnObservedResult<M, R>) -> P + 'static,
    {
        Effect::SpawnObserved(SpawnObserved {
            spawn: self.spawn,
            continuation: Box::new(continuation),
            restart_continuation: None,
            terminal_continuation: None,
            marker: PhantomData,
        })
    }

    /// Maps the initial spawn result and every successful replacement child
    /// into ordinary parent messages.
    ///
    /// The initial continuation can receive `Err` when child construction is
    /// rejected or a restartable child's initial factory panics. The restart
    /// continuation only runs after a replacement exists, so it receives a
    /// `ChildRef` directly. Each message uses the parent's bounded mailbox and
    /// normal traced send path; a full or stopped parent does not gain a hidden
    /// lifecycle queue.
    pub fn then_with_restarts<I, P, F, G>(self, initial: F, restarted: G) -> Effect<I>
    where
        I: Isolate<Message = P, SpawnObserved = SpawnObserved<S, P, M, R>>,
        F: FnOnce(SpawnObservedResult<M, R>) -> P + 'static,
        G: Fn(ChildRef<M, R>) -> P + 'static,
    {
        Effect::SpawnObserved(SpawnObserved {
            spawn: self.spawn,
            continuation: Box::new(initial),
            restart_continuation: Some(Rc::new(restarted)),
            terminal_continuation: None,
            marker: PhantomData,
        })
    }

    /// Maps the observed child result into a split-service event without
    /// exposing the service envelope.
    pub fn then_service_event<I, Event, Request, F>(self, continuation: F) -> Effect<I>
    where
        I: Isolate<
                Message = ServiceMessage<Event, Request>,
                SpawnObserved = SpawnObserved<S, ServiceMessage<Event, Request>, M, R>,
            >,
        F: FnOnce(SpawnObservedResult<M, R>) -> Event + 'static,
        Event: 'static,
        Request: 'static,
    {
        self.then(move |result| ServiceMessage::Event(continuation(result)))
    }

    /// Maps the initial spawn result and every successful replacement child
    /// into split-service events without exposing the service envelope.
    ///
    /// This has the same bounded-delivery semantics as
    /// [`Self::then_with_restarts`]: both events use the parent's ordinary
    /// mailbox and traced send path, with no hidden lifecycle queue.
    pub fn then_service_event_with_restarts<I, Event, Request, F, G>(
        self,
        initial: F,
        restarted: G,
    ) -> Effect<I>
    where
        I: Isolate<
                Message = ServiceMessage<Event, Request>,
                SpawnObserved = SpawnObserved<S, ServiceMessage<Event, Request>, M, R>,
            >,
        F: FnOnce(SpawnObservedResult<M, R>) -> Event + 'static,
        G: Fn(ChildRef<M, R>) -> Event + 'static,
        Event: 'static,
        Request: 'static,
    {
        self.then_with_restarts(
            move |result| ServiceMessage::Event(initial(result)),
            move |child| ServiceMessage::Event(restarted(child)),
        )
    }
}

/// Intermediate builder after [`SpawnObservedBuilder::then_result`].
///
/// Finish with [`Self::then`] or [`Self::then_with_restarts`] so the parent
/// receives initial (and optional replacement) events plus the terminal result.
#[must_use = "a spawn_observed terminal mapper has no effect until finished with then*"]
#[derive(Debug)]
pub struct SpawnObservedTerminalBuilder<S, M, R, T, F> {
    spawn: S,
    map: F,
    marker: PhantomData<(M, R, T)>,
}

impl<S, M, R, T, F> SpawnObservedTerminalBuilder<S, M, R, T, F>
where
    T: Send + 'static,
    F: 'static,
{
    /// Maps the runtime's later child-start result into an ordinary parent
    /// message. The terminal mapper from [`SpawnObservedBuilder::then_result`]
    /// remains attached.
    pub fn then<I, P, C>(self, continuation: C) -> Effect<I>
    where
        I: Isolate<Message = P, SpawnObserved = SpawnObserved<S, P, M, R>>,
        F: Fn(T) -> P + 'static,
        C: FnOnce(SpawnObservedResult<M, R>) -> P + 'static,
        P: 'static,
    {
        let map = self.map;
        Effect::SpawnObserved(SpawnObserved {
            spawn: self.spawn,
            continuation: Box::new(continuation),
            restart_continuation: None,
            terminal_continuation: Some(Rc::new(move |result: crate::StopResult| {
                match result.into_any().downcast::<T>() {
                    Ok(value) => Some(map(*value)),
                    Err(_authority) => None,
                }
            })),
            marker: PhantomData,
        })
    }

    /// Maps the initial spawn result and every successful replacement child,
    /// and keeps the terminal mapper from
    /// [`SpawnObservedBuilder::then_result`].
    pub fn then_with_restarts<I, P, C, G>(self, initial: C, restarted: G) -> Effect<I>
    where
        I: Isolate<Message = P, SpawnObserved = SpawnObserved<S, P, M, R>>,
        F: Fn(T) -> P + 'static,
        C: FnOnce(SpawnObservedResult<M, R>) -> P + 'static,
        G: Fn(ChildRef<M, R>) -> P + 'static,
        P: 'static,
    {
        let map = self.map;
        Effect::SpawnObserved(SpawnObserved {
            spawn: self.spawn,
            continuation: Box::new(initial),
            restart_continuation: Some(Rc::new(restarted)),
            terminal_continuation: Some(Rc::new(move |result: crate::StopResult| {
                match result.into_any().downcast::<T>() {
                    Ok(value) => Some(map(*value)),
                    Err(_authority) => None,
                }
            })),
            marker: PhantomData,
        })
    }
}

/// Intermediate builder after [`SpawnObservedBuilder::then_service_result`].
///
/// Finish with [`Self::then_service_event`] or
/// [`Self::then_service_event_with_restarts`].
#[must_use = "a spawn_observed terminal mapper has no effect until finished with then_service*"]
#[derive(Debug)]
pub struct SpawnObservedServiceTerminalBuilder<S, M, R, T, F> {
    spawn: S,
    map: F,
    marker: PhantomData<(M, R, T)>,
}

impl<S, M, R, T, F> SpawnObservedServiceTerminalBuilder<S, M, R, T, F>
where
    T: Send + 'static,
    F: 'static,
{
    /// Maps the observed child-start result into a split-service event and
    /// keeps the terminal mapper as a service event.
    pub fn then_service_event<I, Event, Request, C>(self, continuation: C) -> Effect<I>
    where
        I: Isolate<
                Message = ServiceMessage<Event, Request>,
                SpawnObserved = SpawnObserved<S, ServiceMessage<Event, Request>, M, R>,
            >,
        F: Fn(T) -> Event + 'static,
        C: FnOnce(SpawnObservedResult<M, R>) -> Event + 'static,
        Event: 'static,
        Request: 'static,
    {
        let map = self.map;
        Effect::SpawnObserved(SpawnObserved {
            spawn: self.spawn,
            continuation: Box::new(move |result| ServiceMessage::Event(continuation(result))),
            restart_continuation: None,
            terminal_continuation: Some(Rc::new(move |result: crate::StopResult| {
                match result.into_any().downcast::<T>() {
                    Ok(value) => Some(ServiceMessage::Event(map(*value))),
                    Err(_authority) => None,
                }
            })),
            marker: PhantomData,
        })
    }

    /// Maps initial and replacement lifecycle into split-service events and
    /// keeps the terminal mapper as a service event.
    pub fn then_service_event_with_restarts<I, Event, Request, C, G>(
        self,
        initial: C,
        restarted: G,
    ) -> Effect<I>
    where
        I: Isolate<
                Message = ServiceMessage<Event, Request>,
                SpawnObserved = SpawnObserved<S, ServiceMessage<Event, Request>, M, R>,
            >,
        F: Fn(T) -> Event + 'static,
        C: FnOnce(SpawnObservedResult<M, R>) -> Event + 'static,
        G: Fn(ChildRef<M, R>) -> Event + 'static,
        Event: 'static,
        Request: 'static,
    {
        let map = self.map;
        Effect::SpawnObserved(SpawnObserved {
            spawn: self.spawn,
            continuation: Box::new(move |result| ServiceMessage::Event(initial(result))),
            restart_continuation: Some(Rc::new(move |child| {
                ServiceMessage::Event(restarted(child))
            })),
            terminal_continuation: Some(Rc::new(move |result: crate::StopResult| {
                match result.into_any().downcast::<T>() {
                    Ok(value) => Some(ServiceMessage::Event(map(*value))),
                    Err(_authority) => None,
                }
            })),
            marker: PhantomData,
        })
    }
}

/// Spawn request plus continuation for delivering a typed child reference.
#[must_use = "a spawn_observed request has no effect until returned as an Effect"]
pub struct SpawnObserved<S, P, M, R = ()> {
    spawn: S,
    continuation: SpawnObservedContinuation<P, M, R>,
    restart_continuation: Option<SpawnRestartedContinuation<P, M, R>>,
    terminal_continuation: Option<SpawnTerminalContinuation<P>>,
    marker: SpawnObservedMarker<M, R>,
}

impl<S, P, M, R> std::fmt::Debug for SpawnObserved<S, P, M, R>
where
    S: std::fmt::Debug,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SpawnObserved")
            .field("spawn", &self.spawn)
            .field(
                "has_terminal_continuation",
                &self.terminal_continuation.is_some(),
            )
            .finish_non_exhaustive()
    }
}

impl<S, P, M, R> SpawnObserved<S, P, M, R> {
    /// Consumes this request into its spawn payload and continuations.
    pub fn into_parts(self) -> SpawnObservedParts<S, P, M, R> {
        (
            self.spawn,
            self.continuation,
            self.restart_continuation,
            self.terminal_continuation,
        )
    }
}

#[cfg(test)]
mod spawn_observed_service_event_tests {
    use std::cell::Cell;
    use std::convert::Infallible;

    use super::*;
    use crate::Outbound;

    #[derive(Debug)]
    enum ParentEvent {
        Started(SpawnObservedResult<u8>),
        Restarted(ChildRef<u8>),
    }

    struct Parent;

    impl Isolate for Parent {
        type Message = ServiceMessage<ParentEvent, Infallible>;
        type Reply = ();
        type Send = Outbound<Infallible>;
        type Spawn = Infallible;
        type SpawnObserved = SpawnObserved<(), Self::Message, u8>;
        type Io = Infallible;
        type Fact = Infallible;
        type Shard = crate::SingleShard;

        fn handle(
            &mut self,
            _message: Self::Message,
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            unreachable!("builder contract test does not execute an isolate")
        }
    }

    struct DropProbe(std::rc::Rc<Cell<usize>>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    fn service_restart_parts() -> SpawnObservedParts<(), <Parent as Isolate>::Message, u8> {
        let effect: Effect<Parent> = SpawnObservedBuilder::new(())
            .then_service_event_with_restarts(ParentEvent::Started, ParentEvent::Restarted);
        let Effect::SpawnObserved(observed) = effect else {
            panic!("builder must create SpawnObserved")
        };
        observed.into_parts()
    }

    #[test]
    fn service_restart_continuations_preserve_initial_and_replacement_types() {
        let (_, initial, restarted, terminal) = service_restart_parts();
        assert!(terminal.is_none());
        assert!(matches!(
            initial(Err(SpawnObservedError::ZeroMailboxCapacity)),
            ServiceMessage::Event(ParentEvent::Started(Err(
                SpawnObservedError::ZeroMailboxCapacity
            )))
        ));

        let (_, initial, restarted_again, _) = service_restart_parts();
        let first = ChildRef::new(Address::new(ShardId::new(3), IsolateId::new(7)));
        assert!(matches!(
            initial(Ok(first)),
            ServiceMessage::Event(ParentEvent::Started(Ok(actual))) if actual == first
        ));

        let replacement = ChildRef::new(Address::new(ShardId::new(3), IsolateId::new(8)));
        let restarted = restarted.expect("restart continuation");
        let restarted_again = restarted_again.expect("restart continuation");
        assert!(matches!(
            restarted(replacement),
            ServiceMessage::Event(ParentEvent::Restarted(actual)) if actual == replacement
        ));
        assert!(matches!(
            restarted_again(replacement),
            ServiceMessage::Event(ParentEvent::Restarted(actual)) if actual == replacement
        ));
        assert_ne!(first.address, replacement.address);
    }

    #[test]
    fn dropping_unexecuted_service_restart_effect_settles_captured_authority() {
        let initial_drops = std::rc::Rc::new(Cell::new(0));
        let restart_drops = std::rc::Rc::new(Cell::new(0));
        let initial_probe = DropProbe(std::rc::Rc::clone(&initial_drops));
        let restart_probe = DropProbe(std::rc::Rc::clone(&restart_drops));

        let effect: Effect<Parent> = SpawnObservedBuilder::new(())
            .then_service_event_with_restarts(
                move |result| {
                    let _authority = initial_probe;
                    ParentEvent::Started(result)
                },
                move |child| {
                    let _authority = &restart_probe;
                    ParentEvent::Restarted(child)
                },
            );
        drop(effect);

        assert_eq!(initial_drops.get(), 1);
        assert_eq!(restart_drops.get(), 1);
    }
}

impl<S, M, R> SpawnObservedBuilder<S, M, R> {
    /// Places the observed child on `shard` instead of the parent's shard.
    ///
    /// The child constructor and its bootstrap must be `Send` to cross the
    /// shard boundary (hence the `S: Send` bound). The child's address still
    /// returns to the parent through the same `.then(...)` continuation, on a
    /// later turn, once the destination shard registers it. Same-shard
    /// `spawn_observed` is unaffected — this method is the only place the
    /// `Send` requirement appears.
    ///
    /// Scope and sharp edges (this is the first cross-shard sub-phase):
    ///
    /// - Only [`ChildDefinition`] is supported as the spawn payload.
    ///   [`RestartableChildDefinition`] is `!Send` (it holds boxed `Fn`
    ///   factories), so `.on_shard(...)` on one is a `Send` trait-bound error;
    ///   cross-shard *restartable* children await the restart protocol.
    /// - A *cross-shard* child is not yet supervision-owned: the owner holds
    ///   its `ChildRef`, but `StopChildren` / supervision do not yet reach
    ///   across shards. `.on_shard(my_own_shard)` degenerates to an ordinary
    ///   owned local `spawn_observed`.
    /// - Targeting a `ShardId` the runtime does not own panics the worker
    ///   (same as any cross-shard `send`/`call` to an unknown shard); a
    ///   *known* but full/stopped shard settles the continuation with
    ///   [`SpawnObservedError::DestinationUnavailable`].
    pub fn on_shard(self, shard: ShardId) -> RemoteSpawnObservedBuilder<S, M, R>
    where
        S: Send,
    {
        RemoteSpawnObservedBuilder {
            spawn: self.spawn,
            target_shard: shard,
            marker: PhantomData,
        }
    }
}

/// Builder returned by [`SpawnObservedBuilder::on_shard`]. Finishes with
/// [`Self::then`] into an [`Effect::SpawnObservedOn`].
#[must_use = "an on_shard spawn request has no effect until returned as an Effect"]
#[derive(Debug)]
pub struct RemoteSpawnObservedBuilder<S, M, R = ()> {
    spawn: S,
    target_shard: ShardId,
    marker: SpawnObservedMarker<M, R>,
}

impl<S, M, R> RemoteSpawnObservedBuilder<S, M, R> {
    /// Maps the runtime's later cross-shard child-start result into a parent
    /// continuation message.
    pub fn then<I, P, F>(self, continuation: F) -> Effect<I>
    where
        I: Isolate<Message = P, SpawnObservedRemote = SpawnObservedRemote<S, P, M, R>>,
        F: FnOnce(SpawnObservedResult<M, R>) -> P + 'static,
    {
        Effect::SpawnObservedOn(SpawnObservedRemote {
            spawn: self.spawn,
            target_shard: self.target_shard,
            continuation: Box::new(continuation),
            marker: PhantomData,
        })
    }
}

/// Cross-shard observed-spawn request: spawn payload, target shard, and the
/// continuation that delivers the typed child reference back to the parent.
#[must_use = "an on_shard spawn request has no effect until returned as an Effect"]
pub struct SpawnObservedRemote<S, P, M, R = ()> {
    spawn: S,
    target_shard: ShardId,
    continuation: SpawnObservedContinuation<P, M, R>,
    marker: SpawnObservedMarker<M, R>,
}

impl<S, P, M, R> std::fmt::Debug for SpawnObservedRemote<S, P, M, R>
where
    S: std::fmt::Debug,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SpawnObservedRemote")
            .field("spawn", &self.spawn)
            .field("target_shard", &self.target_shard)
            .finish_non_exhaustive()
    }
}

impl<S, P, M, R> SpawnObservedRemote<S, P, M, R> {
    /// The shard the child is to be placed on.
    pub fn target_shard(&self) -> ShardId {
        self.target_shard
    }

    /// Consumes this request into its spawn payload, target shard, and
    /// continuation.
    pub fn into_parts(self) -> (S, ShardId, SpawnObservedContinuation<P, M, R>) {
        (self.spawn, self.target_shard, self.continuation)
    }
}

/// A restartable spawn request backed by a repeatable isolate factory.
///
/// Use [`ChildDefinition`] when a child only needs to be created once. Use
/// `RestartableChildDefinition` when the runtime must keep a recipe for creating
/// fresh replacement isolate state later. The factory may capture immutable
/// configuration with normal Rust closure captures, for example
/// `move || Worker::new(tenant_id)`. The factory is `Fn`, not `FnMut`; mutable
/// state shared across restarts must use interior mutability.
#[must_use = "a spawn request has no effect until a runtime executes it"]
pub struct RestartableChildDefinition<I>
where
    I: Isolate,
{
    factory: Box<dyn Fn() -> I>,
    mailbox_capacity: usize,
    bootstrap_factory: Option<Box<dyn Fn() -> I::Message>>,
}

impl<I> std::fmt::Debug for RestartableChildDefinition<I>
where
    I: Isolate,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RestartableChildDefinition")
            .field("mailbox_capacity", &self.mailbox_capacity)
            .finish_non_exhaustive()
    }
}

impl<I> RestartableChildDefinition<I>
where
    I: Isolate,
{
    /// Creates a new restartable spawn request.
    pub fn new<F>(factory: F, mailbox_capacity: usize) -> Self
    where
        F: Fn() -> I + 'static,
    {
        Self {
            factory: Box::new(factory),
            mailbox_capacity,
            bootstrap_factory: None,
        }
    }

    /// Adds one initial child message that the runtime should enqueue after
    /// each child incarnation is created, including restarts.
    pub fn with_initial_message<F>(mut self, bootstrap: F) -> Self
    where
        F: Fn() -> I::Message + 'static,
    {
        self.bootstrap_factory = Some(Box::new(bootstrap));
        self
    }

    /// Returns the requested mailbox capacity for the spawned isolate.
    pub const fn mailbox_capacity(&self) -> usize {
        self.mailbox_capacity
    }

    /// Consumes the request and returns its repeatable factory plus mailbox
    /// capacity.
    pub fn into_parts(self) -> RestartableChildParts<I> {
        (self.factory, self.mailbox_capacity, self.bootstrap_factory)
    }
}

/// Tuple shape returned by [`RestartableChildDefinition::into_parts`].
///
/// Spelled out as a type alias purely so the runtime crate can name it
/// without tripping clippy's `type_complexity` lint.
pub type RestartableChildParts<I> = (
    Box<dyn Fn() -> I>,
    usize,
    Option<Box<dyn Fn() -> <I as Isolate>::Message>>,
);

/// Sendable restartable spawn request for local in-process cross-shard
/// children.
#[must_use = "a spawn request has no effect until a runtime executes it"]
pub struct CrossShardRestartableChildDefinition<I>
where
    I: Isolate,
{
    factory: Box<dyn Fn() -> I + Send + Sync>,
    mailbox_capacity: usize,
    bootstrap_factory: Option<Box<dyn Fn() -> I::Message + Send + Sync>>,
}

impl<I> std::fmt::Debug for CrossShardRestartableChildDefinition<I>
where
    I: Isolate,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CrossShardRestartableChildDefinition")
            .field("mailbox_capacity", &self.mailbox_capacity)
            .finish_non_exhaustive()
    }
}

impl<I> CrossShardRestartableChildDefinition<I>
where
    I: Isolate,
{
    /// Creates a new sendable cross-shard restartable spawn request.
    pub fn new<F>(factory: F, mailbox_capacity: usize) -> Self
    where
        F: Fn() -> I + Send + Sync + 'static,
    {
        Self {
            factory: Box::new(factory),
            mailbox_capacity,
            bootstrap_factory: None,
        }
    }

    /// Adds one initial child message for the first and replacement
    /// incarnations.
    pub fn with_initial_message<F>(mut self, bootstrap: F) -> Self
    where
        F: Fn() -> I::Message + Send + Sync + 'static,
    {
        self.bootstrap_factory = Some(Box::new(bootstrap));
        self
    }

    /// Returns the requested mailbox capacity.
    pub const fn mailbox_capacity(&self) -> usize {
        self.mailbox_capacity
    }

    /// Consumes the request and returns its sendable parts.
    pub fn into_parts(self) -> CrossShardRestartableChildParts<I> {
        (self.factory, self.mailbox_capacity, self.bootstrap_factory)
    }
}

/// Tuple returned by [`CrossShardRestartableChildDefinition::into_parts`].
pub type CrossShardRestartableChildParts<I> = (
    Box<dyn Fn() -> I + Send + Sync>,
    usize,
    Option<Box<dyn Fn() -> <I as Isolate>::Message + Send + Sync>>,
);

impl<I> SpawnAddress for RestartableChildDefinition<I>
where
    I: Isolate,
{
    type Message = I::Message;
    type Reply = I::Reply;
}

impl<I> SpawnAddress for CrossShardRestartableChildDefinition<I>
where
    I: Isolate,
{
    type Message = I::Message;
    type Reply = I::Reply;
}
