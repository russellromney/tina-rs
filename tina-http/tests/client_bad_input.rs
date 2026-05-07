//! Bad-input tests for the service-shaped HTTP/1.1 client.
//!
//! Sends carefully malformed responses from a stdlib reference server
//! and asserts the client surfaces a typed [`HttpClientError`] without
//! panicking or hanging.

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tina::prelude::*;
use tina_http::{
    HttpClient, HttpClientConfig, HttpClientError, HttpClientMsg, HttpRequest, HttpResponse,
    ResponseParseError,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime,
    ThreadedRuntimeConfig, call,
};

fn start_canned_server(canned_response: Vec<u8>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind reference server");
    let addr = listener.local_addr().expect("local addr");
    thread::spawn(move || {
        if let Ok((mut stream, _peer)) = listener.accept() {
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("set read timeout");
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(&canned_response);
            drop(stream);
        }
    });
    addr
}

#[derive(Debug, Clone)]
enum DriverMsg {
    Begin {
        client: Address<HttpClientMsg, Result<HttpResponse, HttpClientError>>,
        target: SocketAddr,
        timeout: Duration,
    },
    Returned(CallOutcome<Result<HttpResponse, HttpClientError>>),
}

struct Driver {
    sender: mpsc::Sender<Result<HttpResponse, HttpClientError>>,
}

impl Isolate for Driver {
    tina::isolate_types! {
        message: DriverMsg,
        reply: (),
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<DriverMsg>,
        shard: SingleShard,
    }

    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::Begin {
                client,
                target,
                timeout,
            } => {
                let request = HttpRequest::get("/").header("Host", "x").build();
                call(client, HttpClientMsg::call(target, request), timeout)
                    .reply(DriverMsg::Returned)
            }
            DriverMsg::Returned(outcome) => {
                let result = match outcome {
                    CallOutcome::Replied(inner) => inner,
                    CallOutcome::Full => Err(HttpClientError::Busy),
                    CallOutcome::Closed => Err(HttpClientError::Closed),
                    CallOutcome::Timeout => Err(HttpClientError::Timeout),
                };
                let _ = self.sender.send(result);
                stop()
            }
        }
    }
}

fn run_one(canned: Vec<u8>) -> Result<HttpResponse, HttpClientError> {
    let target = start_canned_server(canned);

    let runtime = ThreadedRuntime::with_config(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let client = runtime
        .register_with_capacity::<HttpClient<SingleShard>, Infallible>(
            HttpClient::<SingleShard>::new(HttpClientConfig::dev()),
            16,
        )
        .expect("register client");

    let (tx, rx) = mpsc::channel();
    let driver = runtime
        .register_with_capacity::<Driver, Infallible>(Driver { sender: tx }, 16)
        .expect("register driver");

    runtime
        .try_send(
            driver,
            DriverMsg::Begin {
                client,
                target,
                timeout: Duration::from_secs(2),
            },
        )
        .expect("send Begin");

    let result = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("driver delivers result");

    let _ = runtime.shutdown();
    result
}

#[test]
fn missing_content_length_surfaces_parse_error() {
    let canned = b"HTTP/1.1 200 OK\r\nServer: x\r\n\r\nhello".to_vec();
    let err = run_one(canned).expect_err("client must reject missing CL");
    assert!(
        matches!(
            err,
            HttpClientError::Parse(ResponseParseError::MissingContentLength)
        ),
        "expected MissingContentLength, got {err:?}"
    );
}

#[test]
fn chunked_transfer_encoding_surfaces_parse_error() {
    let canned =
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n".to_vec();
    let err = run_one(canned).expect_err("client must reject chunked");
    assert!(
        matches!(
            err,
            HttpClientError::Parse(ResponseParseError::UnsupportedTransferEncoding)
        ),
        "expected UnsupportedTransferEncoding, got {err:?}"
    );
}

#[test]
fn malformed_status_line_surfaces_parse_error() {
    let canned = b"NOT-HTTP garbage\r\n\r\n".to_vec();
    let err = run_one(canned).expect_err("client must reject malformed");
    assert!(
        matches!(
            err,
            HttpClientError::Parse(ResponseParseError::BadStatusLine)
        ),
        "expected BadStatusLine, got {err:?}"
    );
}

#[test]
fn oversized_headers_surface_parse_error() {
    let big = "x".repeat(64 * 1024);
    let canned = format!("HTTP/1.1 200 OK\r\nX-Big: {big}\r\n\r\n").into_bytes();
    let err = run_one(canned).expect_err("client must reject oversized headers");
    assert!(
        matches!(
            err,
            HttpClientError::Parse(ResponseParseError::HeadersTooLarge)
        ),
        "expected HeadersTooLarge, got {err:?}"
    );
}

#[test]
fn timeout_fires_when_server_holds_connection_open() {
    // Server accepts but never writes. Client must hit request_timeout.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind silent server");
    let addr = listener.local_addr().expect("local addr");
    let _silent_thread = thread::spawn(move || {
        if let Ok((stream, _peer)) = listener.accept() {
            // Hold the stream open without writing. Drop after a few
            // seconds so the test thread exits cleanly.
            thread::sleep(Duration::from_secs(3));
            drop(stream);
        }
    });

    let runtime = ThreadedRuntime::with_config(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let mut config = HttpClientConfig::dev();
    config.request_timeout = Duration::from_millis(200);
    let client = runtime
        .register_with_capacity::<HttpClient<SingleShard>, Infallible>(
            HttpClient::<SingleShard>::new(config),
            16,
        )
        .expect("register client");

    let (tx, rx) = mpsc::channel();
    let driver = runtime
        .register_with_capacity::<Driver, Infallible>(Driver { sender: tx }, 16)
        .expect("register driver");

    runtime
        .try_send(
            driver,
            DriverMsg::Begin {
                client,
                target: addr,
                timeout: Duration::from_secs(2),
            },
        )
        .expect("send Begin");

    let result = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("driver delivers result");
    let err = result.expect_err("client must time out");
    assert!(
        matches!(err, HttpClientError::Timeout),
        "expected Timeout, got {err:?}"
    );

    let _ = runtime.shutdown();
}
