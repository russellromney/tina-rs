//! End-to-end happy-path tests for the service-shaped HTTP/1.1 client.
//!
//! Spins up a stdlib `TcpListener` reference server that speaks a tiny
//! canned response, registers an [`HttpClient`] as a long-lived
//! service isolate, and drives it via `call(client, ...).reply(...)`
//! from a Driver isolate. Asserts the parsed [`HttpResponse`] matches
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
enum DriverMsg {
    Begin {
        client: Address<HttpClientMsg, Result<HttpResponse, HttpClientError>>,
        target: SocketAddr,
        request: HttpRequest,
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

    fn handle(&mut self, msg: DriverMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            DriverMsg::Begin {
                client,
                target,
                request,
                timeout,
            } => call(client, HttpClientMsg::call(target, request), timeout)
                .reply(DriverMsg::Returned),
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

    let (tx, rx) = mpsc::channel();
    let driver = runtime
        .register_with_capacity::<Driver, Infallible>(Driver { sender: tx }, 16)
        .expect("register driver");

    let request = HttpRequest::get("/").header("Host", "x").build();
    runtime
        .try_send(
            driver,
            DriverMsg::Begin {
                client,
                target,
                request,
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
    assert_eq!(result.body.len(), body.len());
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
    assert!(result.body.is_empty());
}
