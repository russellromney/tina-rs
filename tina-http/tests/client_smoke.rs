//! End-to-end happy-path tests for the service-shaped HTTP/1.1 client.
//!
//! Spins up a stdlib `TcpListener` reference server that speaks a tiny
//! canned response, registers an [`HttpClient`] as a long-lived
//! service isolate, and drives it via
//! [`ThreadedRuntime::call_blocking`](tina_runtime::ThreadedRuntime::call_blocking).
//! Asserts the parsed [`HttpResponse`] matches
//! the wire bytes the reference server wrote.
//!
//! Bad-input cases live in `client_bad_input.rs`.

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use http::StatusCode;
use tina::prelude::*;
use tina_http::{
    HttpClient, HttpClientConfig, HttpClientError, HttpClientMsg, HttpRequest, HttpResponse,
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
enum BackgroundDriverMsg {
    Begin {
        client: Address<HttpClientMsg, Result<HttpResponse, HttpClientError>>,
        target: SocketAddr,
        request: HttpRequest,
        timeout: Duration,
    },
    Returned(CallOutcome<Result<HttpResponse, HttpClientError>>),
}

struct BackgroundDriver {
    sender: mpsc::Sender<Result<HttpResponse, HttpClientError>>,
}

impl Isolate for BackgroundDriver {
    tina::isolate_types! {
        message: BackgroundDriverMsg,
        reply: (),
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<BackgroundDriverMsg>,
        shard: SingleShard,
    }

    fn handle(
        &mut self,
        msg: BackgroundDriverMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            BackgroundDriverMsg::Begin {
                client,
                target,
                request,
                timeout,
            } => call(client, HttpClientMsg::call(target, request), timeout)
                .reply(BackgroundDriverMsg::Returned),
            BackgroundDriverMsg::Returned(outcome) => {
                let result = match outcome {
                    CallOutcome::Replied(inner) => inner,
                    CallOutcome::Full => Err(HttpClientError::Busy),
                    CallOutcome::Closed => Err(HttpClientError::Closed),
                    CallOutcome::Timeout => Err(HttpClientError::Timeout),
                    CallOutcome::Rejected(_) => Err(HttpClientError::Closed),
                };
                let _ = self.sender.send(result);
                stop()
            }
        }
    }
}

fn run_one_request(canned_response: Vec<u8>) -> Result<HttpResponse, HttpClientError> {
    let target = start_canned_server(canned_response);

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

    let request = HttpRequest::get("/").header("Host", "x").build();
    let result = match runtime
        .call_blocking(
            client,
            HttpClientMsg::call(target, request),
            Duration::from_secs(2),
        )
        .expect("client call runs")
    {
        CallOutcome::Replied(inner) => inner,
        CallOutcome::Full => Err(HttpClientError::Busy),
        CallOutcome::Closed => Err(HttpClientError::Closed),
        CallOutcome::Timeout => Err(HttpClientError::Timeout),
        CallOutcome::Rejected(_) => Err(HttpClientError::Closed),
    };

    let _ = runtime.shutdown();
    result
}

#[test]
fn happy_path_get_returns_parsed_response() {
    let canned = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello".to_vec();
    let result = run_one_request(canned).expect("client succeeds");
    assert_eq!(result.status, StatusCode::OK);
    assert_eq!(result.body, b"hello");
}

#[test]
fn larger_body_spans_multiple_reads() {
    let body: Vec<u8> = (0u32..16384).map(|i| (i % 251) as u8).collect();
    let mut canned =
        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
    canned.extend_from_slice(&body);

    let result = run_one_request(canned).expect("client succeeds");
    assert_eq!(result.status, StatusCode::OK);
    assert_eq!(result.body.declared_length(), Some(body.len()));
    assert_eq!(result.body, body);
}

#[test]
fn call_site_reads_like_any_other_tina_call() {
    // This test exists primarily as documentation: the user-facing
    // call site is one expression, indistinguishable in shape from
    // any other Tina service call.
    let canned = b"HTTP/1.1 204 No Content\r\nServer: x\r\n\r\n".to_vec();
    let result = run_one_request(canned).expect("client succeeds");
    assert_eq!(result.status, StatusCode::NO_CONTENT);
    assert_eq!(result.body.declared_length(), Some(0));
}

#[test]
fn second_call_while_busy_returns_busy() {
    // Black-hole upstream: accepts but never writes. The client's
    // first Call ties up its in-flight slot until request_timeout.
    // While in flight, a second Call must reply Err(Busy).
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind black-hole");
    let target = listener.local_addr().expect("local addr");
    let _silent = thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            thread::sleep(Duration::from_secs(2));
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

    // Mailbox >= 2 so the second Call can land while the first is
    // still in flight.
    let client = runtime
        .register_with_capacity::<HttpClient<SingleShard>, Infallible>(
            HttpClient::<SingleShard>::new(HttpClientConfig::dev()),
            8,
        )
        .expect("register client");

    let (tx_a, _rx_a) = mpsc::channel();
    let driver_a = runtime
        .register_with_capacity::<BackgroundDriver, Infallible>(
            BackgroundDriver { sender: tx_a },
            8,
        )
        .expect("register driver A");
    runtime
        .try_send(
            driver_a,
            BackgroundDriverMsg::Begin {
                client,
                target,
                request: HttpRequest::get("/").header("Host", "x").build(),
                timeout: Duration::from_secs(5),
            },
        )
        .expect("send Begin A");

    // Brief pause so Call_A reaches the client and sets state.
    thread::sleep(Duration::from_millis(50));

    let (tx_b, rx_b) = mpsc::channel();
    let driver_b = runtime
        .register_with_capacity::<BackgroundDriver, Infallible>(
            BackgroundDriver { sender: tx_b },
            8,
        )
        .expect("register driver B");
    runtime
        .try_send(
            driver_b,
            BackgroundDriverMsg::Begin {
                client,
                target,
                request: HttpRequest::get("/").header("Host", "x").build(),
                timeout: Duration::from_secs(2),
            },
        )
        .expect("send Begin B");

    let result_b = rx_b
        .recv_timeout(Duration::from_secs(3))
        .expect("driver B replies");
    assert!(
        matches!(result_b, Err(HttpClientError::Busy)),
        "expected Busy while client is in flight, got {result_b:?}"
    );

    let _ = runtime.shutdown();
}
