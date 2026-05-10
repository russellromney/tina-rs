#![deny(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

//! Core traits and data types for the `tina-rs` discipline.
//!
//! `tina` is intentionally a trait crate: it names the vocabulary that later
//! runtime crates will implement, but it does not ship a scheduler, mailbox,
//! or supervisor.
//!
//! # Effect Shape
//!
//! Phase Sputnik resolves the roadmap's first open question in favor of a
//! **closed** [`Effect`] enum rather than a per-isolate associated effect type.
//!
//! This keeps the dispatcher contract small and uniform: every isolate can only
//! ask for the same handful of verbs (`Reply`, `Send`, `Spawn`, `Stop`,
//! `RestartChildren`, `Call`, and ordered `Batch`). That simplicity matters
//! for the runtime crates we add in later phases, because they can switch on
//! one shared enum instead of handling an open-ended effect language for every
//! isolate.
//!
//! The tradeoff is that the effect *payloads* stay per-isolate via associated
//! types on [`Isolate`]. An isolate decides what a reply looks like, how it
//! packages an outbound send, and what data is needed to request a spawn, while
//! the dispatcher still sees one common envelope. The downside is that adding a
//! brand-new verb means changing this crate, not just defining a new associated
//! type. That is a deliberate Sputnik constraint.
//!
//! # Example
//!
//! The example below compiles and runs without a runtime because handlers only
//! build values; they do not perform I/O directly.
//!
//! ```
//! use tina::{
//!     send, reply, Address, Context, Effect, Isolate, IsolateId, Outbound, Shard, ShardId,
//! };
//!
//! #[derive(Debug, Clone, Copy, PartialEq, Eq)]
//! enum Message {
//!     Add(u64),
//!     Read,
//! }
//!
//! #[derive(Debug, Clone, Copy, PartialEq, Eq)]
//! enum AuditEvent {
//!     Total(u64),
//! }
//!
//! struct InlineShard;
//!
//! impl Shard for InlineShard {
//!     fn id(&self) -> ShardId {
//!         ShardId::new(0)
//!     }
//! }
//!
//! #[derive(Debug)]
//! struct Counter {
//!     total: u64,
//!     audit: Address<AuditEvent>,
//! }
//!
//! #[tina::isolate(
//!     message = Message,
//!     reply = u64,
//!     send = Outbound<AuditEvent>,
//!     shard = InlineShard
//! )]
//! impl Counter {
//!     fn handle(&mut self, msg: Message, _ctx: &mut Context<'_, InlineShard, Self::Reply>) -> Effect<Self> {
//!         match msg {
//!             Message::Add(delta) => {
//!                 self.total += delta;
//!                 send(self.audit, AuditEvent::Total(self.total))
//!             }
//!             Message::Read => reply(self.total),
//!         }
//!     }
//! }
//!
//! let audit = Address::new(ShardId::new(0), IsolateId::new(99));
//! let mut shard = InlineShard;
//! let mut ctx = Context::<_, <Counter as Isolate>::Reply>::new_typed(
//!     &mut shard,
//!     IsolateId::new(1),
//! );
//! let mut counter = Counter { total: 0, audit };
//!
//! match counter.handle(Message::Add(3), &mut ctx) {
//!     Effect::Send(outbound) => {
//!         let (destination, message) = outbound.into_parts();
//!         assert_eq!(destination, audit);
//!         assert_eq!(message, AuditEvent::Total(3));
//!     }
//!     _ => panic!("unexpected effect"),
//! }
//!
//! assert!(matches!(
//!     counter.handle(Message::Read, &mut ctx),
//!     Effect::Reply(3)
//! ));
//! ```

use std::any::{Any, TypeId};
use std::fmt;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

/// Type-erased payload for [`Effect::StopWith`].
///
/// Holds the isolate's final value as `Box<dyn Any + Send>` so [`Effect`]
/// keeps no `Debug` bound on `T`. The host receives the value through
/// `runtime.observe_result::<T>(addr)`; type mismatch is a typed wait
/// outcome, not a panic.
pub struct StopResult(Box<dyn Any + Send>);

impl StopResult {
    /// Boxes a typed final value.
    pub fn new<T: Send + 'static>(value: T) -> Self {
        Self(Box::new(value))
    }

    /// Returns the inner boxed `Any` for runtime downcast.
    pub fn into_any(self) -> Box<dyn Any + Send> {
        self.0
    }

    /// Inner value's `TypeId`.
    pub fn type_id(&self) -> std::any::TypeId {
        (*self.0).type_id()
    }
}

impl fmt::Debug for StopResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StopResult")
            .field("type_id", &(*self.0).type_id())
            .finish()
    }
}

/// Declares a Tina isolate from an inherent `impl` block.
///
/// This is the preferred authoring path for ordinary Tina code. Only `message`
/// is required for single-shard isolates: omitted `shard = ...` defaults to
/// [`SingleShard`]. `reply`, `send`, `spawn`, and `call` default to the
/// no-reply/no-send/no-spawn/no-runtime-call shape.
///
/// ```compile_fail
/// struct DemoShard;
///
/// impl tina::Shard for DemoShard {
///     fn id(&self) -> tina::ShardId {
///         tina::ShardId::new(0)
///     }
/// }
///
/// struct Worker;
///
/// #[tina::isolate(message = (), shard = DemoShard)]
/// impl Worker {
///     async fn handle(
///         &mut self,
///         _msg: (),
///         _ctx: &mut tina::Context<'_, DemoShard, Self::Reply>,
///     ) -> tina::Effect<Self> {
///         tina::noop()
///     }
/// }
/// ```
pub use tina_macros::isolate;

type AddressMarker<M, R> = PhantomData<fn(M, R) -> (M, R)>;

