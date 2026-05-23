//! Effect language for the tina core.
//!
//! Owns the closed [`Effect`] enum, the small [`RequestEffect`] helper,
//! and every effect constructor (`reply`, `send`, `call`, `spawn`,
//! `batch`, `stop`, ...). Re-exported from the crate root.
//!
//! ## Module map (Phase 115 reorg)
//!
//! Constructors stay near `Effect` so a future agent adding a verb has
//! one file to edit. New verbs must also be added to `Isolate`-related
//! associated types — that vocabulary lives in `lib.rs` (and will move
//! to `mod isolate` in a follow-up commit).

use crate::{
    Address, CallRejectedReason, DeferredReply, Isolate, Outbound, RequestContext, SendAddress,
    ServiceEventAddress, ServiceMessage, SpawnAddress, SpawnObservedBuilder, StopResult,
};

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

    /// Start a new isolate instance on another shard and report the typed
    /// child reference back to the parent as an ordinary later message.
    ///
    /// Built by `spawn_observed(child).on_shard(shard).then(...)`. The child
    /// registers on the target shard; the address returns to the parent on a
    /// later turn. Isolates that never spawn cross-shard have
    /// `SpawnObservedRemote = Infallible` and cannot construct this variant.
    SpawnObservedOn(I::SpawnObservedRemote),

    /// Stop the current isolate.
    Stop,

    /// Fail the current isolate as a typed, non-panic failure.
    ///
    /// Same lifecycle as [`Self::Stop`] — the isolate stops and any
    /// in-flight caller settles visibly — but the runtime records it as a
    /// distinct `HandlerReportedFailure` fact and routes it through the
    /// supervision policy exactly like a handler panic. A supervised child
    /// is restarted per its parent's policy and budget; an unsupervised
    /// isolate simply stops. Use this to fail loudly without unwinding the
    /// thread, and keep panic and reported failure separate in the trace.
    Fail,

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

    /// Stop every child this isolate owns, as an explicit supervised
    /// shutdown.
    ///
    /// Each owned child stops through the normal stop path — its mailbox
    /// closes, its in-flight calls cancel so callers settle visibly, and a
    /// `ChildStopped` fact names the child under this owner. The owner
    /// itself keeps running; pair with [`Self::Stop`] (for example
    /// `batch([stop_children(), stop()])`) for a full owner-then-children
    /// shutdown. Plain [`Self::Stop`] is unchanged and never cascades, so
    /// children that should outlive their parent still do.
    StopChildren,

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
/// path produces a `RequestEffect` by consuming [`crate::RequestCall`] through
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
    pub(crate) fn from_consumed_effect(effect: Effect<I>) -> Self {
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
/// [`crate::ServiceRequestAddress`] cannot be passed here, so the common
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
/// delivered through the continuation as [`crate::SpawnObservedError`]. Delivery
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

/// Returns an effect that fails the current isolate as a typed, non-panic
/// failure. See [`Effect::Fail`]: the isolate stops and, if supervised, is
/// restarted per its parent's policy, recorded distinctly from a panic.
pub fn fail<I>() -> Effect<I>
where
    I: Isolate,
{
    Effect::Fail
}

/// Returns an effect that stops every child this isolate owns, as an explicit
/// supervised shutdown. See [`Effect::StopChildren`]: each child settles its
/// callers and is named by a `ChildStopped` fact; the owner keeps running.
pub fn stop_children<I>() -> Effect<I>
where
    I: Isolate,
{
    Effect::StopChildren
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
