//! User-shape proofs for Phase 100 compile-time safety rails.
//!
//! Positive fixtures: a callable service registered through
//! [`Runtime::register_service`] and a send-only worker registered through
//! [`Runtime::register_service_send_only`]. Both shapes must compile, route
//! correctly, and expose the capability-typed handles documented in
//! `tina::SendAddress` and `tina::CallAddress`.
//!
//! Negative fixtures live as `compile_fail` doctests on the items they pin
//! and as a `trybuild` UI test in `tests/safety_rails_compile_fail/`. The
//! `trybuild` form pins the actual diagnostic phrase; the doctests pin
//! compile failure at the documentation site so a future reader sees the
//! shape next to the type.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultMailboxFactory, Runtime, SendOnlyServiceHandle, ServiceHandle, call_typed,
};

// ---------------------------------------------------------------------------
// Positive fixture #1: a callable service with public requests AND internal
// continuations sharing the same mailbox enum. `register_service` returns a
// typed handle that splits send/call at the boundary.
// ---------------------------------------------------------------------------

// `Api` mixes a public callable variant (`Get`) with an internal continuation
// variant (`InternalFillDone`) — the exact mixed-enum shape the address split
// is meant to bound at the boundary. Only `Get` is exercised at runtime by
// these tests; `InternalFillDone` exists to document the bad shape the rail
// guards against.
#[derive(Debug)]
#[allow(dead_code)]
enum ApiMsg {
    Get(String),
    InternalFillDone(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ApiReply(String);

struct Api {
    fills_seen: u32,
}

#[tina_runtime::isolate(message = ApiMsg, reply = ApiReply)]
impl Api {
    fn handle(
        &mut self,
        msg: ApiMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ApiMsg::InternalFillDone(value) => {
                self.fills_seen = value;
                noop()
            }
            // Get arriving without a reply slot is a routing bug, but `handle`
            // must not panic — it stays a noop. Tests below pin that the
            // capability-typed paths put Get on the call lane.
            ApiMsg::Get(_) => noop(),
        }
    }

    fn handle_call(&mut self, msg: ApiMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            ApiMsg::Get(key) => call.reply(ApiReply(format!("value:{key}:{}", self.fills_seen))),
            ApiMsg::InternalFillDone(_) => {
                call.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

#[test]
fn register_service_exposes_split_handles() {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    let handle: ServiceHandle<ApiMsg, ApiReply> =
        runtime.register_service::<Api, Infallible>(Api { fills_seen: 0 }, 8);

    // Both lanes point at the same isolate.
    assert_eq!(handle.send.shard(), handle.call.shard());
    assert_eq!(handle.send.isolate(), handle.call.isolate());
    assert_eq!(handle.send.generation(), handle.call.generation());

    // The send lane has its reply marker erased.
    let _: tina::SendAddress<ApiMsg> = handle.send;
    let _: tina::CallAddress<ApiMsg, ApiReply> = handle.call;
}

// ---------------------------------------------------------------------------
// Positive fixture #2: send-only worker. No `handle_call`, no `.call` lane.
// The handler bumps a counter so the test can assert delivery rather than
// just "the runtime quiesced."
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum WorkerMsg {
    Tick,
}

struct Worker {
    seen: Arc<AtomicU32>,
}

#[tina_runtime::isolate(message = WorkerMsg, send_only)]
impl Worker {
    fn handle(
        &mut self,
        _msg: WorkerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        self.seen.fetch_add(1, Ordering::Release);
        noop()
    }
}

#[test]
fn register_service_send_only_only_exposes_send_lane() {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    let seen = Arc::new(AtomicU32::new(0));
    let handle: SendOnlyServiceHandle<WorkerMsg> = runtime
        .register_service_send_only::<Worker, Infallible>(
            Worker {
                seen: Arc::clone(&seen),
            },
            4,
        );

    let _: tina::SendAddress<WorkerMsg> = handle.send;
    runtime
        .try_send(handle.send.address(), WorkerMsg::Tick)
        .expect("send accepted");
    while runtime.step() > 0 {}

    // The runtime actually delivered the message; not just "quiesced."
    assert_eq!(seen.load(Ordering::Acquire), 1);
}

// ---------------------------------------------------------------------------
// Positive fixture #3: an isolate emitting `tina::send_to(SendAddress, msg)`
// as an Effect. This exercises the capability-typed send path *from inside a
// handler*, complementing the host-side `try_send` form in fixture #2.
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum ProducerMsg {
    Pump(u32),
}

struct Producer {
    worker: tina::SendAddress<WorkerMsg>,
}

#[tina_runtime::isolate(message = ProducerMsg, send = tina::Outbound<WorkerMsg>)]
impl Producer {
    fn handle(
        &mut self,
        msg: ProducerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ProducerMsg::Pump(count) => {
                let mut effects: Vec<Effect<Self>> = Vec::with_capacity(count as usize + 1);
                for _ in 0..count {
                    effects.push(tina::send_to(self.worker, WorkerMsg::Tick));
                }
                effects.push(stop());
                batch(effects)
            }
        }
    }
}

#[test]
fn send_to_inside_isolate_routes_through_send_lane() {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    let seen = Arc::new(AtomicU32::new(0));
    let worker = runtime.register_service_send_only::<Worker, Infallible>(
        Worker {
            seen: Arc::clone(&seen),
        },
        16,
    );
    let producer = runtime.register_with_capacity::<Producer, WorkerMsg>(
        Producer {
            worker: worker.send,
        },
        4,
    );

    runtime
        .try_send(producer, ProducerMsg::Pump(5))
        .expect("pump accepted");
    while runtime.step() > 0 {}

    assert_eq!(seen.load(Ordering::Acquire), 5);
}

// ---------------------------------------------------------------------------
// Positive fixture #4: a real round-trip through `call_typed` against
// `.call`, asserting the *actual reply value* via `stop_with` and
// `observe_result`. The earlier "did the runtime quiesce" proof would have
// passed even on `Rejected`/`Timeout`; this one cannot.
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum ClientMsg {
    Start(tina::CallAddress<ApiMsg, ApiReply>, String),
    Returned(Result<ApiReply, tina_runtime::CallError>),
}

struct Client;

#[tina_runtime::isolate(message = ClientMsg)]
impl Client {
    fn handle(
        &mut self,
        msg: ClientMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ClientMsg::Start(target, key) => {
                call_typed(target, ApiMsg::Get(key), Duration::from_millis(50)).then(
                    |outcome: CallOutcome<ApiReply>| ClientMsg::Returned(outcome.into_result()),
                )
            }
            ClientMsg::Returned(result) => stop_with(result),
        }
    }
}

#[test]
fn call_typed_round_trips_through_call_lane() {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);

    let api = runtime.register_service::<Api, Infallible>(Api { fills_seen: 99 }, 8);
    let client_addr = runtime.register_with_capacity::<Client, Infallible>(Client, 8);
    let waiter = runtime
        .observe_result::<Result<ApiReply, tina_runtime::CallError>, _, _>(client_addr)
        .expect("observe_result registers");

    runtime
        .try_send(client_addr, ClientMsg::Start(api.call, "key".into()))
        .expect("client accepts start");
    while runtime.step() > 0 {}

    let observed = waiter
        .wait(Duration::from_millis(100))
        .expect("client stopped with a typed result");
    let reply = observed.expect("call did not error: not Rejected/Timeout/Full/Closed");
    // The reply embeds the `fills_seen` snapshot (99) and the key; if the call
    // had routed to `handle` (no reply slot) the client would have timed out
    // and observed an `Err`.
    assert_eq!(reply, ApiReply("value:key:99".into()));
}
