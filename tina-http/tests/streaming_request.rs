//! Streaming request body integration test.
//!
//! Connection is configured with `inbound_stream_chunk_size = Some(N)`.
//! A multi-turn service receives `HttpRequest::Stream { source, ... }`,
//! pulls chunks via `call(source, HttpConnectionMsg::RequestBodyNext,
//! t).reply(MyMsg::ChunkArrived)` until `Eof`, accumulates a hash, and
//! replies with the byte count + checksum.

mod common;

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use http::StatusCode;
use tina::prelude::*;
use tina_http::{
    HttpConnectionMsg, HttpLimits, HttpListener, HttpListenerMsg, HttpRequest, HttpRequestBody,
    HttpResponse, RequestChunkReply,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime,
    ThreadedRuntimeConfig, call,
};

use common::TestShard;

#[derive(Debug, Clone)]
enum ConsumerMsg {
    Inbound(HttpRequest),
    ChunkArrived(CallOutcome<RequestChunkReply>),
}

impl From<HttpRequest> for ConsumerMsg {
    fn from(req: HttpRequest) -> Self {
        Self::Inbound(req)
    }
}

struct Consumer {
    chunk_call_timeout: Duration,
    pending_source: Option<Address<HttpConnectionMsg, RequestChunkReply>>,
    accumulated: Vec<u8>,
    expected_length: usize,
}

impl Isolate for Consumer {
    tina::isolate_types! {
        message: ConsumerMsg,
        reply: HttpResponse,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<ConsumerMsg>,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: ConsumerMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ConsumerMsg::Inbound(req) => match req.body {
                HttpRequestBody::Buffered(bytes) => {
                    // Sanity: with streaming threshold set the connection
                    // should hand us a Stream variant for non-empty bodies.
                    // If we receive Buffered, fall back to summarising it.
                    let body_summary =
                        format!("buffered:{}", bytes.iter().fold(0u64, |a, b| a + *b as u64));
                    reply(HttpResponse::with_text(StatusCode::OK, body_summary))
                }
                HttpRequestBody::Stream(stream) => {
                    self.expected_length = stream.content_length;
                    self.accumulated.clear();
                    self.pending_source = Some(stream.source);
                    call(
                        stream.source,
                        HttpConnectionMsg::body_next(),
                        self.chunk_call_timeout,
                    )
                    .reply(ConsumerMsg::ChunkArrived)
                }
            },
            ConsumerMsg::ChunkArrived(outcome) => match outcome {
                CallOutcome::Replied(RequestChunkReply::Chunk(bytes)) => {
                    self.accumulated.extend_from_slice(&bytes);
                    let source = self
                        .pending_source
                        .expect("source set during streaming consume");
                    call(
                        source,
                        HttpConnectionMsg::body_next(),
                        self.chunk_call_timeout,
                    )
                    .reply(ConsumerMsg::ChunkArrived)
                }
                CallOutcome::Replied(RequestChunkReply::Eof) => {
                    let total = self.accumulated.len();
                    let checksum: u64 = self.accumulated.iter().fold(0u64, |a, b| a + *b as u64);
                    let body = format!("stream:{}:{}", total, checksum);
                    self.pending_source = None;
                    self.accumulated.clear();
                    reply(HttpResponse::with_text(StatusCode::OK, body))
                }
                CallOutcome::Full | CallOutcome::Closed | CallOutcome::Timeout => {
                    self.pending_source = None;
                    reply(HttpResponse::internal_error())
                }
            },
        }
    }
}

#[test]
fn streaming_request_consumed_chunk_by_chunk() {
    let chunk_size = 256;
    // Body: 4096 bytes, byte i is `(i % 251) as u8`.
    let body: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    let expected_checksum: u64 = body.iter().fold(0u64, |a, b| a + *b as u64);

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
        .register_with_capacity::<Consumer, Infallible>(
            Consumer {
                chunk_call_timeout: Duration::from_secs(2),
                pending_source: None,
                accumulated: Vec::new(),
                expected_length: 0,
            },
            32,
        )
        .expect("register consumer");

    let limits = HttpLimits {
        inbound_stream_chunk_size: Some(chunk_size),
        ..HttpLimits::default()
    };
    let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback");
    let listener_isolate = HttpListener::<TestShard, ConsumerMsg>::new(
        bind,
        consumer,
        limits,
        Duration::from_secs(5),
        16,
    );
    let listener = runtime
        .register_with_capacity::<HttpListener<TestShard, ConsumerMsg>, _>(listener_isolate, 8)
        .expect("register listener");
    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener, HttpListenerMsg::Start)
        .expect("send Start");
    let addr = bound.wait(Duration::from_secs(2)).expect("bound");

    let mut request = format!(
        "POST /upload HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(&body);

    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    stream.write_all(&request).expect("write request");
    stream.flush().expect("flush");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");

    let text = std::str::from_utf8(&response).expect("utf8");
    assert!(
        text.starts_with("HTTP/1.1 200 OK\r\n"),
        "expected 200, got: {text}"
    );
    let body_part = text
        .rsplit_once("\r\n\r\n")
        .map(|(_, b)| b)
        .expect("body separator");
    let expected = format!("stream:{}:{}", body.len(), expected_checksum);
    assert_eq!(body_part, expected, "consumer summary mismatch");

    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();
}