/// Declares the associated-type slab for one [`Isolate`] impl.
///
/// This macro is intentionally small and boring. Prefer [`#[tina::isolate]`](isolate)
/// for ordinary code; use this when an explicit trait impl is clearer for
/// tests, generated code, or unusual boundaries.
///
/// ```
/// struct DemoShard;
///
/// impl tina::Shard for DemoShard {
///     fn id(&self) -> tina::ShardId {
///         tina::ShardId::new(0)
///     }
/// }
///
/// struct Worker;
///
/// impl tina::Isolate for Worker {
///     tina::isolate_types! {
///         message: (),
///         reply: (),
///         send: tina::Outbound<std::convert::Infallible>,
///         spawn: std::convert::Infallible,
///         call: std::convert::Infallible,
///         shard: DemoShard,
///     }
///
///     fn handle(
///         &mut self,
///         _msg: Self::Message,
///         _ctx: &mut tina::Context<'_, Self::Shard, Self::Reply>,
///     ) -> tina::Effect<Self> {
///         tina::stop()
///     }
/// }
/// ```
#[macro_export]
macro_rules! isolate_types {
    (
        message: $message:ty,
        reply: $reply:ty,
        send: $send:ty,
        spawn: $spawn:ty,
        call: $call:ty,
        shard: $shard:ty $(,)?
    ) => {
        type Message = $message;
        type Reply = $reply;
        type Send = $send;
        type Spawn = $spawn;
        type Call = $call;
        type Shard = $shard;
    };
}

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
    /// A common choice is [`Outbound`] when an isolate needs to address a
    /// single typed mailbox.
    type Send;

    /// The payload produced by [`Effect::Spawn`].
    ///
    /// [`ChildDefinition`] is the simplest one-shot spawn payload.
    /// [`RestartableChildDefinition`] adds a repeatable factory for children
    /// that a runtime may restart later.
    type Spawn;

    /// The payload produced by [`Effect::Call`].
    ///
    /// A call describes one runtime-owned external operation (TCP I/O,
    /// timers, future file I/O, child-process spawn, etc.) plus the
    /// information needed to turn the runtime's later result back into one
    /// ordinary [`Self::Message`] for this isolate. The trait crate stays
    /// substrate-neutral here: concrete request and result vocabularies
    /// belong to runtime crates, not to `tina`.
    ///
    /// Use [`std::convert::Infallible`] when an isolate never issues call
    /// effects.
    type Call;

    /// The shard abstraction available through [`Context`].
    type Shard: Shard + ?Sized;

    /// Handles one inbound message and returns the next runtime effect.
    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self>;
}

/// A closed set of actions that an [`Isolate`] may request from the runtime.
///
/// The enum is closed so later runtime crates can implement a single effect
/// dispatcher. The payloads remain isolate-specific through the associated
/// types on [`Isolate`].
#[must_use = "handlers communicate with the runtime by returning an Effect"]
#[derive(Debug)]
pub enum Effect<I>
where
    I: Isolate,
{
    /// The handler completed without asking the runtime to do anything else.
    Noop,

    /// Return a response to the current caller.
    Reply(I::Reply),

    /// Deliver a typed message to another isolate.
    Send(I::Send),

    /// Start a new isolate instance.
    Spawn(I::Spawn),

    /// Stop the current isolate.
    Stop,

    /// Stop the current isolate and publish a typed final result for a
    /// host-registered `observe_result::<T>` waiter.
    ///
    /// Same lifecycle and `IsolateStopped` event as [`Self::Stop`]. With a
    /// waiter: deliver `T` on type match, `TypeMismatch` otherwise. Without
    /// a waiter: drop the value (no replay cache).
    StopWith(StopResult),

    /// Restart this isolate's children according to the runtime's supervision
    /// policy.
    RestartChildren,

    /// Ask the runtime to perform one external operation on the isolate's
    /// behalf and deliver the result back later as an ordinary
    /// [`Isolate::Message`] value.
    ///
    /// The handler stays synchronous. The runtime owns the resource (TCP
    /// listener, stream, timer, etc.) and assigns deterministic ids; the
    /// isolate only ever sees opaque ids inside its own message vocabulary.
    /// Completion is delivered as a regular later-turn `Message`, never as
    /// a second handler entry point.
    Call(I::Call),

    /// Execute several existing effects in deterministic left-to-right order.
    ///
    /// This keeps the effect set closed while letting one handler turn express
    /// small explicit workflows such as "spawn child, then re-arm accept" or
    /// "send audit record, then send follow-up". A runtime should execute the
    /// contained effects in source order. If a [`Stop`](Self::Stop) appears in
    /// the batch, later effects in the same batch are not executed.
    ///
    /// An empty batch is equivalent to [`Noop`](Self::Noop).
    ///
    /// **Same-stream caveat:** `Batch` does *not* serialize
    /// runtime calls that target the same I/O resource. Issuing several
    /// `tcp_write` calls against the same `StreamId` inside a single batch is
    /// unsupported — the second runtime call against the same stream lane
    /// returns `CallError::ResourceBusy` because the first is still pending.
    /// For "do these writes one after another" semantics, fold the loop
    /// through the isolate's own continuation messages (one
    /// `tcp_write(...).reply(...)`, then another from the next handler turn).
    /// See `docs/tcp-loops.md` for canonical patterns.
    Batch(Vec<Effect<I>>),

    /// Reply through a previously captured deferred reply slot.
    ///
    /// Equivalent to [`Reply`](Self::Reply) but routes the reply through the
    /// named slot instead of the current message's caller. The slot is
    /// one-shot: the runtime consumes it on delivery.
    ReplyTo(DeferredReply<I::Reply>, I::Reply),
}

/// Returns an effect that asks the runtime to do nothing else this turn.
pub fn noop<I>() -> Effect<I>
where
    I: Isolate,
{
    Effect::Noop
}

/// Returns an effect that replies to the current caller.
pub fn reply<I>(value: I::Reply) -> Effect<I>
where
    I: Isolate,
{
    Effect::Reply(value)
}

/// Returns an effect that sends one typed message to another isolate.
pub fn send<I, M, R>(destination: Address<M, R>, message: M) -> Effect<I>
where
    I: Isolate<Send = Outbound<M>>,
{
    Effect::Send(Outbound::new(destination, message))
}

/// Returns an effect that asks the runtime to spawn one child.
pub fn spawn<I>(child: I::Spawn) -> Effect<I>
where
    I: Isolate,
{
    Effect::Spawn(child)
}

/// Returns an effect that stops the current isolate.
pub fn stop<I>() -> Effect<I>
where
    I: Isolate,
{
    Effect::Stop
}

/// Stops the current isolate and offers a typed result to a registered
/// `observe_result::<T>` waiter. Same `IsolateStopped` event as [`stop`].
/// No waiter or type mismatch → value dropped (no replay cache).
pub fn stop_with<I, T>(value: T) -> Effect<I>
where
    I: Isolate,
    T: Send + 'static,
{
    Effect::StopWith(StopResult::new(value))
}

/// Returns an effect that asks the runtime to restart this isolate's direct
/// children according to supervision policy.
pub fn restart_children<I>() -> Effect<I>
where
    I: Isolate,
{
    Effect::RestartChildren
}

/// Returns an effect that executes several existing effects in source order.
pub fn batch<I, T>(effects: T) -> Effect<I>
where
    I: Isolate,
    T: IntoIterator<Item = Effect<I>>,
{
    Effect::Batch(effects.into_iter().collect())
}

