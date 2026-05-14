mod common;

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use common::TestShard;
use prost::Message;
use tina::prelude::*;
use tina_http::{
    GrpcLimits, GrpcRequest, GrpcResponse, GrpcRouter, GrpcStatus, GrpcStatusCode, Http2Listener,
    Http2ListenerMsg, Http2ServerConfig, HttpRequest, HttpResponse, grpc_unary_call_h2c,
};
use tina_runtime::{
    DefaultThreadedMailboxFactory, RuntimeEvent, RuntimeEventKind, ThreadedRuntime,
    ThreadedRuntimeConfig, sleep,
};

const CLIENT_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const FRAME_DATA: u8 = 0x0;
const FRAME_HEADERS: u8 = 0x1;
const FRAME_RST_STREAM: u8 = 0x3;
const FRAME_SETTINGS: u8 = 0x4;
const FLAG_ACK: u8 = 0x1;
const FLAG_END_STREAM: u8 = 0x1;
const FLAG_END_HEADERS: u8 = 0x4;

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

#[derive(Clone, PartialEq, Message)]
struct BlobRequest {
    #[prost(bytes, tag = "1")]
    bytes: Vec<u8>,
}

struct GrpcHarness {
    addr: SocketAddr,
    runtime: Option<ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>>,
    listener: Address<Http2ListenerMsg>,
}

impl GrpcHarness {
    fn start_router(config: Http2ServerConfig, limits: GrpcLimits) -> Self {
        let runtime = runtime();
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
                        "not here\n100%",
                    ))
                },
            )
            .unary(
                "/specimen.Counter/Big",
                |_request: GrpcRequest<CounterRequest>| {
                    Ok(GrpcResponse::new(CounterReply { value: u64::MAX }))
                },
            )
            .unary(
                "/specimen.Counter/BlobLen",
                |request: GrpcRequest<BlobRequest>| {
                    Ok(GrpcResponse::new(CounterReply {
                        value: request.message.bytes.len() as u64,
                    }))
                },
            );
        let service = runtime
            .register_with_capacity::<GrpcRouter<TestShard>, _>(router, 16)
            .expect("register grpc router");
        Self::start_with_service(runtime, service, config)
    }

    fn start_hanging(config: Http2ServerConfig) -> Self {
        let runtime = runtime();
        let service = runtime
            .register_with_capacity::<HangingGrpc, _>(HangingGrpc, 16)
            .expect("register hanging grpc");
        Self::start_with_service(runtime, service, config)
    }

    fn start_with_service<M>(
        runtime: ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
        service: Address<M, HttpResponse>,
        config: Http2ServerConfig,
    ) -> Self
    where
        M: From<HttpRequest> + Send + 'static,
    {
        let listener = runtime
            .register_with_capacity::<Http2Listener<TestShard, M>, _>(
                Http2Listener::<TestShard, M>::new("127.0.0.1:0".parse().unwrap(), service, config),
                config.listener_mailbox_capacity,
            )
            .expect("register listener");
        let bound = runtime.observe_next_bound();
        runtime
            .try_send(listener, Http2ListenerMsg::Start)
            .expect("start listener");
        let addr = bound
            .wait(Duration::from_secs(2))
            .expect("listener publishes bound address");
        Self {
            addr,
            runtime: Some(runtime),
            listener,
        }
    }

    fn shutdown(mut self) {
        let _ = self.shutdown_events();
    }

    fn shutdown_events(&mut self) -> Vec<RuntimeEvent> {
        if let Some(runtime) = self.runtime.take() {
            let _ = runtime.try_send(self.listener, Http2ListenerMsg::Stop);
            runtime.shutdown().unwrap_or_default()
        } else {
            Vec::new()
        }
    }
}

impl Drop for GrpcHarness {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            let _ = runtime.try_send(self.listener, Http2ListenerMsg::Stop);
            let _ = runtime.shutdown();
        }
    }
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

struct HangingGrpc;

#[derive(Debug)]
enum HangingMsg {
    Request,
    Done(tina::RequestContext<HttpResponse>),
}

impl From<HttpRequest> for HangingMsg {
    fn from(_value: HttpRequest) -> Self {
        Self::Request
    }
}

