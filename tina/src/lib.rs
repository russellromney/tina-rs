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
use std::time::{Duration, Instant};

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
        spawn_observed: $spawn_observed:ty,
        call: $call:ty,
        fact: $fact:ty,
        shard: $shard:ty $(,)?
    ) => {
        type Message = $message;
        type Reply = $reply;
        type Send = $send;
        type Spawn = $spawn;
        type SpawnObserved = $spawn_observed;
        type Call = $call;
        type Fact = $fact;
        type Shard = $shard;
    };
    (
        message: $message:ty,
        reply: $reply:ty,
        send: $send:ty,
        spawn: $spawn:ty,
        spawn_observed: $spawn_observed:ty,
        call: $call:ty,
        shard: $shard:ty $(,)?
    ) => {
        $crate::isolate_types! {
            message: $message,
            reply: $reply,
            send: $send,
            spawn: $spawn,
            spawn_observed: $spawn_observed,
            call: $call,
            fact: ::std::convert::Infallible,
            shard: $shard,
        }
    };
    (
        message: $message:ty,
        reply: $reply:ty,
        send: $send:ty,
        spawn: $spawn:ty,
        call: $call:ty,
        fact: $fact:ty,
        shard: $shard:ty $(,)?
    ) => {
        $crate::isolate_types! {
            message: $message,
            reply: $reply,
            send: $send,
            spawn: $spawn,
            spawn_observed: ::std::convert::Infallible,
            call: $call,
            fact: $fact,
            shard: $shard,
        }
    };
    (
        message: $message:ty,
        reply: $reply:ty,
        send: $send:ty,
        spawn: $spawn:ty,
        call: $call:ty,
        shard: $shard:ty $(,)?
    ) => {
        $crate::isolate_types! {
            message: $message,
            reply: $reply,
            send: $send,
            spawn: $spawn,
            spawn_observed: ::std::convert::Infallible,
            call: $call,
            fact: ::std::convert::Infallible,
            shard: $shard,
        }
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

    /// The payload produced by [`Effect::SpawnObserved`].
    type SpawnObserved;

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
    /// rejecting, or promoting it into a [`RequestContext`]. The default
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
/// a runtime rejection — exactly the silent failure Phase 100 moves to the
/// compile boundary. `CallableIsolate` is the type-level "this isolate's
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

    /// Reject the current call without an application reply.
    Reject(CallRejectedReason),

    /// Deliver a typed message to another isolate.
    Send(I::Send),

    /// Start a new isolate instance.
    Spawn(I::Spawn),

    /// Start a new isolate instance and report the typed child reference back
    /// to the parent as an ordinary later message.
    SpawnObserved(I::SpawnObserved),

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
    /// `tcp_write(...).then(...)`, then another from the next handler turn).
    /// See `docs/tcp-loops.md` for canonical patterns.
    Batch(Vec<Effect<I>>),

    /// Reply through a previously captured deferred reply slot.
    ///
    /// Equivalent to [`Reply`](Self::Reply) but routes the reply through the
    /// named slot instead of the current message's caller. The slot is
    /// one-shot: the runtime consumes it on delivery.
    ReplyTo(DeferredReply<I::Reply>, I::Reply),

    /// Emit one replayable [`Isolate::Fact`].
    ///
    /// The runtime converts the fact through `tina_runtime::IntoRuntimeFact`
    /// and records a `RuntimeEventKind::FactObserved` event. Isolates whose
    /// `Fact = std::convert::Infallible` cannot construct this variant: the
    /// type system enforces that ordinary isolates never emit a fact by
    /// accident.
    Fact(I::Fact),
}

/// Returns an effect that asks the runtime to do nothing else this turn.
pub fn noop<I>() -> Effect<I>
where
    I: Isolate,
{
    Effect::Noop
}

/// Returns an effect that emits one replayable [`Isolate::Fact`].
///
/// The compiler enforces that the value's type is exactly `I::Fact`. An
/// ordinary isolate whose `Fact = std::convert::Infallible` cannot call
/// `fact::<Self>(...)` with anything but an uninhabited value — there is no
/// such value, so the call site fails to type-check.
///
/// Protocol isolates declare `Fact = tina_runtime::ProtocolFact` (or another
/// type that implements `tina_runtime::IntoRuntimeFact`) and emit facts at the
/// point the protocol fact becomes true.
pub fn fact<I>(value: I::Fact) -> Effect<I>
where
    I: Isolate,
{
    Effect::Fact(value)
}

/// Returns an effect that replies to the current caller.
pub fn reply<I>(value: I::Reply) -> Effect<I>
where
    I: Isolate,
{
    Effect::Reply(value)
}

/// Returns an effect that rejects the current call.
pub fn reject<I>(reason: CallRejectedReason) -> Effect<I>
where
    I: Isolate,
{
    Effect::Reject(reason)
}

/// Request-lane effect returned by split-service request handlers.
///
/// This is deliberately narrower than [`Effect`]. The copied split-service
/// path produces a `RequestEffect` by consuming [`RequestCall`] through
/// `reply`, `reject`, `capture`, `defer`, or `defer_cancelable`. Ordinary
/// `noop()` is not a `RequestEffect`, so "forgot to answer caller" becomes a
/// compile error on the copied path.
#[must_use = "request handlers communicate with the runtime by returning a RequestEffect"]
pub struct RequestEffect<I>
where
    I: Isolate,
{
    effect: Effect<I>,
}

impl<I> RequestEffect<I>
where
    I: Isolate,
{
    fn from_consumed_effect(effect: Effect<I>) -> Self {
        Self { effect }
    }

    /// Converts this request effect into the ordinary runtime effect.
    pub fn into_effect(self) -> Effect<I> {
        self.effect
    }
}