/// Returns an effect that replies through a previously captured deferred slot.
///
/// The slot is one-shot. The runtime consumes it whether or not the original
/// caller is still alive; if the caller already closed (timeout or shutdown),
/// the reply is rejected and a trace fact records the reason.
///
/// `DeferredReply` is not `Clone`, so a duplicate `reply_to` against the
/// same slot is a compile error rather than a runtime trace fact:
///
/// ```compile_fail
/// # use tina::{DeferredReply, Effect, Isolate, Outbound, reply_to};
/// # struct S;
/// # impl Isolate for S {
/// #     type Message = (); type Reply = u32;
/// #     type Send = Outbound<std::convert::Infallible>;
/// #     type Spawn = std::convert::Infallible;
/// #     type Call = std::convert::Infallible;
/// #     type Shard = tina::SingleShard;
/// #     fn handle(&mut self, _: (), _: &mut tina::Context<'_, Self::Shard, Self::Reply>) -> Effect<Self> {
/// #         tina::noop()
/// #     }
/// # }
/// fn _double_reply(slot: DeferredReply<u32>) -> (Effect<S>, Effect<S>) {
///     (reply_to(slot, 1), reply_to(slot, 2)) // borrow of moved value
/// }
/// ```
pub fn reply_to<I>(slot: DeferredReply<I::Reply>, value: I::Reply) -> Effect<I>
where
    I: Isolate,
{
    Effect::ReplyTo(slot, value)
}

/// Documented sugar for ordered runtime-call sequences.
///
/// `sequence(...)` is equivalent to [`batch`]: the runtime executes the
/// contained effects in source order, a [`Stop`](Effect::Stop) short-
/// circuits the rest, and an empty input is [`Noop`](Effect::Noop). The
/// difference is *intent*: use `sequence` when the items are runtime calls
/// or sends that should happen left-to-right, and use [`batch`] when the
/// items happen to be a small list of unrelated effects.
///
/// The same caveat applies as for [`batch`]: items targeting the same I/O
/// resource (e.g. multiple `tcp_write` calls on the same stream) still
/// return `CallError::ResourceBusy` for the second-and-later calls. For
/// "write, then read, then write again" patterns on a single stream,
/// continue using continuation messages from the isolate's handler.
pub fn sequence<I, T>(effects: T) -> Effect<I>
where
    I: Isolate,
    T: IntoIterator<Item = Effect<I>>,
{
    Effect::Batch(effects.into_iter().collect())
}

/// A bounded, typed inbox.
///
/// Sputnik only names the capability. Concrete mailbox implementations arrive
/// in Phase Pioneer.
///
/// `recv` takes `&self` because real SPSC implementations rely on interior
/// mutability (atomics over a ring buffer). Phase Pioneer may revisit this
/// with a `Sender`/`Receiver` split — see ROADMAP "Open questions".
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

    /// Closes the mailbox so subsequent `try_send` calls return
    /// [`TrySendError::Closed`]. Idempotent. Already-buffered messages
    /// remain visible to `recv` until drained.
    fn close(&self);
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

    fn close(&self) {
        (**self).close()
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
}

impl RestartBudget {
    /// Creates a restart budget with a fixed number of allowed restarts.
    pub const fn new(max_restarts: u32) -> Self {
        Self { max_restarts }
    }

    /// Returns the maximum number of restarts allowed in this budget window.
    pub const fn max_restarts(self) -> u32 {
        self.max_restarts
    }

    /// Starts restart accounting at zero consumed restarts.
    pub const fn tracker(self) -> RestartBudgetState {
        RestartBudgetState {
            budget: self,
            restarts_used: 0,
        }
    }
}