impl Isolate for HangingGrpc {
    tina::isolate_types! {
        message: HangingMsg,
        reply: HttpResponse,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: tina_runtime::RuntimeCall<HangingMsg>,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: HangingMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            HangingMsg::Request => noop(),
            HangingMsg::Done(request) => reply_to_request(request, HttpResponse::ok()),
        }
    }

    fn handle_call(&mut self, msg: HangingMsg, call: tina::CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            HangingMsg::Request => {
                let request = call.into_request_context();
                sleep(Duration::from_millis(250)).reply(move |_| HangingMsg::Done(request))
            }
            HangingMsg::Done(_) => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

#[derive(Debug)]
struct Frame {
    ty: u8,
    flags: u8,
    stream_id: u32,
    payload: Vec<u8>,
}

fn connect_h2(addr: SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .expect("write timeout");
    stream.write_all(CLIENT_PREFACE).expect("write preface");
    write_frame(&mut stream, FRAME_SETTINGS, 0, 0, &[]);
    let mut saw_settings = false;
    let mut saw_ack = false;
    for _ in 0..4 {
        let frame = read_frame(&mut stream);
        if frame.ty == FRAME_SETTINGS && frame.flags & FLAG_ACK == 0 {
            saw_settings = true;
            write_frame(&mut stream, FRAME_SETTINGS, FLAG_ACK, 0, &[]);
        } else if frame.ty == FRAME_SETTINGS && frame.flags & FLAG_ACK != 0 {
            saw_ack = true;
        }
        if saw_settings && saw_ack {
            return stream;
        }
    }
    panic!("settings handshake failed");
}

fn write_frame(stream: &mut TcpStream, ty: u8, flags: u8, stream_id: u32, payload: &[u8]) {
    let len = payload.len();
    let mut out = Vec::with_capacity(9 + len);
    out.push(((len >> 16) & 0xff) as u8);
    out.push(((len >> 8) & 0xff) as u8);
    out.push((len & 0xff) as u8);
    out.push(ty);
    out.push(flags);
    out.extend_from_slice(&(stream_id & 0x7fff_ffff).to_be_bytes());
    out.extend_from_slice(payload);
    stream.write_all(&out).expect("write frame");
    stream.flush().expect("flush frame");
}

fn read_frame(stream: &mut TcpStream) -> Frame {
    let mut head = [0_u8; 9];
    stream.read_exact(&mut head).expect("read frame head");
    let len = ((head[0] as usize) << 16) | ((head[1] as usize) << 8) | head[2] as usize;
    let mut payload = vec![0_u8; len];
    stream.read_exact(&mut payload).expect("read frame payload");
    let mut sid = [0_u8; 4];
    sid.copy_from_slice(&head[5..9]);
    Frame {
        ty: head[3],
        flags: head[4],
        stream_id: u32::from_be_bytes(sid) & 0x7fff_ffff,
        payload,
    }
}

fn request_headers(path: &str, content_type: &str) -> Vec<u8> {
    request_headers_with_encoding(path, content_type, None)
}

fn request_headers_with_encoding(
    path: &str,
    content_type: &str,
    grpc_encoding: Option<&str>,
) -> Vec<u8> {
    let mut block = Vec::new();
    literal(":method", "POST", &mut block);
    literal(":scheme", "http", &mut block);
    literal(":path", path, &mut block);
    literal(":authority", "localhost", &mut block);
    literal("content-type", content_type, &mut block);
    literal("te", "trailers", &mut block);
    if let Some(encoding) = grpc_encoding {
        literal("grpc-encoding", encoding, &mut block);
    }
    block
}

fn literal(name: &str, value: &str, out: &mut Vec<u8>) {
    out.push(0);
    hpack_string(name, out);
    hpack_string(value, out);
}

fn hpack_string(value: &str, out: &mut Vec<u8>) {
    assert!(value.len() < 127);
    out.push(value.len() as u8);
    out.extend_from_slice(value.as_bytes());
}

fn grpc_body<M: Message>(message: &M) -> Vec<u8> {
    tina_http::encode_grpc_message(message, GrpcLimits::default()).expect("encode grpc body")
}

fn raw_grpc_status(
    stream: &mut TcpStream,
    stream_id: u32,
    path: &str,
    content_type: &str,
    body: &[u8],
) -> GrpcStatusCode {
    write_frame(
        stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        stream_id,
        &request_headers(path, content_type),
    );
    write_frame(stream, FRAME_DATA, FLAG_END_STREAM, stream_id, body);
    read_status(stream, stream_id)
}

fn raw_grpc_status_with_encoding(
    stream: &mut TcpStream,
    stream_id: u32,
    path: &str,
    content_type: &str,
    grpc_encoding: Option<&str>,
    body: &[u8],
) -> GrpcStatusCode {
    write_frame(
        stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        stream_id,
        &request_headers_with_encoding(path, content_type, grpc_encoding),
    );
    write_frame(stream, FRAME_DATA, FLAG_END_STREAM, stream_id, body);
    read_status(stream, stream_id)
}

fn read_status(stream: &mut TcpStream, stream_id: u32) -> GrpcStatusCode {
    for _ in 0..16 {
        let frame = read_frame(stream);
        if frame.stream_id != stream_id {
            continue;
        }
        if frame.ty == FRAME_HEADERS && frame.flags & FLAG_END_STREAM != 0 {
            return decode_status(&frame.payload);
        }
    }
    panic!("missing grpc status trailers");
}

fn read_until_rst(stream: &mut TcpStream, stream_id: u32) -> Frame {
    for _ in 0..16 {
        let frame = read_frame(stream);
        if frame.ty == FRAME_RST_STREAM && frame.stream_id == stream_id {
            return frame;
        }
    }
    panic!("missing rst stream");
}

fn decode_status(block: &[u8]) -> GrpcStatusCode {
    let mut cursor = 0;
    while cursor < block.len() {
        assert_eq!(block[cursor], 0);
        cursor += 1;
        let (name, used) = read_hpack_string(&block[cursor..]);
        cursor += used;
        let (value, used) = read_hpack_string(&block[cursor..]);
        cursor += used;
        if name == "grpc-status" {
            return GrpcStatusCode::from_u16(value.parse().expect("status number"));
        }
    }
    panic!("no grpc-status");
}

fn read_hpack_string(input: &[u8]) -> (String, usize) {
    let len = input[0] as usize;
    let end = 1 + len;
    (std::str::from_utf8(&input[1..end]).unwrap().to_owned(), end)
}

#[test]
fn grpc_happy_unary_request_response() {
    let harness = GrpcHarness::start_router(Http2ServerConfig::default(), GrpcLimits::default());
    let reply: CounterReply = grpc_unary_call_h2c(
        harness.addr,
        "/specimen.Counter/Increment",
        &CounterRequest { delta: 41 },
        Duration::from_secs(2),
        GrpcLimits::default(),
    )
    .expect("grpc unary reply");
    assert_eq!(reply.value, 42);
    harness.shutdown();
}

#[test]
fn grpc_large_unary_request_splits_http2_data_frames() {
    let harness = GrpcHarness::start_router(Http2ServerConfig::default(), GrpcLimits::default());
    let reply: CounterReply = grpc_unary_call_h2c(
        harness.addr,
        "/specimen.Counter/BlobLen",
        &BlobRequest {
            bytes: vec![7; 20_000],
        },
        Duration::from_secs(2),
        GrpcLimits::default(),
    )
    .expect("large grpc unary reply");
    assert_eq!(reply.value, 20_000);
    harness.shutdown();
}

#[test]
fn grpc_typed_status_propagates_in_trailers() {
    let harness = GrpcHarness::start_router(Http2ServerConfig::default(), GrpcLimits::default());
    let error = grpc_unary_call_h2c::<_, CounterReply>(
        harness.addr,
        "/specimen.Counter/Status",
        &CounterRequest { delta: 1 },
        Duration::from_secs(2),
        GrpcLimits::default(),
    )
    .expect_err("typed status");
    match error {
        tina_http::GrpcError::Status(GrpcStatus { code, message }) => {
            assert_eq!(code, GrpcStatusCode::NotFound);
            assert_eq!(message.as_deref(), Some("not here\n100%"));
        }
        other => panic!("expected typed status, got {other:?}"),
    }
    harness.shutdown();
}

#[test]
fn grpc_unknown_method_is_unimplemented() {
    let harness = GrpcHarness::start_router(Http2ServerConfig::default(), GrpcLimits::default());
    let mut stream = connect_h2(harness.addr);
    let status = raw_grpc_status(
        &mut stream,
        1,
        "/specimen.Counter/Missing",
        "application/grpc+proto",
        &grpc_body(&CounterRequest { delta: 1 }),
    );
    assert_eq!(status, GrpcStatusCode::Unimplemented);
    harness.shutdown();
}

#[test]
fn grpc_zero_message_body_is_invalid_argument() {
    let harness = GrpcHarness::start_router(Http2ServerConfig::default(), GrpcLimits::default());
    let mut stream = connect_h2(harness.addr);
    let status = raw_grpc_status(
        &mut stream,
        1,
        "/specimen.Counter/Increment",
        "application/grpc+proto",
        &[],
    );
    assert_eq!(status, GrpcStatusCode::InvalidArgument);
    harness.shutdown();
}

#[test]
fn grpc_bad_request_decode_path_is_invalid_argument() {
    let harness = GrpcHarness::start_router(Http2ServerConfig::default(), GrpcLimits::default());
    let mut stream = connect_h2(harness.addr);
    let status = raw_grpc_status(
        &mut stream,
        1,
        "/specimen.Counter/Increment",
        "application/grpc+proto",
        &[0, 0, 0, 0, 10, 1],
    );
    assert_eq!(status, GrpcStatusCode::InvalidArgument);
    harness.shutdown();
}

#[test]
fn grpc_two_messages_on_unary_is_invalid_argument() {
    let harness = GrpcHarness::start_router(Http2ServerConfig::default(), GrpcLimits::default());
    let mut stream = connect_h2(harness.addr);
    let mut body = grpc_body(&CounterRequest { delta: 1 });
    body.extend_from_slice(&grpc_body(&CounterRequest { delta: 2 }));
    let status = raw_grpc_status(
        &mut stream,
        1,
        "/specimen.Counter/Increment",
        "application/grpc+proto",
        &body,
    );
    assert_eq!(status, GrpcStatusCode::InvalidArgument);
    harness.shutdown();
}

#[test]
fn grpc_compressed_request_rejects_but_identity_encoding_is_ok() {
    let harness = GrpcHarness::start_router(Http2ServerConfig::default(), GrpcLimits::default());
    let mut stream = connect_h2(harness.addr);
    let status = raw_grpc_status_with_encoding(
        &mut stream,
        1,
        "/specimen.Counter/Increment",
        "application/grpc+proto; charset=utf-8",
        Some("identity"),
        &grpc_body(&CounterRequest { delta: 1 }),
    );
    assert_eq!(status, GrpcStatusCode::Ok);

    let mut compressed = grpc_body(&CounterRequest { delta: 1 });
    compressed[0] = 1;
    let status = raw_grpc_status_with_encoding(
        &mut stream,
        3,
        "/specimen.Counter/Increment",
        "application/grpc+proto",
        Some("gzip"),
        &compressed,
    );
    assert_eq!(status, GrpcStatusCode::Unimplemented);
    harness.shutdown();
}

#[test]
fn grpc_body_message_cap_is_resource_exhausted() {
    let harness = GrpcHarness::start_router(
        Http2ServerConfig::default(),
        GrpcLimits {
            max_message_bytes: 1,
        },
    );
    let mut stream = connect_h2(harness.addr);
    let status = raw_grpc_status(
        &mut stream,
        1,
        "/specimen.Counter/Increment",
        "application/grpc+proto",
        &grpc_body(&CounterRequest { delta: 300 }),
    );
    assert_eq!(status, GrpcStatusCode::ResourceExhausted);
    harness.shutdown();
}

#[test]
fn grpc_response_message_cap_is_resource_exhausted() {
    let harness = GrpcHarness::start_router(
        Http2ServerConfig::default(),
        GrpcLimits {
            max_message_bytes: 2,
        },
    );
    let error = grpc_unary_call_h2c::<_, CounterReply>(
        harness.addr,
        "/specimen.Counter/Big",
        &CounterRequest { delta: 1 },
        Duration::from_secs(2),
        GrpcLimits {
            max_message_bytes: 32,
        },
    )
    .expect_err("response cap status");
    assert!(matches!(
        error,
        tina_http::GrpcError::Status(GrpcStatus {
            code: GrpcStatusCode::ResourceExhausted,
            ..
        })
    ));
    harness.shutdown();
}

#[test]
fn grpc_http2_body_cap_resets_before_service_decode() {
    let harness = GrpcHarness::start_router(
        Http2ServerConfig {
            limits: tina_http::Http2Limits {
                max_body_bytes: 4,
                ..tina_http::Http2Limits::default()
            },
            ..Http2ServerConfig::default()
        },
        GrpcLimits::default(),
    );
    let mut stream = connect_h2(harness.addr);
    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("/specimen.Counter/Increment", "application/grpc+proto"),
    );
    write_frame(
        &mut stream,
        FRAME_DATA,
        FLAG_END_STREAM,
        1,
        &grpc_body(&CounterRequest { delta: 1 }),
    );
    assert_eq!(read_until_rst(&mut stream, 1).stream_id, 1);
    harness.shutdown();
}

