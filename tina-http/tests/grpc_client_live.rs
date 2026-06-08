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

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::TestShard;
use prost::Message;
use tina::prelude::*;
use tina::{CallContext, reply_to_request};
use tina_http::{
    GrpcBufferedServerStreamingResponse, GrpcBufferedStreamLimits, GrpcClient,
    GrpcClientStreamingRequest, GrpcError, GrpcLimits, GrpcRequest, GrpcRequestStream,
    GrpcResponse, GrpcRouter, GrpcServerStreamingResponse, GrpcStatus, GrpcStatusCode,
    GrpcStreamDecoder, GrpcStreamItem, GrpcStreamReply, GrpcStreamingCall, GrpcStreamingResponse,
    GrpcTarget, GrpcUnaryOutcome, Http2ClientConnection, Http2ClientLimits, Http2ClientMsg,
    Http2ClientOutcome, Http2ClientReply, Http2ConnectionReply, Http2Listener, Http2ListenerMsg,
    Http2ServerConfig, Http2Target, IterBodySource, ResponseChunkMsg, ResponseChunkReply,
    grpc_stream_finish, grpc_stream_message,
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
    let limits = GrpcLimits::default();
    let buffered_limits = GrpcBufferedStreamLimits::new(limits, 4, 1024);
    let one_message_buffered_limits = GrpcBufferedStreamLimits::new(limits, 1, 1024);
    // A small pool of server-streaming responses ("Watch" emits 1 then 2)
    // and bidi echo sources ("Chat" echoes each request +100), one
    // consumed per call.
    let watch_responses = Arc::new(Mutex::new(Vec::new()));
    for _ in 0..4 {
        let response = GrpcServerStreamingResponse::from_messages(
            runtime,
            vec![CounterReply { value: 1 }, CounterReply { value: 2 }],
            limits,
            16,
        )
        .expect("register watch response");
        watch_responses.lock().unwrap().push(response);
    }
    let watch_for_route = Arc::clone(&watch_responses);

    let chat_sources = Arc::new(Mutex::new(Vec::new()));
    for _ in 0..4 {
        let stream_slot = Arc::new(Mutex::new(None));
        let source = runtime
            .register_with_capacity::<StreamingEchoSource, Infallible>(
                StreamingEchoSource {
                    stream_slot: Arc::clone(&stream_slot),
                    pending: None,
                    eof: false,
                    limits,
                },
                16,
            )
            .expect("register echo source");
        chat_sources.lock().unwrap().push((stream_slot, source));
    }
    let chat_for_route = Arc::clone(&chat_sources);

    let router = GrpcRouter::<TestShard>::new(limits)
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
        )
        .server_streaming(
            "/specimen.Counter/Watch",
            move |_request: GrpcRequest<CounterRequest>| {
                watch_for_route
                    .lock()
                    .unwrap()
                    .pop()
                    .ok_or_else(|| GrpcStatus::new(GrpcStatusCode::ResourceExhausted))
            },
        )
        .server_streaming_buffered(
            "/specimen.Counter/WatchBuffered",
            move |_request: GrpcRequest<CounterRequest>| {
                GrpcBufferedServerStreamingResponse::from_messages(
                    vec![CounterReply { value: 3 }, CounterReply { value: 4 }],
                    buffered_limits,
                )
                .map_err(|_| GrpcStatus::new(GrpcStatusCode::Internal))
            },
        )
        .server_streaming_buffered(
            "/specimen.Counter/WatchBufferedTooMany",
            move |_request: GrpcRequest<CounterRequest>| {
                GrpcBufferedServerStreamingResponse::from_messages(
                    vec![CounterReply { value: 3 }, CounterReply { value: 4 }],
                    one_message_buffered_limits,
                )
                .map_err(|_| GrpcStatus::new(GrpcStatusCode::ResourceExhausted))
            },
        )
        .client_streaming(
            "/specimen.Counter/Sum",
            |request: GrpcClientStreamingRequest<CounterRequest>| {
                Ok(GrpcResponse::new(CounterReply {
                    value: request.messages.iter().map(|m| m.delta).sum(),
                }))
            },
        )
        .streaming(
            "/specimen.Counter/Chat",
            move |request: GrpcStreamingCall<CounterRequest, CounterReply>| {
                let (stream_slot, source) = chat_for_route
                    .lock()
                    .unwrap()
                    .pop()
                    .ok_or_else(|| GrpcStatus::new(GrpcStatusCode::ResourceExhausted))?;
                *stream_slot.lock().unwrap() = Some(request.requests);
                Ok(GrpcStreamingResponse::new(source))
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
    // Drive construction through the typed GrpcTarget so it stays
    // load-bearing (not just an exported shape).
    let target = GrpcTarget::h2c("grpc-test", addr);
    let conn = runtime
        .register_with_capacity::<Http2ClientConnection<TestShard>, _>(
            target.http2_connection::<TestShard>(),
            32,
        )
        .expect("register client connection");
    runtime
        .try_send(conn, Http2ClientMsg::Begin)
        .expect("begin client");
    GrpcClient::new(conn, target.limits())
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
fn unknown_method_returns_unimplemented_status() {
    // The Tina gRPC server answers an unrouted path with HTTP 200 +
    // grpc-status Unimplemented (a trailers-only response). The client
    // must surface that as a typed Status, reading grpc-status from the
    // header block.
    let runtime = runtime();
    let (listener, addr) = start_server(&runtime);
    let client = make_grpc_client(&runtime, addr);

    let outcome: GrpcUnaryOutcome<CounterReply> = call_unary(
        &runtime,
        &client,
        "/specimen.Counter/DoesNotExist",
        &CounterRequest { delta: 0 },
    );
    match outcome {
        GrpcUnaryOutcome::Status(status) => {
            assert_eq!(status.code, GrpcStatusCode::Unimplemented);
        }
        other => panic!("expected Status(Unimplemented), got {other:?}"),
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

/// Open a streamed-response call (server-streaming / bidi) and return the
/// stream id and the gRPC status carried in the response head (this
/// server emits the streaming status in the head's headers).
fn open_streamed(
    runtime: &ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
    client: &GrpcClient,
    open: Http2ClientMsg,
) -> (u32, Option<GrpcStatus>) {
    let reply = runtime
        .call_blocking(client.connection(), open, Duration::from_secs(5))
        .expect("open returns");
    match reply {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            stream_id,
            outcome: Http2ClientOutcome::ResponseStreaming { status, headers },
        }) => {
            assert_eq!(status.as_u16(), 200, "streamed head HTTP status");
            (stream_id, client.stream_head_status(&headers))
        }
        other => panic!("expected ResponseStreaming head, got {other:?}"),
    }
}

/// Pull a streamed gRPC response to completion, returning the decoded
/// messages and the final `End`-trailers status.
fn collect_grpc_stream<Resp: Message + Default + Clone>(
    runtime: &ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
    client: &GrpcClient,
    stream_id: u32,
) -> (Vec<Resp>, Option<GrpcStatus>) {
    let mut decoder = GrpcStreamDecoder::new(GrpcLimits::default());
    let mut messages = Vec::new();
    loop {
        let reply = runtime
            .call_blocking(
                client.connection(),
                Http2ClientMsg::ResponseNext { stream_id },
                Duration::from_secs(5),
            )
            .expect("response pull returns");
        let chunk = match reply {
            CallOutcome::Replied(Http2ClientReply::ResponseChunk { chunk, .. }) => chunk,
            other => panic!("expected ResponseChunk, got {other:?}"),
        };
        for item in client.decode_stream_chunk::<Resp>(&mut decoder, chunk) {
            match item {
                GrpcStreamItem::Message(message) => messages.push(message),
                GrpcStreamItem::Status(status) => return (messages, Some(status)),
                GrpcStreamItem::Transport(t) => panic!("transport teardown mid-stream: {t:?}"),
                GrpcStreamItem::Malformed(e) => panic!("malformed stream frame: {e:?}"),
                other => panic!("unexpected stream item: {other:?}"),
            }
        }
    }
}

#[test]
fn server_streaming_receives_all_messages_then_status() {
    // Server-streaming: one request, a pulled response of N messages
    // ending in a status. The Watch route emits CounterReply{1}, {2}.
    let runtime = runtime();
    let (listener, addr) = start_server(&runtime);
    let client = make_grpc_client(&runtime, addr);

    let open = client
        .server_streaming_request("/specimen.Counter/Watch", &CounterRequest { delta: 0 })
        .expect("encode server-streaming request");
    // The streaming gRPC status arrives in the End trailers, not the head
    // (the head only carries the HTTP 200 + content-type).
    let (stream_id, _head_status) = open_streamed(&runtime, &client, open);

    let (messages, end_status): (Vec<CounterReply>, _) =
        collect_grpc_stream(&runtime, &client, stream_id);
    assert_eq!(
        messages.iter().map(|m| m.value).collect::<Vec<_>>(),
        vec![1, 2],
        "all server-streamed messages received in order"
    );
    assert_eq!(
        end_status
            .expect("server-streaming ends with a status")
            .code,
        GrpcStatusCode::Ok
    );

    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    let _ = runtime.try_send(client.connection(), Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn buffered_server_streaming_receives_all_messages_then_status() {
    let runtime = runtime();
    let (listener, addr) = start_server(&runtime);
    let client = make_grpc_client(&runtime, addr);

    let open = client
        .server_streaming_request(
            "/specimen.Counter/WatchBuffered",
            &CounterRequest { delta: 0 },
        )
        .expect("encode buffered server-streaming request");
    let (stream_id, _head_status) = open_streamed(&runtime, &client, open);

    let (messages, end_status): (Vec<CounterReply>, _) =
        collect_grpc_stream(&runtime, &client, stream_id);
    assert_eq!(
        messages.iter().map(|m| m.value).collect::<Vec<_>>(),
        vec![3, 4],
        "all buffered server-streamed messages received in order"
    );
    assert_eq!(
        end_status
            .expect("buffered server-streaming ends with a status")
            .code,
        GrpcStatusCode::Ok
    );

    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    let _ = runtime.try_send(client.connection(), Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn buffered_server_streaming_bound_failure_returns_resource_exhausted() {
    let runtime = runtime();
    let (listener, addr) = start_server(&runtime);
    let client = make_grpc_client(&runtime, addr);

    let open = client
        .server_streaming_request(
            "/specimen.Counter/WatchBufferedTooMany",
            &CounterRequest { delta: 0 },
        )
        .expect("encode bounded buffered server-streaming request");
    let (stream_id, _head_status) = open_streamed(&runtime, &client, open);

    let (messages, end_status): (Vec<CounterReply>, _) =
        collect_grpc_stream(&runtime, &client, stream_id);
    assert!(
        messages.is_empty(),
        "bounded construction failure must not stream partial messages"
    );
    assert_eq!(
        end_status
            .expect("bounded construction failure returns a gRPC status")
            .code,
        GrpcStatusCode::ResourceExhausted
    );

    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    let _ = runtime.try_send(client.connection(), Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn client_streaming_sends_multiple_messages_then_receives_status() {
    // Client-streaming: a streamed request body of several messages, one
    // buffered response message + status. The Sum route adds the deltas.
    let runtime = runtime();
    let (listener, addr) = start_server(&runtime);
    let client = make_grpc_client(&runtime, addr);

    let frames: Vec<Vec<u8>> = [10_u64, 20, 30]
        .iter()
        .map(|&delta| {
            client
                .frame(&CounterRequest { delta })
                .expect("frame request")
        })
        .collect();
    let source = IterBodySource::<TestShard>::register(&runtime, frames.into_iter(), 16)
        .expect("register request source");

    let submit = client
        .client_streaming_request("/specimen.Counter/Sum", source)
        .expect("build client-streaming request");
    let reply = runtime
        .call_blocking(client.connection(), submit, Duration::from_secs(5))
        .expect("call returns");
    let outcome: GrpcUnaryOutcome<CounterReply> = match reply {
        CallOutcome::Replied(reply) => client.unary_outcome_from_reply(reply),
        other => panic!("expected Replied, got {other:?}"),
    };
    match outcome {
        GrpcUnaryOutcome::Ok(reply) => assert_eq!(reply.value, 60, "sum of streamed deltas"),
        other => panic!("expected Ok(60), got {other:?}"),
    }

    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    let _ = runtime.try_send(client.connection(), Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn bidi_request_and_response_streams_progress_independently() {
    // Bidi: a streamed request body and a pulled response. The Chat route
    // echoes each request message back as `delta + 100`, so receiving
    // [101, 102, 103] proves both directions ran over one stream.
    let runtime = runtime();
    let (listener, addr) = start_server(&runtime);
    let client = make_grpc_client(&runtime, addr);

    let frames: Vec<Vec<u8>> = [1_u64, 2, 3]
        .iter()
        .map(|&delta| {
            client
                .frame(&CounterRequest { delta })
                .expect("frame request")
        })
        .collect();
    let source = IterBodySource::<TestShard>::register(&runtime, frames.into_iter(), 16)
        .expect("register request source");

    let open = client
        .bidi_request("/specimen.Counter/Chat", source)
        .expect("build bidi request");
    let (stream_id, _head_status) = open_streamed(&runtime, &client, open);
    let (messages, _end_status): (Vec<CounterReply>, _) =
        collect_grpc_stream(&runtime, &client, stream_id);
    assert_eq!(
        messages.iter().map(|m| m.value).collect::<Vec<_>>(),
        vec![101, 102, 103],
        "each streamed request echoed back as a streamed response"
    );

    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    let _ = runtime.try_send(client.connection(), Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
}

/// Bidi echo source ported from the gRPC server live tests: pulls request
/// messages and replies each as `delta + 100`, finishing on request EOF.
struct StreamingEchoSource {
    stream_slot: Arc<Mutex<Option<GrpcRequestStream<CounterRequest>>>>,
    pending: Option<tina::RequestContext<ResponseChunkReply>>,
    eof: bool,
    limits: GrpcLimits,
}

impl StreamingEchoSource {
    fn finish_with_status(&mut self, status: GrpcStatus) -> ResponseChunkReply {
        self.eof = true;
        grpc_stream_finish(status)
    }

    fn reply_for_message(&mut self, request: CounterRequest) -> ResponseChunkReply {
        grpc_stream_message(
            &CounterReply {
                value: request.delta + 100,
            },
            self.limits,
        )
        .unwrap_or_else(|_| {
            self.finish_with_status(GrpcStatus::new(GrpcStatusCode::ResourceExhausted))
        })
    }

    fn pull_request(&self) -> Effect<Self> {
        self.stream_slot
            .lock()
            .unwrap()
            .as_ref()
            .expect("request stream installed")
            .pull_next_effect(Duration::from_secs(2))
    }

    fn handle_request_chunk_outcome(
        &mut self,
        outcome: tina_runtime::CallOutcome<Http2ConnectionReply>,
    ) -> Effect<Self> {
        let Some(pending) = self.pending.take() else {
            return noop();
        };
        let reply = {
            let mut guard = self.stream_slot.lock().unwrap();
            guard
                .as_mut()
                .expect("request stream installed")
                .accept_http2_outcome(outcome)
        };
        self.reply_to_stream_result(pending, reply)
    }

    fn reply_to_stream_result(
        &mut self,
        pending: tina::RequestContext<ResponseChunkReply>,
        reply: GrpcStreamReply<CounterRequest>,
    ) -> Effect<Self> {
        match reply {
            GrpcStreamReply::Message(request) => {
                reply_to_request(pending, self.reply_for_message(request))
            }
            GrpcStreamReply::NeedMore => {
                self.pending = Some(pending);
                self.pull_request()
            }
            GrpcStreamReply::Eof => {
                self.eof = true;
                reply_to_request(pending, ResponseChunkReply::Eof)
            }
            GrpcStreamReply::Status(status) => {
                reply_to_request(pending, self.finish_with_status(status))
            }
            GrpcStreamReply::Cancelled => reply_to_request(
                pending,
                self.finish_with_status(GrpcStatus::new(GrpcStatusCode::Cancelled)),
            ),
            GrpcStreamReply::DeadlineExceeded => reply_to_request(
                pending,
                self.finish_with_status(GrpcStatus::new(GrpcStatusCode::DeadlineExceeded)),
            ),
        }
    }
}

impl Isolate for StreamingEchoSource {
    tina::isolate_types! {
        message: ResponseChunkMsg,
        reply: ResponseChunkReply,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: tina_runtime::RuntimeCall<ResponseChunkMsg>,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: ResponseChunkMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ResponseChunkMsg::Cancel => stop(),
            ResponseChunkMsg::Next => reply(ResponseChunkReply::Eof),
            ResponseChunkMsg::Http2RequestChunk(outcome) => {
                self.handle_request_chunk_outcome(outcome)
            }
        }
    }

    fn handle_call(&mut self, msg: ResponseChunkMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            ResponseChunkMsg::Cancel => stop(),
            ResponseChunkMsg::Next => {
                if self.eof {
                    return call.reply(ResponseChunkReply::Eof);
                }
                let reply = {
                    let mut guard = self.stream_slot.lock().unwrap();
                    guard
                        .as_mut()
                        .expect("request stream installed")
                        .next_buffered()
                };
                if !matches!(reply, GrpcStreamReply::NeedMore) {
                    return self.reply_to_stream_result(call.into_request_context(), reply);
                }
                self.pending = Some(call.into_request_context());
                self.pull_request()
            }
            ResponseChunkMsg::Http2RequestChunk(outcome) => {
                self.handle_request_chunk_outcome(outcome)
            }
        }
    }
}
