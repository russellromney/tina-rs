//! Live proofs for the native gRPC unary client.
//!
//! Stands up a real Tina gRPC server (`GrpcRouter`) and dials it from
//! the native `GrpcClient` over an `Http2ClientConnection` — no Tokio,
//! no blocking helper. Asserts:
//! - unary OK → `GrpcUnaryOutcome::Ok(decoded message)`
//! - unary non-OK status → `GrpcUnaryOutcome::Status(..)` (the status is
//!   the caller outcome, not hidden in a success)
//! - the received status is emitted as a `GrpcFinalStatusReceived`
//!   protocol fact
//! - a too-large request is rejected before the wire (`EncodeTooLarge`)

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use common::TestShard;
use prost::Message;
use tina::prelude::*;
use tina_http::{
    GrpcClient, GrpcError, GrpcLimits, GrpcRequest, GrpcResponse, GrpcRouter, GrpcStatus,
    GrpcStatusCode, GrpcUnaryOutcome, Http2ClientConnection, Http2ClientLimits, Http2ClientMsg,
    Http2Listener, Http2ListenerMsg, Http2ServerConfig, Http2Target,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, ProtocolFact, RuntimeEventKind, RuntimeFact,
    ThreadedRuntime, ThreadedRuntimeConfig,
};

#[derive(Clone, PartialEq, Message)]
struct CounterRequest {
    #[prost(uint64, tag = "1")]
    delta: u64,
}

#[derive(Clone, PartialEq, Message)]
struct CounterReply {
    #[prost(uint64, tag = "1")]
    value: u64,
}

fn runtime() -> ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> {
    ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    )
}

fn start_server(
    runtime: &ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
) -> (Address<Http2ListenerMsg>, SocketAddr) {
    let router = GrpcRouter::<TestShard>::new(GrpcLimits::default())
        .unary(
            "/specimen.Counter/Increment",
            |request: GrpcRequest<CounterRequest>| {
                Ok(GrpcResponse::new(CounterReply {
                    value: request.message.delta + 1,
                }))
            },
        )
        .unary(
            "/specimen.Counter/Status",
            |_request: GrpcRequest<CounterRequest>| {
                Err::<GrpcResponse<CounterReply>, _>(GrpcStatus::with_message(
                    GrpcStatusCode::NotFound,
                    "no such counter",
                ))
            },
        );
    let service = runtime
        .register_with_capacity::<GrpcRouter<TestShard>, _>(router, 16)
        .expect("register grpc router");
    let config = Http2ServerConfig::default();
    let listener = runtime
        .register_with_capacity::<Http2Listener<TestShard, tina_http::GrpcRouterMsg>, _>(
            Http2Listener::<TestShard, tina_http::GrpcRouterMsg>::new(
                "127.0.0.1:0".parse().unwrap(),
                service,
                config,
            ),
            config.listener_mailbox_capacity,
        )
        .expect("register listener");
    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener, Http2ListenerMsg::Start)
        .expect("start listener");
    let addr = bound
        .wait(Duration::from_secs(2))
        .expect("listener bound address");
    (listener, addr)
}

fn make_grpc_client(
    runtime: &ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
    addr: SocketAddr,
) -> GrpcClient {
    let target = Http2Target::H2c {
        authority: "grpc-test".into(),
        addr,
    };
    let conn = runtime
        .register_with_capacity::<Http2ClientConnection<TestShard>, _>(
            Http2ClientConnection::<TestShard>::new(target, Http2ClientLimits::default()),
            32,
        )
        .expect("register client connection");
    runtime
        .try_send(conn, Http2ClientMsg::Begin)
        .expect("begin client");
    GrpcClient::new(conn, GrpcLimits::default())
}

