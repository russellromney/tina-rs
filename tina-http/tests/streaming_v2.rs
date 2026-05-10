//! Tests for the upward-shifted streaming surface:
//!
//! - `IterBodySource` — wrapping a closure-iterator into a chunk
//!   source, no custom `Isolate` impl.
//! - `HttpResponse::stream_known_length` — loud-API constructor for
//!   the `Content-Length` framing path.
//! - `HttpResponse::stream_chunked` — loud-API constructor for the
//!   `Transfer-Encoding: chunked` framing path. Wire correctness
//!   verified by a small chunked decoder local to this file.
//! - Mid-stream client close visibility through `body_io_error_count`.

mod common;

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use http::StatusCode;
use tina::prelude::*;
use tina_http::{
    BodyMetrics, HttpLimits, HttpListener, HttpListenerMsg, HttpRequest, HttpResponse,
    IterBodySource,
};
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig};

use common::TestShard;

/// One service that knows two routes:
/// - `GET /known` returns a known-length stream from
///   `IterBodySource` (4 chunks of `chunk_size` bytes).
/// - `GET /chunked` returns a chunked stream from
///   `IterBodySource` (the same iterator).
struct StreamRouter {
    known_source: Address<tina_http::ResponseChunkMsg, tina_http::ResponseChunkReply>,
    chunked_source: Address<tina_http::ResponseChunkMsg, tina_http::ResponseChunkReply>,
    known_length: usize,
}

impl Isolate for StreamRouter {
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
        let response = match request.path.as_str() {
            "/known" => HttpResponse::stream_known_length(
                StatusCode::OK,
                self.known_length,
                self.known_source,
            ),
            "/chunked" => HttpResponse::stream_chunked(StatusCode::OK, self.chunked_source),
            _ => HttpResponse::not_found(),
        };
        reply(response)
    }
}

