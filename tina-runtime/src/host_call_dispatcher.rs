//! Persistent host-call dispatcher (phase 145 / Rock 5).
//!
//! One long-lived isolate per worker shard handles every `call_blocking` from
//! the host. The old `HostCallDriver` registered a fresh isolate per call —
//! mailbox alloc, adapter box, handler box, isolate entry, call-context queue
//! — adding ~5–7 allocations per call. The dispatcher pays those once at
//! worker startup and amortizes them across every host call for the worker's
//! lifetime.
//!
//! Type erasure: the dispatcher's `Message` carries a typed task as
//! `Box<dyn HostCallTaskBegin<S>>` for the issue phase and
//! `Box<dyn HostCallTaskComplete>` for the reply-delivery phase. Each
//! concrete `Begin` knows its own `M` / `R` and issues the typed call
//! internally; the `.then` translator closes over the reply sender so when
//! the call completes the runtime produces a `Returned` carrying a
//! `Complete` that delivers the outcome.
//!
//! Bounds preserved (same as the old per-call driver):
//!
//! - `CallOutcome::{Replied, Full, Closed, Timeout, Rejected}` — surfaced by
//!   the underlying `call` effect; the dispatcher just routes.
//! - `ThreadedRuntimeError::HostWaitTimeout` — host-side `recv_timeout`,
//!   unchanged.
//! - Shutdown — the dispatcher isolate is cancelled like any other; any
//!   sender held by an in-flight task drops, host sees `Disconnected` →
//!   `WorkerStopped`.

use std::convert::Infallible;
use std::marker::PhantomData;
use std::time::Duration;

use tina::{Address, Context, Effect, Isolate, Outbound as TinaOutbound, Shard};

use crate::call::{CallOutcome, RuntimeCall, call};
use crate::host_call_reply_pool::TypedReplySender;

/// One persistent dispatcher isolate per worker shard.
pub(crate) struct HostCallDispatcher<S: Shard + 'static> {
    _marker: PhantomData<S>,
}

impl<S: Shard + 'static> HostCallDispatcher<S> {
    pub(crate) const fn new() -> Self {
        Self {
            _marker: PhantomData,
        }
    }
}

/// Two phases of a host call, both type-erased so the dispatcher's mailbox
/// stays one concrete enum.
pub(crate) enum DispatcherMsg<S: Shard + 'static> {
    /// Host enqueued a new call: execute it (issue the typed call).
    Begin(Box<dyn HostCallTaskBegin<S>>),
    /// The call replied: deliver the outcome to the host.
    Returned(Box<dyn HostCallTaskComplete>),
}

/// "Issue a host call" task. Concrete impls know their `M` / `R` and call
/// the typed `call(target, message, timeout)` effect internally.
pub(crate) trait HostCallTaskBegin<S: Shard + 'static>: Send + 'static {
    fn execute(self: Box<Self>) -> Effect<HostCallDispatcher<S>>;
}

/// "Deliver this outcome to the waiting host thread" task. Concrete impls
/// hold the typed outcome and the typed reply sender.
pub(crate) trait HostCallTaskComplete: Send + 'static {
    fn complete(self: Box<Self>);
}

pub(crate) struct ConcreteHostCallBegin<S, M, R>
where
    S: Shard + Send + 'static,
    M: Send + 'static,
    R: Send + 'static,
{
    pub(crate) target: Address<M, R>,
    pub(crate) message: M,
    pub(crate) timeout: Duration,
    pub(crate) sender: TypedReplySender<CallOutcome<R>>,
    pub(crate) _marker: PhantomData<S>,
}

impl<S, M, R> HostCallTaskBegin<S> for ConcreteHostCallBegin<S, M, R>
where
    S: Shard + Send + 'static,
    M: Send + 'static,
    R: Send + 'static,
{
    fn execute(self: Box<Self>) -> Effect<HostCallDispatcher<S>> {
        let ConcreteHostCallBegin {
            target,
            message,
            timeout,
            sender,
            _marker,
        } = *self;
        call(target, message, timeout).then(move |outcome: CallOutcome<R>| {
            DispatcherMsg::Returned(Box::new(ConcreteHostCallComplete { outcome, sender }))
        })
    }
}

pub(crate) struct ConcreteHostCallComplete<R: Send + 'static> {
    pub(crate) outcome: CallOutcome<R>,
    pub(crate) sender: TypedReplySender<CallOutcome<R>>,
}

impl<R: Send + 'static> HostCallTaskComplete for ConcreteHostCallComplete<R> {
    fn complete(self: Box<Self>) {
        let ConcreteHostCallComplete { outcome, sender } = *self;
        // The host may have already given up (HostWaitTimeout) and dropped the
        // receiver — in which case `send` stores into a shared state nobody
        // will read, and the channel is freed when the sender drops. The
        // runtime's late-reply trace event already records that the call
        // completed.
        sender.send(outcome);
    }
}

impl<S: Shard + 'static> Isolate for HostCallDispatcher<S> {
    tina::isolate_types! {
        message: DispatcherMsg<S>,
        reply: (),
        send: TinaOutbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<DispatcherMsg<S>>,
        shard: S,
    }

    fn handle(
        &mut self,
        msg: DispatcherMsg<S>,
        _ctx: &mut Context<'_, S, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DispatcherMsg::Begin(task) => task.execute(),
            DispatcherMsg::Returned(complete) => {
                complete.complete();
                tina::noop()
            }
        }
    }
}
