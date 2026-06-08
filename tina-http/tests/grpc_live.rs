mod common;

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::TestShard;
use prost::Message;
use tina::prelude::*;
use tina_http::{
    GrpcBufferedServerStreamingResponse, GrpcBufferedStreamLimits, GrpcClientStreamingRequest,
    GrpcLimits, GrpcRawStreamingRequest, GrpcRawStreamingResponse, GrpcRequest, GrpcRequestStream,
    GrpcResponse, GrpcRouter, GrpcServerStreamingResponse, GrpcStatus, GrpcStatusCode,
    GrpcStreamReply, GrpcStreamingCall, GrpcStreamingResponse, Http2Limits, Http2Listener,
    Http2ListenerMsg, Http2ServerConfig, Http2ServiceMessage, HttpRequest, HttpResponse,
    grpc_stream_finish, grpc_stream_message, grpc_unary_call_h2c_blocking,
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
        let streaming_sources = Arc::new(Mutex::new(Vec::new()));
        for _ in 0..8 {
            let stream_slot = Arc::new(Mutex::new(None));
            let source = runtime
                .register_with_capacity::<StreamingEchoSource, Infallible>(
                    StreamingEchoSource {
                        stream_slot: Arc::clone(&stream_slot),
                        pending: None,
                        eof: false,
                        limits,
                        received_cancel: Arc::new(AtomicBool::new(false)),
                    },
                    16,
                )
                .expect("register streaming source");
            streaming_sources
                .lock()
                .expect("streaming sources")
                .push((stream_slot, source));
        }
        let streaming_sources_for_route = Arc::clone(&streaming_sources);
        let buffered_limits = GrpcBufferedStreamLimits::new(limits, 4, 1024);
        let watch_responses = Arc::new(Mutex::new(Vec::new()));
        for _ in 0..8 {
            let watch_response = GrpcServerStreamingResponse::from_messages(
                &runtime,
                vec![CounterReply { value: 1 }, CounterReply { value: 2 }],
                GrpcLimits::default(),
                16,
            )
            .expect("register watch response");
            watch_responses
                .lock()
                .expect("watch responses")
                .push(watch_response);
        }
        let watch_responses_for_route = Arc::clone(&watch_responses);
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
            )
            .server_streaming(
                "/specimen.Counter/Watch",
                move |_request: GrpcRequest<CounterRequest>| {
                    watch_responses_for_route
                        .lock()
                        .expect("watch responses")
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
            .client_streaming(
                "/specimen.Counter/Sum",
                |request: GrpcClientStreamingRequest<CounterRequest>| {
                    Ok(GrpcResponse::new(CounterReply {
                        value: request.messages.iter().map(|message| message.delta).sum(),
                    }))
                },
            )
            .streaming(
                "/specimen.Counter/Chat",
                move |request: GrpcStreamingCall<CounterRequest, CounterReply>| {
                    let (stream_slot, source) = streaming_sources_for_route
                        .lock()
                        .expect("streaming sources")
                        .pop()
                        .ok_or_else(|| GrpcStatus::new(GrpcStatusCode::ResourceExhausted))?;
                    *stream_slot.lock().expect("streaming stream slot") = Some(request.requests);
                    Ok(GrpcStreamingResponse::new(source))
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
        M: Http2ServiceMessage,
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

    fn wait_for_event(
        &self,
        timeout: Duration,
        mut predicate: impl FnMut(&RuntimeEventKind) -> bool,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(runtime) = self.runtime.as_ref() {
                let trace = runtime.trace();
                if trace.events().iter().any(|event| predicate(&event.kind())) {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
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

struct CancelRecordingSource {
    chunk: Vec<u8>,
    received_next: Arc<AtomicBool>,
    received_cancel: Arc<AtomicBool>,
}

impl Isolate for CancelRecordingSource {
    tina::isolate_types! {
        message: tina_http::ResponseChunkMsg,
        reply: tina_http::ResponseChunkReply,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: Infallible,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: tina_http::ResponseChunkMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            tina_http::ResponseChunkMsg::Next => {
                self.received_next.store(true, Ordering::Release);
                reply(tina_http::ResponseChunkReply::Chunk(self.chunk.clone()))
            }
            tina_http::ResponseChunkMsg::Cancel => {
                self.received_cancel.store(true, Ordering::Release);
                stop()
            }
            tina_http::ResponseChunkMsg::Http2RequestChunk(_) => noop(),
        }
    }

    fn handle_call(
        &mut self,
        msg: tina_http::ResponseChunkMsg,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        match msg {
            tina_http::ResponseChunkMsg::Next => {
                self.received_next.store(true, Ordering::Release);
                call.reply(tina_http::ResponseChunkReply::Chunk(self.chunk.clone()))
            }
            tina_http::ResponseChunkMsg::Cancel => {
                self.received_cancel.store(true, Ordering::Release);
                stop()
            }
            tina_http::ResponseChunkMsg::Http2RequestChunk(_) => {
                call.reply(tina_http::ResponseChunkReply::Eof)
            }
        }
    }
}

struct StreamingEchoSource {
    stream_slot: Arc<Mutex<Option<GrpcRequestStream<CounterRequest>>>>,
    pending: Option<tina::RequestContext<tina_http::ResponseChunkReply>>,
    eof: bool,
    limits: GrpcLimits,
    received_cancel: Arc<AtomicBool>,
}

impl StreamingEchoSource {
    fn finish_with_status(&mut self, status: GrpcStatus) -> tina_http::ResponseChunkReply {
        self.eof = true;
        grpc_stream_finish(status)
    }

    fn reply_for_message(&mut self, request: CounterRequest) -> tina_http::ResponseChunkReply {
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
            .expect("streaming stream slot")
            .as_ref()
            .expect("streaming request stream installed")
            .pull_next_effect(Duration::from_secs(2))
    }

    fn handle_request_chunk_outcome(
        &mut self,
        outcome: tina_runtime::CallOutcome<tina_http::Http2ConnectionReply>,
    ) -> Effect<Self> {
        let Some(pending) = self.pending.take() else {
            return noop();
        };
        let reply = {
            let mut guard = self.stream_slot.lock().expect("streaming stream slot");
            let requests = guard.as_mut().expect("streaming request stream installed");
            requests.accept_http2_outcome(outcome)
        };
        self.reply_to_stream_result(pending, reply)
    }

    fn reply_to_stream_result(
        &mut self,
        pending: tina::RequestContext<tina_http::ResponseChunkReply>,
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
                reply_to_request(pending, tina_http::ResponseChunkReply::Eof)
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
        message: tina_http::ResponseChunkMsg,
        reply: tina_http::ResponseChunkReply,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: tina_runtime::RuntimeCall<tina_http::ResponseChunkMsg>,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: tina_http::ResponseChunkMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            tina_http::ResponseChunkMsg::Cancel => {
                self.received_cancel.store(true, Ordering::Release);
                stop()
            }
            tina_http::ResponseChunkMsg::Next => reply(tina_http::ResponseChunkReply::Eof),
            tina_http::ResponseChunkMsg::Http2RequestChunk(outcome) => {
                self.handle_request_chunk_outcome(outcome)
            }
        }
    }

    fn handle_call(
        &mut self,
        msg: tina_http::ResponseChunkMsg,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        match msg {
            tina_http::ResponseChunkMsg::Cancel => {
                self.received_cancel.store(true, Ordering::Release);
                stop()
            }
            tina_http::ResponseChunkMsg::Next => {
                if self.eof {
                    return call.reply(tina_http::ResponseChunkReply::Eof);
                }
                let reply = {
                    let mut guard = self.stream_slot.lock().expect("streaming stream slot");
                    guard
                        .as_mut()
                        .expect("streaming request stream installed")
                        .next_buffered()
                };
                if !matches!(reply, GrpcStreamReply::NeedMore) {
                    return self.reply_to_stream_result(call.into_request_context(), reply);
                }
                self.pending = Some(call.into_request_context());
                self.pull_request()
            }
            tina_http::ResponseChunkMsg::Http2RequestChunk(outcome) => {
                self.handle_request_chunk_outcome(outcome)
            }
        }
    }
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
                sleep(Duration::from_millis(250)).then(move |_| HangingMsg::Done(request))
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

fn wait_for_atomic_flag(flag: &AtomicBool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if flag.load(Ordering::Acquire) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn request_headers(path: &str, content_type: &str) -> Vec<u8> {
    request_headers_with_encoding(path, content_type, None)
}

fn request_headers_with_content_length(path: &str, content_type: &str, len: usize) -> Vec<u8> {
    let mut block = request_headers(path, content_type);
    literal("content-length", &len.to_string(), &mut block);
    block
}

fn request_trailers() -> Vec<u8> {
    let mut block = Vec::new();
    literal("x-request-trailer", "not-supported", &mut block);
    block
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

fn read_body_and_status(stream: &mut TcpStream, stream_id: u32) -> (Vec<u8>, GrpcStatusCode) {
    let mut body = Vec::new();
    for _ in 0..512 {
        let frame = read_frame(stream);
        if frame.stream_id != stream_id {
            continue;
        }
        match frame.ty {
            FRAME_HEADERS if frame.flags & FLAG_END_STREAM != 0 => {
                return (body, decode_status(&frame.payload));
            }
            FRAME_HEADERS => {}
            FRAME_DATA => body.extend_from_slice(&frame.payload),
            FRAME_RST_STREAM => panic!("unexpected reset: {frame:?}"),
            _ => {}
        }
    }
    panic!("missing grpc response end");
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

fn decode_grpc_replies(body: &[u8]) -> Vec<CounterReply> {
    let mut cursor = 0;
    let mut replies = Vec::new();
    while cursor < body.len() {
        assert_eq!(body[cursor], 0);
        cursor += 1;
        let len = u32::from_be_bytes([
            body[cursor],
            body[cursor + 1],
            body[cursor + 2],
            body[cursor + 3],
        ]) as usize;
        cursor += 4;
        let end = cursor + len;
        replies.push(CounterReply::decode(&body[cursor..end]).expect("decode reply"));
        cursor = end;
    }
    replies
}

fn decode_one_grpc_reply(body: &[u8]) -> CounterReply {
    let replies = decode_grpc_replies(body);
    assert_eq!(replies.len(), 1);
    replies.into_iter().next().unwrap()
}

fn read_next_data_for_stream(stream: &mut TcpStream, stream_id: u32) -> Vec<u8> {
    for _ in 0..32 {
        let frame = read_frame(stream);
        match (frame.ty, frame.stream_id) {
            (FRAME_DATA, id) if id == stream_id => return frame.payload,
            (FRAME_HEADERS, id) if id == stream_id => {}
            (FRAME_RST_STREAM, id) if id == stream_id => panic!("unexpected reset: {frame:?}"),
            _ => {}
        }
    }
    panic!("missing data for stream {stream_id}");
}

fn read_statuses(stream: &mut TcpStream, stream_ids: &[u32]) -> Vec<(u32, GrpcStatusCode)> {
    let mut statuses = Vec::new();
    for _ in 0..64 {
        let frame = read_frame(stream);
        if frame.ty == FRAME_HEADERS
            && frame.flags & FLAG_END_STREAM != 0
            && stream_ids.contains(&frame.stream_id)
        {
            statuses.push((frame.stream_id, decode_status(&frame.payload)));
            if statuses.len() == stream_ids.len() {
                return statuses;
            }
        }
    }
    panic!("missing grpc statuses for {stream_ids:?}; got {statuses:?}");
}

#[test]
fn grpc_happy_unary_request_response() {
    let harness = GrpcHarness::start_router(Http2ServerConfig::default(), GrpcLimits::default());
    let reply: CounterReply = grpc_unary_call_h2c_blocking(
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
fn grpc_server_streaming_sends_messages_then_status_trailers() {
    let harness = GrpcHarness::start_router(Http2ServerConfig::default(), GrpcLimits::default());
    let mut stream = connect_h2(harness.addr);
    let body = grpc_body(&CounterRequest { delta: 0 });

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("/specimen.Counter/Watch", "application/grpc+proto"),
    );
    write_frame(&mut stream, FRAME_DATA, FLAG_END_STREAM, 1, &body);

    let (body, status) = read_body_and_status(&mut stream, 1);
    assert_eq!(status, GrpcStatusCode::Ok);
    let replies = decode_grpc_replies(&body);
    assert_eq!(
        replies.iter().map(|reply| reply.value).collect::<Vec<_>>(),
        vec![1, 2]
    );
    harness.shutdown();
}

#[test]
fn grpc_server_streaming_route_works_more_than_once() {
    let harness = GrpcHarness::start_router(Http2ServerConfig::default(), GrpcLimits::default());
    let mut stream = connect_h2(harness.addr);
    let body = grpc_body(&CounterRequest { delta: 0 });

    for stream_id in [1, 3] {
        write_frame(
            &mut stream,
            FRAME_HEADERS,
            FLAG_END_HEADERS,
            stream_id,
            &request_headers("/specimen.Counter/Watch", "application/grpc+proto"),
        );
        write_frame(&mut stream, FRAME_DATA, FLAG_END_STREAM, stream_id, &body);
        let (body, status) = read_body_and_status(&mut stream, stream_id);
        assert_eq!(status, GrpcStatusCode::Ok);
        assert_eq!(
            decode_grpc_replies(&body)
                .iter()
                .map(|reply| reply.value)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }
    harness.shutdown();
}

#[test]
fn grpc_buffered_server_streaming_sends_messages_then_status_trailers() {
    let harness = GrpcHarness::start_router(Http2ServerConfig::default(), GrpcLimits::default());
    let mut stream = connect_h2(harness.addr);
    let body = grpc_body(&CounterRequest { delta: 0 });

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("/specimen.Counter/WatchBuffered", "application/grpc+proto"),
    );
    write_frame(&mut stream, FRAME_DATA, FLAG_END_STREAM, 1, &body);

    let (body, status) = read_body_and_status(&mut stream, 1);
    assert_eq!(status, GrpcStatusCode::Ok);
    assert_eq!(
        decode_grpc_replies(&body)
            .iter()
            .map(|reply| reply.value)
            .collect::<Vec<_>>(),
        vec![3, 4]
    );
    harness.shutdown();
}

#[test]
fn grpc_server_streaming_peer_reset_cancels_response_source() {
    let runtime = runtime();
    let received_cancel = Arc::new(AtomicBool::new(false));
    let encoded =
        tina_http::encode_grpc_message(&CounterReply { value: 99 }, GrpcLimits::default())
            .expect("encode stream chunk");
    let source = runtime
        .register_with_capacity::<CancelRecordingSource, Infallible>(
            CancelRecordingSource {
                chunk: encoded,
                received_next: Arc::new(AtomicBool::new(false)),
                received_cancel: Arc::clone(&received_cancel),
            },
            16,
        )
        .expect("register cancel source");
    let router = GrpcRouter::<TestShard>::new(GrpcLimits::default()).server_streaming(
        "/specimen.Counter/CancelWatch",
        move |_request: GrpcRequest<CounterRequest>| Ok(GrpcServerStreamingResponse::new(source)),
    );
    let service = runtime
        .register_with_capacity::<GrpcRouter<TestShard>, _>(router, 16)
        .expect("register grpc router");
    let harness = GrpcHarness::start_with_service(runtime, service, Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);
    let body = grpc_body(&CounterRequest { delta: 0 });

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("/specimen.Counter/CancelWatch", "application/grpc+proto"),
    );
    write_frame(&mut stream, FRAME_DATA, FLAG_END_STREAM, 1, &body);

    for _ in 0..16 {
        let frame = read_frame(&mut stream);
        if frame.stream_id == 1 && frame.ty == FRAME_DATA {
            break;
        }
    }
    write_frame(&mut stream, FRAME_RST_STREAM, 0, 1, &0_u32.to_be_bytes());
    assert!(
        wait_for_atomic_flag(&received_cancel, Duration::from_secs(1)),
        "server-streaming peer reset must cancel response source"
    );
    harness.shutdown();
}

#[test]
fn grpc_server_streaming_non_reading_peer_reset_cancels_blocked_source() {
    let runtime = runtime();
    let received_next = Arc::new(AtomicBool::new(false));
    let received_cancel = Arc::new(AtomicBool::new(false));
    let source = runtime
        .register_with_capacity::<CancelRecordingSource, Infallible>(
            CancelRecordingSource {
                chunk: vec![0; 128 * 1024],
                received_next: Arc::clone(&received_next),
                received_cancel: Arc::clone(&received_cancel),
            },
            16,
        )
        .expect("register blocked cancel source");
    let router = GrpcRouter::<TestShard>::new(GrpcLimits::default()).server_streaming(
        "/specimen.Counter/BlockedCancelWatch",
        move |_request: GrpcRequest<CounterRequest>| Ok(GrpcServerStreamingResponse::new(source)),
    );
    let service = runtime
        .register_with_capacity::<GrpcRouter<TestShard>, _>(router, 16)
        .expect("register grpc router");
    let harness = GrpcHarness::start_with_service(runtime, service, Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);
    let body = grpc_body(&CounterRequest { delta: 0 });

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers(
            "/specimen.Counter/BlockedCancelWatch",
            "application/grpc+proto",
        ),
    );
    write_frame(&mut stream, FRAME_DATA, FLAG_END_STREAM, 1, &body);
    assert!(
        wait_for_atomic_flag(&received_next, Duration::from_secs(1)),
        "server must install and pull the response source before the reset proof starts"
    );

    write_frame(&mut stream, FRAME_RST_STREAM, 0, 1, &0_u32.to_be_bytes());
    assert!(
        wait_for_atomic_flag(&received_cancel, Duration::from_secs(1)),
        "peer reset must cancel a response source even when the client never drains DATA"
    );
    harness.shutdown();
}

#[test]
fn grpc_client_streaming_reads_multiple_request_messages() {
    let harness = GrpcHarness::start_router(Http2ServerConfig::default(), GrpcLimits::default());
    let mut stream = connect_h2(harness.addr);
    let mut body = Vec::new();
    body.extend_from_slice(&grpc_body(&CounterRequest { delta: 2 }));
    body.extend_from_slice(&grpc_body(&CounterRequest { delta: 40 }));

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("/specimen.Counter/Sum", "application/grpc+proto"),
    );
    write_frame(&mut stream, FRAME_DATA, 0, 1, &body[..body.len() / 2]);
    write_frame(
        &mut stream,
        FRAME_DATA,
        FLAG_END_STREAM,
        1,
        &body[body.len() / 2..],
    );

    let (body, status) = read_body_and_status(&mut stream, 1);
    assert_eq!(status, GrpcStatusCode::Ok);
    assert_eq!(decode_one_grpc_reply(&body).value, 42);
    harness.shutdown();
}

#[test]
fn grpc_client_streaming_handles_many_small_messages() {
    let harness = GrpcHarness::start_router(Http2ServerConfig::default(), GrpcLimits::default());
    let mut stream = connect_h2(harness.addr);
    let mut body = Vec::new();
    let expected: u64 = (0..1000).sum();
    for delta in 0..1000 {
        body.extend_from_slice(&grpc_body(&CounterRequest { delta }));
    }

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("/specimen.Counter/Sum", "application/grpc+proto"),
    );
    for chunk in body.chunks(97) {
        write_frame(&mut stream, FRAME_DATA, 0, 1, chunk);
    }
    write_frame(&mut stream, FRAME_DATA, FLAG_END_STREAM, 1, &[]);

    let (body, status) = read_body_and_status(&mut stream, 1);
    assert_eq!(status, GrpcStatusCode::Ok);
    assert_eq!(decode_one_grpc_reply(&body).value, expected);
    harness.shutdown();
}

#[test]
fn grpc_streaming_sends_response_before_request_eof() {
    let harness = GrpcHarness::start_router(Http2ServerConfig::default(), GrpcLimits::default());
    let mut stream = connect_h2(harness.addr);
    let first = grpc_body(&CounterRequest { delta: 1 });
    let second = grpc_body(&CounterRequest { delta: 2 });

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("/specimen.Counter/Chat", "application/grpc+proto"),
    );
    write_frame(&mut stream, FRAME_DATA, 0, 1, &first);

    let first_response = read_next_data_for_stream(&mut stream, 1);
    assert_eq!(decode_one_grpc_reply(&first_response).value, 101);

    write_frame(&mut stream, FRAME_DATA, FLAG_END_STREAM, 1, &second);
    let (mut rest, status) = read_body_and_status(&mut stream, 1);
    assert_eq!(status, GrpcStatusCode::Ok);
    if rest.is_empty() {
        rest = read_next_data_for_stream(&mut stream, 1);
    }
    assert_eq!(decode_one_grpc_reply(&rest).value, 102);
    harness.shutdown();
}

#[test]
fn grpc_streaming_concurrent_streams_do_not_cross_talk() {
    let harness = GrpcHarness::start_router(Http2ServerConfig::default(), GrpcLimits::default());
    let mut stream = connect_h2(harness.addr);

    for stream_id in [1, 3] {
        write_frame(
            &mut stream,
            FRAME_HEADERS,
            FLAG_END_HEADERS,
            stream_id,
            &request_headers("/specimen.Counter/Chat", "application/grpc+proto"),
        );
    }
    write_frame(
        &mut stream,
        FRAME_DATA,
        0,
        1,
        &grpc_body(&CounterRequest { delta: 10 }),
    );
    write_frame(
        &mut stream,
        FRAME_DATA,
        0,
        3,
        &grpc_body(&CounterRequest { delta: 30 }),
    );

    let mut reply_1 = None;
    let mut reply_3 = None;
    for _ in 0..32 {
        let frame = read_frame(&mut stream);
        match (frame.ty, frame.stream_id) {
            (FRAME_DATA, 1) => reply_1 = Some(decode_one_grpc_reply(&frame.payload).value),
            (FRAME_DATA, 3) => reply_3 = Some(decode_one_grpc_reply(&frame.payload).value),
            (FRAME_HEADERS, _) => {}
            (FRAME_RST_STREAM, id) => panic!("unexpected reset for stream {id}: {frame:?}"),
            _ => {}
        }
        if reply_1.is_some() && reply_3.is_some() {
            break;
        }
    }
    assert_eq!(reply_1, Some(110));
    assert_eq!(reply_3, Some(130));

    write_frame(&mut stream, FRAME_DATA, FLAG_END_STREAM, 1, &[]);
    write_frame(&mut stream, FRAME_DATA, FLAG_END_STREAM, 3, &[]);
    let mut statuses = read_statuses(&mut stream, &[1, 3]);
    statuses.sort_by_key(|(stream_id, _)| *stream_id);
    assert_eq!(
        statuses,
        vec![(1, GrpcStatusCode::Ok), (3, GrpcStatusCode::Ok)]
    );
    harness.shutdown();
}

#[test]
fn grpc_streaming_malformed_frame_sets_final_status() {
    let harness = GrpcHarness::start_router(Http2ServerConfig::default(), GrpcLimits::default());
    let mut stream = connect_h2(harness.addr);
    let mut compressed = Vec::new();
    compressed.push(1);
    compressed.extend_from_slice(&0_u32.to_be_bytes());

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("/specimen.Counter/Chat", "application/grpc+proto"),
    );
    write_frame(&mut stream, FRAME_DATA, FLAG_END_STREAM, 1, &compressed);

    assert_eq!(read_status(&mut stream, 1), GrpcStatusCode::Unimplemented);
    harness.shutdown();
}

#[test]
fn grpc_streaming_declared_message_cap_sets_resource_exhausted() {
    let harness = GrpcHarness::start_router(
        Http2ServerConfig::default(),
        GrpcLimits {
            max_message_bytes: 4,
        },
    );
    let mut stream = connect_h2(harness.addr);
    let mut malicious = Vec::new();
    malicious.push(0);
    malicious.extend_from_slice(&1024_u32.to_be_bytes());

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("/specimen.Counter/Chat", "application/grpc+proto"),
    );
    write_frame(&mut stream, FRAME_DATA, FLAG_END_STREAM, 1, &malicious);

    assert_eq!(
        read_status(&mut stream, 1),
        GrpcStatusCode::ResourceExhausted
    );
    harness.shutdown();
}

#[test]
fn grpc_streaming_peer_reset_cancels_response_source() {
    let runtime = runtime();
    let stream_slot = Arc::new(Mutex::new(None));
    let received_cancel = Arc::new(AtomicBool::new(false));
    let source = runtime
        .register_with_capacity::<StreamingEchoSource, Infallible>(
            StreamingEchoSource {
                stream_slot: Arc::clone(&stream_slot),
                pending: None,
                eof: false,
                limits: GrpcLimits::default(),
                received_cancel: Arc::clone(&received_cancel),
            },
            16,
        )
        .expect("register streaming source");
    let router = GrpcRouter::<TestShard>::new(GrpcLimits::default()).streaming(
        "/specimen.Counter/Chat",
        move |request: GrpcStreamingCall<CounterRequest, CounterReply>| {
            *stream_slot.lock().expect("streaming stream slot") = Some(request.requests);
            Ok(GrpcStreamingResponse::new(source))
        },
    );
    let service = runtime
        .register_with_capacity::<GrpcRouter<TestShard>, _>(router, 16)
        .expect("register grpc router");
    let harness = GrpcHarness::start_with_service(runtime, service, Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("/specimen.Counter/Chat", "application/grpc+proto"),
    );
    write_frame(
        &mut stream,
        FRAME_DATA,
        0,
        1,
        &grpc_body(&CounterRequest { delta: 1 }),
    );
    assert_eq!(
        decode_one_grpc_reply(&read_next_data_for_stream(&mut stream, 1)).value,
        101
    );
    write_frame(&mut stream, FRAME_RST_STREAM, 0, 1, &0_u32.to_be_bytes());

    assert!(
        wait_for_atomic_flag(&received_cancel, Duration::from_secs(1)),
        "peer reset must cancel streaming response source"
    );
    harness.shutdown();
}

#[test]
fn grpc_streaming_raw_sends_response_before_request_eof() {
    let runtime = runtime();
    let stream_slot = Arc::new(Mutex::new(None));
    let source = runtime
        .register_with_capacity::<StreamingEchoSource, Infallible>(
            StreamingEchoSource {
                stream_slot: Arc::clone(&stream_slot),
                pending: None,
                eof: false,
                limits: GrpcLimits::default(),
                received_cancel: Arc::new(AtomicBool::new(false)),
            },
            16,
        )
        .expect("register raw streaming source");
    let router = GrpcRouter::<TestShard>::new(GrpcLimits::default()).streaming_raw(
        "/specimen.Counter/RawChat",
        move |request: GrpcRawStreamingRequest<CounterRequest>| {
            *stream_slot.lock().expect("streaming stream slot") = Some(GrpcRequestStream::new(
                request.stream,
                GrpcLimits::default(),
            ));
            Ok(GrpcRawStreamingResponse::new(source))
        },
    );
    let service = runtime
        .register_with_capacity::<GrpcRouter<TestShard>, _>(router, 16)
        .expect("register grpc router");
    let harness = GrpcHarness::start_with_service(runtime, service, Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("/specimen.Counter/RawChat", "application/grpc+proto"),
    );
    write_frame(
        &mut stream,
        FRAME_DATA,
        0,
        1,
        &grpc_body(&CounterRequest { delta: 9 }),
    );
    assert_eq!(
        decode_one_grpc_reply(&read_next_data_for_stream(&mut stream, 1)).value,
        109
    );
    write_frame(&mut stream, FRAME_DATA, FLAG_END_STREAM, 1, &[]);
    assert_eq!(read_status(&mut stream, 1), GrpcStatusCode::Ok);
    harness.shutdown();
}

#[test]
fn grpc_client_streaming_declared_message_cap_fails_before_service() {
    let runtime = runtime();
    let service_calls = Arc::new(AtomicUsize::new(0));
    let calls_for_route = Arc::clone(&service_calls);
    let router = GrpcRouter::<TestShard>::new(GrpcLimits {
        max_message_bytes: 8,
    })
    .client_streaming(
        "/specimen.Counter/Sum",
        move |request: GrpcClientStreamingRequest<CounterRequest>| {
            calls_for_route.fetch_add(1, Ordering::AcqRel);
            Ok(GrpcResponse::new(CounterReply {
                value: request.messages.iter().map(|message| message.delta).sum(),
            }))
        },
    );
    let service = runtime
        .register_with_capacity::<GrpcRouter<TestShard>, _>(router, 16)
        .expect("register grpc router");
    let harness = GrpcHarness::start_with_service(runtime, service, Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);
    let mut malicious = Vec::new();
    malicious.push(0);
    malicious.extend_from_slice(&1024_u32.to_be_bytes());

    let status = raw_grpc_status(
        &mut stream,
        1,
        "/specimen.Counter/Sum",
        "application/grpc+proto",
        &malicious,
    );
    assert_eq!(status, GrpcStatusCode::ResourceExhausted);
    assert_eq!(
        service_calls.load(Ordering::Acquire),
        0,
        "oversized declared message must fail before invoking user service"
    );
    harness.shutdown();
}

#[test]
fn grpc_streaming_modes_share_one_http2_connection_without_cross_talk() {
    let harness = GrpcHarness::start_router(Http2ServerConfig::default(), GrpcLimits::default());
    let mut stream = connect_h2(harness.addr);
    let watch_body = grpc_body(&CounterRequest { delta: 0 });
    let mut sum_body = Vec::new();
    sum_body.extend_from_slice(&grpc_body(&CounterRequest { delta: 10 }));
    sum_body.extend_from_slice(&grpc_body(&CounterRequest { delta: 32 }));

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("/specimen.Counter/Watch", "application/grpc+proto"),
    );
    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        3,
        &request_headers("/specimen.Counter/Sum", "application/grpc+proto"),
    );
    write_frame(&mut stream, FRAME_DATA, FLAG_END_STREAM, 1, &watch_body);
    write_frame(
        &mut stream,
        FRAME_DATA,
        0,
        3,
        &sum_body[..sum_body.len() / 2],
    );
    write_frame(
        &mut stream,
        FRAME_DATA,
        FLAG_END_STREAM,
        3,
        &sum_body[sum_body.len() / 2..],
    );

    let mut watch_body = Vec::new();
    let mut sum_body = Vec::new();
    let mut watch_status = None;
    let mut sum_status = None;
    for _ in 0..64 {
        let frame = read_frame(&mut stream);
        match (frame.ty, frame.stream_id) {
            (FRAME_DATA, 1) => watch_body.extend_from_slice(&frame.payload),
            (FRAME_DATA, 3) => sum_body.extend_from_slice(&frame.payload),
            (FRAME_HEADERS, 1) if frame.flags & FLAG_END_STREAM != 0 => {
                watch_status = Some(decode_status(&frame.payload));
            }
            (FRAME_HEADERS, 3) if frame.flags & FLAG_END_STREAM != 0 => {
                sum_status = Some(decode_status(&frame.payload));
            }
            (FRAME_HEADERS, _) => {}
            (FRAME_RST_STREAM, id) => panic!("unexpected reset for stream {id}: {frame:?}"),
            _ => {}
        }
        if watch_status.is_some() && sum_status.is_some() {
            break;
        }
    }

    assert_eq!(watch_status, Some(GrpcStatusCode::Ok));
    assert_eq!(sum_status, Some(GrpcStatusCode::Ok));
    assert_eq!(
        decode_grpc_replies(&watch_body)
            .iter()
            .map(|reply| reply.value)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(decode_one_grpc_reply(&sum_body).value, 42);
    harness.shutdown();
}

#[test]
fn grpc_request_trailers_are_rejected_not_treated_as_eof() {
    let harness = GrpcHarness::start_router(Http2ServerConfig::default(), GrpcLimits::default());
    let mut stream = connect_h2(harness.addr);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("/specimen.Counter/Sum", "application/grpc+proto"),
    );
    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM,
        1,
        &request_trailers(),
    );

    let rst = read_until_rst(&mut stream, 1);
    assert_eq!(rst.stream_id, 1);
    harness.shutdown();
}

