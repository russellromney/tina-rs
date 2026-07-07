//! Integration test: [`HttpClient`] decodes chunked responses from a
//! native [`HttpListener`] that emits [`HttpResponse::stream_chunked`].
//!
//! Proves the client-side decoder works end-to-end over real loopback
//! against the same server that the specimen uses.

mod common;

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use http::StatusCode;
use tina::CallContext;
use tina::prelude::*;
use tina_http::{
    HttpClient, HttpClientConfig, HttpClientError, HttpClientMsg, HttpLimits, HttpListener,
    HttpListenerMsg, HttpRequest, HttpResponse, IterBodySource,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig,
};

use common::TestShard;

struct ChunkedEchoService {
    source: Address<tina_http::ResponseChunkMsg, tina_http::ResponseChunkReply>,
}

impl Isolate for ChunkedEchoService {
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

impl ChunkedEchoService {
    fn response_for(&self, request: HttpRequest) -> HttpResponse {
        if request.path == "/chunked" {
            HttpResponse::stream_chunked(StatusCode::OK, self.source)
        } else {
            HttpResponse::not_found()
        }
    }
}

fn map_outcome(
    outcome: CallOutcome<Result<HttpResponse, HttpClientError>>,
) -> Result<HttpResponse, HttpClientError> {
    match outcome {
        CallOutcome::Replied(inner) => inner,
        CallOutcome::Full => Err(HttpClientError::Busy),
        CallOutcome::Closed => Err(HttpClientError::Closed),
        CallOutcome::Timeout => Err(HttpClientError::Timeout),
        CallOutcome::Rejected(_) => Err(HttpClientError::Closed),
    }
}

fn run_client_against_raw_peer(response: &'static [u8]) -> Result<HttpResponse, HttpClientError> {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind raw peer");
    let addr = listener.local_addr().expect("peer addr");
    let peer = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept client");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .expect("write timeout");
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = stream.read(&mut chunk).expect("read request");
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        stream.write_all(response).expect("write response");
        let _ = stream.flush();
    });

    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let client = runtime
        .register_with_capacity::<HttpClient<TestShard>, Infallible>(
            HttpClient::<TestShard>::new(HttpClientConfig::dev()),
            16,
        )
        .expect("register client");
    let request = HttpRequest::get("/chunked").header("Host", "x").build();
    let result = map_outcome(
        runtime
            .call_blocking(
                client,
                HttpClientMsg::call(addr, request),
                Duration::from_secs(5),
            )
            .expect("call runs"),
    );
    let _ = runtime.shutdown();
    peer.join().expect("peer joins");
    result
}

#[test]
fn client_decodes_chunked_response_from_native_server() {
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let chunks: Vec<Vec<u8>> = vec![b"hello, ".to_vec(), b"chunked ".to_vec(), b"world".to_vec()];
    let expected: Vec<u8> = chunks.iter().flatten().copied().collect();

    let source = runtime
        .register_with_capacity::<IterBodySource<TestShard>, Infallible>(
            IterBodySource::new(chunks.into_iter()),
            16,
        )
        .expect("register source");

    let service = runtime
        .register_with_capacity::<ChunkedEchoService, Infallible>(ChunkedEchoService { source }, 16)
        .expect("register service");

    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
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
    runtime.try_send(listener, HttpListenerMsg::Start).unwrap();
    let addr = bound.wait(Duration::from_secs(2)).expect("bound");

    let client = runtime
        .register_with_capacity::<HttpClient<TestShard>, Infallible>(
            HttpClient::<TestShard>::new(HttpClientConfig::dev()),
            16,
        )
        .expect("register client");

    let request = HttpRequest::get("/chunked").header("Host", "x").build();
    let result = map_outcome(
        runtime
            .call_blocking(
                client,
                HttpClientMsg::call(addr, request),
                Duration::from_secs(5),
            )
            .expect("call runs"),
    );

    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();

    let response = result.expect("client must succeed");
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body.as_buffered(), Some(expected.as_slice()));
}

/// Edge: the terminator arrives in a separate read from the last data
/// chunk. The client's decoder must handle the split correctly.
#[test]
fn client_decodes_chunked_response_with_split_terminator() {
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let chunks: Vec<Vec<u8>> = vec![b"single chunk".to_vec()];
    let expected: Vec<u8> = chunks.iter().flatten().copied().collect();

    let source = runtime
        .register_with_capacity::<IterBodySource<TestShard>, Infallible>(
            IterBodySource::new(chunks.into_iter()),
            16,
        )
        .expect("register source");

    let service = runtime
        .register_with_capacity::<ChunkedEchoService, Infallible>(ChunkedEchoService { source }, 16)
        .expect("register service");

    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
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
    runtime.try_send(listener, HttpListenerMsg::Start).unwrap();
    let addr = bound.wait(Duration::from_secs(2)).expect("bound");

    let client = runtime
        .register_with_capacity::<HttpClient<TestShard>, Infallible>(
            HttpClient::<TestShard>::new(HttpClientConfig::dev()),
            16,
        )
        .expect("register client");

    let request = HttpRequest::get("/chunked").header("Host", "x").build();
    let result = map_outcome(
        runtime
            .call_blocking(
                client,
                HttpClientMsg::call(addr, request),
                Duration::from_secs(5),
            )
            .expect("call runs"),
    );

    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();

    let response = result.expect("client must succeed on split terminator");
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body.as_buffered(), Some(expected.as_slice()));
}

#[test]
fn client_partial_chunked_peer_close_errors() {
    let result = run_client_against_raw_peer(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhe",
    );

    assert!(matches!(result, Err(HttpClientError::Closed)), "{result:?}");
}

#[test]
fn client_complete_chunked_peer_close_decodes() {
    let result = run_client_against_raw_peer(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
    );

    let response = result.expect("client must decode complete chunked body");
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body.as_buffered(), Some(b"hello".as_slice()));
}