/// Restart accounting state for a specific [`RestartBudget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RestartBudgetState {
    budget: RestartBudget,
    restarts_used: u32,
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
        if self.is_exhausted() {
            return Err(RestartBudgetExceeded {
                attempted_restart: self.restarts_used.saturating_add(1),
                max_restarts: self.budget.max_restarts,
            });
        }

        Ok(Self {
            budget: self.budget,
            restarts_used: self.restarts_used + 1,
        })
    }

    /// Resets the consumed restart count to zero.
    pub const fn reset(self) -> Self {
        Self {
            budget: self.budget,
            restarts_used: 0,
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

/// Executor-per-core abstraction.
///
/// Runtime crates will implement this trait for their shard type. Sputnik keeps
/// the surface deliberately small: a shard knows its identifier and can mint
/// typed addresses on that shard.
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

/// Per-handler context provided by the runtime.
///
/// `Context` lets a handler inspect its current shard and build typed
/// [`Address`] values without performing side effects directly.
#[derive(Debug)]
pub struct Context<'a, S, R = ()>
where
    S: Shard + ?Sized,
{
    shard: &'a mut S,
    current_isolate: IsolateId,
    caller: Option<MessageCaller>,
    _reply: PhantomData<fn(R) -> R>,
}

impl<'a, S> Context<'a, S, ()>
where
    S: Shard + ?Sized,
{
    /// Creates a new handler context for the current isolate.
    pub fn new(shard: &'a mut S, current_isolate: IsolateId) -> Self {
        Context::new_typed(shard, current_isolate)
    }
}

impl<'a, S, R> Context<'a, S, R>
where
    S: Shard + ?Sized,
{
    /// Creates a new reply-typed handler context for the current isolate.
    ///
    /// Runtime crates use this when invoking [`Isolate::handle`] so
    /// deferred reply slots inherit the current isolate's reply type.
    #[doc(hidden)]
    pub fn new_typed(shard: &'a mut S, current_isolate: IsolateId) -> Self {
        Self {
            shard,
            current_isolate,
            caller: None,
            _reply: PhantomData,
        }
    }

    /// Attach the current message's caller. Runtime-only constructor.
    ///
    /// Used by runtime crates so handlers can capture the caller as a
    /// deferred reply slot via [`take_reply_slot`](Self::take_reply_slot).
    /// Ordinary application code does not call this.
    #[doc(hidden)]
    pub fn with_caller(mut self, caller: MessageCaller) -> Self {
        self.caller = Some(caller);
        self
    }

    /// Captures the current caller as a one-shot deferred reply slot.
    ///
    /// On success, returns a typed [`DeferredReply<R>`] that the handler
    /// (or a later handler turn on the same isolate) may pass to
    /// [`reply_to`] to answer the original caller.
    ///
    /// Errors:
    ///
    /// - [`TakeReplySlotError::NoCaller`]: the current message was a
    ///   plain send, or the slot was already taken on this turn.
    /// - [`TakeReplySlotError::CrossShardUnsupported`]: the current
    ///   call came from another shard. First-form deferred reply
    ///   slots only support same-shard callers because caller-liveness
    ///   sweep depends on the local pending-isolate-call table.
    ///
    /// Capturing is irreversible: once taken, the runtime will not also
    /// honor an [`Effect::Reply`] for the same call. Returning
    /// `Effect::Reply` after `take_reply_slot` is a no-op against the
    /// original caller.
    ///
    /// The reply type comes from the context, not from the caller. The
    /// old "name the isolate again" shape does not compile:
    ///
    /// ```compile_fail
    /// # use tina::{Context, DeferredReply, IsolateId, SingleShard};
    /// let mut shard = SingleShard;
    /// let mut ctx = Context::<_, u32>::new_typed(&mut shard, IsolateId::new(1));
    /// let _slot: Result<DeferredReply<u32>, _> = ctx.take_reply_slot::<()>();
    /// ```
    ///
    /// The correct shape lets Rust infer the slot payload from
    /// `Context<'_, S, R>`:
    ///
    /// ```
    /// # use tina::{Context, DeferredReply, IsolateId, SingleShard};
    /// let mut shard = SingleShard;
    /// let mut ctx = Context::<_, u32>::new_typed(&mut shard, IsolateId::new(1));
    /// let _slot: Result<DeferredReply<u32>, _> = ctx.take_reply_slot();
    /// ```
    pub fn take_reply_slot(&mut self) -> Result<DeferredReply<R>, TakeReplySlotError>
    where
        R: 'static,
    {
        // Peek routing first so a Remote refusal does not consume the caller.
        match self.caller.as_ref().map(|c| c.routing()) {
            None => return Err(TakeReplySlotError::NoCaller),
            Some(CallRouting::Remote { .. }) => {
                return Err(TakeReplySlotError::CrossShardUnsupported);
            }
            Some(CallRouting::Local) => {}
        }
        let caller = self.caller.take().expect("checked above");
        let handle = caller.capture();
        Ok(DeferredReply {
            handle,
            _marker: PhantomData,
        })
    }

    /// Returns true while the current message still has a caller available
    /// to capture (i.e. a deferred reply slot can still be taken this turn).
    pub fn has_caller(&self) -> bool {
        self.caller.is_some()
    }

    /// Returns the identifier of the shard currently executing the handler.
    pub fn shard_id(&self) -> ShardId {
        self.shard.id()
    }

    /// Returns the identifier of the currently executing isolate.
    pub const fn isolate_id(&self) -> IsolateId {
        self.current_isolate
    }

    /// Returns a mutable reference to the underlying shard abstraction.
    pub fn shard(&mut self) -> &mut S {
        self.shard
    }

    /// Builds an [`Address`] for an isolate on any shard.
    pub fn address<M>(&self, shard: ShardId, isolate: IsolateId) -> Address<M> {
        Address::new(shard, isolate)
    }

    /// Builds an [`Address`] for another isolate on the current shard.
    pub fn local_address<M>(&self, isolate: IsolateId) -> Address<M> {
        Address::new(self.shard_id(), isolate)
    }

    /// Builds an [`Address`] for the currently executing isolate.
    pub fn me<M>(&self) -> Address<M> {
        Address::new(self.shard_id(), self.current_isolate)
    }

    /// Returns an effect that sends one message back to the current isolate.
    pub fn send_self<I, M>(&self, message: M) -> Effect<I>
    where
        I: Isolate<Shard = S, Message = M, Send = Outbound<M>>,
    {
        Effect::Send(Outbound::new(self.me(), message))
    }
}

/// Typed address for one isolate mailbox incarnation.
///
/// The message type parameter makes invalid sends unrepresentable at the call
/// site: an `Address<HttpMsg>` cannot be used where `Address<AuditEvent>` is
/// required.
///
/// The reply type parameter is used by runtime crates that support
/// isolate-to-isolate calls. Ordinary sends ignore it, but `call(target, ...)`
/// can infer the target's reply type from `Address<Message, Reply>` instead of
/// trusting a separate turbofish at the call site.
///
/// An address identifies one incarnation of an isolate: shard id, isolate id,
/// and generation. Runtime-issued addresses should be preferred for real
/// delivery. Manually constructed addresses are useful in tests and examples,
/// but may be stale or unknown to a runtime.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Address<M, R = ()> {
    shard: ShardId,
    isolate: IsolateId,
    generation: AddressGeneration,
    marker: AddressMarker<M, R>,
}

impl<M, R> Copy for Address<M, R> {}

impl<M, R> Clone for Address<M, R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<M> Address<M> {
    /// Creates a new typed address for the initial generation.
    pub const fn new(shard: ShardId, isolate: IsolateId) -> Self {
        Self::new_with_generation(shard, isolate, AddressGeneration::new(0))
    }
}

impl<M, R> Address<M, R> {
    /// Creates a new typed address from shard, isolate, and generation.
    pub const fn new_with_generation(
        shard: ShardId,
        isolate: IsolateId,
        generation: AddressGeneration,
    ) -> Self {
        Self {
            shard,
            isolate,
            generation,
            marker: PhantomData,
        }
    }

    /// Returns the shard that owns this address.
    pub const fn shard(self) -> ShardId {
        self.shard
    }

    /// Returns the isolate identifier on the owning shard.
    pub const fn isolate(self) -> IsolateId {
        self.isolate
    }

    /// Returns the isolate generation this address targets.
    pub const fn generation(self) -> AddressGeneration {
        self.generation
    }

    /// Returns the same runtime address with a different reply marker.
    ///
    /// Runtime-issued addresses already carry the right reply type. This is
    /// mostly useful for tests that manually construct addresses.
    pub const fn with_reply<Reply>(self) -> Address<M, Reply> {
        Address::new_with_generation(self.shard, self.isolate, self.generation)
    }
}

/// ```compile_fail
/// use tina::{Address, IsolateId, Outbound, ShardId};
///
/// enum HttpEvent {
///     Request,
/// }
///
/// enum AuditEvent {
///     Event,
/// }
///
/// let http_only = Address::<HttpEvent>::new(ShardId::new(0), IsolateId::new(7));
/// let _invalid = Outbound::new(http_only, AuditEvent::Event);
/// ```
/// A typed outbound send request.
///
/// `Outbound` is intentionally not `Clone`/`PartialEq`. Real message
/// types are often non-`Clone` (`Bytes`, file handles, large buffers), and
/// a send request is meant to be moved into the runtime, not duplicated.
#[must_use = "a send request has no effect until a runtime executes it"]
#[derive(Debug)]
pub struct Outbound<M> {
    destination: Address<M>,
    message: M,
}

impl<M> Outbound<M> {
    /// Creates a new outbound send request.
    pub fn new<R>(destination: Address<M, R>, message: M) -> Self {
        Self {
            destination: destination.with_reply::<()>(),
            message,
        }
    }

    /// Returns the destination address.
    pub const fn destination(&self) -> Address<M> {
        self.destination
    }

    /// Returns a shared reference to the outbound message.
    pub const fn message(&self) -> &M {
        &self.message
    }

    /// Splits the request into its destination and message payload.
    pub fn into_parts(self) -> (Address<M>, M) {
        (self.destination, self.message)
    }
}

/// A minimal spawn request for Sputnik.
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
    /// Creates a new spawn request.
    ///
    /// TODO: Phase Pioneer adds supervision metadata once the supervisor layer
    /// exists. Sputnik intentionally keeps spawn requests minimal.
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

/// Logical identifier for a shard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShardId(u32);

impl ShardId {
    /// Creates a shard identifier from a raw integer.
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the raw shard identifier.
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Logical identifier for an isolate within the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IsolateId(u64);

impl IsolateId {
    /// Creates an isolate identifier from a raw integer.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw isolate identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Generation for one isolate identifier.
///
/// The initial generation is `AddressGeneration::new(0)`. Runtime crates can
/// use later generations to reject stale addresses if an isolate identifier is
/// ever reused for a replacement incarnation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AddressGeneration(u64);

impl AddressGeneration {
    /// Creates an address generation from a raw integer.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw generation identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// One-shot typed handle for replying to a captured caller later.
///
/// A `DeferredReply<R>` is created by
/// [`Context::take_reply_slot`]. The handler may store it in isolate
/// state and use [`reply_to`] from a later turn to answer the original
/// caller. The slot is one-shot: each `reply_to` consumes the slot.
///
/// The slot type is derived from the current isolate's [`Isolate::Reply`]
/// through [`Context`]. Handlers do not name the reply type when
/// capturing a slot, so the normal path cannot accidentally capture a
/// slot for another isolate's reply payload.
///
/// Lifecycle and trace facts:
///
/// - capture emits `DeferredReplyCaptured`;
/// - reply through an open slot emits `DeferredReplySent`;
/// - reply through a slot whose caller already closed emits
///   `DeferredReplyRejected` with `CallerClosed`;
/// - dropping a slot whose caller is still open emits
///   `DeferredReplyDropped`;
/// - caller timeout/closed first emits `DeferredReplyRejected` with
///   `CallerClosed`. Later user disposals on that slot emit no further
///   events — the terminal fact already happened.
#[derive(Debug)]
pub struct DeferredReply<R> {
    handle: DeferredReplyHandle,
    _marker: PhantomData<fn(R) -> R>,
}

impl<R> DeferredReply<R> {
    /// Returns the runtime-assigned slot identifier.
    pub fn slot_id(&self) -> u64 {
        self.handle.slot_id()
    }

    /// Returns the current state of the slot.
    pub fn state(&self) -> DeferredSlotState {
        self.handle.state()
    }

    /// Returns true while a reply through this slot can still reach the
    /// original caller. False after the caller closed or the slot was
    /// already replied to.
    pub fn is_open(&self) -> bool {
        self.state() == DeferredSlotState::Open
    }
}

/// Type-erased handle for a runtime-owned deferred reply slot.
///
/// Application code does not construct or unwrap this directly; it lives
/// inside [`DeferredReply<R>`]. Runtime crates allocate it through
/// [`runtime_internal`].
///
/// `DeferredReplyHandle` is intentionally not `Clone`. The user-facing
/// API exposes only [`slot_id`](Self::slot_id) and
/// [`state`](Self::state); reconstruction or duplication of slots is
/// reserved for runtime crates via the [`runtime_internal`] module.
#[derive(Debug)]
pub struct DeferredReplyHandle {
    shared: Arc<DeferredSlotShared>,
}

impl DeferredReplyHandle {
    /// Returns the runtime-assigned slot identifier.
    pub fn slot_id(&self) -> u64 {
        self.shared.slot_id
    }

    /// Returns the current state of the slot.
    pub fn state(&self) -> DeferredSlotState {
        self.shared.state()
    }
}

/// Shared state between a [`DeferredReplyHandle`] in isolate state and
/// the runtime registry. The runtime mutates `state` to record caller
/// liveness.
///
/// State is an `AtomicU8` so `DeferredSlotShared` is `Sync` and
/// `Arc<DeferredSlotShared>` is `Send`. The user-side handle then
/// satisfies `Send + 'static` for `ThreadedRuntime` registration.
/// Atomic ordering is `Relaxed` because the slot is always handled on
/// one shard thread; the atomic is the cheapest type the borrow
/// checker accepts in `Send + Sync` form.
#[derive(Debug)]
pub struct DeferredSlotShared {
    slot_id: u64,
    state: AtomicU8,
    /// `TypeId` of the original caller's expected reply payload. The
    /// runtime sets this from the dispatching `Address<_, R>`'s `R`.
    /// Normal handlers cannot choose a different deferred reply type
    /// because [`Context::take_reply_slot`] derives it from the current
    /// isolate. The runtime still checks erased payloads against this
    /// id before invoking the original caller's translator, so hidden
    /// runtime-internal misuse surfaces as a typed trace fact rather
    /// than panicking the translator.
    expected_reply_type_id: TypeId,
}

const SLOT_STATE_OPEN: u8 = 0;
const SLOT_STATE_REPLIED: u8 = 1;
const SLOT_STATE_CLOSED: u8 = 2;

impl DeferredSlotShared {
    /// Builds an `Open` shared slot. Runtime-only constructor.
    #[doc(hidden)]
    pub fn new(slot_id: u64, expected_reply_type_id: TypeId) -> Self {
        Self {
            slot_id,
            state: AtomicU8::new(SLOT_STATE_OPEN),
            expected_reply_type_id,
        }
    }

    /// Returns the slot identifier.
    pub fn slot_id(&self) -> u64 {
        self.slot_id
    }

    /// Returns the original caller's expected reply payload `TypeId`.
    pub fn expected_reply_type_id(&self) -> TypeId {
        self.expected_reply_type_id
    }

    /// Reads the current state.
    pub fn state(&self) -> DeferredSlotState {
        match self.state.load(Ordering::Relaxed) {
            SLOT_STATE_OPEN => DeferredSlotState::Open,
            SLOT_STATE_REPLIED => DeferredSlotState::Replied,
            SLOT_STATE_CLOSED => DeferredSlotState::Closed,
            other => unreachable!("invalid slot state byte: {other}"),
        }
    }

    /// Updates the state. Runtime-only.
    #[doc(hidden)]
    pub fn set_state(&self, state: DeferredSlotState) {
        let byte = match state {
            DeferredSlotState::Open => SLOT_STATE_OPEN,
            DeferredSlotState::Replied => SLOT_STATE_REPLIED,
            DeferredSlotState::Closed => SLOT_STATE_CLOSED,
        };
        self.state.store(byte, Ordering::Relaxed);
    }
}

/// Lifecycle states a [`DeferredReply`] may pass through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeferredSlotState {
    /// The slot is still open; a reply can still reach the caller.
    Open,
    /// The slot has been replied to (terminal).
    Replied,
    /// The original caller already closed (timeout, requester stopped, or
    /// service stopped). Subsequent replies through this slot are
    /// rejected; no event fires for the rejection because the closed
    /// state was already a terminal trace fact.
    Closed,
}

/// Caller-owned handle for one in-flight isolate call.
///
/// Built by `tina_runtime::call_with_handle(addr, msg, t).reply(...)`.
/// Pass to `cancel_call(handle)` to close the wait. Move-only and
/// `!Clone`: one handle, one cancel.
///
/// Rules:
/// - cancels the wait, not work the callee already accepted; late
///   replies become `CallReplyRejected { CallerCancelled }` trace facts;
/// - double cancel returns `AlreadyCancelled`; cancel after settle
///   returns `AlreadyCompleted`;
/// - does not release pool leases — that is its own primitive;
/// - dropping the handle does not cancel; the call runs to completion;
/// - the runtime stamps the handle with the originating shard id on
///   dispatch. A `cancel_call` issued from a different shard returns
///   `CancelOutcome::WrongShard` instead of falling through to a
///   silent wrong-result.
///
/// Type-system guarantees (compile-fail proofs):
///
/// `CallHandle` is not a reply token. `reply_to` requires a
/// `DeferredReply<R>`, not a `CallHandle<R>`:
///
/// ```compile_fail
/// # use tina::{CallHandle, Effect, Isolate, Outbound, reply_to};
/// # struct S;
/// # impl Isolate for S {
/// #     type Message = (); type Reply = u32;
/// #     type Send = Outbound<std::convert::Infallible>;
/// #     type Spawn = std::convert::Infallible;
/// #     type Call = std::convert::Infallible;
/// #     type Shard = tina::SingleShard;
/// #     fn handle(&mut self, _: (), _: &mut tina::Context<'_, Self::Shard, Self::Reply>) -> Effect<Self> {
/// #         tina::noop()
/// #     }
/// # }
/// fn _bad(handle: CallHandle<u32>) -> Effect<S> {
///     reply_to(handle, 1) // expected DeferredReply, found CallHandle
/// }
/// ```
///
/// `CallHandle` is not `Clone` — the type system enforces "one cancel
/// per handle":
///
/// ```compile_fail
/// # use tina::CallHandle;
/// fn _bad(h: CallHandle<u32>) -> CallHandle<u32> {
///     h.clone() // CallHandle does not implement Clone
/// }
/// ```
#[must_use = "use the CallHandle (cancel_call, store in state, or `let _ = ...`) — \
              dropping it lets the call run to completion"]
pub struct CallHandle<R> {
    handle: CallHandleInner,
    _marker: PhantomData<fn(R) -> R>,
}

impl<R> std::fmt::Debug for CallHandle<R> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CallHandle")
            .field("call_id", &self.handle.shared.call_id())
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

impl<R> CallHandle<R> {
    /// Returns the runtime-assigned call id, or `None` if the effect
    /// has not yet been dispatched.
    pub fn call_id(&self) -> Option<u64> {
        self.handle.shared.call_id()
    }

    /// Returns the current state of the underlying call.
    pub fn state(&self) -> CallHandleState {
        self.handle.shared.state()
    }
}

/// Type-erased inner handle. Runtime-only; mint via [`runtime_internal`].
#[doc(hidden)]
#[derive(Debug)]
pub struct CallHandleInner {
    shared: Arc<CallHandleShared>,
}

const HANDLE_STATE_PENDING: u8 = 0;
const HANDLE_STATE_SETTLED: u8 = 1;
const HANDLE_STATE_CANCELLED: u8 = 2;

/// Shared state between a [`CallHandle`] and the runtime's pending-call
/// registry. Runtime-only; user code cannot construct one.
///
/// **Concurrency invariant.** All writes (`set_state`, `set_call_id`,
/// `set_shard_id`) happen on one shard thread — the runtime that owns
/// the pending entry. Reads can come from another thread (a host
/// polling `handle.state()`, or another shard's runtime checking the
/// shard fingerprint before honoring a cross-shard cancel). Writers
/// use `Release`, readers use `Acquire`, so observers see a consistent
/// transition without seeing torn payloads.
#[doc(hidden)]
#[derive(Debug)]
pub struct CallHandleShared {
    state: AtomicU8,
    /// `0` until dispatched, then `CallId::get()`.
    call_id: AtomicU64,
    /// `u32::MAX` until dispatched, then `ShardId::get()`. Stamped at
    /// the same site as `call_id` so a non-`MAX` shard id implies
    /// a non-zero call id (single-writer + Release/Acquire).
    ///
    /// Used by the runtime to reject a cross-shard `cancel_call` —
    /// the pending-call registry lives on the originating shard, so
    /// a different shard's runtime would silently fall through to
    /// `AlreadyCompleted` if it attempted the lookup.
    shard_id: AtomicU32,
    expected_reply_type_id: TypeId,
}

/// Sentinel for `shard_id` "not yet stamped." `u32::MAX` is reserved
/// so any user-defined `ShardId` is distinguishable from "unstamped."
const SHARD_ID_UNSTAMPED: u32 = u32::MAX;

impl CallHandleShared {
    /// Builds a fresh `Pending` shared cell. Runtime-only.
    #[doc(hidden)]
    pub fn new(expected_reply_type_id: TypeId) -> Self {
        Self {
            state: AtomicU8::new(HANDLE_STATE_PENDING),
            call_id: AtomicU64::new(0),
            shard_id: AtomicU32::new(SHARD_ID_UNSTAMPED),
            expected_reply_type_id,
        }
    }

    /// Reads the current handle state.
    pub fn state(&self) -> CallHandleState {
        match self.state.load(Ordering::Acquire) {
            HANDLE_STATE_PENDING => CallHandleState::Pending,
            HANDLE_STATE_SETTLED => CallHandleState::Settled,
            HANDLE_STATE_CANCELLED => CallHandleState::Cancelled,
            other => unreachable!("invalid call handle state byte: {other}"),
        }
    }

    /// Updates handle state. Runtime-only; called on the shard thread
    /// that owns the pending entry.
    #[doc(hidden)]
    pub fn set_state(&self, state: CallHandleState) {
        let byte = match state {
            CallHandleState::Pending => HANDLE_STATE_PENDING,
            CallHandleState::Settled => HANDLE_STATE_SETTLED,
            CallHandleState::Cancelled => HANDLE_STATE_CANCELLED,
        };
        self.state.store(byte, Ordering::Release);
    }

    /// Returns the runtime-assigned call id, or `None` while not yet
    /// dispatched.
    pub fn call_id(&self) -> Option<u64> {
        let raw = self.call_id.load(Ordering::Acquire);
        if raw == 0 { None } else { Some(raw) }
    }

    /// Returns the originating shard id, or `None` while not yet
    /// dispatched. Stamped together with `call_id`.
    pub fn shard_id(&self) -> Option<u32> {
        let raw = self.shard_id.load(Ordering::Acquire);
        if raw == SHARD_ID_UNSTAMPED {
            None
        } else {
            Some(raw)
        }
    }

    /// Stamps the call id on dispatch. Runtime-only.
    ///
    /// The two `set_call_id` sites in `tina-runtime`'s
    /// `dispatch_isolate_call` are mutually exclusive (success vs.
    /// send-rejected branches); a second stamp on the same handle
    /// would be a runtime invariant violation, so we panic loudly
    /// on it rather than silently first-stamp-wins.
    #[doc(hidden)]
    pub fn set_call_id(&self, call_id: u64) {
        assert!(call_id != 0, "runtime call id must be non-zero");
        let prior = self.call_id.swap(call_id, Ordering::Release);
        assert!(
            prior == 0,
            "tina: CallHandleShared::set_call_id stamped twice (prior={prior} new={call_id}); \
             this is a tina-runtime invariant violation",
        );
    }

    /// Stamps the originating shard id on dispatch. Runtime-only.
    /// Same single-stamp invariant as `set_call_id`.
    #[doc(hidden)]
    pub fn set_shard_id(&self, shard_id: u32) {
        assert!(
            shard_id != SHARD_ID_UNSTAMPED,
            "ShardId == u32::MAX is reserved as the unstamped sentinel",
        );
        let prior = self.shard_id.swap(shard_id, Ordering::Release);
        assert!(
            prior == SHARD_ID_UNSTAMPED,
            "tina: CallHandleShared::set_shard_id stamped twice (prior={prior} new={shard_id}); \
             this is a tina-runtime invariant violation",
        );
    }

    /// Returns the dispatching `Address<_, R>`'s `R` type id.
    pub fn expected_reply_type_id(&self) -> TypeId {
        self.expected_reply_type_id
    }
}

/// Lifecycle states a [`CallHandle`] may pass through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallHandleState {
    /// The call is still in the runtime's pending table.
    Pending,
    /// The call already replied, timed out, was rejected, or its caller
    /// closed. Cancelling now returns `AlreadyCompleted`.
    Settled,
    /// The call was explicitly cancelled. Cancelling again returns
    /// `AlreadyCancelled`.
    Cancelled,
}

/// Why a wait was cancelled. Distinct names keep timeout, explicit
/// cancel, and lifecycle close separate in the trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CancelCause {
    /// `cancel_call(handle)` from the caller.
    CallerCancelled,
    /// Mandatory call timeout elapsed.
    CallerTimedOut,
    /// Owning isolate stopped.
    OwnerStopped,
    /// Runtime stopped.
    RuntimeStopped,
}

/// Outcome of a `cancel_call(handle)` request.
#[must_use = "CancelOutcome reports whether the wait was actually reclaimed"]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CancelOutcome {
    /// Wait closed; capacity reclaimed; late replies become rejected facts.
    Cancelled,
    /// Already replied, timed out, or otherwise settled.
    AlreadyCompleted,
    /// Already cancelled.
    AlreadyCancelled,
    /// The handle was minted on a different shard. The cancel did
    /// not run; the originating shard's pending-call entry is
    /// untouched. Send the cancel to the right shard, or store the
    /// handle on the originating isolate and cancel from there.
    WrongShard,
}

