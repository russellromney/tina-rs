mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use common::{Counter, TestShard};
use tina::prelude::*;
use tina_http::{
    Http2Limits, Http2Listener, Http2ListenerMsg, Http2ServerConfig, Http2ServiceMessage,
    HttpRequest, HttpResponse, IterBodySource,
};
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig};

const CLIENT_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const FRAME_DATA: u8 = 0x0;
const FRAME_HEADERS: u8 = 0x1;
const FRAME_PING: u8 = 0x6;
const FRAME_RST_STREAM: u8 = 0x3;
const FRAME_SETTINGS: u8 = 0x4;
const FRAME_GOAWAY: u8 = 0x7;
const FRAME_WINDOW_UPDATE: u8 = 0x8;
const FLAG_ACK: u8 = 0x1;
const FLAG_END_STREAM: u8 = 0x1;
const FLAG_END_HEADERS: u8 = 0x4;
const FLAG_PADDED: u8 = 0x8;
const FLAG_PRIORITY: u8 = 0x20;
const SETTINGS_ENABLE_PUSH: u16 = 0x2;
const SETTINGS_INITIAL_WINDOW_SIZE: u16 = 0x4;
const SETTINGS_MAX_FRAME_SIZE: u16 = 0x5;

struct Http2Harness {
    addr: SocketAddr,
    runtime: Option<ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>>,
    listener: Address<Http2ListenerMsg>,
}

impl Http2Harness {
    fn start(config: Http2ServerConfig) -> Self {
        let runtime = ThreadedRuntime::with_config(
            TestShard,
            DefaultThreadedMailboxFactory,
            ThreadedRuntimeConfig {
                command_capacity: 64,
                idle_wait: Duration::from_millis(1),
                ..Default::default()
            },
        );
        let counter = runtime
            .register_with_capacity::<Counter, std::convert::Infallible>(Counter::default(), 16)
            .expect("register counter");
        let listener = runtime
            .register_with_capacity::<Http2Listener<TestShard>, _>(
                Http2Listener::<TestShard>::new("127.0.0.1:0".parse().unwrap(), counter, config),
                config.listener_mailbox_capacity,
            )
            .expect("register http2 listener");
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
            .expect("register http2 listener");
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
        if let Some(runtime) = self.runtime.take() {
            let _ = runtime.try_send(self.listener, Http2ListenerMsg::Stop);
            let _ = runtime.shutdown();
        }
    }
}

impl Drop for Http2Harness {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            let _ = runtime.try_send(self.listener, Http2ListenerMsg::Stop);
            let _ = runtime.shutdown();
        }
    }
}

