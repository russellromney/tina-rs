//! Lifecycle tests for connections with bodies in flight.
//!
//! - Stop the listener mid-stream and confirm the runtime drains
//!   without leaking body charges in the metrics.
//! - Force a runtime shutdown while a streamed response is being
//!   produced. Either the response completes or it doesn't, but
//!   `BodyMetrics::drained()` must hold once shutdown returns.

mod common;

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use http::StatusCode;
use tina::CallContext;
use tina::prelude::*;
use tina_http::{
    BodyMetrics, HttpLimits, HttpListener, HttpListenerMsg, HttpRequest, HttpResponse,
    ResponseChunkMsg, ResponseChunkReply, ResponseStream,
};
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig};

use common::TestShard;

/// Slow producer: waits a few ms between chunks so a Stop sent
/// during streaming can interleave between writes. Records whether
/// it ever produced any chunks so the test can confirm streaming
/// actually started before the listener was stopped.
struct SlowChunkProducer {
    chunk_size: usize,
    total_chunks: usize,
    yielded: usize,
    saw_progress: Arc<AtomicBool>,
    inter_chunk_pause: Duration,
}

impl Isolate for SlowChunkProducer {
    tina::isolate_types! {
        message: ResponseChunkMsg,
        reply: ResponseChunkReply,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        io: Infallible,
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

impl SlowChunkProducer {
    fn next_chunk(&mut self, _msg: ResponseChunkMsg) -> ResponseChunkReply {
        if self.yielded >= self.total_chunks {
            return ResponseChunkReply::Eof;
        }
        // Mark progress so the test knows streaming actually began.
        self.saw_progress.store(true, Ordering::Release);
        let i = self.yielded;
        self.yielded += 1;
        // Block the isolate briefly. Crude but sufficient — the
        // runtime's chunked write loop pauses while this thread
        // sleeps. Real backpressure would race a sleep call.
        std::thread::sleep(self.inter_chunk_pause);
        ResponseChunkReply::Chunk(vec![(i % 251) as u8; self.chunk_size])
    }
}

struct StreamingService {
    producer: Address<ResponseChunkMsg, ResponseChunkReply>,
    content_length: usize,
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

impl Isolate for StreamingService {
    tina::isolate_types! {
        message: HttpRequest,
        reply: HttpResponse,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        io: Infallible,
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

#[test]
fn shutdown_while_streaming_response_drains_body_metrics() {
    // Service streams a large body chunk by chunk with deliberate
    // inter-chunk pauses. The test client reads the head and then
    // closes the TCP connection. Independent of which chunk is
    // in-flight at that moment, runtime shutdown must release every
    // body charge.
    let metrics = BodyMetrics::new();
    let saw_progress = Arc::new(AtomicBool::new(false));
    let chunk_size = 4 * 1024;
    let total_chunks = 32;
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
        .register_with_capacity::<SlowChunkProducer, Infallible>(
            SlowChunkProducer {
                chunk_size,
                total_chunks,
                yielded: 0,
                saw_progress: Arc::clone(&saw_progress),
                inter_chunk_pause: Duration::from_millis(20),
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
        Duration::from_secs(5),
        16,
    )
    .with_metrics(metrics.clone());
    let listener = runtime
        .register_with_capacity::<HttpListener<TestShard>, _>(listener_isolate, 8)
        .expect("register listener");
    let bound = runtime
        .observe_next_bound()
        .expect("register bind observer");
    runtime.try_send(listener, HttpListenerMsg::Start).unwrap();
    let addr = bound.wait(Duration::from_secs(2)).expect("bound");

    // Open a TCP connection, send the request, read the head, then
    // drop without finishing the body. The connection isolate is
    // mid-stream when the client disappears.
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(b"GET /big HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    stream.flush().unwrap();

    // Read the head only — enough to know streaming has started.
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).unwrap();
    assert!(n > 0, "should at least read the response head");

    // Wait briefly so progress is made, then stop the listener and
    // shut the runtime down. Drop the client mid-stream.
    std::thread::sleep(Duration::from_millis(80));
    drop(stream);

    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();

    assert!(
        saw_progress.load(Ordering::Acquire),
        "the producer should have yielded at least one chunk before shutdown"
    );

    let snap = metrics.snapshot();
    assert!(
        snap.drained(),
        "body charges must release on shutdown even mid-stream; got {snap:?}"
    );
    // The connection only pulls the next chunk after the previous
    // one fully drains, so peak charge equals one chunk exactly.
    assert!(
        snap.response_body_high_water > 0,
        "high water > 0 after some chunks streamed"
    );
    assert_eq!(
        snap.response_body_high_water, chunk_size,
        "in-flight chunk caps high water at exactly one chunk; got {}",
        snap.response_body_high_water
    );
}

#[test]
fn stop_listener_during_active_request_does_not_leak_metrics() {
    // Sends a small POST, lets the service handle it, and then
    // stops the listener. After shutdown all metrics should drain.
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

    let counter = runtime
        .register_with_capacity::<common::Counter, Infallible>(common::Counter::default(), 16)
        .expect("register counter");

    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listener_isolate = HttpListener::<TestShard>::new(
        bind,
        counter,
        HttpLimits::default(),
        Duration::from_secs(2),
        16,
    )
    .with_metrics(metrics.clone());
    let listener = runtime
        .register_with_capacity::<HttpListener<TestShard>, _>(listener_isolate, 8)
        .expect("register listener");
    let bound = runtime
        .observe_next_bound()
        .expect("register bind observer");
    runtime.try_send(listener, HttpListenerMsg::Start).unwrap();
    let addr = bound.wait(Duration::from_secs(2)).expect("bound");

    let body = vec![b'a'; 256];
    let mut request = format!(
        "POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(&body);
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream.write_all(&request).unwrap();
    stream.flush().unwrap();
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);

    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();

    let snap = metrics.snapshot();
    assert!(
        snap.drained(),
        "Stop + shutdown must drain all body charges; got {snap:?}"
    );
}