impl CancelOutcome {
    /// Returns whether this outcome successfully cancelled a pending wait.
    pub const fn is_cancelled(self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

/// Reasons [`Context::take_reply_slot`] may refuse a capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TakeReplySlotError {
    /// The current message has no caller, or the slot was already taken
    /// on this turn.
    NoCaller,
    /// The current call came from a different shard. First-form
    /// deferred reply slots only support same-shard callers because
    /// caller-liveness sweep depends on the local pending-isolate-call
    /// table.
    CrossShardUnsupported,
}

/// Runtime-supplied capture hook attached to a [`Context`].
///
/// Constructed by runtimes when delivering a call message; consumed by
/// the first call to [`Context::take_reply_slot`]. Holds primitives
/// inline plus an `Rc` to the runtime's slot registry — no per-call
/// boxed closure.
#[derive(Debug)]
pub struct MessageCaller {
    registry: Rc<DeferredSlotRegistry>,
    call_id: u64,
    capturing_isolate: IsolateId,
    routing: CallRouting,
    /// `TypeId` of the original caller's expected reply payload, as
    /// declared by the dispatching `Address<_, R>`. The runtime
    /// supplies this when constructing the caller; the captured slot
    /// inherits it.
    expected_reply_type_id: TypeId,
}

impl MessageCaller {
    /// Constructs a caller. Runtime-only.
    #[doc(hidden)]
    pub fn new(
        registry: Rc<DeferredSlotRegistry>,
        call_id: u64,
        capturing_isolate: IsolateId,
        routing: CallRouting,
        expected_reply_type_id: TypeId,
    ) -> Self {
        Self {
            registry,
            call_id,
            capturing_isolate,
            routing,
            expected_reply_type_id,
        }
    }