/// Returns an effect that sends one typed message to another isolate.
pub fn send<I, M, R>(destination: Address<M, R>, message: M) -> Effect<I>
where
    I: Isolate<Send = Outbound<M>>,
{
    Effect::Send(Outbound::new(destination, message))
}

/// Returns an effect that sends one typed message to a send-only address.
///
/// This is the preferred form when a service exposes a [`SendAddress<M>`] via
/// `tina_runtime::Runtime::register_service`. It carries the same runtime
/// semantics as [`send`] but takes a capability-typed address so a callable
/// service handle cannot be passed by accident.
pub fn send_to<I, M>(destination: SendAddress<M>, message: M) -> Effect<I>
where
    I: Isolate<Send = Outbound<M>>,
{
    Effect::Send(Outbound::new(destination.address(), message))
}

/// Returns an effect that sends one public service event.
///
/// This is the split-service spelling of [`send_to`]. A
/// [`ServiceEventAddress`] cannot be passed to request helpers, and a
/// [`ServiceRequestAddress`] cannot be passed here, so the common
/// "request went down the event lane" mistake fails at compile time on the
/// copied path.
pub fn send_event<I, E, Q>(destination: ServiceEventAddress<E, Q>, event: E) -> Effect<I>
where
    I: Isolate<Send = Outbound<ServiceMessage<E, Q>>>,
{
    Effect::Send(Outbound::new(
        destination.address().address(),
        ServiceMessage::Event(event),
    ))
}

/// Returns an effect that asks the runtime to spawn one child.
pub fn spawn<I>(child: I::Spawn) -> Effect<I>
where
    I: Isolate,
{
    Effect::Spawn(child)
}

/// Returns a builder for a spawn request that reports the typed child
/// reference back to the spawning parent as an ordinary later message.
///
/// Spawn construction rejections that can be known before a child exists are
/// delivered through the continuation as [`SpawnObservedError`]. Delivery
/// rejection for the continuation itself is traced like any other send
/// rejection; the runtime does not create a hidden queue or bypass the
/// parent's bounded mailbox to force that message through.
pub fn spawn_observed<S>(
    child: S,
) -> SpawnObservedBuilder<S, <S as SpawnAddress>::Message, <S as SpawnAddress>::Reply>
where
    S: SpawnAddress,
{
    SpawnObservedBuilder::new(child)
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
/// #     type SpawnObserved = std::convert::Infallible;
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

/// Replies to the caller through a [`RequestContext`].
///
/// This is the [`RequestContext`] spelling of [`reply_to`]. It consumes
/// the context and produces a [`Effect::ReplyTo`] just like the
/// underlying [`DeferredReply`] form.
pub fn reply_to_request<I>(req: RequestContext<I::Reply>, value: I::Reply) -> Effect<I>
where
    I: Isolate,
{
    let slot = req.into_deferred();
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
    current_generation: AddressGeneration,
    caller: Option<MessageCaller>,
    /// Runtime/sim-stamped current time. `None` for hand-built contexts
    /// in tests that do not exercise time. Calls to [`Context::now`] or
    /// [`Context::deadline_after`] panic loudly when this is `None`,
    /// making "I forgot to plumb a clock" a runtime fault rather than a
    /// hidden `Instant::now()` call.
    now: Option<Instant>,
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
            current_generation: AddressGeneration::new(0),
            caller: None,
            now: None,
            _reply: PhantomData,
        }
    }

    /// Attach the current isolate generation. Runtime-only constructor.
    #[doc(hidden)]
    pub fn with_current_generation(mut self, generation: AddressGeneration) -> Self {
        self.current_generation = generation;
        self
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

    /// Stamps the runtime/sim-observed current time. Runtime-only
    /// constructor: live runtimes pass their `Clock::now()`, the
    /// simulator passes a deterministic `Instant` derived from its
    /// virtual clock anchor and `virtual_now`. Set before invoking
    /// [`Isolate::handle`].
    ///
    /// `Context::now` and `Context::deadline_after` read this field
    /// and panic if it has not been stamped — the rule is that handlers
    /// that ask for time get the runtime's truth, not `Instant::now()`.
    #[doc(hidden)]
    pub fn with_now(mut self, now: Instant) -> Self {
        self.now = Some(now);
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
        if self.caller.is_none() {
            return Err(TakeReplySlotError::NoCaller);
        }
        let caller = self.caller.take().expect("checked above");
        let handle = caller.capture();
        Ok(DeferredReply {
            handle,
            _marker: PhantomData,
        })
    }

    /// Captures the current caller as a [`RequestContext<R>`].
    ///
    /// This is the blessed app-facing name for the same primitive as
    /// [`take_reply_slot`](Self::take_reply_slot). The name signals
    /// intent: "I will reply later through a multi-turn workflow."
    ///
    /// The return type is a [`RequestContext`] so callers who see the
    /// type know the handler means to carry the promise across turns.
    /// Underneath it is the same move-only deferred reply slot.
    ///
    /// ```
    /// # use tina::{Context, IsolateId, RequestContext, SingleShard};
    /// let mut shard = SingleShard;
    /// let mut ctx = Context::<_, u32>::new_typed(&mut shard, IsolateId::new(1));
    /// let _req: Result<RequestContext<u32>, _> = ctx.take_request_context();
    /// ```
    pub fn take_request_context(&mut self) -> Result<RequestContext<R>, TakeReplySlotError>
    where
        R: 'static,
    {
        self.take_reply_slot().map(RequestContext)
    }

    /// Returns true while the current message still has a caller available
    /// to capture (i.e. a deferred reply slot can still be taken this turn).
    pub fn has_caller(&self) -> bool {
        self.caller.is_some()
    }

    /// Current time as observed by the runtime/simulator that invoked
    /// this handler.
    ///
    /// Live runtimes return their monotonic clock's `now()`. The
    /// simulator returns a deterministic `Instant` derived from its
    /// virtual-time anchor plus `virtual_now`, so DST/replay tests see
    /// the same `now` they advanced the simulator to.
    ///
    /// **Panics** if the runtime did not stamp a current time before
    /// invoking the handler. Hand-built test contexts (built via
    /// [`Context::new`] or [`Context::new_typed`] without `with_now`)
    /// hit this path. Tests that need a clock should construct the
    /// context with [`Context::with_now`].
    pub fn now(&self) -> Instant {
        self.now.expect(
            "Context::now() requires the runtime/simulator to have stamped a current time \
             via Context::with_now before invoking the handler. Hand-built test contexts \
             must call .with_now(now) explicitly; live runtime / simulator handler paths \
             stamp this for you.",
        )
    }

    /// Builds a [`Deadline`] anchored at `Context::now() + after`.
    ///
    /// Sugar over [`Deadline::from_instant`]. The deadline is honest under
    /// live and simulator runtimes because the runtime stamped the
    /// "now" the deadline derives from. Compare against
    /// [`Context::now`] (or pass forward through other handlers — propagate,
    /// don't re-stamp) when you need to know how much budget remains.
    pub fn deadline_after(&self, after: Duration) -> Deadline {
        Deadline::from_instant(self.now(), after)
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
        Address::new_with_generation(
            self.shard_id(),
            self.current_isolate,
            self.current_generation,
        )
    }

    /// Returns an effect that sends one message back to the current isolate.
    pub fn send_self<I, M>(&self, message: M) -> Effect<I>
    where
        I: Isolate<Shard = S, Message = M, Send = Outbound<M>>,
    {
        Effect::Send(Outbound::new(self.me(), message))
    }
}