#[test]
fn grpc_timeout_maps_to_deadline_exceeded() {
    let harness = GrpcHarness::start_hanging(Http2ServerConfig {
        service_call_timeout: Duration::from_millis(50),
        ..Http2ServerConfig::default()
    });
    let mut stream = connect_h2(harness.addr);
    let status = raw_grpc_status(
        &mut stream,
        1,
        "/specimen.Counter/Hang",
        "application/grpc+proto",
        &grpc_body(&CounterRequest { delta: 1 }),
    );
    assert_eq!(status, GrpcStatusCode::DeadlineExceeded);
    harness.shutdown();
}

#[test]
fn grpc_peer_reset_cancels_stream_without_killing_connection() {
    let harness = GrpcHarness::start_router(Http2ServerConfig::default(), GrpcLimits::default());
    let mut stream = connect_h2(harness.addr);
    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("/specimen.Counter/Increment", "application/grpc+proto"),
    );
    write_frame(&mut stream, FRAME_RST_STREAM, 0, 1, &0_u32.to_be_bytes());
    let status = raw_grpc_status(
        &mut stream,
        3,
        "/specimen.Counter/Increment",
        "application/grpc+proto",
        &grpc_body(&CounterRequest { delta: 2 }),
    );
    assert_eq!(status, GrpcStatusCode::Ok);
    harness.shutdown();
}