    /// Returns the current call's routing kind. Used by
    /// [`Context::take_reply_slot`] to refuse cross-shard captures.
    pub fn routing(&self) -> CallRouting {
        self.routing
    }

    /// Allocate the slot, register a pending capture in the runtime's
    /// registry, return the typed handle. Consumes self so the caller
    /// can be taken at most once per message.
    fn capture(self) -> DeferredReplyHandle {
        let slot_id = self.registry.allocate_slot_id();
        let shared = Arc::new(DeferredSlotShared::new(
            slot_id,
            self.expected_reply_type_id,
        ));
        self.registry.register_pending(PendingCapture {
            slot_id,
            call_id: self.call_id,
            capturing_isolate: self.capturing_isolate,
            shared: shared.clone(),
            routing: self.routing,
        });
        runtime_internal::handle_from_shared(shared)
    }
}

/// Runtime-neutral routing kind for a message that carries a caller.
///
/// Used both by [`MessageCaller::routing`] and by the registry's
/// pending-capture entries so the runtime can find the matching call
/// later. Cross-shard data is exposed as primitives so `tina` does not
/// have to know the runtime's address types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallRouting {
    /// Caller is on the same shard as the service. Reply settles via
    /// the local pending-isolate-call table.
    Local,

    /// Caller is on a different shard. Reply must travel through the
    /// remote reply path.
    Remote {
        /// Requester shard id.
        requester_shard: ShardId,
        /// Requester isolate id on its shard.
        requester_isolate: IsolateId,
        /// Requester address generation.
        requester_generation: AddressGeneration,
        /// Cause id of the original call attempt on the requester
        /// shard. Opaque to `tina`.
        cause: u64,
    },
}