/// The explicit reply authority for one call handler turn.
///
/// A send handler receives only [`Context`]. A call handler receives a
/// `CallContext`, making the caller authority visible at the type boundary.
/// Consume it with [`reply`](Self::reply), [`reject`](Self::reject), or
/// [`into_request_context`](Self::into_request_context).
#[must_use = "a CallContext must be replied, rejected, or promoted into RequestContext"]
#[derive(Debug)]
pub struct CallContext<'a, I>
where
    I: Isolate,
{
    ctx: Context<'a, I::Shard, I::Reply>,
    _isolate: PhantomData<fn(I) -> I>,
}

/// Caller authority for split-service request handlers.
///
/// This is the copied-path wrapper around [`CallContext`]. It exposes
/// authority-consuming operations that return [`RequestEffect`], so a split
/// request handler cannot accidentally return ordinary `noop()`.
#[must_use = "a RequestCall must be replied, rejected, captured, or deferred"]
pub struct RequestCall<'a, I>
where
    I: Isolate,
{
    inner: CallContext<'a, I>,
}

impl<'a, I> RequestCall<'a, I>
where
    I: Isolate,
{
    /// Wraps a raw [`CallContext`] as split-service request authority.
    pub fn new(inner: CallContext<'a, I>) -> Self {
        Self { inner }
    }

    /// Replies to the caller now.
    pub fn reply(self, value: I::Reply) -> RequestEffect<I> {
        RequestEffect::from_consumed_effect(self.inner.reply(value))
    }

    /// Replies to the caller now and executes additional explicit effects
    /// afterwards.
    pub fn reply_and(self, value: I::Reply, mut effects: Vec<Effect<I>>) -> RequestEffect<I> {
        let mut batch = Vec::with_capacity(effects.len() + 1);
        batch.push(self.inner.reply(value));
        batch.append(&mut effects);
        RequestEffect::from_consumed_effect(Effect::Batch(batch))
    }

    /// Rejects the caller now.
    pub fn reject(self, reason: CallRejectedReason) -> RequestEffect<I> {
        RequestEffect::from_consumed_effect(self.inner.reject(reason))
    }

    /// Captures caller authority into a [`RequestContext`] and lets the
    /// handler return explicit runtime work.
    ///
    /// Use this when the service parks the caller in bounded local state, such
    /// as `PendingReplies`, or carries the context through a hand-written
    /// continuation.
    pub fn capture<F>(self, build: F) -> RequestEffect<I>
    where
        I::Reply: 'static,
        F: FnOnce(RequestContext<I::Reply>) -> Effect<I>,
    {
        RequestEffect::from_consumed_effect(build(self.inner.into_request_context()))
    }

    /// Fallible variant of [`capture`](Self::capture).
    ///
    /// Returns the original [`RequestCall`] alongside a typed error when the
    /// caller cannot be captured. Lets bounded helpers like `park_request`
    /// check admission first and surface caller authority back on failure
    /// instead of stranding it inside the helper.
    pub fn try_capture<F>(self, build: F) -> Result<RequestEffect<I>, (Self, TakeReplySlotError)>
    where
        I::Reply: 'static,
        F: FnOnce(RequestContext<I::Reply>) -> Effect<I>,
    {
        match self.inner.try_into_request_context() {
            Ok(req) => Ok(RequestEffect::from_consumed_effect(build(req))),
            Err((ctx, err)) => Err((RequestCall { inner: ctx }, err)),
        }
    }

    /// Defers this caller reply through one visible runtime-owned work item.
    pub fn defer<W>(self, work: W) -> W::RequestDeferred
    where
        W: RequestDeferThrough<I>,
        I::Reply: 'static,
    {
        work.defer_request_through(self)
    }

    /// Defers this caller reply through cancelable runtime-owned work.
    pub fn defer_cancelable<W>(self, work: W) -> W::RequestDeferredCancelable
    where
        W: RequestDeferCancelableThrough<I>,
        I::Reply: 'static,
    {
        work.defer_cancelable_request_through(self)
    }

    /// Explicit escape hatch back to raw [`CallContext`].
    ///
    /// Prefer the narrower methods above. If this is used, the normal runtime
    /// abandoned-authority guard is the remaining safety rail.
    pub fn into_call_context(self) -> CallContext<'a, I> {
        self.inner
    }
}

