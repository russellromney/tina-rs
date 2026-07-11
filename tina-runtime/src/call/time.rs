//! Time-and-sleep rail vocabulary.
//!
//! Owns the `sleep` constructor, the `sleep_then` shortcut, and
//! `SleepCall` with its `then` / `then_service_event` / `then_with_request` /
//! `then_event` continuations.

use std::time::Duration;

use super::{
    CallError, CallInput, CallOutcome, CallOutput, DeferredTypedCall, RequestDeferredTypedCall,
    RuntimeCall, SendOutcome, TypedCall,
};

/// Returns a sleep effect that ignores the infallible timer payload and
/// delivers `message` back later.
///
/// This is the small common path for "wait, then continue" state machines.
pub fn sleep_then<I, M>(after: Duration, message: M) -> tina::Effect<I>
where
    I: tina::Isolate<Message = M, Io = RuntimeCall<M>>,
    M: 'static,
{
    sleep(after).then(move |_| message)
}

impl<T> CallOutcome<T> {
    /// Converts a call outcome into the successful reply or a call error.
    pub fn into_result(self) -> Result<T, CallError> {
        match self {
            Self::Replied(reply) => Ok(reply),
            Self::Full => Err(CallError::TargetFull),
            Self::Closed => Err(CallError::TargetClosed),
            Self::Timeout => Err(CallError::Timeout),
            Self::Rejected(reason) => Err(CallError::Rejected(reason)),
        }
    }
}

impl SendOutcome {
    /// Returns whether the send was accepted by the runtime boundary.
    pub const fn is_accepted(self) -> bool {
        matches!(self, Self::Accepted)
    }

    /// Returns whether the send hit bounded backpressure.
    pub const fn is_full(self) -> bool {
        matches!(self, Self::Full)
    }

    /// Returns whether the target was closed or stale.
    pub const fn is_closed(self) -> bool {
        matches!(self, Self::Closed)
    }
}

impl<T, E> TypedCall<T, E> {
    pub(super) fn new(request: CallInput, decode: fn(CallOutput) -> Result<T, E>) -> Self {
        Self { request, decode }
    }

    /// Turns this prepared runtime-owned call into one ordinary later message.
    #[deprecated(
        since = "0.1.0",
        note = "use `.then(...)` for ordinary continuations; use `call_ctx.defer(work).reply(...)` in handle_call when preserving caller authority"
    )]
    pub fn reply<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(Result<T, E>) -> M + 'static,
        T: 'static,
        E: 'static,
    {
        self.then(translator)
    }

    /// Turns this prepared runtime-owned call into one ordinary later message.
    pub fn then<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(Result<T, E>) -> M + 'static,
        T: 'static,
        E: 'static,
    {
        let decode = self.decode;
        tina::Effect::Io(RuntimeCall::new(self.request, move |output| {
            translator(decode(output))
        }))
    }

    /// Turns this runtime-owned call into a split service's later event.
    ///
    /// The translator still receives the complete typed result. This helper
    /// hides only the split-service routing envelope; it does not discard I/O
    /// failures or otherwise change the call's outcome.
    pub fn then_service_event<I, F, Event, Request>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<
                Message = tina::ServiceMessage<Event, Request>,
                Io = RuntimeCall<tina::ServiceMessage<Event, Request>>,
            >,
        F: FnOnce(Result<T, E>) -> Event + 'static,
        T: 'static,
        E: 'static,
        Event: 'static,
        Request: 'static,
    {
        self.then(move |result| tina::ServiceMessage::Event(translator(result)))
    }

    /// Like [`reply`](Self::reply), but also carries the caller's
    /// [`RequestContext`] into the continuation message.
    #[deprecated(
        since = "0.1.0",
        note = "use `.then_with_request(...)`; use `call_ctx.defer(work).reply(...)` when starting from CallContext"
    )]
    pub fn reply_with_request<I, F, M, Q>(
        self,
        req: tina::RequestContext<Q>,
        translator: F,
    ) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, Result<T, E>) -> M + 'static,
        T: 'static,
        E: 'static,
        M: 'static,
        Q: 'static,
    {
        self.then_with_request(req, translator)
    }

    /// Alias for [`reply_with_request`](Self::reply_with_request) using the
    /// ordinary-continuation vocabulary.
    pub fn then_with_request<I, F, M, Q>(
        self,
        req: tina::RequestContext<Q>,
        translator: F,
    ) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, Result<T, E>) -> M + 'static,
        T: 'static,
        E: 'static,
        M: 'static,
        Q: 'static,
    {
        let decode = self.decode;
        tina::Effect::Io(RuntimeCall::new(self.request, move |output| {
            translator(req, decode(output))
        }))
    }
}

impl<T, Q, E> DeferredTypedCall<T, Q, E>
where
    T: 'static,
    Q: 'static,
    E: 'static,
{
    /// Builds the continuation that carries the captured caller request.
    ///
    /// This does not reply to the caller by itself. The generated message must
    /// later consume the [`RequestContext`](tina::RequestContext) with
    /// `reply_to`.
    pub fn reply<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Reply = Q, Io = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, Result<T, E>) -> M + 'static,
        M: 'static,
    {
        self.inner.then_with_request(self.request, translator)
    }
}

