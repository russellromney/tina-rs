//! Public-surface tests for `RequestCall::now` (findings-ledger #36).
//!
//! `CallContext::now` already mirrors the runtime clock; `RequestCall`
//! wraps a `CallContext` as split-service caller authority but had no way
//! to read the clock without consuming itself via `into_call_context`.
//! These tests prove the new accessor reports the same stamped time and,
//! crucially, that reading it does not consume the `RequestCall` — a
//! handler can still `reject`/`defer` the caller afterwards.

use std::convert::Infallible;
use std::time::{Duration, Instant};

use tina::{CallContext, CallRejectedReason, Context, Effect, Isolate, IsolateId, RequestCall};

/// Minimal isolate shape purely to exercise `RequestCall::now`. `handle` is
/// never invoked here, so `Message` is `Infallible` — there is nothing to
/// construct.
#[derive(Debug)]
struct PingService;

impl Isolate for PingService {
    tina::isolate_types! {
        message: Infallible,
        reply: (),
        send: Infallible,
        spawn: Infallible,
        io: Infallible,
        shard: tina::SingleShard,
    }

    fn handle(
        &mut self,
        _msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match _msg {}
    }
}

fn request_call_with_now(stamped: Instant) -> RequestCall<'static, PingService> {
    let shard: &'static mut tina::SingleShard = Box::leak(Box::new(tina::SingleShard));
    let registry = std::rc::Rc::new(tina::DeferredSlotRegistry::new());
    let caller = tina::MessageCaller::new(
        registry,
        1,
        IsolateId::new(1),
        tina::CallRouting::Local,
        std::any::TypeId::of::<()>(),
    );
    // SAFETY: this helper builds the complete caller/context pair for this
    // isolated public-surface test; no live runtime registry is involved.
    let call = unsafe {
        let ctx = Context::<_, ()>::new_typed(shard, IsolateId::new(1))
            .with_now(stamped)
            .with_caller(caller);
        CallContext::new(ctx)
    };
    RequestCall::new(call)
}

#[test]
fn request_call_now_matches_stamped_runtime_clock() {
    let stamped = Instant::now() - Duration::from_secs(30);
    let call = request_call_with_now(stamped);

    assert_eq!(
        call.now(),
        stamped,
        "RequestCall::now() must report the same time the runtime/simulator stamped \
         on the underlying CallContext for this turn, not a fresh Instant::now()"
    );
}

#[test]
fn request_call_now_borrows_and_does_not_consume_the_call() {
    let stamped = Instant::now();
    let call = request_call_with_now(stamped);

    // The whole point: reading the clock ahead of a deadline/rate-window
    // check must not strand caller authority. `now()` takes `&self`, so the
    // call is still usable for `reject` (or `defer`/`capture`) afterwards.
    let read_at = call.now();
    assert_eq!(read_at, stamped);

    let effect = call.reject(CallRejectedReason::UnsupportedMessage);
    match effect.into_effect() {
        Effect::Reject(CallRejectedReason::UnsupportedMessage) => {}
        other => panic!("expected Reject(UnsupportedMessage), got {other:?}"),
    }
}
