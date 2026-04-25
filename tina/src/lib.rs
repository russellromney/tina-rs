#![forbid(unsafe_code)]
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
//! ask for the same handful of verbs (`Reply`, `Send`, `Spawn`, `Stop`, and
//! `RestartChildren`). That simplicity matters for the runtime crates we add in
//! later phases, because they can switch on one shared enum instead of handling
//! an open-ended effect language for every isolate.
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
//! use std::convert::Infallible;
//!
//! use tina::{Address, Context, Effect, Isolate, IsolateId, SendMessage, Shard, ShardId};
//!
//! #[derive(Debug, Clone, Copy, PartialEq, Eq)]
//! enum CounterMsg {
//!     Add(u64),
//!     Read,
//! }
//!
//! #[derive(Debug, Clone, Copy, PartialEq, Eq)]
//! enum AuditMsg {
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
//!     audit: Address<AuditMsg>,
//! }
//!
//! impl Isolate for Counter {
//!     type Message = CounterMsg;
//!     type Reply = u64;
//!     type Send = SendMessage<AuditMsg>;
//!     type Spawn = Infallible;
//!     type Shard = InlineShard;
//!
//!     fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
//!         match msg {
//!             CounterMsg::Add(delta) => {
//!                 self.total += delta;
//!                 Effect::Send(SendMessage::new(self.audit, AuditMsg::Total(self.total)))
//!             }
//!             CounterMsg::Read => Effect::Reply(self.total),
//!         }
//!     }
//! }
//!
//! let audit = Address::new(ShardId::new(0), IsolateId::new(99));
//! let mut shard = InlineShard;
//! let mut ctx = Context::new(&mut shard, IsolateId::new(1));
//! let mut counter = Counter { total: 0, audit };
//!
//! match counter.handle(CounterMsg::Add(3), &mut ctx) {
//!     Effect::Send(outbound) => {
//!         let (destination, message) = outbound.into_parts();
//!         assert_eq!(destination, audit);
//!         assert_eq!(message, AuditMsg::Total(3));
//!     }
//!     _ => panic!("unexpected effect"),
//! }
//!
//! assert!(matches!(
//!     counter.handle(CounterMsg::Read, &mut ctx),
//!     Effect::Reply(3)
//! ));
//! ```

use std::marker::PhantomData;

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
    /// A common choice is [`SendMessage`] when an isolate needs to address a
    /// single typed mailbox.
    type Send;

    /// The payload produced by [`Effect::Spawn`].
    ///
    /// [`SpawnSpec`] is the simplest choice in Sputnik. Later phases can layer
    /// richer supervision and boot metadata on top.
    type Spawn;

    /// The shard abstraction available through [`Context`].
    type Shard: Shard + ?Sized;

    /// Handles one inbound message and returns the next runtime effect.
    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self>;
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

    /// Restart this isolate's children according to the runtime's supervision
    /// policy.
    RestartChildren,
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

/// Per-handler context provided by the runtime.
///
/// `Context` lets a handler inspect its current shard and build typed
/// [`Address`] values without performing side effects directly.
#[derive(Debug)]
pub struct Context<'a, S>
where
    S: Shard + ?Sized,
{
    shard: &'a mut S,
    current_isolate: IsolateId,
}

impl<'a, S> Context<'a, S>
where
    S: Shard + ?Sized,
{
    /// Creates a new handler context for the current isolate.
    pub fn new(shard: &'a mut S, current_isolate: IsolateId) -> Self {
        Self {
            shard,
            current_isolate,
        }
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
    pub fn current_address<M>(&self) -> Address<M> {
        Address::new(self.shard_id(), self.current_isolate)
    }
}

/// Typed address for an isolate mailbox.
///
/// The message type parameter makes invalid sends unrepresentable at the call
/// site: an `Address<HttpMsg>` cannot be used where `Address<AuditMsg>` is
/// required.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Address<M> {
    shard: ShardId,
    isolate: IsolateId,
    marker: PhantomData<fn(M) -> M>,
}

impl<M> Copy for Address<M> {}

impl<M> Clone for Address<M> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<M> Address<M> {
    /// Creates a new typed address from a shard and isolate identifier.
    pub const fn new(shard: ShardId, isolate: IsolateId) -> Self {
        Self {
            shard,
            isolate,
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
}

/// ```compile_fail
/// use tina::{Address, IsolateId, SendMessage, ShardId};
///
/// enum HttpMsg {
///     Request,
/// }
///
/// enum AuditMsg {
///     Event,
/// }
///
/// let http_only = Address::<HttpMsg>::new(ShardId::new(0), IsolateId::new(7));
/// let _invalid = SendMessage::new(http_only, AuditMsg::Event);
/// ```
/// A typed outbound send request.
///
/// `SendMessage` is intentionally not `Clone`/`PartialEq`. Real message
/// types are often non-`Clone` (`Bytes`, file handles, large buffers), and
/// a send request is meant to be moved into the runtime, not duplicated.
#[must_use = "a send request has no effect until a runtime executes it"]
#[derive(Debug)]
pub struct SendMessage<M> {
    destination: Address<M>,
    message: M,
}

impl<M> SendMessage<M> {
    /// Creates a new outbound send request.
    pub fn new(destination: Address<M>, message: M) -> Self {
        Self {
            destination,
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
pub struct SpawnSpec<I>
where
    I: Isolate,
{
    isolate: I,
    mailbox_capacity: usize,
}

impl<I> SpawnSpec<I>
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
        }
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
    pub fn into_parts(self) -> (I, usize) {
        (self.isolate, self.mailbox_capacity)
    }
}

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
