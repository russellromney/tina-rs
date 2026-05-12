//! Body-pressure metrics integration tests.
//!
//! Confirms `BodyMetrics` charges and releases body bytes through
//! every relevant lifecycle edge: buffered request, streaming
//! request, buffered response, streaming response, oversized body
//! reject, mid-body truncation, and post-shutdown drain.

mod common;

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use http::StatusCode;
use tina::prelude::*;
use tina_http::{
    BodyMetrics, HttpConnectionMsg, HttpLimits, HttpListener, HttpListenerMsg, HttpRequest,
    HttpRequestBody, HttpResponse, RequestChunkReply, ResponseChunkMsg, ResponseChunkReply,
    ResponseStream,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime,
    ThreadedRuntimeConfig, call,
};

use common::TestShard;

/// A counter-style service that echoes the body length so the test
/// can confirm the request actually got through. All work is
/// buffered.
#[derive(Debug, Default)]
struct EchoLen;

impl Isolate for EchoLen {
    tina::isolate_types! {
        message: HttpRequest,
        reply: HttpResponse,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: Infallible,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        request: HttpRequest,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        let n = match request.body {
            HttpRequestBody::Buffered(b) => b.len(),
            HttpRequestBody::Stream(_) => 0,
        };
        reply(HttpResponse::with_text(StatusCode::OK, n.to_string()))
    }
}