#[test]
fn grpc_peer_reset_cancels_accepted_service_call() {
    let mut harness = GrpcHarness::start_hanging(Http2ServerConfig {
        service_call_timeout: Duration::from_secs(1),
        ..Http2ServerConfig::default()
    });
    let mut stream = connect_h2(harness.addr);
    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("/specimen.Counter/Hang", "application/grpc+proto"),
    );
    write_frame(
        &mut stream,
        FRAME_DATA,
        FLAG_END_STREAM,
        1,
        &grpc_body(&CounterRequest { delta: 1 }),
    );
    write_frame(&mut stream, FRAME_RST_STREAM, 0, 1, &0_u32.to_be_bytes());
    std::thread::sleep(Duration::from_millis(20));
    let events = harness.shutdown_events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event.kind(), RuntimeEventKind::CallCancelled { .. })),
        "peer reset must cancel the accepted service call; events: {events:?}"
    );
}

#[test]
fn grpc_concurrent_stream_cap_uses_http2_reset() {
    let harness = GrpcHarness::start_router(
        Http2ServerConfig {
            limits: tina_http::Http2Limits {
                max_concurrent_streams: 1,
                ..tina_http::Http2Limits::default()
            },
            ..Http2ServerConfig::default()
        },
        GrpcLimits::default(),
    );
    let mut stream = connect_h2(harness.addr);
    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("/specimen.Counter/Increment", "application/grpc+proto"),
    );
    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        3,
        &request_headers("/specimen.Counter/Increment", "application/grpc+proto"),
    );
    assert_eq!(read_until_rst(&mut stream, 3).stream_id, 3);
    write_frame(&mut stream, FRAME_RST_STREAM, 0, 1, &0_u32.to_be_bytes());
    harness.shutdown();
}

#[test]
fn grpc_content_type_mismatch_rejects_as_invalid_argument() {
    let harness = GrpcHarness::start_router(Http2ServerConfig::default(), GrpcLimits::default());
    let mut stream = connect_h2(harness.addr);
    let status = raw_grpc_status(
        &mut stream,
        1,
        "/specimen.Counter/Increment",
        "application/json",
        &grpc_body(&CounterRequest { delta: 1 }),
    );
    assert_eq!(status, GrpcStatusCode::InvalidArgument);
    harness.shutdown();
}