#[test]
fn grpc_streaming_request_content_length_overrun_resets_stream() {
    let harness = GrpcHarness::start_router(Http2ServerConfig::default(), GrpcLimits::default());
    let mut stream = connect_h2(harness.addr);
    let body = grpc_body(&CounterRequest { delta: 7 });

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers_with_content_length(
            "/specimen.Counter/Sum",
            "application/grpc+proto",
            body.len() - 1,
        ),
    );
    write_frame(&mut stream, FRAME_DATA, FLAG_END_STREAM, 1, &body);

    let rst = read_until_rst(&mut stream, 1);
    assert_eq!(rst.stream_id, 1);
    harness.shutdown();
}

#[test]
fn grpc_streaming_request_content_length_underrun_resets_stream() {
    let harness = GrpcHarness::start_router(Http2ServerConfig::default(), GrpcLimits::default());
    let mut stream = connect_h2(harness.addr);
    let body = grpc_body(&CounterRequest { delta: 7 });

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers_with_content_length(
            "/specimen.Counter/Sum",
            "application/grpc+proto",
            body.len() + 1,
        ),
    );
    write_frame(&mut stream, FRAME_DATA, FLAG_END_STREAM, 1, &body);

    let rst = read_until_rst(&mut stream, 1);
    assert_eq!(rst.stream_id, 1);
    harness.shutdown();
}