impl<'request, T, I, E> RequestDeferredTypedCall<'request, T, I, E>
where
    T: 'static,
    I: tina::Isolate,
    I::Reply: 'static,
    E: 'static,
{
    /// Builds a request effect whose continuation carries caller authority.
    pub fn reply<F, M>(self, translator: F) -> tina::RequestEffect<I>
    where
        I: tina::Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<I::Reply>, Result<T, E>) -> M + 'static,
        M: 'static,
    {
        self.permit.apply(self.inner.reply(translator))
    }

    /// Builds a request effect whose continuation is a split service event.
    pub fn reply_service_event<I, F, Event, Request>(self, translator: F) -> tina::RequestEffect<I>
    where
        I: tina::Isolate<
                Message = tina::ServiceMessage<Event, Request>,
                Reply = Q,
                Io = RuntimeCall<tina::ServiceMessage<Event, Request>>,
            >,
        F: FnOnce(tina::RequestContext<Q>, Result<T, E>) -> Event + 'static,
        Event: 'static,
        Request: 'static,
    {
        crate::call::request_effect_from_consumed_effect(
            self.inner
                .reply(move |req, result| tina::ServiceMessage::Event(translator(req, result))),
        )
    }
}

impl<T, E, I> tina::DeferThrough<I> for TypedCall<T, E>
where
    T: 'static,
    E: 'static,
    I: tina::Isolate,
    I::Reply: 'static,
{
    type Deferred = DeferredTypedCall<T, I::Reply, E>;

    fn defer_through(self, call: tina::CallContext<'_, I>) -> Self::Deferred {
        DeferredTypedCall {
            inner: self,
            request: call.into_request_context(),
        }
    }
}

impl<T, E, I> tina::RequestDeferThrough<I> for TypedCall<T, E>
where
    T: 'static,
    E: 'static,
    I: tina::Isolate,
    I::Reply: 'static,
{
    type RequestDeferred<'request>
        = RequestDeferredTypedCall<'request, T, I, E>
    where
        I: 'request;

    fn defer_request_through<'request>(
        self,
        call: tina::RequestCall<'request, I>,
    ) -> Self::RequestDeferred<'request> {
        let (request, permit) = call.into_request_context_with_permit();
        RequestDeferredTypedCall {
            inner: DeferredTypedCall {
                inner: self,
                request,
            },
            permit,
        }
    }
}

/// Returns a typed sleep helper that later yields `Result<(), CallError>`.
///
/// The returned [`SleepCall`] wraps an internal `TypedCall<()>` so the user
/// surface keeps `.then(...)` and adds `.then_event(...)` for the common
/// "wake me later with this event" path. Sleep is the only `TypedCall<()>`
/// that gets [`SleepCall::then_event`] — non-timer `TypedCall<()>` (file,
/// process, TCP close, ...) must surface their error path with `.then(...)`.
pub fn sleep(after: Duration) -> SleepCall {
    SleepCall::new(TypedCall::new(
        CallInput::Sleep { after },
        CallOutput::into_timer_fired,
    ))
}

/// Sleep-only wrapper around `TypedCall<()>`. Adds the unit-event sugar
/// [`SleepCall::then_event`] without exposing it on every `TypedCall<()>`.
///
/// `then` and `then_with_request` forward unchanged so existing
/// `sleep(d).then(...)` paths still compile.
#[doc(hidden)]
pub struct SleepCall {
    inner: TypedCall<()>,
}

impl SleepCall {
    fn new(inner: TypedCall<()>) -> Self {
        Self { inner }
    }

    /// Ordinary "wake me later" continuation. Identical to
    /// [`TypedCall::then`] for the timer payload.
    pub fn then<I, F, M>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(Result<(), CallError>) -> M + 'static,
    {
        self.inner.then(translator)
    }

    /// Turns this timer completion into a split service's later event.
    ///
    /// Unlike [`Self::then_event`], this keeps the timer result visible to the
    /// domain-event translator and only hides `ServiceMessage::Event(...)`.
    pub fn then_service_event<I, F, E, Q>(self, translator: F) -> tina::Effect<I>
    where
        I: tina::Isolate<
                Message = tina::ServiceMessage<E, Q>,
                Io = RuntimeCall<tina::ServiceMessage<E, Q>>,
            >,
        F: FnOnce(Result<(), CallError>) -> E + 'static,
        E: 'static,
        Q: 'static,
    {
        self.inner.then_service_event(translator)
    }

    /// Carry caller authority into the wake continuation.
    pub fn then_with_request<I, F, M, Q>(
        self,
        req: tina::RequestContext<Q>,
        translator: F,
    ) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce(tina::RequestContext<Q>, Result<(), CallError>) -> M + 'static,
        M: 'static,
        Q: 'static,
    {
        self.inner.then_with_request(req, translator)
    }