/// Runtime-owned bookkeeping for in-flight deferred reply captures.
///
/// Shared via `Rc` between the runtime's step loop and the
/// [`MessageCaller`] handed to a service handler. The runtime layers
/// its own promoted-slot registry on top of this one for routing and
/// sweeps; tina only owns the slot-id source and the pending-capture
/// queue that the runtime drains after each handler turn.
#[derive(Debug, Default)]
pub struct DeferredSlotRegistry {
    inner: std::cell::RefCell<DeferredSlotRegistryInner>,
}

#[derive(Debug, Default)]
struct DeferredSlotRegistryInner {
    next_slot_id: u64,
    pending_captures: Vec<PendingCapture>,
}

/// One newly captured slot waiting for the runtime to promote.
///
/// Runtime drains these after the handler turn to attach routing
/// records and emit `DeferredReplyCaptured` trace events.
#[derive(Debug)]
pub struct PendingCapture {
    /// Runtime-assigned slot identifier.
    pub slot_id: u64,
    /// Original call id this slot answers.
    pub call_id: u64,
    /// Isolate that captured the slot.
    pub capturing_isolate: IsolateId,
    /// Shared state between user-side handle and runtime registry.
    pub shared: Arc<DeferredSlotShared>,
    /// Where to deliver the reply.
    pub routing: CallRouting,
}