impl<'a, I> CallContext<'a, I>
where
    I: Isolate,
{
    /// Runtime-only constructor.
    #[doc(hidden)]
    pub fn new(ctx: Context<'a, I::Shard, I::Reply>) -> Self {
        Self {
            ctx,
            _isolate: PhantomData,
        }
    }

    /// Replies to the caller.
    pub fn reply(self, value: I::Reply) -> Effect<I> {
        Effect::Reply(value)
    }

    /// Rejects the caller with a runtime-level reason.
    pub fn reject(self, reason: CallRejectedReason) -> Effect<I> {
        Effect::Reject(reason)
    }

    /// Defers this caller reply through one visible runtime-owned work item.
    ///
    /// The returned builder is supplied by the runtime crate that owns `work`.
    /// It must carry a [`RequestContext`] into an ordinary continuation message;
    /// it does not auto-reply to the caller and it does not make caller
    /// authority ambient.
    pub fn defer<W>(self, work: W) -> W::Deferred
    where
        W: DeferThrough<I>,
        I::Reply: 'static,
    {
        work.defer_through(self)
    }

    /// Defers this caller reply through one visible runtime-owned work item
    /// that also exposes explicit cancellation control.
    ///
    /// Cancelable work must return a visible pending token instead of hiding
    /// caller authority inside the worker continuation. That token owns the
    /// [`RequestContext`] and the runtime cancel handle together, so both
    /// worker-return and cancel-return paths can explicitly answer the caller.
    ///
    /// Runtime crates may provide a bounded admission helper for this builder.
    /// For `tina_runtime::call_cancelable(...)`, prefer the copyable shape:
    ///
    /// ```text
    /// call_ctx
    ///     .defer_cancelable(call_cancelable(...))
    ///     .try_admit(&mut pending, key, Msg::Returned)
    /// ```
    ///
    /// That helper returns the child effect only after the pending token is
    /// stored. On `Full` or duplicate admission, the rejected token remains
    /// available so the caller can recover authority and answer immediately.
    pub fn defer_cancelable<W>(self, work: W) -> W::DeferredCancelable
    where
        W: DeferCancelableThrough<I>,
        I::Reply: 'static,
    {
        work.defer_cancelable_through(self)
    }

    /// Carries the caller authority into a later handler turn.
    pub fn into_request_context(mut self) -> RequestContext<I::Reply>
    where
        I::Reply: 'static,
    {
        self.ctx
            .take_request_context()
            .expect("CallContext always carries a caller authority")
    }

    /// Fallible promotion to [`RequestContext`].
    ///
    /// Returns the original [`CallContext`] alongside a typed error so
    /// helpers like `park_call` can return caller authority unchanged when
    /// the slot cannot be captured (cross-shard, missing caller).
    pub fn try_into_request_context(
        mut self,
    ) -> Result<RequestContext<I::Reply>, (Self, TakeReplySlotError)>
    where
        I::Reply: 'static,
    {
        match self.ctx.take_request_context() {
            Ok(req) => Ok(req),
            Err(err) => Err((self, err)),
        }
    }

    /// Returns the identifier of the shard currently executing the handler.
    pub fn shard_id(&self) -> ShardId {
        self.ctx.shard_id()
    }

    /// Returns the runtime's current observed time. Mirrors
    /// [`Context::now`](crate::Context::now) so call handlers can thread the
    /// runtime clock into helpers without exposing the inner [`Context`].
    pub fn now(&self) -> std::time::Instant {
        self.ctx.now()
    }

    /// Builds an [`Address`] for the currently executing isolate.
    pub fn me(&self) -> Address<I::Message, I::Reply> {
        Address::<I::Message, I::Reply>::new_with_generation(
            self.shard_id(),
            self.ctx.isolate_id(),
            self.ctx.current_generation,
        )
    }

    /// Returns an effect that sends one message back to the current isolate.
    pub fn send_self<M>(&self, message: M) -> Effect<I>
    where
        I: Isolate<Message = M, Send = Outbound<M>>,
    {
        Effect::Send(Outbound::new(self.me(), message))
    }
}

