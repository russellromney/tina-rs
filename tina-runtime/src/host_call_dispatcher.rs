//! Persistent host-call dispatcher.
//!
//! One long-lived isolate per worker shard handles every `call_blocking` from
//! the host. The old `HostCallDriver` registered a fresh isolate per call —
//! mailbox alloc, adapter box, handler box, isolate entry, call-context queue
//! — adding ~5–7 allocations per call. The dispatcher pays those once at
//! worker startup and amortizes them across every host call for the worker's
//! lifetime.
//!
//! Type erasure: the dispatcher's `Message` carries a typed begin task as
//! `Box<dyn HostCallTaskBegin<S>>`. Each concrete `Begin` knows its own `M` /
//! `R` and issues the typed call internally. The `.then` translator closes
//! over the reply sender and delivers the host-visible outcome before
//! producing a bookkeeping `Returned` message. That makes the host answer
//! independent of dispatcher mailbox pressure on the return path.
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
    /// The call replied and the host has already been answered.
    Returned,
}

/// "Issue a host call" task. Concrete impls know their `M` / `R` and call
/// the typed `call(target, message, timeout)` effect internally.
pub(crate) trait HostCallTaskBegin<S: Shard + 'static>: Send + 'static {
    fn execute(self: Box<Self>) -> Effect<HostCallDispatcher<S>>;
    fn reject_full(self: Box<Self>);
    fn reject_closed(self: Box<Self>);
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
            // Answer the parked host thread in the translator itself. A later
            // `Returned` bookkeeping message can be dropped under dispatcher
            // pressure without stranding the host-side reply channel.
            sender.send(outcome);
            DispatcherMsg::Returned
        })
    }

    fn reject_full(self: Box<Self>) {
        self.sender.send(CallOutcome::Full);
    }

    fn reject_closed(self: Box<Self>) {
        self.sender.send(CallOutcome::Closed);
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
            DispatcherMsg::Returned => tina::noop(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tina::ShardId;

    use crate::host_call_reply_pool::{checkin, checkout};

    #[derive(Debug, Clone, Copy)]
    struct TestShard;

    impl Shard for TestShard {
        fn id(&self) -> ShardId {
            ShardId::new(0)
        }
    }

    struct RejectOnly {
        sender: TypedReplySender<CallOutcome<u32>>,
    }

    impl HostCallTaskBegin<TestShard> for RejectOnly {
        fn execute(self: Box<Self>) -> Effect<HostCallDispatcher<TestShard>> {
            unreachable!("reject-only test task should not execute")
        }

        fn reject_full(self: Box<Self>) {
            self.sender.send(CallOutcome::Full);
        }

        fn reject_closed(self: Box<Self>) {
            self.sender.send(CallOutcome::Closed);
        }
    }

    #[test]
    fn rejected_begin_reports_full_instead_of_disconnect() {
        let reply = checkout::<CallOutcome<u32>>();
        let task: Box<dyn HostCallTaskBegin<TestShard>> = Box::new(RejectOnly {
            sender: reply.sender(),
        });

        task.reject_full();

        assert!(matches!(
            reply.recv_timeout(Duration::from_secs(1)),
            Ok(CallOutcome::Full)
        ));
        checkin(reply);
    }
}
