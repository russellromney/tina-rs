//! Regression for FINDINGS.md finding 26: a runtime-owned call effect
//! returned from `handle_call` must deliver its completion to `handle`,
//! not back to `handle_call`.
//!
//! `system_realtime_rooms` hit this: a connection isolate returned
//! `call(addr, ..., timeout).then(...)` from `handle_call` and the
//! continuation message landed in `handle_call`, where its handler arm
//! routed everything to `call.reject(UnsupportedMessage)`. The connection
//! never drained, the queue filled, the room evicted the peer.
//!
//! User truth this test pins:
//!
//! - one isolate has both `handle` and `handle_call`;
//! - `handle_call` for one variant returns a runtime call effect with a
//!   `.then(_)` continuation that captures the caller's request context;
//! - the runtime delivers the completion as an ordinary message that
//!   reaches `handle`, not `handle_call`;
//! - the original caller receives the final reply;
//! - the trace records no `CallRejected { UnsupportedMessage }` event
//!   from the continuation landing.
//!
//! Substrate: hermetic timer (`sleep`) on the live threaded runtime — no
//! network, no fixtures.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeEventKind, SleepReply, ThreadedRuntime,
    sleep,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EchoReply(u32);

#[derive(Debug)]
enum EchoMsg {
    Start(u32),
    Finished(tina::RequestContext<EchoReply>, u32, SleepReply),
}

struct Echo;

#[tina_runtime::isolate(message = EchoMsg, reply = EchoReply)]
impl Echo {
    fn handle(
        &mut self,
        msg: EchoMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            EchoMsg::Start(_) => noop(),
            EchoMsg::Finished(req, value, Ok(())) => tina::reply_to(req, EchoReply(value)),
            EchoMsg::Finished(_, _, Err(_)) => noop(),
        }
    }

    fn handle_call(&mut self, msg: EchoMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            EchoMsg::Start(value) => {
                let req = call.into_request_context();
                sleep(Duration::from_millis(20)).then_with_request(req, move |req, result| {
                    EchoMsg::Finished(req, value, result)
                })
            }
            EchoMsg::Finished(_, _, _) => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

#[test]
fn runtime_call_returned_from_handle_call_completes_as_event() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));
    let echo = runtime
        .register_with_capacity::<_, Infallible>(Echo, 16)
        .expect("register Echo");

    let outcome = runtime
        .call_blocking(echo, EchoMsg::Start(42), Duration::from_secs(2))
        .expect("call_blocking surfaces");

    assert!(
        matches!(outcome, CallOutcome::Replied(EchoReply(42))),
        "original caller must receive the deferred reply, got {outcome:?}",
    );

    let rejections = runtime
        .trace()
        .events()
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallRejected {
                    reason: tina::CallRejectedReason::UnsupportedMessage,
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        rejections, 0,
        "no UnsupportedMessage rejection: the continuation must land in \
         handle, not handle_call",
    );

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}