#[derive(Debug)]
struct TestFrame {
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
    panic!("did not complete settings handshake");
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

fn write_window_update(stream: &mut TcpStream, stream_id: u32, increment: u32) {
    write_frame(
        stream,
        FRAME_WINDOW_UPDATE,
        0,
        stream_id,
        &(increment & 0x7fff_ffff).to_be_bytes(),
    );
}

fn write_settings(stream: &mut TcpStream, settings: &[(u16, u32)]) {
    let mut payload = Vec::with_capacity(settings.len() * 6);
    for (id, value) in settings {
        payload.extend_from_slice(&id.to_be_bytes());
        payload.extend_from_slice(&value.to_be_bytes());
    }
    write_frame(stream, FRAME_SETTINGS, 0, 0, &payload);
}

fn write_data_chunks(stream: &mut TcpStream, stream_id: u32, body: &[u8]) {
    for (idx, chunk) in body.chunks(16 * 1024).enumerate() {
        let end_stream = (idx + 1) * 16 * 1024 >= body.len();
        write_frame(
            stream,
            FRAME_DATA,
            if end_stream { FLAG_END_STREAM } else { 0 },
            stream_id,
            chunk,
        );
    }
}

fn read_frame(stream: &mut TcpStream) -> TestFrame {
    read_frame_result(stream).expect("read frame")
}

fn read_frame_result(stream: &mut TcpStream) -> std::io::Result<TestFrame> {
    let mut head = [0_u8; 9];
    stream.read_exact(&mut head)?;
    let len = ((head[0] as usize) << 16) | ((head[1] as usize) << 8) | head[2] as usize;
    let mut payload = vec![0_u8; len];
    stream.read_exact(&mut payload)?;
    let mut sid = [0_u8; 4];
    sid.copy_from_slice(&head[5..9]);
    Ok(TestFrame {
        ty: head[3],
        flags: head[4],
        stream_id: u32::from_be_bytes(sid) & 0x7fff_ffff,
        payload,
    })
}

fn request_headers(method: &str, path: &str) -> Vec<u8> {
    let mut block = Vec::new();
    literal(":method", method, &mut block);
    literal(":scheme", "http", &mut block);
    literal(":path", path, &mut block);
    literal(":authority", "localhost", &mut block);
    block
}

fn request_headers_without_authority(method: &str, path: &str) -> Vec<u8> {
    let mut block = Vec::new();
    literal(":method", method, &mut block);
    literal(":scheme", "http", &mut block);
    literal(":path", path, &mut block);
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

fn read_response_body(stream: &mut TcpStream, stream_id: u32) -> Vec<u8> {
    let mut body = Vec::new();
    for _ in 0..8 {
        let frame = read_frame(stream);
        if frame.stream_id != stream_id {
            continue;
        }
        match frame.ty {
            FRAME_HEADERS => {
                if frame.flags & FLAG_END_STREAM != 0 {
                    return body;
                }
            }
            FRAME_DATA => {
                body.extend_from_slice(&frame.payload);
                if frame.flags & FLAG_END_STREAM != 0 {
                    return body;
                }
            }
            FRAME_WINDOW_UPDATE => {}
            FRAME_RST_STREAM => panic!("stream {stream_id} reset: {frame:?}"),
            other => panic!("unexpected response frame {other}: {frame:?}"),
        }
    }
    panic!("response body did not finish");
}

fn read_until_rst(stream: &mut TcpStream, stream_id: u32) -> TestFrame {
    for _ in 0..10 {
        let frame = read_frame(stream);
        if frame.ty == FRAME_RST_STREAM && frame.stream_id == stream_id {
            return frame;
        }
    }
    panic!("did not see RST_STREAM for {stream_id}");
}

fn read_until_goaway(stream: &mut TcpStream) -> TestFrame {
    for _ in 0..10 {
        let frame = read_frame(stream);
        if frame.ty == FRAME_GOAWAY {
            return frame;
        }
    }
    panic!("did not see GOAWAY");
}

// Wire-level error codes from RFC 7540 §7.
const ERR_PROTOCOL_ERROR: u32 = 0x1;
const ERR_FLOW_CONTROL_ERROR: u32 = 0x3;
const ERR_SETTINGS_ERROR: u32 = 0x4;
const ERR_FRAME_SIZE_ERROR: u32 = 0x6;
const ERR_REFUSED_STREAM: u32 = 0x7;
const ERR_ENHANCE_YOUR_CALM: u32 = 0xb;

fn goaway_error_code(frame: &TestFrame) -> u32 {
    assert_eq!(frame.ty, FRAME_GOAWAY, "expected GOAWAY frame");
    assert!(
        frame.payload.len() >= 8,
        "GOAWAY payload must carry last_stream_id (4B) + error_code (4B); got {} bytes",
        frame.payload.len()
    );
    let mut buf = [0_u8; 4];
    buf.copy_from_slice(&frame.payload[4..8]);
    u32::from_be_bytes(buf)
}

fn rst_stream_error_code(frame: &TestFrame) -> u32 {
    assert_eq!(frame.ty, FRAME_RST_STREAM, "expected RST_STREAM frame");
    assert_eq!(
        frame.payload.len(),
        4,
        "RST_STREAM payload must be exactly 4 bytes"
    );
    let mut buf = [0_u8; 4];
    buf.copy_from_slice(&frame.payload[..4]);
    u32::from_be_bytes(buf)
}

fn window_update_increment(frame: &TestFrame) -> u32 {
    assert_eq!(
        frame.ty, FRAME_WINDOW_UPDATE,
        "expected WINDOW_UPDATE frame"
    );
    assert_eq!(
        frame.payload.len(),
        4,
        "WINDOW_UPDATE payload must be exactly 4 bytes"
    );
    let mut buf = [0_u8; 4];
    buf.copy_from_slice(&frame.payload[..4]);
    u32::from_be_bytes(buf) & 0x7fff_ffff
}

struct StreamingResponseService {
    source: Address<tina_http::ResponseChunkMsg, tina_http::ResponseChunkReply>,
}

struct MixedResponseService {
    source: Address<tina_http::ResponseChunkMsg, tina_http::ResponseChunkReply>,
}

impl Isolate for StreamingResponseService {
    tina::isolate_types! {
        message: HttpRequest,
        reply: HttpResponse,
        send: tina::Outbound<std::convert::Infallible>,
        spawn: std::convert::Infallible,
        call: std::convert::Infallible,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        _request: HttpRequest,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        reply(HttpResponse::stream_chunked(
            http::StatusCode::OK,
            self.source,
        ))
    }

    fn handle_call(
        &mut self,
        _request: HttpRequest,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        call.reply(HttpResponse::stream_chunked(
            http::StatusCode::OK,
            self.source,
        ))
    }
}

impl Isolate for MixedResponseService {
    tina::isolate_types! {
        message: HttpRequest,
        reply: HttpResponse,
        send: tina::Outbound<std::convert::Infallible>,
        spawn: std::convert::Infallible,
        call: std::convert::Infallible,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        request: HttpRequest,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        reply(self.response_for(request))
    }

    fn handle_call(
        &mut self,
        request: HttpRequest,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        call.reply(self.response_for(request))
    }
}

impl MixedResponseService {
    fn response_for(&self, request: HttpRequest) -> HttpResponse {
        match request.path.as_str() {
            "/stream" => HttpResponse::stream_chunked(http::StatusCode::OK, self.source),
            "/counter" => HttpResponse::text("0"),
            "/echo" => HttpResponse::with_body(
                http::StatusCode::OK,
                request.body.as_buffered().unwrap_or_default().to_vec(),
            ),
            _ => HttpResponse::with_status(http::StatusCode::NOT_FOUND),
        }
    }
}

#[test]
fn http2_h2c_unary_request_response() {
    let harness = Http2Harness::start(Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM,
        1,
        &request_headers("GET", "/counter"),
    );

    assert_eq!(read_response_body(&mut stream, 1), b"0");
    harness.shutdown();
}

#[test]
fn http2_padded_data_delivers_only_unpadded_body() {
    let harness = Http2Harness::start(Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("POST", "/echo"),
    );
    let mut payload = vec![2];
    payload.extend_from_slice(b"abc");
    payload.extend_from_slice(b"zz");
    write_frame(
        &mut stream,
        FRAME_DATA,
        FLAG_PADDED | FLAG_END_STREAM,
        1,
        &payload,
    );

    assert_eq!(read_response_body(&mut stream, 1), b"abc");
    harness.shutdown();
}

#[test]
fn http2_padded_post_does_not_poison_followup_stream() {
    let harness = Http2Harness::start(Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("POST", "/echo"),
    );
    let mut payload = vec![2];
    payload.extend_from_slice(b"abc");
    payload.extend_from_slice(b"zz");
    write_frame(
        &mut stream,
        FRAME_DATA,
        FLAG_PADDED | FLAG_END_STREAM,
        1,
        &payload,
    );
    assert_eq!(read_response_body(&mut stream, 1), b"abc");

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM,
        3,
        &request_headers("GET", "/counter"),
    );
    assert_eq!(
        read_response_body(&mut stream, 3),
        b"0",
        "padding bytes from stream 1 must not leak into the next user-visible stream"
    );
    harness.shutdown();
}

#[test]
fn http2_bad_data_padding_sends_protocol_goaway() {
    let harness = Http2Harness::start(Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("POST", "/echo"),
    );
    write_frame(
        &mut stream,
        FRAME_DATA,
        FLAG_PADDED | FLAG_END_STREAM,
        1,
        &[3, b'a', b'b'],
    );

    let frame = read_until_goaway(&mut stream);
    assert_eq!(goaway_error_code(&frame), ERR_PROTOCOL_ERROR);
    harness.shutdown();
}

#[test]
fn http2_priority_headers_with_valid_hpack_succeeds() {
    let harness = Http2Harness::start(Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);
    let mut payload = vec![0, 0, 0, 0, 0];
    payload.extend_from_slice(&request_headers("GET", "/counter"));

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM | FLAG_PRIORITY,
        1,
        &payload,
    );

