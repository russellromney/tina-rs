//! Streaming response body integration test.
//!
//! A service replies with `HttpResponse::with_stream(...)` pointing at a
//! `ChunkProducer` isolate. The connection isolate writes the head with
//! the declared `Content-Length`, then pulls chunks via `call(source,
//! Next, t).reply(StreamChunk)` and writes each one to the wire. Each
//! pull happens *after* the previous chunk has fully drained — this is
//! the per-chunk backpressure rock 4 calls for.

mod common;

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use http::StatusCode;
use tina::CallContext;
use tina::prelude::*;
use tina_http::{
    HttpLimits, HttpListener, HttpListenerMsg, HttpRequest, HttpResponse, ResponseChunkMsg,
    ResponseChunkReply, ResponseStream,
};
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig};

use common::TestShard;

/// Yields `total_chunks` chunks of `chunk_size` bytes each, where the
/// `i`-th chunk is filled with `(i % 251) as u8`. Then `Eof`.
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
        msg: ResponseChunkMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        reply(self.next_chunk(msg))
    }

    fn handle_call(&mut self, msg: ResponseChunkMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(self.next_chunk(msg))
    }
}

impl ChunkProducer {
    fn next_chunk(&mut self, _msg: ResponseChunkMsg) -> ResponseChunkReply {
        if self.yielded >= self.total_chunks {
            return ResponseChunkReply::Eof;
        }
        let i = self.yielded;
        self.yielded += 1;
        let byte = (i % 251) as u8;
        let chunk = vec![byte; self.chunk_size];
        ResponseChunkReply::Chunk(chunk)
    }
}

/// Service whose `/big` route replies with a streaming body backed by a
/// ChunkProducer.
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
        reply(self.response_for(request))
    }

    fn handle_call(&mut self, request: HttpRequest, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(self.response_for(request))
    }
}

impl StreamingService {
    fn response_for(&self, request: HttpRequest) -> HttpResponse {
        if request.path == "/big" {
            HttpResponse::with_stream(
                StatusCode::OK,
                ResponseStream {
                    content_length: self.content_length,
                    source: self.producer,
                },
            )
        } else {
            HttpResponse::not_found()
        }
    }
}

#[test]
fn streaming_response_writes_full_body_chunk_by_chunk() {
    let chunk_size = 4096;
    let total_chunks = 16;
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

    let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback");
    let listener_isolate = HttpListener::<TestShard>::new(
        bind,
        service,
        HttpLimits::default(),
        Duration::from_secs(2),
        16,
    );
    let listener = runtime
        .register_with_capacity::<HttpListener<TestShard>, _>(listener_isolate, 8)
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
    stream
        .write_all(b"GET /big HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .expect("write request");
    stream.flush().expect("flush");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).expect("read response");

    let separator = b"\r\n\r\n";
    let head_end = buf
        .windows(separator.len())
        .position(|w| w == separator)
        .expect("response has CRLFCRLF");
    let head = std::str::from_utf8(&buf[..head_end]).expect("ASCII head");
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "head = {head:?}");
    assert!(
        head.contains(&format!("Content-Length: {total_bytes}\r\n")),
        "head missing Content-Length: {head:?}"
    );

    let body = &buf[head_end + separator.len()..];
    assert_eq!(body.len(), total_bytes, "body length mismatch");

    // Verify byte content matches what ChunkProducer produced.
    let mut expected = Vec::with_capacity(total_bytes);
    for i in 0..total_chunks {
        let byte = (i % 251) as u8;
        expected.extend(std::iter::repeat_n(byte, chunk_size));
    }
    assert_eq!(body, &expected[..], "streamed body content mismatch");

    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn streaming_response_handles_early_eof() {
    // Producer yields 2 chunks then Eof, but service declared
    // content_length covering 4 chunks. The connection closes the wire
    // honestly after Eof rather than continuing to pull.
    let chunk_size = 256;

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
                total_chunks: 2,
                yielded: 0,
            },
            16,
        )
        .expect("register producer");

    let service = runtime
        .register_with_capacity::<StreamingService, Infallible>(
            StreamingService {
                producer,
                // Declared length is bigger than what producer delivers.
                content_length: chunk_size * 4,
            },
            16,
        )
        .expect("register service");

    let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback");
    let listener_isolate = HttpListener::<TestShard>::new(
        bind,
        service,
        HttpLimits::default(),
        Duration::from_secs(2),
        16,
    );
    let listener = runtime
        .register_with_capacity::<HttpListener<TestShard>, _>(listener_isolate, 8)
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
    stream
        .write_all(b"GET /big HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .expect("write request");
    stream.flush().expect("flush");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).expect("read response");

    // The wire body is shorter than declared Content-Length (Eof
    // truncated). Wire framing is honest about declared length even if
    // source under-produced. Client sees truncation.
    let separator = b"\r\n\r\n";
    let head_end = buf
        .windows(separator.len())
        .position(|w| w == separator)
        .expect("response has CRLFCRLF");
    let body = &buf[head_end + separator.len()..];
    // Source produced 2 chunks of chunk_size bytes; declared 4 chunks.
    assert_eq!(body.len(), chunk_size * 2, "early-Eof truncated body");

    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();
}