fn call_unary<Req: Message, Resp: Message + Default>(
    runtime: &ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
    client: &GrpcClient,
    path: &str,
    request: &Req,
) -> GrpcUnaryOutcome<Resp> {
    let submit = client.unary_request(path, request).expect("encode request");
    let reply = runtime
        .call_blocking(client.connection(), submit, Duration::from_secs(5))
        .expect("call returns");
    match reply {
        CallOutcome::Replied(reply) => client.unary_outcome_from_reply::<Resp>(reply),
        other => panic!("expected Replied, got {other:?}"),
    }
}

fn protocol_facts(
    runtime: &ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
) -> Vec<ProtocolFact> {
    runtime
        .complete_trace()
        .expect("complete trace")
        .into_iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::FactObserved {
                fact: RuntimeFact::Protocol(protocol),
            } => Some(protocol),
            _ => None,
        })
        .collect()
}

#[test]
fn unary_ok_returns_decoded_message() {
    let runtime = runtime();
    let (listener, addr) = start_server(&runtime);
    let client = make_grpc_client(&runtime, addr);

    let outcome: GrpcUnaryOutcome<CounterReply> = call_unary(
        &runtime,
        &client,
        "/specimen.Counter/Increment",
        &CounterRequest { delta: 41 },
    );
    match outcome {
        GrpcUnaryOutcome::Ok(reply) => assert_eq!(reply.value, 42),
        other => panic!("expected Ok(42), got {other:?}"),
    }

    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    let _ = runtime.try_send(client.connection(), Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn unary_non_ok_status_is_the_caller_outcome_not_a_success() {
    let runtime = runtime();
    let (listener, addr) = start_server(&runtime);
    let client = make_grpc_client(&runtime, addr);

    let outcome: GrpcUnaryOutcome<CounterReply> = call_unary(
        &runtime,
        &client,
        "/specimen.Counter/Status",
        &CounterRequest { delta: 0 },
    );
    match outcome {
        GrpcUnaryOutcome::Status(status) => {
            assert_eq!(status.code, GrpcStatusCode::NotFound);
            assert_eq!(status.message.as_deref(), Some("no such counter"));
        }
        other => panic!("expected Status(NotFound), got {other:?}"),
    }

    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    let _ = runtime.try_send(client.connection(), Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn received_grpc_status_is_emitted_as_a_protocol_fact() {
    let runtime = runtime();
    let (listener, addr) = start_server(&runtime);
    let client = make_grpc_client(&runtime, addr);

    let _: GrpcUnaryOutcome<CounterReply> = call_unary(
        &runtime,
        &client,
        "/specimen.Counter/Increment",
        &CounterRequest { delta: 1 },
    );

    let facts = protocol_facts(&runtime);
    assert!(
        facts.iter().any(|f| matches!(
            f,
            ProtocolFact::GrpcFinalStatusReceived {
                status: tina_runtime::GrpcStatusCode::Ok,
                ..
            }
        )),
        "expected a GrpcFinalStatusReceived(Ok) fact, got {facts:?}",
    );

    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    let _ = runtime.try_send(client.connection(), Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn oversized_request_is_rejected_before_the_wire() {
    let runtime = runtime();
    let (listener, addr) = start_server(&runtime);
    let target = Http2Target::H2c {
        authority: "grpc-test".into(),
        addr,
    };
    let conn = runtime
        .register_with_capacity::<Http2ClientConnection<TestShard>, _>(
            Http2ClientConnection::<TestShard>::new(target, Http2ClientLimits::default()),
            32,
        )
        .expect("register connection");
    runtime
        .try_send(conn, Http2ClientMsg::Begin)
        .expect("begin");
    // Tiny message cap so any message overflows on encode.
    let client = GrpcClient::new(
        conn,
        GrpcLimits {
            max_message_bytes: 1,
        },
    );

    let err = client
        .unary_request("/specimen.Counter/Increment", &CounterRequest { delta: 9 })
        .expect_err("oversized request must be rejected before the wire");
    assert!(
        matches!(err, GrpcError::EncodeTooLarge { .. }),
        "got {err:?}"
    );

    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    let _ = runtime.try_send(conn, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
}