#[test]
fn buffered_request_charges_and_releases_resident_body() {
    let metrics = BodyMetrics::new();

    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let svc = runtime
        .register_with_capacity::<EchoLen, Infallible>(EchoLen, 16)
        .expect("register service");

    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listener_isolate = HttpListener::<TestShard>::new(
        bind,
        svc,
        HttpLimits::default(),
        Duration::from_secs(2),
        16,
    )
    .with_metrics(metrics.clone());
    let listener = runtime
        .register_with_capacity::<HttpListener<TestShard>, _>(listener_isolate, 8)
        .expect("register listener");
    let bound = runtime.observe_next_bound();
    runtime.try_send(listener, HttpListenerMsg::Start).unwrap();
    let addr = bound.wait(Duration::from_secs(2)).expect("bound");

    // Send an 8 KiB POST. The connection accumulates the body in
    // `read_buf`, then dispatches buffered. Metrics should record a
    // positive high-water and zero current after dispatch.
    let body = vec![b'x'; 8 * 1024];
    let mut request = format!(
        "POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(&body);

    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(&request).unwrap();
    stream.flush().unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    assert!(
        std::str::from_utf8(&response)
            .unwrap()
            .starts_with("HTTP/1.1 200"),
        "expected 200, got {response:?}"
    );

    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();

    let snap = metrics.snapshot();
    assert!(
        snap.request_body_high_water >= body.len(),
        "high water must reflect at least the resident body bytes; got {}, expected >= {}",
        snap.request_body_high_water,
        body.len()
    );
    assert!(
        snap.drained(),
        "all body charges must release on shutdown; got {snap:?}"
    );
    assert_eq!(snap.body_full_count, 0, "happy path: no Full");
    assert_eq!(snap.body_timeout_count, 0, "happy path: no timeout");
    assert_eq!(snap.body_io_error_count, 0, "happy path: no IO error");
}

#[test]
fn oversized_request_increments_body_full_count() {
    let metrics = BodyMetrics::new();
    let max_body_bytes = 1024;

    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let svc = runtime
        .register_with_capacity::<EchoLen, Infallible>(EchoLen, 16)
        .expect("register service");

    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let limits = HttpLimits {
        max_body_bytes,
        ..HttpLimits::default()
    };
    let listener_isolate =
        HttpListener::<TestShard>::new(bind, svc, limits, Duration::from_secs(2), 16)
            .with_metrics(metrics.clone());
    let listener = runtime
        .register_with_capacity::<HttpListener<TestShard>, _>(listener_isolate, 8)
        .expect("register listener");
    let bound = runtime.observe_next_bound();
    runtime.try_send(listener, HttpListenerMsg::Start).unwrap();
    let addr = bound.wait(Duration::from_secs(2)).expect("bound");

    // Declare a body 4× the cap. The parser rejects with
    // BodyTooLarge before any service dispatch.
    let declared = max_body_bytes * 4;
    let request = format!(
        "POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
    );
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let text = std::str::from_utf8(&response).unwrap();
    assert!(
        text.starts_with("HTTP/1.1 413"),
        "expected 413 Payload Too Large, got {text:?}"
    );

    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();

    let snap = metrics.snapshot();
    assert_eq!(
        snap.body_full_count, 1,
        "BodyTooLarge must increment body_full_count once"
    );
    assert!(
        snap.drained(),
        "no body bytes were ever admitted; current must be zero"
    );
}

/// Streaming consumer that pulls every chunk and replies with the
/// total length. Reused for the streaming-metrics test.
#[derive(Debug, Clone)]
enum StreamMsg {
    Inbound(HttpRequest),
    ChunkArrived(CallOutcome<RequestChunkReply>),
}

impl From<HttpRequest> for StreamMsg {
    fn from(req: HttpRequest) -> Self {
        Self::Inbound(req)
    }
}

struct StreamingConsumer {
    chunk_call_timeout: Duration,
    pending_source: Option<Address<HttpConnectionMsg, RequestChunkReply>>,
    total: usize,
}

impl Isolate for StreamingConsumer {
    tina::isolate_types! {
        message: StreamMsg,
        reply: HttpResponse,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<StreamMsg>,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: StreamMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            StreamMsg::Inbound(req) => match req.body {
                HttpRequestBody::Stream(stream) => {
                    self.total = 0;
                    self.pending_source = Some(stream.source);
                    call(
                        stream.source,
                        HttpConnectionMsg::body_next(),
                        self.chunk_call_timeout,
                    )
                    .reply(StreamMsg::ChunkArrived)
                }
                HttpRequestBody::Buffered(b) => {
                    reply(HttpResponse::with_text(StatusCode::OK, b.len().to_string()))
                }
            },
            StreamMsg::ChunkArrived(outcome) => match outcome {
                CallOutcome::Replied(RequestChunkReply::Chunk(bytes)) => {
                    self.total += bytes.len();
                    let source = self.pending_source.expect("source set");
                    call(
                        source,
                        HttpConnectionMsg::body_next(),
                        self.chunk_call_timeout,
                    )
                    .reply(StreamMsg::ChunkArrived)
                }
                CallOutcome::Replied(RequestChunkReply::Eof) => {
                    self.pending_source = None;
                    reply(HttpResponse::with_text(
                        StatusCode::OK,
                        self.total.to_string(),
                    ))
                }
                CallOutcome::Replied(RequestChunkReply::Error(_))
                | CallOutcome::Full
                | CallOutcome::Closed
                | CallOutcome::Rejected(_)
                | CallOutcome::Timeout => {
                    self.pending_source = None;
                    reply(HttpResponse::internal_error())
                }
            },
        }
    }
}

