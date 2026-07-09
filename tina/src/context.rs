//! Handler-side context types and deferred-reply machinery.
//!
//! Owns [`Context`], [`CallContext`], [`RequestCall`], [`RequestContext`],
//! the `DeferThrough` trait family, deferred-reply slots,
//! [`CallHandle`] cancellation authority, [`Deadline`], [`CallRouting`],
//! and [`MessageCaller`]. Re-exported from the crate root.
//!
//! ## Module map
//!
//! Most handler-side ergonomics types belong here. Address types live
//! in `mod address`; effects live in `mod effect`; isolate-shape
//! vocabulary lives in `mod isolate`.

use std::any::TypeId;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::{
    Address, AddressGeneration, Effect, Isolate, IsolateId, Outbound, RequestEffect, Shard, ShardId,
};

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
    /// request context via [`take_request_context`](Self::take_request_context).
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
    /// Internal primitive behind [`Self::take_request_context`].
    pub(crate) fn take_reply_slot(&mut self) -> Result<DeferredReply<R>, TakeReplySlotError>
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
    /// This is the app-facing capture primitive. The name signals intent:
    /// "I will reply later through a multi-turn workflow."
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

    /// Returns the runtime's current observed time. Mirrors
    /// [`CallContext::now`] so handlers can read the clock — for deadline
    /// math or rate-window checks — before handing the caller off to
    /// [`defer`](Self::defer) or [`defer_cancelable`](Self::defer_cancelable).
    /// Borrows rather than consumes, so the [`RequestCall`] is still usable
    /// afterwards.
    pub fn now(&self) -> std::time::Instant {
        self.inner.now()
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

/// One-shot typed handle for replying to a captured caller later.
///
/// A `DeferredReply<R>` is usually obtained through
/// [`RequestContext::into_deferred`]. The handler may store it in isolate
/// state and use [`crate::reply_to`] from a later turn to answer the original
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
    pub(crate) handle: DeferredReplyHandle,
    pub(crate) _marker: PhantomData<fn(R) -> R>,
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
/// isolate state, and eventually passes it to [`crate::reply_to`] or
/// carries it into a continuation message.
///
/// Like [`DeferredReply`], it is move-only (`!Clone`). It does not
/// auto-reply, auto-retry, or hide effects. It is typed so the reply
/// payload must match the original caller's expected type.
///
/// Capture via [`Context::take_request_context`].
///
#[must_use = "a RequestContext must eventually be replied to or intentionally dropped"]
#[derive(Debug)]
pub struct RequestContext<R>(pub(crate) DeferredReply<R>);

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

impl<R> From<RequestContext<R>> for DeferredReply<R> {
    fn from(value: RequestContext<R>) -> Self {
        value.into_deferred()
    }
}

/// Type-erased handle for a runtime-owned deferred reply slot.
///
/// Application code does not construct or unwrap this directly; it lives
/// inside [`DeferredReply<R>`]. Runtime crates allocate it through
/// `runtime_internal`.
///
/// `DeferredReplyHandle` is intentionally not `Clone`. The user-facing
/// API exposes only [`slot_id`](Self::slot_id) and
/// [`state`](Self::state); reconstruction or duplication of slots is
/// reserved for runtime crates via the `runtime_internal` module.
#[derive(Debug)]
pub struct DeferredReplyHandle {
    pub(crate) shared: Arc<DeferredSlotShared>,
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
/// **Concurrency invariant.** All writes happen on the shard thread that owns
/// the deferred slot registry. Reads can come from another thread through a
/// host-held handle or moved isolate state. Writers use `Release`, readers use
/// `Acquire`, so observers see a consistent terminal transition.
#[derive(Debug)]
pub struct DeferredSlotShared {
    slot_id: u64,
    state: AtomicU8,
    /// `TypeId` of the original caller's expected reply payload. The
    /// runtime sets this from the dispatching `Address<_, R>`'s `R`.
    /// Normal handlers cannot choose a different deferred reply type
    /// because [`Context::take_request_context`] derives it from the current
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
        match self.state.load(Ordering::Acquire) {
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
        self.state.store(byte, Ordering::Release);
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
/// #     type Io = std::convert::Infallible;
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
    pub(crate) handle: CallHandleInner,
    pub(crate) _marker: PhantomData<fn(R) -> R>,
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

/// Type-erased inner handle. Runtime-only; mint via `runtime_internal`.
#[doc(hidden)]
#[derive(Debug)]
pub struct CallHandleInner {
    pub(crate) shared: Arc<CallHandleShared>,
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
    /// The handle is still pending but has not been admitted by the runtime.
    ///
    /// This can happen when a handler emits `cancel_call(handle)` before the
    /// matching `call_cancelable(...).then(...)` effect in the same batch. No
    /// call slot exists yet, so there is nothing to reclaim and no pending
    /// remote work to cancel.
    NotAdmitted,
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

/// Reasons [`Context::take_request_context`] may refuse a capture.
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
/// the first call to [`Context::take_request_context`]. Holds primitives
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
        crate::runtime_internal::handle_from_shared(shared)
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