    assert_eq!(read_response_body(&mut stream, 1), b"0");
    harness.shutdown();
}

#[test]
fn http2_padded_priority_headers_with_valid_hpack_succeeds() {
    let harness = Http2Harness::start(Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);
    let mut payload = vec![1, 0, 0, 0, 0, 0];
    payload.extend_from_slice(&request_headers("GET", "/counter"));
    payload.push(0);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM | FLAG_PADDED | FLAG_PRIORITY,
        1,
        &payload,
    );

    assert_eq!(read_response_body(&mut stream, 1), b"0");
    harness.shutdown();
}

#[test]
fn http2_malformed_padded_priority_headers_rejects() {
    let harness = Http2Harness::start(Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM | FLAG_PADDED | FLAG_PRIORITY,
        1,
        &[0, 1, 2],
    );

    let frame = read_until_goaway(&mut stream);
    assert_eq!(goaway_error_code(&frame), ERR_PROTOCOL_ERROR);
    harness.shutdown();
}

#[test]
fn http2_forbidden_connection_header_rejects() {
    let harness = Http2Harness::start(Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);
    let mut block = request_headers("GET", "/counter");
    literal("connection", "keep-alive", &mut block);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM,
        1,
        &block,
    );

    let frame = read_until_goaway(&mut stream);
    assert_eq!(goaway_error_code(&frame), ERR_PROTOCOL_ERROR);
    harness.shutdown();
}

#[test]
fn http2_te_header_must_be_trailers() {
    let harness = Http2Harness::start(Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);
    let mut block = request_headers("GET", "/counter");
    literal("te", "gzip", &mut block);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM,
        1,
        &block,
    );

    let frame = read_until_goaway(&mut stream);
    assert_eq!(goaway_error_code(&frame), ERR_PROTOCOL_ERROR);
    harness.shutdown();
}

#[test]
fn http2_te_trailers_header_is_allowed() {
    let harness = Http2Harness::start(Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);
    let mut block = request_headers("GET", "/counter");
    literal("te", "trailers", &mut block);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM,
        1,
        &block,
    );

    assert_eq!(read_response_body(&mut stream, 1), b"0");
    harness.shutdown();
}

#[test]
fn http2_uppercase_header_rejects() {
    let harness = Http2Harness::start(Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);
    let mut block = request_headers("GET", "/counter");
    literal("X-Bad", "nope", &mut block);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM,
        1,
        &block,
    );

    let frame = read_until_goaway(&mut stream);
    assert_eq!(goaway_error_code(&frame), ERR_PROTOCOL_ERROR);
    harness.shutdown();
}

#[test]
fn http2_missing_authority_rejects() {
    let harness = Http2Harness::start(Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM,
        1,
        &request_headers_without_authority("GET", "/counter"),
    );

    let frame = read_until_goaway(&mut stream);
    assert_eq!(goaway_error_code(&frame), ERR_PROTOCOL_ERROR);
    harness.shutdown();
}

#[test]
fn http2_empty_path_rejects_before_service_dispatch() {
    let harness = Http2Harness::start(Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM,
        1,
        &request_headers("GET", ""),
    );

    let frame = read_until_goaway(&mut stream);
    assert_eq!(goaway_error_code(&frame), ERR_PROTOCOL_ERROR);
    harness.shutdown();
}