#[test]
fn streaming_request_charges_per_chunk_and_releases_per_pull() {
    // Body large enough to force several READ_CHUNK reads but small
    // enough that the in-flight slice is bounded. With chunk_size=512
    // and total=8192 we expect high water to cap at one or two chunks
    // worth, never the full body.
    let metrics = BodyMetrics::new();
    let chunk_size = 512;
    let body: Vec<u8> = (0..8192u32).map(|i| (i % 251) as u8).collect();

    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let consumer = runtime
        .register_with_capacity::<StreamingConsumer, Infallible>(
            StreamingConsumer {
                chunk_call_timeout: Duration::from_secs(2),
                pending_source: None,
                total: 0,
            },
            32,
        )
        .expect("register consumer");

    let limits = HttpLimits {
        inbound_stream_chunk_size: Some(chunk_size),
        ..HttpLimits::default()
    };
    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listener_isolate = HttpListener::<TestShard, StreamMsg>::new(
        bind,
        consumer,
        limits,
        Duration::from_secs(5),
        16,
    )
    .with_metrics(metrics.clone());
    let listener = runtime
        .register_with_capacity::<HttpListener<TestShard, StreamMsg>, _>(listener_isolate, 8)
        .expect("register listener");
    let bound = runtime.observe_next_bound();
    runtime.try_send(listener, HttpListenerMsg::Start).unwrap();
    let addr = bound.wait(Duration::from_secs(2)).expect("bound");

    let mut request = format!(
        "POST /upload HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(&body);

    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(&request).unwrap();
    stream.flush().unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let text = std::str::from_utf8(&response).unwrap();
    assert!(text.starts_with("HTTP/1.1 200"), "got {text:?}");

    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();

    let snap = metrics.snapshot();
    assert!(
        snap.request_body_high_water > 0,
        "must have charged at least one chunk's worth"
    );
    // Subsequent socket reads honor `chunk_size`, but the initial
    // head-read returns up to `READ_CHUNK = 4096` bytes — so the
    // residual buffer between dispatch and first service pull can
    // be that big. Bounded by READ_CHUNK regardless of body size.
    assert!(
        snap.request_body_high_water <= 4096,
        "high water bounded by per-syscall ceiling READ_CHUNK; got {}",
        snap.request_body_high_water
    );
    assert!(
        snap.drained(),
        "every charge must release after stream completes; got {snap:?}"
    );
}

/// Regression for the chunk-size cap on follow-up socket reads:
/// if the head and body arrive in separate writes, the initial
/// `tcp_read` returns just the head, all body reads are post-
/// dispatch, and each is capped by `chunk_size`. Peak resident
/// under that pattern is exactly `chunk_size`.
#[test]
fn streaming_request_caps_resident_bytes_at_chunk_size_when_head_and_body_split() {
    let metrics = BodyMetrics::new();
    let chunk_size = 256;
    let body: Vec<u8> = (0..8192u32).map(|i| (i % 251) as u8).collect();

    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let consumer = runtime
        .register_with_capacity::<StreamingConsumer, Infallible>(
            StreamingConsumer {
                chunk_call_timeout: Duration::from_secs(2),
                pending_source: None,
                total: 0,
            },
            32,
        )
        .expect("register consumer");

    let limits = HttpLimits {
        inbound_stream_chunk_size: Some(chunk_size),
        ..HttpLimits::default()
    };
    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listener_isolate = HttpListener::<TestShard, StreamMsg>::new(
        bind,
        consumer,
        limits,
        Duration::from_secs(5),
        16,
    )
    .with_metrics(metrics.clone());
    let listener = runtime
        .register_with_capacity::<HttpListener<TestShard, StreamMsg>, _>(listener_isolate, 8)
        .expect("register listener");
    let bound = runtime.observe_next_bound();
    runtime.try_send(listener, HttpListenerMsg::Start).unwrap();
    let addr = bound.wait(Duration::from_secs(2)).expect("bound");

    // Phase 1: send only the head. The connection's first
    // tcp_read parses the head but receives zero body bytes.
    let head = format!(
        "POST /upload HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(head.as_bytes()).unwrap();
    stream.flush().unwrap();
    // Pause so the head-read returns just head bytes. With this
    // separation, subsequent reads (driven by service body_next
    // pulls) are all chunk_size-capped.
    std::thread::sleep(Duration::from_millis(20));

    // Phase 2: dribble the body bytes. Every socket read on the
    // server side is capped by chunk_size, so the buffer never
    // holds more than that.
    stream.write_all(&body).unwrap();
    stream.flush().unwrap();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    assert!(
        std::str::from_utf8(&response)
            .unwrap()
            .starts_with("HTTP/1.1 200"),
    );

    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();

    let snap = metrics.snapshot();
    assert!(snap.drained(), "charges drain on shutdown; got {snap:?}");
    assert!(snap.request_body_high_water > 0, "body bytes were charged");
    assert!(
        snap.request_body_high_water <= chunk_size,
        "split head/body upload caps high water at chunk_size = {}; got {}. \
         If this fails, the per-pull socket-read cap regressed.",
        chunk_size,
        snap.request_body_high_water
    );
}

/// Source that yields a fixed number of chunks; reused across the
/// streamed-response metrics test.
struct ChunkProducer {
    chunk_size: usize,
    total_chunks: usize,
    yielded: usize,
}

impl Isolate for ChunkProducer {
    tina::isolate_types! {
        message: ResponseChunkMsg,
        reply: ResponseChunkReply,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: Infallible,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        _msg: ResponseChunkMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        if self.yielded >= self.total_chunks {
            return reply(ResponseChunkReply::Eof);
        }
        let i = self.yielded;
        self.yielded += 1;
        reply(ResponseChunkReply::Chunk(vec![
            (i % 251) as u8;
            self.chunk_size
        ]))
    }
}

struct StreamingService {
    producer: Address<ResponseChunkMsg, ResponseChunkReply>,
    content_length: usize,
}

impl Isolate for StreamingService {
    tina::isolate_types! {
        message: HttpRequest,
        reply: HttpResponse,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: Infallible,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        request: HttpRequest,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        let response = if request.path == "/big" {
            HttpResponse::with_stream(
                StatusCode::OK,
                ResponseStream {
                    content_length: self.content_length,
                    source: self.producer,
                },
            )
        } else {
            HttpResponse::not_found()
        };
        reply(response)
    }
}

#[test]
fn streaming_response_charges_per_chunk_and_releases_after_write() {
    let metrics = BodyMetrics::new();
    let chunk_size = 1024;
    let total_chunks = 8;
    let total_bytes = chunk_size * total_chunks;

    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let producer = runtime
        .register_with_capacity::<ChunkProducer, Infallible>(
            ChunkProducer {
                chunk_size,
                total_chunks,
                yielded: 0,
            },
            16,
        )
        .expect("register producer");
    let service = runtime
        .register_with_capacity::<StreamingService, Infallible>(
            StreamingService {
                producer,
                content_length: total_bytes,
            },
            16,
        )
        .expect("register service");

    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listener_isolate = HttpListener::<TestShard>::new(
        bind,
        service,
        HttpLimits::default(),
        Duration::from_secs(2),
        16,
    )
    .with_metrics(metrics.clone());
    let listener = runtime
        .register_with_capacity::<HttpListener<TestShard>, _>(listener_isolate, 8)
        .expect("register listener");
    let bound = runtime.observe_next_bound();
    runtime.try_send(listener, HttpListenerMsg::Start).unwrap();
    let addr = bound.wait(Duration::from_secs(2)).expect("bound");

    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(b"GET /big HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    stream.flush().unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).unwrap();
    assert_eq!(
        buf.windows(b"\r\n\r\n".len())
            .position(|w| w == b"\r\n\r\n")
            .map(|i| buf.len() - (i + 4))
            .unwrap_or(0),
        total_bytes,
        "wire body must equal declared length"
    );

    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();

    let snap = metrics.snapshot();
    assert!(
        snap.response_body_high_water > 0,
        "must have charged at least one chunk's worth on the response side"
    );
    assert!(
        snap.response_body_high_water <= total_bytes,
        "high water cannot exceed declared length"
    );
    assert!(
        snap.drained(),
        "all response charges must release after writes complete; got {snap:?}"
    );
    assert_eq!(snap.body_full_count, 0);
    assert_eq!(snap.body_timeout_count, 0);
    assert_eq!(
        snap.body_io_error_count, 0,
        "happy path: source delivered exactly content_length"
    );
}

#[test]
fn chunked_request_charges_decoded_bytes_and_drains() {
    let metrics = BodyMetrics::new();
    let chunk_size = 512;
    let body: Vec<u8> = (0..8192u32).map(|i| (i % 251) as u8).collect();

    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let consumer = runtime
        .register_with_capacity::<StreamingConsumer, Infallible>(
            StreamingConsumer {
                chunk_call_timeout: Duration::from_secs(2),
                pending_source: None,
                total: 0,
            },
            32,
        )
        .expect("register consumer");

    let limits = HttpLimits {
        inbound_stream_chunk_size: Some(chunk_size),
        ..HttpLimits::default()
    };
    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listener_isolate = HttpListener::<TestShard, StreamMsg>::new(
        bind,
        consumer,
        limits,
        Duration::from_secs(5),
        16,
    )
    .with_metrics(metrics.clone());
    let listener = runtime
        .register_with_capacity::<HttpListener<TestShard, StreamMsg>, _>(listener_isolate, 8)
        .expect("register listener");
    let bound = runtime.observe_next_bound();
    runtime.try_send(listener, HttpListenerMsg::Start).unwrap();
    let addr = bound.wait(Duration::from_secs(2)).expect("bound");

    let mut chunked_body = Vec::new();
    for chunk in body.chunks(1024) {
        chunked_body.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        chunked_body.extend_from_slice(chunk);
        chunked_body.extend_from_slice(b"\r\n");
    }
    chunked_body.extend_from_slice(b"0\r\n\r\n");

    let mut request =
        "POST /upload HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
            .to_string()
            .into_bytes();
    request.extend_from_slice(&chunked_body);

    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(&request).unwrap();
    stream.flush().unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let text = std::str::from_utf8(&response).unwrap();
    assert!(text.starts_with("HTTP/1.1 200"), "got {text:?}");

    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();

    let snap = metrics.snapshot();
    assert!(
        snap.request_body_high_water > 0,
        "must have charged at least one chunk's worth"
    );
    assert!(
        snap.request_body_high_water <= 4096,
        "high water bounded by per-syscall ceiling READ_CHUNK; got {}",
        snap.request_body_high_water
    );
    assert!(
        snap.drained(),
        "every charge must release after stream completes; got {snap:?}"
    );
    assert_eq!(snap.body_io_error_count, 0, "happy path: no IO error");
    assert_eq!(snap.body_full_count, 0);
    assert_eq!(snap.body_timeout_count, 0);
}

#[test]
fn streaming_response_under_produces_records_io_error() {
    // Source declares 4 chunks but only yields 2 then Eof. The wire
    // body is shorter than declared Content-Length — that is a
    // truncation event from the metrics' view.
    let metrics = BodyMetrics::new();
    let chunk_size = 256;
    let yielded_chunks = 2;
    let declared_chunks = 4;

    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let producer = runtime
        .register_with_capacity::<ChunkProducer, Infallible>(
            ChunkProducer {
                chunk_size,
                total_chunks: yielded_chunks,
                yielded: 0,
            },
            16,
        )
        .expect("register producer");
    let service = runtime
        .register_with_capacity::<StreamingService, Infallible>(
            StreamingService {
                producer,
                content_length: chunk_size * declared_chunks,
            },
            16,
        )
        .expect("register service");

    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listener_isolate = HttpListener::<TestShard>::new(
        bind,
        service,
        HttpLimits::default(),
        Duration::from_secs(2),
        16,
    )
    .with_metrics(metrics.clone());
    let listener = runtime
        .register_with_capacity::<HttpListener<TestShard>, _>(listener_isolate, 8)
        .expect("register listener");
    let bound = runtime.observe_next_bound();
    runtime.try_send(listener, HttpListenerMsg::Start).unwrap();
    let addr = bound.wait(Duration::from_secs(2)).expect("bound");

    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(b"GET /big HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    stream.flush().unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).unwrap();

    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();

    let snap = metrics.snapshot();
    assert_eq!(
        snap.body_io_error_count, 1,
        "early Eof against a declared length must record one IO error; got {snap:?}"
    );
    assert!(snap.drained(), "still must drain on close; got {snap:?}");
}