#[test]
fn grpc_streaming_request_total_body_cap_counts_consumed_chunks() {
    let config = Http2ServerConfig {
        limits: Http2Limits {
            max_body_bytes: 12,
            ..Http2Limits::default()
        },
        ..Http2ServerConfig::default()
    };
    let harness = GrpcHarness::start_router(config, GrpcLimits::default());
    let mut stream = connect_h2(harness.addr);
    let first = grpc_body(&CounterRequest { delta: 1 });
    let second = grpc_body(&CounterRequest { delta: 2 });

    assert!(first.len() <= 12);
    assert!(first.len() + second.len() > 12);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("/specimen.Counter/Sum", "application/grpc+proto"),
    );
    write_frame(&mut stream, FRAME_DATA, 0, 1, &first);
    write_frame(&mut stream, FRAME_DATA, FLAG_END_STREAM, 1, &second);

    let rst = read_until_rst(&mut stream, 1);
    assert_eq!(rst.stream_id, 1);
    harness.shutdown();
}

#[test]
fn grpc_large_unary_request_splits_http2_data_frames() {
    let harness = GrpcHarness::start_router(Http2ServerConfig::default(), GrpcLimits::default());
    let reply: CounterReply = grpc_unary_call_h2c_blocking(
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
    let error = grpc_unary_call_h2c_blocking::<_, CounterReply>(
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
    let error = grpc_unary_call_h2c_blocking::<_, CounterReply>(
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
    assert!(
        harness.wait_for_event(Duration::from_secs(1), |kind| {
            matches!(
                kind,
                RuntimeEventKind::CallDispatchAttempted {
                    call_kind: tina_runtime::CallKind::IsolateCall,
                    ..
                }
            )
        }),
        "service call should be accepted before peer reset"
    );
    write_frame(&mut stream, FRAME_RST_STREAM, 0, 1, &0_u32.to_be_bytes());
    assert!(
        harness.wait_for_event(Duration::from_secs(1), |kind| {
            matches!(kind, RuntimeEventKind::CallCancelled { .. })
        }),
        "peer reset should cancel the accepted service call"
    );
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