fn build_runtime() -> ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> {
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

#[test]
fn iter_body_source_serves_known_length_response_via_loud_api() {
    let metrics = BodyMetrics::new();
    let chunk_size = 1024;
    let total_chunks = 4;
    let total_bytes = chunk_size * total_chunks;

    let runtime = build_runtime();

    let known_chunks = (0..total_chunks).map(move |i| vec![(i % 251) as u8; chunk_size]);
    let known_source = runtime
        .register_with_capacity::<IterBodySource<TestShard>, Infallible>(
            IterBodySource::new(known_chunks),
            16,
        )
        .expect("register known-length source");

    // Routes-only test for known-length here. Chunked source unused.
    let chunked_chunks: Vec<Vec<u8>> = Vec::new();
    let chunked_source = runtime
        .register_with_capacity::<IterBodySource<TestShard>, Infallible>(
            IterBodySource::new(chunked_chunks.into_iter()),
            16,
        )
        .expect("register chunked source");

    let service = runtime
        .register_with_capacity::<StreamRouter, Infallible>(
            StreamRouter {
                known_source,
                chunked_source,
                known_length: total_bytes,
            },
            16,
        )
        .expect("register service");

    let listener_isolate = HttpListener::<TestShard>::new(
        "127.0.0.1:0".parse().unwrap(),
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

    let response = scripted_get(addr, "/known");
    let (head, body) = split_head_body(&response);
    assert!(
        head.contains(&format!("Content-Length: {total_bytes}\r\n")),
        "head must declare Content-Length, got {head:?}"
    );
    assert!(
        !head.contains("Transfer-Encoding"),
        "known-length must not emit Transfer-Encoding; got {head:?}"
    );
    assert_eq!(body.len(), total_bytes, "wire body matches declared length");

    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();

    let snap = metrics.snapshot();
    assert!(snap.drained());
    assert_eq!(
        snap.response_body_high_water, chunk_size,
        "peak body in flight is exactly one chunk"
    );
    assert_eq!(snap.body_io_error_count, 0);
}

#[test]
fn stream_chunked_emits_transfer_encoding_chunked_with_terminator() {
    let metrics = BodyMetrics::new();
    let chunks_data: Vec<Vec<u8>> =
        vec![b"hello, ".to_vec(), b"chunked ".to_vec(), b"world".to_vec()];
    let total_decoded: Vec<u8> = chunks_data.iter().flatten().copied().collect();

    let runtime = build_runtime();

    // Empty known source — only chunked is exercised.
    let known_source = runtime
        .register_with_capacity::<IterBodySource<TestShard>, Infallible>(
            IterBodySource::new(std::iter::empty::<Vec<u8>>()),
            16,
        )
        .expect("register dummy known source");

    let chunked_source = runtime
        .register_with_capacity::<IterBodySource<TestShard>, Infallible>(
            IterBodySource::new(chunks_data.into_iter()),
            16,
        )
        .expect("register chunked source");

    let service = runtime
        .register_with_capacity::<StreamRouter, Infallible>(
            StreamRouter {
                known_source,
                chunked_source,
                known_length: 0,
            },
            16,
        )
        .expect("register service");

    let listener_isolate = HttpListener::<TestShard>::new(
        "127.0.0.1:0".parse().unwrap(),
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

    let response = scripted_get(addr, "/chunked");
    let (head, body_wire) = split_head_body(&response);
    assert!(
        head.contains("Transfer-Encoding: chunked\r\n"),
        "chunked response must declare Transfer-Encoding: chunked; got {head:?}"
    );
    assert!(
        !head.to_lowercase().contains("content-length"),
        "chunked response must not emit Content-Length; got {head:?}"
    );

    let decoded = decode_chunked(body_wire).expect("body parses as chunked");
    assert_eq!(
        decoded, total_decoded,
        "decoded chunked body matches concatenated source chunks"
    );

    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();

    let snap = metrics.snapshot();
    assert!(snap.drained());
    // Peak charge is exactly the largest single chunk because the
    // connection pulls chunk N+1 only after chunk N drains. The
    // chunks here are 7, 8, 5 bytes; 8 is the max.
    let largest_chunk = vec![b"hello, ".len(), b"chunked ".len(), b"world".len()]
        .into_iter()
        .max()
        .unwrap();
    assert_eq!(largest_chunk, 8);
    assert_eq!(
        snap.response_body_high_water, largest_chunk,
        "peak body in flight equals the largest single chunk; \
         got {}, expected {}",
        snap.response_body_high_water, largest_chunk
    );
    assert_eq!(snap.body_io_error_count, 0, "chunked happy path");
}

#[test]
fn early_client_close_during_chunked_records_body_io_error() {
    // Chunked stream serving a slow producer. Client closes after
    // reading the head. Server's next `tcp_write` fails — that's a
    // mid-stream truncation event and must be visible in metrics.
    let metrics = BodyMetrics::new();

    let runtime = build_runtime();

    // 64 chunks of 4 KiB so the wire definitely backs up against
    // the slow / closed reader before the source drains.
    let chunks = (0..64u32).map(|i| vec![(i % 251) as u8; 4 * 1024]);
    let known_source = runtime
        .register_with_capacity::<IterBodySource<TestShard>, Infallible>(
            IterBodySource::new(std::iter::empty::<Vec<u8>>()),
            16,
        )
        .expect("register dummy known source");
    let chunked_source = runtime
        .register_with_capacity::<IterBodySource<TestShard>, Infallible>(
            IterBodySource::new(chunks),
            16,
        )
        .expect("register chunked source");

    let service = runtime
        .register_with_capacity::<StreamRouter, Infallible>(
            StreamRouter {
                known_source,
                chunked_source,
                known_length: 0,
            },
            16,
        )
        .expect("register service");

    let listener_isolate = HttpListener::<TestShard>::new(
        "127.0.0.1:0".parse().unwrap(),
        service,
        HttpLimits::default(),
        Duration::from_secs(5),
        16,
    )
    .with_metrics(metrics.clone());
    let listener = runtime
        .register_with_capacity::<HttpListener<TestShard>, _>(listener_isolate, 8)
        .expect("register listener");
    let bound = runtime.observe_next_bound();
    runtime.try_send(listener, HttpListenerMsg::Start).unwrap();
    let addr = bound.wait(Duration::from_secs(2)).expect("bound");

    // Open, send request, read just the head, then drop.
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(b"GET /chunked HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    stream.flush().unwrap();
    // Read just enough to know the head landed, then drop.
    let mut buf = [0u8; 1024];
    let _ = stream.read(&mut buf).unwrap();
    drop(stream);

    // Give the runtime a moment to attempt subsequent writes and
    // observe the failure.
    std::thread::sleep(Duration::from_millis(100));

    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();

    let snap = metrics.snapshot();
    assert!(
        snap.drained(),
        "all charges release on shutdown even mid-stream; got {snap:?}"
    );
    // Bounded both ways: at least one io_error (the client did
    // close mid-stream), at most a few — a regression that
    // counted per byte rather than per failed write would push
    // this well over 4.
    assert!(
        (1..=4).contains(&snap.body_io_error_count),
        "client close mid-stream must surface as 1..=4 body_io_errors; got {snap:?}"
    );
}

// --- helpers ----------------------------------------------------

fn scripted_get(addr: SocketAddr, path: &str) -> Vec<u8> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    response
}

/// Splits a wire response into ASCII head + raw body bytes.
fn split_head_body(response: &[u8]) -> (String, &[u8]) {
    let separator = b"\r\n\r\n";
    let head_end = response
        .windows(separator.len())
        .position(|w| w == separator)
        .expect("response has CRLFCRLF");
    let head = std::str::from_utf8(&response[..head_end])
        .expect("head is ASCII")
        .to_owned();
    let body = &response[head_end + separator.len()..];
    (head, body)
}

/// Minimal chunked-transfer-encoding decoder. Reads
/// `[hex size]\r\n[bytes]\r\n` chunks until a `0\r\n\r\n`
/// terminator. Returns the concatenated data bytes.
fn decode_chunked(mut bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let crlf = bytes
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| "chunked: missing size CRLF".to_string())?;
        let size_str = std::str::from_utf8(&bytes[..crlf])
            .map_err(|_| "chunked: size line is not ASCII".to_string())?;
        // Allow optional ";extension" tail per the spec; we don't
        // emit any, but be lenient when reading.
        let size_str = size_str.split(';').next().unwrap_or("");
        let size = usize::from_str_radix(size_str.trim(), 16)
            .map_err(|e| format!("chunked: bad size {size_str:?}: {e}"))?;
        bytes = &bytes[crlf + 2..];
        if size == 0 {
            // Trailing CRLF after the zero-size line ends the body.
            // We accept either "0\r\n\r\n" or "0\r\n" + trailers + "\r\n".
            return Ok(out);
        }
        if bytes.len() < size + 2 {
            return Err(format!(
                "chunked: truncated chunk (size={size}, remaining={})",
                bytes.len()
            ));
        }
        out.extend_from_slice(&bytes[..size]);
        if &bytes[size..size + 2] != b"\r\n" {
            return Err("chunked: missing CRLF after data".to_string());
        }
        bytes = &bytes[size + 2..];
    }
}

#[test]
fn chunked_decoder_round_trips_a_known_payload() {
    // Sanity check the helper itself before trusting it on real wires.
    let wire = b"5\r\nhello\r\n7\r\n, world\r\n0\r\n\r\n";
    let decoded = decode_chunked(wire).unwrap();
    assert_eq!(decoded, b"hello, world");
}