impl DeferredSlotRegistry {
    /// Creates a fresh registry with no slots. Runtime-only.
    #[doc(hidden)]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the next slot id and records its allocation.
    /// Runtime-only.
    #[doc(hidden)]
    pub fn allocate_slot_id(&self) -> u64 {
        let mut inner = self.inner.borrow_mut();
        inner.next_slot_id += 1;
        inner.next_slot_id
    }

    /// Records a pending capture for the runtime to drain after the
    /// handler turn. Runtime-only.
    #[doc(hidden)]
    pub fn register_pending(&self, capture: PendingCapture) {
        self.inner.borrow_mut().pending_captures.push(capture);
    }

    /// Drains and returns every pending capture recorded since the
    /// last drain. Runtime-only.
    #[doc(hidden)]
    pub fn drain_pending(&self) -> Vec<PendingCapture> {
        std::mem::take(&mut self.inner.borrow_mut().pending_captures)
    }
}

/// The preferred first import for ordinary Tina application code.
/// Runtime-only conduits for deferred reply slot construction.
///
/// This module is **not** a stable public API. It exists so
/// `tina-runtime` and `tina-sim` can mint deferred reply handles and
/// extract them from [`DeferredReply`] when erasing effects. Reaching
/// into it from application code defeats the one-shot type-system
/// guarantees on [`reply_to`] (a duplicate reply becomes possible if
/// you rebuild a [`DeferredReply`] out of band) and may deliver
/// payloads of the wrong type to the runtime, panicking the
/// per-call translator.
///
/// If you find yourself wanting to call anything in here, write a
/// runtime-side helper instead.
#[doc(hidden)]
pub mod runtime_internal {
    use std::sync::Arc;

    use crate::{
        CallHandle, CallHandleInner, CallHandleShared, DeferredReply, DeferredReplyHandle,
        DeferredSlotShared,
    };

    /// Build a handle from a runtime-allocated shared slot.
    pub fn handle_from_shared(shared: Arc<DeferredSlotShared>) -> DeferredReplyHandle {
        DeferredReplyHandle { shared }
    }

    /// Borrow the shared slot for routing/sweep checks.
    pub fn handle_shared(handle: &DeferredReplyHandle) -> &Arc<DeferredSlotShared> {
        &handle.shared
    }

    /// Move the handle out of a typed slot. Used by erase paths.
    pub fn deferred_into_handle<R>(slot: DeferredReply<R>) -> DeferredReplyHandle {
        slot.handle
    }

    /// Wrap a handle into a typed slot. Used internally by
    /// [`crate::Context::take_reply_slot`]; runtime crates do not
    /// normally need this.
    pub fn deferred_from_handle<R>(handle: DeferredReplyHandle) -> DeferredReply<R> {
        DeferredReply {
            handle,
            _marker: std::marker::PhantomData,
        }
    }

    /// Borrow the inner handle of a typed slot without consuming it.
    /// Pair with [`handle_shared`] to clone the shared slot for an
    /// observer that survives `reply_to`.
    pub fn deferred_handle_ref<R>(slot: &DeferredReply<R>) -> &DeferredReplyHandle {
        &slot.handle
    }

    /// Build a typed [`CallHandle`] from a runtime-allocated shared cell.
    pub fn call_handle_from_shared<R>(shared: Arc<CallHandleShared>) -> CallHandle<R> {
        CallHandle {
            handle: CallHandleInner { shared },
            _marker: std::marker::PhantomData,
        }
    }

    /// Borrow the shared cell for runtime-side dispatch and lookup.
    pub fn call_handle_shared<R>(handle: &CallHandle<R>) -> &Arc<CallHandleShared> {
        &handle.handle.shared
    }

    /// Move the typed handle into its erased inner form. Used by
    /// `cancel_call`'s erase path to drop the reply-type marker.
    pub fn call_handle_into_inner<R>(handle: CallHandle<R>) -> CallHandleInner {
        handle.handle
    }

    /// Borrow the shared cell from an erased inner handle.
    pub fn call_handle_inner_shared(inner: &CallHandleInner) -> &Arc<CallHandleShared> {
        &inner.shared
    }

    /// Consume the erased inner handle, returning its shared cell.
    pub fn call_handle_inner_into_shared(inner: CallHandleInner) -> Arc<CallHandleShared> {
        inner.shared
    }
}

/// Common imports for ordinary Tina application code.
pub mod prelude {
    pub use crate::{
        Address, CallHandle, CallHandleState, CancelCause, CancelOutcome, ChildDefinition, Context,
        DeferredReply, Effect, Isolate, IsolateId, Outbound, RestartableChildDefinition, Shard,
        ShardId, SingleShard, batch, isolate, isolate_types, noop, reply, reply_to,
        restart_children, send, sequence, spawn, stop, stop_with,
    };
}

pub mod pool;