#[test]
fn http2_settings_max_frame_size_controls_outbound_splitting() {
    let config = Http2ServerConfig {
        limits: Http2Limits {
            max_body_bytes: 30_000,
            max_response_body_bytes: 30_000,
            initial_connection_window: 30_000,
            initial_stream_window: 30_000,
            ..Http2Limits::default()
        },
        ..Http2ServerConfig::default()
    };
    let harness = Http2Harness::start(config);
    let mut stream = connect_h2(harness.addr);
    write_settings(&mut stream, &[(SETTINGS_MAX_FRAME_SIZE, 20_000)]);
    let ack = read_frame(&mut stream);
    assert_eq!(ack.ty, FRAME_SETTINGS);
    assert_ne!(ack.flags & FLAG_ACK, 0);

    let body = vec![b'x'; 18_000];
    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("POST", "/echo"),
    );
    write_data_chunks(&mut stream, 1, &body);

    let mut data_frames = 0;
    let mut received = Vec::new();
    for _ in 0..8 {
        let frame = read_frame(&mut stream);
        match frame.ty {
            FRAME_DATA if frame.stream_id == 1 => {
                data_frames += 1;
                received.extend_from_slice(&frame.payload);
                if frame.flags & FLAG_END_STREAM != 0 {
                    break;
                }
            }
            FRAME_HEADERS | FRAME_WINDOW_UPDATE => {}
            other => panic!("unexpected frame {other}: {frame:?}"),
        }
    }
    assert_eq!(received, body);
    assert_eq!(
        data_frames, 1,
        "peer max frame size should allow one DATA frame"
    );
    harness.shutdown();
}

#[test]
fn http2_peer_max_frame_size_splits_large_user_response() {
    let config = Http2ServerConfig {
        limits: Http2Limits {
            max_body_bytes: 40_000,
            max_response_body_bytes: 40_000,
            initial_connection_window: 40_000,
            initial_stream_window: 40_000,
            ..Http2Limits::default()
        },
        ..Http2ServerConfig::default()
    };
    let harness = Http2Harness::start(config);
    let mut stream = connect_h2(harness.addr);
    write_settings(&mut stream, &[(SETTINGS_MAX_FRAME_SIZE, 16_384)]);
    let ack = read_frame(&mut stream);
    assert_eq!(ack.ty, FRAME_SETTINGS);
    assert_ne!(ack.flags & FLAG_ACK, 0);

    let body = vec![b'x'; 25_000];
    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("POST", "/echo"),
    );
    write_data_chunks(&mut stream, 1, &body);

    let mut data_frames = 0;
    let mut received = Vec::new();
    for _ in 0..8 {
        let frame = read_frame(&mut stream);
        match frame.ty {
            FRAME_DATA if frame.stream_id == 1 => {
                assert!(
                    frame.payload.len() <= 16_384,
                    "outbound DATA exceeded peer max frame size: {}",
                    frame.payload.len()
                );
                data_frames += 1;
                received.extend_from_slice(&frame.payload);
                if frame.flags & FLAG_END_STREAM != 0 {
                    break;
                }
            }
            FRAME_HEADERS | FRAME_WINDOW_UPDATE => {}
            other => panic!("unexpected frame {other}: {frame:?}"),
        }
    }
    assert_eq!(received, body);
    assert!(
        data_frames >= 2,
        "large response should be split by peer max frame size; got {data_frames}"
    );
    harness.shutdown();
}

#[test]
fn http2_multi_frame_response_marks_end_stream_only_on_last_data_frame() {
    // Guards the Phase 152 buffered-response framing rewrite: the server now
    // builds each DATA frame straight into the write queue (header bytes, then
    // the borrowed body slice) instead of cloning each chunk into a `Frame`
    // and re-encoding it. The exact edges that change could break: byte order,
    // END_STREAM placement, and the HEADERS terminal flag.
    let config = Http2ServerConfig {
        limits: Http2Limits {
            max_body_bytes: 60_000,
            max_response_body_bytes: 60_000,
            initial_connection_window: 60_000,
            initial_stream_window: 60_000,
            ..Http2Limits::default()
        },
        ..Http2ServerConfig::default()
    };
    let harness = Http2Harness::start(config);
    let mut stream = connect_h2(harness.addr);
    write_settings(&mut stream, &[(SETTINGS_MAX_FRAME_SIZE, 16_384)]);
    let ack = read_frame(&mut stream);
    assert_eq!(ack.ty, FRAME_SETTINGS);
    assert_ne!(ack.flags & FLAG_ACK, 0);

    // A distinct byte per position so a reorder, truncation, or duplication is
    // caught; sized to force three outbound DATA frames at a 16384 frame size.
    let body: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("POST", "/echo"),
    );
    write_data_chunks(&mut stream, 1, &body);

    let mut received = Vec::new();
    let mut data_frames = 0;
    let mut end_stream_frames = 0;
    let mut headers_end_stream = false;
    let mut last_frame_had_end_stream = false;
    for _ in 0..16 {
        let frame = read_frame(&mut stream);
        if frame.stream_id != 1 {
            continue;
        }
        match frame.ty {
            FRAME_HEADERS => {
                if frame.flags & FLAG_END_STREAM != 0 {
                    headers_end_stream = true;
                }
            }
            FRAME_DATA => {
                // No DATA frame after a terminal one may arrive.
                assert!(
                    !last_frame_had_end_stream,
                    "a DATA frame followed one already marked END_STREAM"
                );
                data_frames += 1;
                let this_end = frame.flags & FLAG_END_STREAM != 0;
                last_frame_had_end_stream = this_end;
                if this_end {
                    end_stream_frames += 1;
                }
                received.extend_from_slice(&frame.payload);
                if this_end {
                    break;
                }
            }
            FRAME_WINDOW_UPDATE => {}
            other => panic!("unexpected frame {other}: {frame:?}"),
        }
    }
    assert_eq!(
        received, body,
        "multi-frame body must reassemble byte-for-byte"
    );
    assert!(
        data_frames >= 3,
        "patterned body should split into >= 3 DATA frames; got {data_frames}"
    );
    assert_eq!(
        end_stream_frames, 1,
        "exactly one DATA frame carries END_STREAM"
    );
    assert!(
        last_frame_had_end_stream,
        "the final DATA frame must carry END_STREAM"
    );
    assert!(
        !headers_end_stream,
        "HEADERS must not claim END_STREAM while a body follows"
    );
    harness.shutdown();
}

