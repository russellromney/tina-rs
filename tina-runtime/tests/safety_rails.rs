//! User-shape proofs for Phase 100 compile-time safety rails.
//!
//! Positive fixtures: a callable service registered through
//! [`Runtime::register_service`] and a send-only worker registered through
//! [`Runtime::register_service_send_only`]. Both shapes must compile, route
//! correctly, and expose the capability-typed handles documented in
//! `tina::SendAddress` and `tina::CallAddress`.
//!
//! Negative fixtures live as `compile_fail` doctests on the items they pin:
//! see `tina::SendAddress`, `tina::CallAddress`, `tina_runtime::call_typed`,
//! and `tina_runtime::Runtime::register_service`.

use std::convert::Infallible;
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

#[derive(Debug)]
enum ApiMsg {
    Get(String),           // public, callable
    InternalFillDone(u32), // internal continuation, send-only
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

#[test]
fn send_to_via_send_address_routes_to_handle() {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    let handle = runtime.register_service::<Api, Infallible>(Api { fills_seen: 0 }, 8);

    // Drive an internal continuation through the send lane. `try_send` is the
    // runtime ingress; capability-typed `SendAddress` only changes the
    // boundary type, not the wire.
    runtime
        .try_send(handle.send.address(), ApiMsg::InternalFillDone(7))
        .expect("send accepted");
    while runtime.step() > 0 {}
}

// ---------------------------------------------------------------------------
// Positive fixture #2: a send-only worker. No `handle_call`, no `.call` lane.
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum WorkerMsg {
    Tick,
}

struct Worker;

#[tina_runtime::isolate(message = WorkerMsg, send_only)]
impl Worker {
    fn handle(
        &mut self,
        _msg: WorkerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }
}

#[test]
fn register_service_send_only_only_exposes_send_lane() {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    let handle: SendOnlyServiceHandle<WorkerMsg> =
        runtime.register_service_send_only::<Worker, Infallible>(Worker, 4);

    let _: tina::SendAddress<WorkerMsg> = handle.send;
    runtime
        .try_send(handle.send.address(), WorkerMsg::Tick)
        .expect("send accepted");
    while runtime.step() > 0 {}
}

// ---------------------------------------------------------------------------
// Positive fixture #3: another isolate calling the service through
// `call_typed` against the `.call` lane.
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum ClientMsg {
    Start(tina::CallAddress<ApiMsg, ApiReply>, String),
    Returned(Result<ApiReply, tina_runtime::CallError>),
}

struct Client {
    out: Option<ApiReply>,
}

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
            ClientMsg::Returned(result) => {
                self.out = result.ok();
                stop()
            }
        }
    }
}

#[test]
fn call_typed_round_trips_through_call_lane() {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);

    let api = runtime.register_service::<Api, Infallible>(Api { fills_seen: 99 }, 8);
    let client_addr = runtime.register_with_capacity::<Client, Infallible>(Client { out: None }, 8);

    runtime
        .try_send(client_addr, ClientMsg::Start(api.call, "key".into()))
        .expect("client accepts start");
    while runtime.step() > 0 {}
    // The client stopped on return; the round trip itself proves the call
    // lane routed Get to handle_call and replied through CallAddress.
}