/// Runtime-provided support for [`CallContext::defer`].
///
/// `tina` owns caller authority vocabulary but not concrete runtime work
/// builders. Runtime crates implement this trait for their own prepared work
/// types so user code can write `call_ctx.defer(work).reply(Msg::Done)`
/// without `tina` depending on those runtime crates.
///
/// Implementations must be only sugar for consuming the [`CallContext`] into a
/// [`RequestContext`] and carrying that context into a continuation message.
/// They must not create hidden state, hidden retries, or hidden final replies.
pub trait DeferThrough<I>
where
    I: Isolate,
{
    /// Builder returned after caller authority has been captured.
    type Deferred;

    /// Consumes caller authority and prepares the deferred continuation.
    fn defer_through(self, call: CallContext<'_, I>) -> Self::Deferred;
}

/// Runtime-provided support for [`RequestCall::defer`].
pub trait RequestDeferThrough<I>
where
    I: Isolate,
{
    /// Builder returned after request authority has been captured.
    type RequestDeferred;

    /// Consumes request authority and prepares deferred work.
    fn defer_request_through(self, call: RequestCall<'_, I>) -> Self::RequestDeferred;
}

/// Runtime-provided support for [`CallContext::defer_cancelable`].
///
/// Implementations must consume the caller authority into a visible pending
/// token that user code admits into bounded isolate state. They must not hide
/// the [`RequestContext`] solely inside a worker-return continuation, because
/// a cancellation path must still be able to answer the original caller.
///
/// A concrete runtime builder should prefer a helper that makes admission the
/// step that returns the child effect, so storage failure cannot accidentally
/// dispatch child work.
pub trait DeferCancelableThrough<I>
where
    I: Isolate,
{
    /// Builder returned after caller authority has been captured.
    type DeferredCancelable;

    /// Consumes caller authority and prepares the cancelable deferred work.
    fn defer_cancelable_through(self, call: CallContext<'_, I>) -> Self::DeferredCancelable;
}

/// Runtime-provided support for [`RequestCall::defer_cancelable`].
pub trait RequestDeferCancelableThrough<I>
where
    I: Isolate,
{
    /// Builder returned after request authority has been captured.
    type RequestDeferredCancelable;

    /// Consumes request authority and prepares cancelable deferred work.
    fn defer_cancelable_request_through(
        self,
        call: RequestCall<'_, I>,
    ) -> Self::RequestDeferredCancelable;
}

/// Runtime-level reason a call was rejected without an application reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallRejectedReason {
    /// The callee returned without consuming the call authority.
    ReplyAbandoned,
    /// The callee panicked before consuming the call authority.
    HandlerPanicked,
    /// The callee has no call handler for this message shape.
    UnsupportedMessage,
}

impl CallRejectedReason {
    /// Human-facing diagnostic hint for this rejection, when one is useful.
    pub const fn diagnostic_hint(self) -> Option<&'static str> {
        match self {
            Self::ReplyAbandoned => Some(
                "call handler returned without consuming CallContext; use \
                 call_ctx.reply(...), call_ctx.reject(...), or \
                 call_ctx.defer(work).reply(...)",
            ),
            Self::HandlerPanicked | Self::UnsupportedMessage => None,
        }
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

/// A send-only capability for one isolate.
///
/// `SendAddress<M>` is the compile-time rail for "send a message and do not
/// expect a reply." It wraps an [`Address<M, ()>`](Address) and refuses to be
/// converted into a [`CallAddress`]: a function that takes a `CallAddress`
/// cannot accept a `SendAddress`, so the wrong path is a compile error rather
/// than a runtime rejection.
///
/// Construct one through [`Address::send_only`] or through the typed handles
/// returned by `tina_runtime::Runtime::register_service`. The escape hatch
/// [`SendAddress::address`] returns the underlying raw [`Address`] for the rare
/// case where low-level access is required.
///
/// ```compile_fail
/// # use tina::{Address, IsolateId, SendAddress, ShardId};
/// # enum Msg { Tick }
/// # struct Reply;
/// fn want_call(_call: tina::CallAddress<Msg, Reply>) {}
///
/// let raw: Address<Msg, Reply> =
///     Address::new_with_generation(ShardId::new(0), IsolateId::new(1), tina::AddressGeneration::new(0));
/// let send_only: SendAddress<Msg> = raw.send_only();
/// // SendAddress is not a CallAddress, so this does not compile.
/// want_call(send_only);
/// ```
#[derive(Debug)]
#[repr(transparent)]
pub struct SendAddress<M> {
    address: Address<M, ()>,
}

impl<M> Copy for SendAddress<M> {}

impl<M> Clone for SendAddress<M> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<M> PartialEq for SendAddress<M> {
    fn eq(&self, other: &Self) -> bool {
        self.address.shard() == other.address.shard()
            && self.address.isolate() == other.address.isolate()
            && self.address.generation() == other.address.generation()
    }
}

impl<M> Eq for SendAddress<M> {}

impl<M> SendAddress<M> {
    /// Wraps a runtime-issued [`Address`] as a send-only capability.
    ///
    /// The reply marker is erased: a `SendAddress` cannot recover a typed
    /// reply lane and is therefore not callable through `tina_runtime::call`.
    pub const fn from_address<R>(address: Address<M, R>) -> Self {
        Self {
            address: address.with_reply::<()>(),
        }
    }

    /// Returns the underlying raw [`Address`].
    ///
    /// This is the escape hatch for low-level code that needs the original
    /// [`Address`]. Prefer keeping `SendAddress` at the boundary so wrong-path
    /// calls remain compile-time errors.
    pub const fn address(self) -> Address<M, ()> {
        self.address
    }

    /// Returns the shard that owns this address.
    pub const fn shard(self) -> ShardId {
        self.address.shard()
    }

    /// Returns the isolate identifier on the owning shard.
    pub const fn isolate(self) -> IsolateId {
        self.address.isolate()
    }

    /// Returns the isolate generation this address targets.
    pub const fn generation(self) -> AddressGeneration {
        self.address.generation()
    }
}

/// A callable capability for one isolate.
///
/// `CallAddress<M, R>` is the compile-time rail for "send a message and wait
/// for a reply of type `R`." Runtime helpers like
/// `tina_runtime::call_typed` accept only `CallAddress`, so passing a
/// [`SendAddress`] (or a raw [`Address`] without the explicit upgrade) is a
/// compile error.
///
/// Construct one through [`Address::callable`] or through the typed handles
/// returned by `tina_runtime::Runtime::register_service`. The escape hatch
/// [`CallAddress::address`] returns the underlying raw [`Address`].
///
/// ```compile_fail
/// # use std::time::Duration;
/// # use tina::{Address, IsolateId, SendAddress, ShardId};
/// # enum Msg { Tick }
/// # struct Reply;
/// fn want_send(_send: SendAddress<Msg>) {}
///
/// let raw: Address<Msg, Reply> =
///     Address::new_with_generation(ShardId::new(0), IsolateId::new(1), tina::AddressGeneration::new(0));
/// let callable: tina::CallAddress<Msg, Reply> = raw.callable();
/// // CallAddress is not a SendAddress, so this does not compile.
/// want_send(callable);
/// ```
#[derive(Debug)]
#[repr(transparent)]
pub struct CallAddress<M, R> {
    address: Address<M, R>,
}

impl<M, R> Copy for CallAddress<M, R> {}

impl<M, R> Clone for CallAddress<M, R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<M, R> PartialEq for CallAddress<M, R> {
    fn eq(&self, other: &Self) -> bool {
        self.address.shard() == other.address.shard()
            && self.address.isolate() == other.address.isolate()
            && self.address.generation() == other.address.generation()
    }
}

impl<M, R> Eq for CallAddress<M, R> {}

impl<M, R> CallAddress<M, R> {
    /// Wraps a runtime-issued [`Address`] as a callable capability.
    pub const fn from_address(address: Address<M, R>) -> Self {
        Self { address }
    }

    /// Returns the underlying raw [`Address`].
    ///
    /// This is the escape hatch for low-level code that needs the original
    /// [`Address<M, R>`]. Prefer keeping `CallAddress` at the boundary so the
    /// wrong-path "send a callable as if it were send-only" is a compile error.
    pub const fn address(self) -> Address<M, R> {
        self.address
    }

    /// Returns the shard that owns this address.
    pub const fn shard(self) -> ShardId {
        self.address.shard()
    }

    /// Returns the isolate identifier on the owning shard.
    pub const fn isolate(self) -> IsolateId {
        self.address.isolate()
    }

    /// Returns the isolate generation this address targets.
    pub const fn generation(self) -> AddressGeneration {
        self.address.generation()
    }
}

impl<M, R> Address<M, R> {
    /// Returns a send-only capability for this address.
    ///
    /// The reply marker is erased so the resulting [`SendAddress`] cannot be
    /// used as a [`CallAddress`].
    pub const fn send_only(self) -> SendAddress<M> {
        SendAddress::from_address(self)
    }

    /// Returns a callable capability for this address.
    pub const fn callable(self) -> CallAddress<M, R> {
        CallAddress::from_address(self)
    }
}

/// Message envelope used by the split-service authoring path.
///
/// `Event` values are mailbox facts. `Request` values carry caller authority.
/// User code normally does not construct this enum directly; it uses
/// [`send_event`] and `tina_runtime::call_request`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceMessage<Event, Request> {
    /// Fire-and-forget mailbox event.
    Event(Event),
    /// Callable request with caller authority.
    Request(Request),
}

/// Send capability for split-service events.
#[derive(Debug)]
#[repr(transparent)]
pub struct ServiceEventAddress<Event, Request> {
    address: SendAddress<ServiceMessage<Event, Request>>,
}

impl<Event, Request> Copy for ServiceEventAddress<Event, Request> {}

impl<Event, Request> Clone for ServiceEventAddress<Event, Request> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Event, Request> ServiceEventAddress<Event, Request> {
    /// Wraps a send address for the split service envelope.
    pub const fn from_send_address(address: SendAddress<ServiceMessage<Event, Request>>) -> Self {
        Self { address }
    }

    /// Returns the underlying send capability.
    pub const fn address(self) -> SendAddress<ServiceMessage<Event, Request>> {
        self.address
    }
}

/// Call capability for split-service requests.
#[derive(Debug)]
#[repr(transparent)]
pub struct ServiceRequestAddress<Event, Request, Reply> {
    address: CallAddress<ServiceMessage<Event, Request>, Reply>,
}

impl<Event, Request, Reply> Copy for ServiceRequestAddress<Event, Request, Reply> {}

impl<Event, Request, Reply> Clone for ServiceRequestAddress<Event, Request, Reply> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Event, Request, Reply> ServiceRequestAddress<Event, Request, Reply> {
    /// Wraps a call address for the split service envelope.
    pub const fn from_call_address(
        address: CallAddress<ServiceMessage<Event, Request>, Reply>,
    ) -> Self {
        Self { address }
    }

    /// Returns the underlying call capability.
    pub const fn address(self) -> CallAddress<ServiceMessage<Event, Request>, Reply> {
        self.address
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
    /// Plain [`spawn`] keeps its existing panic-on-zero behavior. The observed
    /// form can report the rejection through its continuation before any child
    /// is recorded.
    ZeroMailboxCapacity,
}

/// Type-level child address information carried by supported spawn requests.
pub trait SpawnAddress {
    /// Child message type accepted by the spawned isolate.
    type Message;
    /// Child reply type produced by the spawned isolate.
    type Reply;
}

impl SpawnAddress for std::convert::Infallible {
    type Message = std::convert::Infallible;
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

/// Parts consumed by runtime adapters that understand observed spawn.
pub type SpawnObservedParts<S, P, M, R = ()> = (S, SpawnObservedContinuation<P, M, R>);

type SpawnObservedMarker<M, R> = PhantomData<fn() -> (M, R)>;

/// Builder returned by [`spawn_observed`].
#[must_use = "a spawn_observed request has no effect until returned as an Effect"]
#[derive(Debug)]
pub struct SpawnObservedBuilder<S, M, R = ()> {
    spawn: S,
    marker: SpawnObservedMarker<M, R>,
}

impl<S, M, R> SpawnObservedBuilder<S, M, R> {
    const fn new(spawn: S) -> Self {
        Self {
            spawn,
            marker: PhantomData,
        }
    }

    /// Maps the runtime's later child-start result into a parent message.
    #[deprecated(
        since = "0.1.0",
        note = "use `.then(...)` for ordinary continuations; use `call_ctx.defer(work).reply(...)` in handle_call when preserving caller authority"
    )]
    pub fn reply<I, P, F>(self, continuation: F) -> Effect<I>
    where
        I: Isolate<Message = P, SpawnObserved = SpawnObserved<S, P, M, R>>,
        F: FnOnce(SpawnObservedResult<M, R>) -> P + 'static,
    {
        self.then(continuation)
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
            marker: PhantomData,
        })
    }
}