#[test]
fn http2_settings_initial_window_shrink_blocks_until_window_update() {
    let harness = Http2Harness::start(Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);
    write_settings(&mut stream, &[(SETTINGS_INITIAL_WINDOW_SIZE, 0)]);
    let ack = read_frame(&mut stream);
    assert_eq!(ack.ty, FRAME_SETTINGS);
    assert_ne!(ack.flags & FLAG_ACK, 0);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM,
        1,
        &request_headers("GET", "/counter"),
    );

    stream
        .set_read_timeout(Some(Duration::from_millis(150)))
        .expect("short read timeout");
    assert!(
        read_frame_result(&mut stream).is_err(),
        "shrunk peer stream window should block outbound response DATA"
    );
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("restore read timeout");
    write_window_update(&mut stream, 1, 1);
    assert_eq!(read_response_body(&mut stream, 1), b"0");
    harness.shutdown();
}

#[test]
fn http2_settings_initial_window_increase_unblocks_pending_response() {
    let harness = Http2Harness::start(Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);
    write_settings(&mut stream, &[(SETTINGS_INITIAL_WINDOW_SIZE, 0)]);
    let ack = read_frame(&mut stream);
    assert_eq!(ack.ty, FRAME_SETTINGS);
    assert_ne!(ack.flags & FLAG_ACK, 0);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM,
        1,
        &request_headers("GET", "/counter"),
    );

    stream
        .set_read_timeout(Some(Duration::from_millis(150)))
        .expect("short read timeout");
    assert!(
        read_frame_result(&mut stream).is_err(),
        "zero peer stream window should park outbound response DATA"
    );

    write_settings(&mut stream, &[(SETTINGS_INITIAL_WINDOW_SIZE, 1)]);
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("restore read timeout");
    assert_eq!(read_response_body(&mut stream, 1), b"0");
    harness.shutdown();
}

#[test]
fn http2_invalid_settings_value_sends_goaway() {
    let harness = Http2Harness::start(Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);

    write_settings(&mut stream, &[(SETTINGS_INITIAL_WINDOW_SIZE, 0x8000_0000)]);

    let frame = read_until_goaway(&mut stream);
    assert_eq!(goaway_error_code(&frame), ERR_FLOW_CONTROL_ERROR);
    harness.shutdown();
}

#[test]
fn http2_invalid_enable_push_value_sends_protocol_error() {
    let harness = Http2Harness::start(Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);

    write_settings(&mut stream, &[(SETTINGS_ENABLE_PUSH, 2)]);

    let frame = read_until_goaway(&mut stream);
    assert_eq!(goaway_error_code(&frame), ERR_PROTOCOL_ERROR);
    harness.shutdown();
}

#[test]
fn http2_non_default_header_table_size_sends_settings_error() {
    let harness = Http2Harness::start(Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);

    write_settings(&mut stream, &[(0x1, 1024)]);

    let frame = read_until_goaway(&mut stream);
    assert_eq!(goaway_error_code(&frame), ERR_SETTINGS_ERROR);
    harness.shutdown();
}

#[test]
fn http2_rapid_reset_storm_sends_enhance_your_calm_goaway() {
    let config = Http2ServerConfig {
        limits: Http2Limits {
            rapid_reset_max_resets: 2,
            ..Http2Limits::default()
        },
        ..Http2ServerConfig::default()
    };
    let harness = Http2Harness::start(config);
    let mut stream = connect_h2(harness.addr);

    for stream_id in [1, 3, 5] {
        write_frame(
            &mut stream,
            FRAME_HEADERS,
            FLAG_END_HEADERS,
            stream_id,
            &request_headers("POST", "/echo"),
        );
        write_frame(
            &mut stream,
            FRAME_RST_STREAM,
            0,
            stream_id,
            &0_u32.to_be_bytes(),
        );
    }

    let frame = read_until_goaway(&mut stream);
    assert_eq!(goaway_error_code(&frame), ERR_ENHANCE_YOUR_CALM);
    harness.shutdown();
}

#[test]
fn http2_normal_reset_rate_allows_later_request() {
    let config = Http2ServerConfig {
        limits: Http2Limits {
            rapid_reset_max_resets: 4,
            ..Http2Limits::default()
        },
        ..Http2ServerConfig::default()
    };
    let harness = Http2Harness::start(config);
    let mut stream = connect_h2(harness.addr);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("POST", "/echo"),
    );
    write_frame(&mut stream, FRAME_RST_STREAM, 0, 1, &0_u32.to_be_bytes());
    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM,
        3,
        &request_headers("GET", "/counter"),
    );
    assert_eq!(read_response_body(&mut stream, 3), b"0");
    harness.shutdown();
}