/// Test that dispatch happens *before* the full body lands on the
/// wire — this is the real-streaming property. Sends the request head
/// plus a small body prefix, waits for the service to mark itself
/// dispatched (signal flips when `Inbound` arrives at the consumer),
/// then sends the rest of the body.
#[test]
fn streaming_request_dispatches_before_full_body_arrives() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Instant;

    let chunk_size = 256;
    let body: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    let expected_checksum: u64 = body.iter().fold(0u64, |a, b| a + *b as u64);

    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let dispatched = Arc::new(AtomicBool::new(false));
    let consumer = runtime
        .register_with_capacity::<NotifyingConsumer, Infallible>(
            NotifyingConsumer {
                chunk_call_timeout: Duration::from_secs(2),
                pending_source: None,
                accumulated: Vec::new(),
                expected_length: 0,
                dispatched: Arc::clone(&dispatched),
            },
            32,
        )
        .expect("register consumer");

    let limits = HttpLimits {
        inbound_stream_chunk_size: Some(chunk_size),
        ..HttpLimits::default()
    };
    let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback");
    let listener_isolate = HttpListener::<TestShard, NotifyingMsg>::new(
        bind,
        consumer,
        limits,
        Duration::from_secs(5),
        16,
    );
    let listener = runtime
        .register_with_capacity::<HttpListener<TestShard, NotifyingMsg>, _>(listener_isolate, 8)
        .expect("register listener");
    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener, HttpListenerMsg::Start)
        .expect("send Start");
    let addr = bound.wait(Duration::from_secs(2)).expect("bound");

    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");

    // Phase 1: send the head and a small prefix of the body. With real
    // streaming the connection dispatches as soon as the head parses;
    // it does NOT wait for `Content-Length` bytes to arrive.
    let head = format!(
        "POST /upload HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).expect("write head");
    let phase1_bytes = 128;
    stream
        .write_all(&body[..phase1_bytes])
        .expect("write phase 1 body");
    stream.flush().expect("flush phase 1");

    // Wait for the consumer to mark dispatched. With real streaming the
    // service receives `Inbound` before the rest of the body is on the
    // wire. Bound the wait so a regression to buffer-then-dispatch
    // fails the test rather than hanging.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !dispatched.load(Ordering::Acquire) {
        if Instant::now() > deadline {
            panic!(
                "service did not receive Inbound before full body sent — \
                 connection still buffering whole body before dispatch"
            );
        }
        thread::sleep(Duration::from_millis(5));
    }

    // Phase 2: send the rest of the body. The consumer is already
    // pulling chunks; these bytes complete the stream.
    stream
        .write_all(&body[phase1_bytes..])
        .expect("write phase 2 body");
    stream.flush().expect("flush phase 2");

    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");

    let text = std::str::from_utf8(&response).expect("utf8");
    assert!(
        text.starts_with("HTTP/1.1 200 OK\r\n"),
        "expected 200, got: {text}"
    );
    let body_part = text
        .rsplit_once("\r\n\r\n")
        .map(|(_, b)| b)
        .expect("body separator");
    let expected = format!("stream:{}:{}", body.len(), expected_checksum);
    assert_eq!(
        body_part, expected,
        "consumer summary mismatch after phased upload"
    );

    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();
}

#[derive(Debug, Clone)]
enum NotifyingMsg {
    Inbound(HttpRequest),
    ChunkArrived(CallOutcome<RequestChunkReply>),
}

impl From<HttpRequest> for NotifyingMsg {
    fn from(req: HttpRequest) -> Self {
        Self::Inbound(req)
    }
}

struct NotifyingConsumer {
    chunk_call_timeout: Duration,
    pending_source: Option<Address<HttpConnectionMsg, RequestChunkReply>>,
    accumulated: Vec<u8>,
    expected_length: usize,
    dispatched: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Isolate for NotifyingConsumer {
    tina::isolate_types! {
        message: NotifyingMsg,
        reply: HttpResponse,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<NotifyingMsg>,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: NotifyingMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            NotifyingMsg::Inbound(req) => {
                self.dispatched
                    .store(true, std::sync::atomic::Ordering::Release);
                match req.body {
                    HttpRequestBody::Buffered(_) => {
                        // If we receive Buffered, streaming is broken
                        // (test harness configured streaming on).
                        reply(HttpResponse::internal_error())
                    }
                    HttpRequestBody::Stream(stream) => {
                        self.expected_length = stream.content_length;
                        self.accumulated.clear();
                        self.pending_source = Some(stream.source);
                        call(
                            stream.source,
                            HttpConnectionMsg::body_next(),
                            self.chunk_call_timeout,
                        )
                        .reply(NotifyingMsg::ChunkArrived)
                    }
                }
            }
            NotifyingMsg::ChunkArrived(outcome) => match outcome {
                CallOutcome::Replied(RequestChunkReply::Chunk(bytes)) => {
                    self.accumulated.extend_from_slice(&bytes);
                    let source = self
                        .pending_source
                        .expect("source set during streaming consume");
                    call(
                        source,
                        HttpConnectionMsg::body_next(),
                        self.chunk_call_timeout,
                    )
                    .reply(NotifyingMsg::ChunkArrived)
                }
                CallOutcome::Replied(RequestChunkReply::Eof) => {
                    let total = self.accumulated.len();
                    let checksum: u64 = self.accumulated.iter().fold(0u64, |a, b| a + *b as u64);
                    let body = format!("stream:{total}:{checksum}");
                    self.pending_source = None;
                    self.accumulated.clear();
                    reply(HttpResponse::with_text(StatusCode::OK, body))
                }
                CallOutcome::Full | CallOutcome::Closed | CallOutcome::Timeout => {
                    self.pending_source = None;
                    reply(HttpResponse::internal_error())
                }
            },
        }
    }
}