/// Spawn request plus continuation for delivering a typed child reference.
#[must_use = "a spawn_observed request has no effect until returned as an Effect"]
pub struct SpawnObserved<S, P, M, R = ()> {
    spawn: S,
    continuation: SpawnObservedContinuation<P, M, R>,
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
            .finish_non_exhaustive()
    }
}

impl<S, P, M, R> SpawnObserved<S, P, M, R> {
    /// Consumes this request into its spawn payload and continuation.
    pub fn into_parts(self) -> SpawnObservedParts<S, P, M, R> {
        (self.spawn, self.continuation)
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

impl<I> SpawnAddress for RestartableChildDefinition<I>
where
    I: Isolate,
{
    type Message = I::Message;
    type Reply = I::Reply;
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

/// A caller promise explicitly carried across handler turns.
///
/// `RequestContext<R>` is the blessed vocabulary for multi-turn
/// request/reply workflows. It is a thin newtype over
/// [`DeferredReply<R>`] so the type system teaches the pattern: a
/// handler that must reply later takes `RequestContext`, stores it in
/// isolate state, and eventually passes it to [`reply_to_request`] or
/// carries it into a continuation message.
///
/// Like [`DeferredReply`], it is move-only (`!Clone`). It does not
/// auto-reply, auto-retry, or hide effects. It is typed so the reply
/// payload must match the original caller's expected type.
///
/// Capture via [`Context::take_request_context`] (the app-facing name)
/// or keep using [`Context::take_reply_slot`] for the same underlying
/// slot. Both return the same primitive; the name signals intent.
///
#[must_use = "a RequestContext must eventually be replied to or intentionally dropped"]
#[derive(Debug)]
pub struct RequestContext<R>(DeferredReply<R>);

impl<R> RequestContext<R> {
    /// Returns the runtime-assigned slot identifier.
    pub fn slot_id(&self) -> u64 {
        self.0.slot_id()
    }

    /// Returns true while a reply through this context can still reach
    /// the original caller.
    pub fn is_open(&self) -> bool {
        self.0.is_open()
    }

    /// Consumes the context and returns the underlying deferred reply
    /// slot. This is an escape hatch for code that already speaks
    /// [`DeferredReply`] and does not want to change.
    pub fn into_deferred(self) -> DeferredReply<R> {
        self.0
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
/// Built by `tina_runtime::call_cancelable(addr, msg, t).then(...)`.
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
/// #     type SpawnObserved = std::convert::Infallible;
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

/// Absolute deadline anchored to a runtime/simulator-stamped `Instant`.
///
/// `Deadline` is a value type. It does not cancel anything by itself
/// and does not own a clock — it carries the absolute monotonic time
/// at which a budget runs out, plus accessors that take an explicit
/// `now: Instant`. Callers compare the stored deadline to a `now` they
/// got from the runtime ([`Context::now`]) or the simulator's virtual
/// clock; there is no hidden `Instant::now()` call.
///
/// # Honest under live and simulator clocks
///
/// The intended construction sites are:
///
/// - [`Context::deadline_after`] — sugar that anchors to the runtime's
///   stamped `now`. Live runtimes pass their monotonic clock; the
///   simulator passes its virtual-clock-derived `Instant`. Both produce
///   `Deadline` values whose `remaining_or_zero(ctx.now())` answers the
///   same question.
/// - [`Deadline::from_instant`] — explicit constructor for host code
///   and tests where the caller controls "now" (host edge,
///   property-based tests, hand-rolled simulator drivers).
///
/// There is **no** `Deadline::after(Duration)` shortcut: it would have
/// to call `Instant::now()` internally, which silently breaks DST/replay.
///
/// # First form is a budget, not a wish
///
/// `Deadline` does not retry, does not extend itself, and does not
/// cancel work. It is the budget you propagate through a chain of calls
/// (A → B → C) so each hop sees the *same* shrinking remainder. To stop
/// waiting when the budget runs out, pass `remaining_or_zero(now)` as
/// the call timeout: an expired deadline becomes a `Duration::ZERO`
/// timeout, which the runtime turns into the usual `CallOutcome::Timeout`.
///
/// ```
/// use std::time::Duration;
/// use tina::Deadline;
///
/// let now = std::time::Instant::now();
/// let deadline = Deadline::from_instant(now, Duration::from_millis(500));
///
/// // Half the budget consumed elsewhere…
/// let later = now + Duration::from_millis(250);
/// assert!(!deadline.expired(later));
/// assert!(deadline.remaining_or_zero(later) <= Duration::from_millis(250));
///
/// // After the deadline:
/// let after = now + Duration::from_secs(1);
/// assert!(deadline.expired(after));
/// assert_eq!(deadline.remaining_or_zero(after), Duration::ZERO);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use = "Deadline is a budget; pass it through, compare against `now`, or store it"]
pub struct Deadline {
    deadline: Instant,
}

/// One century, used as the saturating ceiling when `now + after`
/// would overflow `Instant`. Far above any sane budget and far below
/// the `Instant` overflow threshold on every supported platform.
const DEADLINE_SATURATION_CEILING: Duration = Duration::from_secs(60 * 60 * 24 * 365 * 100);

impl Deadline {
    /// Builds a deadline anchored at `now + after`.
    ///
    /// `now` is taken explicitly so DST/replay-claimed code cannot
    /// silently depend on `std::time::Instant::now()`. Inside a handler,
    /// prefer [`Context::deadline_after`]; outside (host code, tests,
    /// simulator drivers), pass an `Instant` you control.
    ///
    /// **Overflow.** If `now + after` overflows `Instant`, the deadline
    /// saturates to `now + 100 years` rather than expiring immediately.
    /// "Effectively never" is the right answer for absurd budgets like
    /// `Duration::MAX`; "expires now" would silently break callers that
    /// pass an unchecked configured value. If the saturation ceiling
    /// itself overflows on a hostile platform, the deadline collapses
    /// to `now`, which the accessors then report as already-expired.
    pub fn from_instant(now: Instant, after: Duration) -> Self {
        let deadline = now
            .checked_add(after)
            .unwrap_or_else(|| now.checked_add(DEADLINE_SATURATION_CEILING).unwrap_or(now));
        Self { deadline }
    }

    /// Returns the absolute deadline `Instant` for code that wants to
    /// build its own arithmetic. Most callers should prefer
    /// [`remaining_or_zero`](Self::remaining_or_zero) or
    /// [`expired`](Self::expired).
    pub const fn instant(self) -> Instant {
        self.deadline
    }

    /// Time left until the deadline, given an explicit `now`. Returns
    /// `None` if the deadline has already passed.
    pub fn remaining(self, now: Instant) -> Option<Duration> {
        self.deadline.checked_duration_since(now)
    }

    /// Time left until the deadline, given an explicit `now`. Returns
    /// `Duration::ZERO` once the deadline has passed — the right shape
    /// to pass straight to a call timeout, where `ZERO` means "do not
    /// wait."
    pub fn remaining_or_zero(self, now: Instant) -> Duration {
        self.remaining(now).unwrap_or(Duration::ZERO)
    }

    /// Whether the deadline has already passed at `now`.
    pub fn expired(self, now: Instant) -> bool {
        now >= self.deadline
    }
}

/// Reasons [`Context::take_reply_slot`] may refuse a capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TakeReplySlotError {
    /// The current message has no caller, or the slot was already taken
    /// on this turn.
    NoCaller,
    /// Reserved for runtimes that cannot carry a remote caller as a
    /// deferred reply slot.
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

    /// Returns the current call's routing kind.
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
        DeferredSlotShared, RequestContext,
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

    /// Move the deferred reply out of a [`RequestContext`].
    pub fn request_context_into_deferred<R>(req: RequestContext<R>) -> DeferredReply<R> {
        req.0
    }

    /// Wrap a deferred reply into a [`RequestContext`].
    pub fn request_context_from_deferred<R>(slot: DeferredReply<R>) -> RequestContext<R> {
        RequestContext(slot)
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

    /// Build a request-lane effect after caller authority has already been
    /// consumed.
    ///
    /// This is intentionally hidden under `runtime_internal` so copied app code
    /// cannot casually manufacture a request effect from `noop()`. Runtime-side
    /// adapters use it after they have consumed a `RequestCall` into a
    /// `RequestContext`.
    pub fn request_effect_from_consumed_effect<I>(
        effect: crate::Effect<I>,
    ) -> crate::RequestEffect<I>
    where
        I: crate::Isolate,
    {
        crate::RequestEffect::from_consumed_effect(effect)
    }
}

/// Common imports for ordinary Tina application code.
pub mod prelude {
    pub use crate::{
        Address, CallHandle, CallHandleState, CancelCause, CancelOutcome, ChildDefinition,
        ChildRef, Context, Deadline, DeferCancelableThrough, DeferThrough, DeferredReply, Effect,
        Isolate, IsolateId, Outbound, PendingCallSet, PendingCallSetInsertError, RequestCall,
        RequestContext, RequestDeferCancelableThrough, RequestDeferThrough, RequestEffect,
        RestartableChildDefinition, Shard, ShardId, SingleShard, SpawnObservedError, batch,
        isolate, isolate_types, noop, reply, reply_to, reply_to_request, restart_children, send,
        sequence, spawn, spawn_observed, stop, stop_with,
        time::{
            Backoff, BackoffDelay, IntervalDelay, MissedTickPolicy, RecurringCatchUp,
            RecurringTick, RecurringTickDecision, RecurringTickReport, RecurringTickStale,
            RecurringTickToken, TimerConfigError, TimerDecision, TimerInterval,
        },
    };
}

pub mod capacity;
mod pending_call_set;
pub mod pool;
pub mod time;
pub use pending_call_set::{PendingCallSet, PendingCallSetInsertError};