#[test]
fn http2_streaming_response_sends_multiple_data_frames() {
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let source = IterBodySource::<TestShard>::register(
        &runtime,
        vec![b"hello ".to_vec(), b"stream".to_vec()].into_iter(),
        16,
    )
    .expect("register source");
    let service = runtime
        .register_with_capacity::<StreamingResponseService, std::convert::Infallible>(
            StreamingResponseService { source },
            16,
        )
        .expect("register service");
    let harness = Http2Harness::start_with_service(runtime, service, Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM,
        1,
        &request_headers("GET", "/stream"),
    );

    assert_eq!(read_response_body(&mut stream, 1), b"hello stream");
    harness.shutdown();
}

#[test]
fn http2_bad_preface_closes_connection() {
    let harness = Http2Harness::start(Http2ServerConfig::default());
    let mut stream =
        TcpStream::connect_timeout(&harness.addr, Duration::from_secs(2)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    stream.write_all(b"nope").expect("write bad preface");
    stream.flush().expect("flush");
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    assert!(buf.is_empty() || (buf.len() > 3 && buf[3] == FRAME_GOAWAY));
    harness.shutdown();
}

#[test]
fn http2_stream_cap_full_resets_new_stream_without_growing_table() {
    let config = Http2ServerConfig {
        limits: Http2Limits {
            max_concurrent_streams: 1,
            ..Http2Limits::default()
        },
        ..Http2ServerConfig::default()
    };
    let harness = Http2Harness::start(config);
    let mut stream = connect_h2(harness.addr);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("POST", "/echo"),
    );
    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM,
        3,
        &request_headers("GET", "/counter"),
    );

    let frame = read_until_rst(&mut stream, 3);
    assert_eq!(frame.stream_id, 3);
    // RFC 7540 §7: stream cap exceeded is ENHANCE_YOUR_CALM, which lets
    // the peer distinguish "you're sending too fast" from a generic
    // protocol error.
    assert_eq!(rst_stream_error_code(&frame), ERR_ENHANCE_YOUR_CALM);

    write_frame(&mut stream, FRAME_RST_STREAM, 0, 1, &0_u32.to_be_bytes());
    harness.shutdown();
}

#[test]
fn http2_concurrent_streams_complete_independently_up_to_cap() {
    let harness = Http2Harness::start(Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("POST", "/echo"),
    );
    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM,
        3,
        &request_headers("GET", "/counter"),
    );
    write_frame(&mut stream, FRAME_DATA, FLAG_END_STREAM, 1, b"abc");

    let mut body_1 = Vec::new();
    let mut body_3 = Vec::new();
    for _ in 0..16 {
        let frame = read_frame(&mut stream);
        match (frame.ty, frame.stream_id) {
            (FRAME_DATA, 1) => body_1.extend_from_slice(&frame.payload),
            (FRAME_DATA, 3) => body_3.extend_from_slice(&frame.payload),
            (FRAME_HEADERS, _) | (FRAME_WINDOW_UPDATE, _) => {}
            (FRAME_RST_STREAM, id) => panic!("unexpected reset for stream {id}: {frame:?}"),
            _ => {}
        }
        if body_1 == b"abc" && body_3 == b"0" {
            break;
        }
    }
    assert_eq!(body_1, b"abc");
    assert_eq!(body_3, b"0");
    harness.shutdown();
}

#[test]
fn http2_oversized_body_resets_stream_without_connection_close() {
    let config = Http2ServerConfig {
        limits: Http2Limits {
            max_body_bytes: 2,
            ..Http2Limits::default()
        },
        ..Http2ServerConfig::default()
    };
    let harness = Http2Harness::start(config);
    let mut stream = connect_h2(harness.addr);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("POST", "/echo"),
    );
    write_frame(&mut stream, FRAME_DATA, FLAG_END_STREAM, 1, b"abc");

    let frame = read_until_rst(&mut stream, 1);
    assert_eq!(frame.stream_id, 1);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM,
        3,
        &request_headers("GET", "/counter"),
    );
    assert_eq!(read_response_body(&mut stream, 3), b"0");
    harness.shutdown();
}

#[test]
fn http2_oversized_frame_sends_goaway() {
    let config = Http2ServerConfig {
        limits: Http2Limits {
            max_frame_size: 1,
            ..Http2Limits::default()
        },
        ..Http2ServerConfig::default()
    };
    let harness = Http2Harness::start(config);
    let mut stream = connect_h2(harness.addr);

    write_frame(&mut stream, FRAME_PING, 0, 0, b"12345678");

    let frame = read_until_goaway(&mut stream);
    assert_eq!(frame.stream_id, 0);
    // RFC 7540 §6.5.2: oversized frame is FRAME_SIZE_ERROR, not the generic
    // PROTOCOL_ERROR. The typed Http2ProtocolError::FrameTooLarge { .. }
    // path must map to the FRAME_SIZE_ERROR wire code in handle_read.
    assert_eq!(goaway_error_code(&frame), ERR_FRAME_SIZE_ERROR);
    harness.shutdown();
}