    /// Timer-only sugar: produce the next event without reading the timer
    /// reply. Use when the handler enum has nothing to gain from a
    /// `SleepReply`-shaped field.
    ///
    /// This helper is intentionally only on `SleepCall`. A non-timer
    /// `TypedCall<()>` (file, process, TCP close, signal, bridge) must be
    /// consumed with `.then(...)` so its error path stays visible.
    ///
    /// Positive shape: the user enum has nothing timer-shaped in it.
    ///
    /// ```
    /// # use std::convert::Infallible;
    /// # use std::time::Duration;
    /// # use tina::prelude::*;
    /// # use tina_runtime::{sleep, RuntimeCall};
    /// #[derive(Debug)]
    /// enum Msg {
    ///     Wake { id: u64 },
    /// }
    /// # struct Svc;
    /// # impl Isolate for Svc {
    /// #     type Message = Msg;
    /// #     type Reply = ();
    /// #     type Send = tina::Outbound<Infallible>;
    /// #     type Spawn = Infallible;
    /// #     type SpawnObserved = Infallible;
    /// #     type Io = RuntimeCall<Msg>;
    /// #     type Fact = Infallible;
    /// #     type Shard = tina::SingleShard;
    /// #     fn handle(&mut self, _m: Msg, _ctx: &mut Context<'_, Self::Shard, ()>) -> Effect<Self> {
    /// #         tina::noop()
    /// #     }
    /// # }
    /// fn schedule(id: u64) -> Effect<Svc> {
    ///     sleep(Duration::from_millis(1)).then_event(move || Msg::Wake { id })
    /// }
    /// ```
    ///
    /// Compile-fail: `then_event` is not on `TypedCall<()>`. The TCP
    /// close path also returns `TypedCall<()>` but its error must stay
    /// visible, so `.then(...)` is the only way to consume it.
    ///
    /// ```compile_fail
    /// # use std::convert::Infallible;
    /// # use tina::prelude::*;
    /// # use tina_runtime::{tcp_close_stream, RuntimeCall, StreamId};
    /// # struct Svc;
    /// # #[derive(Debug)] enum Msg { Done }
    /// # impl Isolate for Svc {
    /// #     type Message = Msg;
    /// #     type Reply = ();
    /// #     type Send = tina::Outbound<Infallible>;
    /// #     type Spawn = Infallible;
    /// #     type SpawnObserved = Infallible;
    /// #     type Io = RuntimeCall<Msg>;
    /// #     type Shard = tina::SingleShard;
    /// #     fn handle(&mut self, _m: Msg, _ctx: &mut Context<'_, Self::Shard, ()>) -> Effect<Self> {
    /// #         tina::noop()
    /// #     }
    /// # }
    /// fn close(stream: StreamId) -> Effect<Svc> {
    ///     // tcp_close_stream returns TypedCall<()>, not SleepCall.
    ///     tcp_close_stream(stream).then_event(|| Msg::Done)
    /// }
    /// ```
    ///
    /// Compile-fail: `file_close` is a second non-timer `TypedCall<()>`
    /// that must surface its error. Same protection applies.
    ///
    /// ```compile_fail
    /// # use std::convert::Infallible;
    /// # use tina::prelude::*;
    /// # use tina_runtime::{file_close, FileId, RuntimeCall};
    /// # struct Svc;
    /// # #[derive(Debug)] enum Msg { Done }
    /// # impl Isolate for Svc {
    /// #     type Message = Msg;
    /// #     type Reply = ();
    /// #     type Send = tina::Outbound<Infallible>;
    /// #     type Spawn = Infallible;
    /// #     type SpawnObserved = Infallible;
    /// #     type Io = RuntimeCall<Msg>;
    /// #     type Shard = tina::SingleShard;
    /// #     fn handle(&mut self, _m: Msg, _ctx: &mut Context<'_, Self::Shard, ()>) -> Effect<Self> {
    /// #         tina::noop()
    /// #     }
    /// # }
    /// fn close(file: FileId) -> Effect<Svc> {
    ///     file_close(file).then_event(|| Msg::Done)
    /// }
    /// ```
    pub fn then_event<I, F, M>(self, event: F) -> tina::Effect<I>
    where
        I: tina::Isolate<Message = M, Io = RuntimeCall<M>>,
        F: FnOnce() -> M + 'static,
        M: 'static,
    {
        self.inner.then(move |_| event())
    }
}

impl<I> tina::DeferThrough<I> for SleepCall
where
    I: tina::Isolate,
    I::Reply: 'static,
{
    type Deferred = DeferredTypedCall<(), I::Reply>;

    fn defer_through(self, call: tina::CallContext<'_, I>) -> Self::Deferred {
        <TypedCall<()> as tina::DeferThrough<I>>::defer_through(self.inner, call)
    }
}

impl<I> tina::RequestDeferThrough<I> for SleepCall
where
    I: tina::Isolate,
    I::Reply: 'static,
{
    type RequestDeferred<'request>
        = RequestDeferredTypedCall<'request, (), I>
    where
        I: 'request;

    fn defer_request_through<'request>(
        self,
        call: tina::RequestCall<'request, I>,
    ) -> Self::RequestDeferred<'request> {
        <TypedCall<()> as tina::RequestDeferThrough<I>>::defer_request_through(self.inner, call)
    }
}