#[test]
fn http2_oversized_header_sends_goaway() {
    let config = Http2ServerConfig {
        limits: Http2Limits {
            max_header_bytes: 8,
            ..Http2Limits::default()
        },
        ..Http2ServerConfig::default()
    };
    let harness = Http2Harness::start(config);
    let mut stream = connect_h2(harness.addr);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM,
        1,
        &request_headers("GET", "/counter"),
    );

    let frame = read_until_goaway(&mut stream);
    assert_eq!(frame.stream_id, 0);
    // Oversized header block is a PROTOCOL_ERROR per RFC 7540, not the
    // FRAME_SIZE_ERROR used for oversized DATA payloads. Pinning this
    // mapping catches regressions where the typed error -> wire code path
    // drifts.
    assert_eq!(goaway_error_code(&frame), ERR_PROTOCOL_ERROR);
    harness.shutdown();
}

#[test]
fn http2_oversized_response_resets_stream_without_cloning_unbounded_body() {
    let config = Http2ServerConfig {
        limits: Http2Limits {
            max_response_body_bytes: 2,
            ..Http2Limits::default()
        },
        ..Http2ServerConfig::default()
    };
    let harness = Http2Harness::start(config);
    let mut stream = connect_h2(harness.addr);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("POST", "/echo"),
    );
    write_frame(&mut stream, FRAME_DATA, FLAG_END_STREAM, 1, b"abc");

    let frame = read_until_rst(&mut stream, 1);
    assert_eq!(frame.stream_id, 1);
    harness.shutdown();
}

#[test]
fn http2_window_update_unblocks_bounded_pending_response() {
    let config = Http2ServerConfig {
        limits: Http2Limits {
            initial_connection_window: 100_000,
            initial_stream_window: 100_000,
            max_body_bytes: 100_000,
            max_response_body_bytes: 100_000,
            connection_outbound_queue_capacity: 16,
            ..Http2Limits::default()
        },
        ..Http2ServerConfig::default()
    };
    let harness = Http2Harness::start(config);
    let mut stream = connect_h2(harness.addr);
    let body = vec![b'x'; 70_000];

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("POST", "/echo"),
    );
    write_data_chunks(&mut stream, 1, &body);

    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("short read timeout");
    loop {
        match read_frame_result(&mut stream) {
            Ok(frame) if frame.ty == FRAME_WINDOW_UPDATE => {}
            Ok(frame) => panic!("response should be flow-control blocked before update: {frame:?}"),
            Err(_) => break,
        }
    }

    write_window_update(&mut stream, 0, 100_000);
    write_window_update(&mut stream, 1, 100_000);
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("restore read timeout");

    assert_eq!(read_response_body(&mut stream, 1), body);
    harness.shutdown();
}

#[test]
fn http2_stream_window_blocked_response_does_not_block_unrelated_stream() {
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let source =
        IterBodySource::<TestShard>::register(&runtime, vec![vec![b'x'; 80_000]].into_iter(), 16)
            .expect("register source");
    let service = runtime
        .register_with_capacity::<MixedResponseService, std::convert::Infallible>(
            MixedResponseService { source },
            16,
        )
        .expect("register service");
    let harness = Http2Harness::start_with_service(runtime, service, Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM,
        1,
        &request_headers("GET", "/stream"),
    );

    let mut stream_1_data = 0;
    while stream_1_data < 65_535 {
        let frame = read_frame(&mut stream);
        if frame.stream_id == 1 && frame.ty == FRAME_DATA {
            stream_1_data += frame.payload.len();
        }
    }
    write_window_update(&mut stream, 0, 65_535);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM,
        3,
        &request_headers("GET", "/counter"),
    );
    assert_eq!(read_response_body(&mut stream, 3), b"0");
    harness.shutdown();
}

#[test]
fn http2_inbound_request_chunks_continue_while_response_stream_is_blocked() {
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let source =
        IterBodySource::<TestShard>::register(&runtime, vec![vec![b'x'; 80_000]].into_iter(), 16)
            .expect("register source");
    let service = runtime
        .register_with_capacity::<MixedResponseService, std::convert::Infallible>(
            MixedResponseService { source },
            16,
        )
        .expect("register service");
    let harness = Http2Harness::start_with_service(runtime, service, Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM,
        1,
        &request_headers("GET", "/stream"),
    );
    let mut stream_1_data = 0;
    while stream_1_data < 65_535 {
        let frame = read_frame(&mut stream);
        if frame.stream_id == 1 && frame.ty == FRAME_DATA {
            stream_1_data += frame.payload.len();
        }
    }
    write_window_update(&mut stream, 0, 65_535);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        3,
        &request_headers("POST", "/echo"),
    );
    write_frame(&mut stream, FRAME_DATA, 0, 3, b"abc");
    write_frame(&mut stream, FRAME_DATA, FLAG_END_STREAM, 3, b"def");

    assert_eq!(read_response_body(&mut stream, 3), b"abcdef");
    harness.shutdown();
}

#[test]
fn http2_peer_goaway_stops_new_streams_visibly() {
    let harness = Http2Harness::start(Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);

    let mut payload = Vec::new();
    payload.extend_from_slice(&0_u32.to_be_bytes());
    payload.extend_from_slice(&0_u32.to_be_bytes());
    write_frame(&mut stream, FRAME_GOAWAY, 0, 0, &payload);
    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM,
        1,
        &request_headers("GET", "/counter"),
    );

    let frame = read_until_rst(&mut stream, 1);
    assert_eq!(frame.stream_id, 1);
    // After a peer GOAWAY, the server refuses *new* streams with
    // REFUSED_STREAM so the peer knows the work was never started, not
    // partially-applied.
    assert_eq!(rst_stream_error_code(&frame), ERR_REFUSED_STREAM);
    harness.shutdown();
}

#[test]
fn http2_inbound_data_obeys_stream_window() {
    let config = Http2ServerConfig {
        limits: Http2Limits {
            initial_connection_window: 2,
            initial_stream_window: 2,
            ..Http2Limits::default()
        },
        ..Http2ServerConfig::default()
    };
    let harness = Http2Harness::start(config);
    let mut stream = connect_h2(harness.addr);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("POST", "/echo"),
    );
    write_frame(&mut stream, FRAME_DATA, FLAG_END_STREAM, 1, b"abc");

    let frame = read_frame(&mut stream);
    assert_eq!(frame.ty, FRAME_GOAWAY);
    // Receiving more DATA than the advertised window allows is the
    // canonical FLOW_CONTROL_ERROR, not a generic PROTOCOL_ERROR. Pin the
    // wire code so the typed Http2ProtocolError::FlowControl -> GOAWAY
    // mapping does not regress.
    assert_eq!(goaway_error_code(&frame), ERR_FLOW_CONTROL_ERROR);
    harness.shutdown();
}

#[test]
fn http2_stream_flow_control_error_resets_only_bad_stream() {
    let config = Http2ServerConfig {
        limits: Http2Limits {
            initial_connection_window: 100,
            initial_stream_window: 2,
            ..Http2Limits::default()
        },
        ..Http2ServerConfig::default()
    };
    let harness = Http2Harness::start(config);
    let mut stream = connect_h2(harness.addr);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("POST", "/echo"),
    );
    write_frame(&mut stream, FRAME_DATA, FLAG_END_STREAM, 1, b"abc");

    let frame = read_until_rst(&mut stream, 1);
    assert_eq!(rst_stream_error_code(&frame), ERR_FLOW_CONTROL_ERROR);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM,
        3,
        &request_headers("GET", "/counter"),
    );
    assert_eq!(read_response_body(&mut stream, 3), b"0");
    harness.shutdown();
}

#[test]
fn http2_zero_stream_window_update_resets_only_bad_stream() {
    let harness = Http2Harness::start(Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("POST", "/echo"),
    );
    write_window_update(&mut stream, 1, 0);

    let frame = read_until_rst(&mut stream, 1);
    assert_eq!(rst_stream_error_code(&frame), ERR_PROTOCOL_ERROR);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM,
        3,
        &request_headers("GET", "/counter"),
    );
    assert_eq!(read_response_body(&mut stream, 3), b"0");
    harness.shutdown();
}

#[test]
fn http2_data_consumption_returns_window_credit() {
    let harness = Http2Harness::start(Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("POST", "/echo"),
    );
    write_frame(&mut stream, FRAME_DATA, FLAG_END_STREAM, 1, b"abc");

    let mut saw_connection_credit = false;
    let mut saw_stream_credit = false;
    let mut body = Vec::new();
    for _ in 0..10 {
        let frame = read_frame(&mut stream);
        match frame.ty {
            FRAME_WINDOW_UPDATE if frame.stream_id == 0 => saw_connection_credit = true,
            FRAME_WINDOW_UPDATE if frame.stream_id == 1 => saw_stream_credit = true,
            FRAME_DATA if frame.stream_id == 1 => {
                body.extend_from_slice(&frame.payload);
                if frame.flags & FLAG_END_STREAM != 0 {
                    break;
                }
            }
            FRAME_HEADERS if frame.stream_id == 1 => {}
            other => panic!("unexpected frame {other}: {frame:?}"),
        }
    }
    assert!(saw_connection_credit);
    assert!(saw_stream_credit);
    assert_eq!(body, b"abc");
    harness.shutdown();
}

#[test]
fn http2_padded_data_returns_window_credit_for_full_payload() {
    let harness = Http2Harness::start(Http2ServerConfig::default());
    let mut stream = connect_h2(harness.addr);

    write_frame(
        &mut stream,
        FRAME_HEADERS,
        FLAG_END_HEADERS,
        1,
        &request_headers("POST", "/echo"),
    );
    let mut payload = vec![2];
    payload.extend_from_slice(b"abc");
    payload.extend_from_slice(b"zz");
    write_frame(
        &mut stream,
        FRAME_DATA,
        FLAG_PADDED | FLAG_END_STREAM,
        1,
        &payload,
    );

    let mut connection_credit = None;
    let mut stream_credit = None;
    let mut body = Vec::new();
    for _ in 0..10 {
        let frame = read_frame(&mut stream);
        match frame.ty {
            FRAME_WINDOW_UPDATE if frame.stream_id == 0 => {
                connection_credit = Some(window_update_increment(&frame));
            }
            FRAME_WINDOW_UPDATE if frame.stream_id == 1 => {
                stream_credit = Some(window_update_increment(&frame));
            }
            FRAME_DATA if frame.stream_id == 1 => {
                body.extend_from_slice(&frame.payload);
                if frame.flags & FLAG_END_STREAM != 0 {
                    break;
                }
            }
            FRAME_HEADERS if frame.stream_id == 1 => {}
            other => panic!("unexpected frame {other}: {frame:?}"),
        }
    }

    assert_eq!(body, b"abc", "service sees only unpadded data bytes");
    assert_eq!(
        connection_credit,
        Some(6),
        "flow credit includes pad length"
    );
    assert_eq!(stream_credit, Some(6), "flow credit includes padding bytes");
    harness.shutdown();
}
