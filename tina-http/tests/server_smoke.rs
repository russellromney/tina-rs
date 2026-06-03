//! End-to-end happy-path smoke tests for the native HTTP/1.1 server.
//!
//! Spins up a `ThreadedRuntime` with one shard, registers the shared
//! [`Counter`] service from [`common`], and makes real-TCP requests
//! against an `HttpListener` bound to a loopback ephemeral port.
//!
//! Bad-input cases live in `server_bad_input.rs`. Overload, large-body,
//! and graceful-shutdown cases live in `server_pressure.rs`.

mod common;

use std::convert::Infallible;
use std::net::TcpListener;
use std::time::Duration;

use common::{
    Counter, TestHarness, TestShard, assert_status_and_body, assert_status_starts_with,
    count_tcp_write_completions, scripted_request,
};
use tina_http::{HttpLimits, HttpListener, HttpListenerMsg, HttpStartupError};
use tina_runtime::{
    CallKind, CallOutcome, DefaultThreadedMailboxFactory, RuntimeEventKind, ThreadedRuntime,
    ThreadedRuntimeConfig,
};

fn assert_connection_close(response: &[u8]) {
    let text = std::str::from_utf8(response).expect("http response is utf8");
    assert!(
        text.contains("\r\nConnection: close\r\n"),
        "first-form server must declare terminal responses honestly: {text}"
    );
}

#[test]
fn native_http_server_serves_get_and_post() {
    let harness = TestHarness::start();

    // Three requests in sequence (one request per connection).
    let response_a = scripted_request(harness.addr, b"GET /counter HTTP/1.1\r\nHost: x\r\n\r\n");
    let response_b = scripted_request(
        harness.addr,
        b"POST /counter HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
    );
    let response_c = scripted_request(harness.addr, b"GET /counter HTTP/1.1\r\nHost: x\r\n\r\n");
    let response_d = scripted_request(harness.addr, b"GET /missing HTTP/1.1\r\nHost: x\r\n\r\n");

    assert_status_and_body(&response_a, "200", "0");
    assert_connection_close(&response_a);
    assert_status_and_body(&response_b, "200", "1");
    assert_connection_close(&response_b);
    assert_status_and_body(&response_c, "200", "1");
    assert_connection_close(&response_c);
    assert_status_starts_with(&response_d, "404");
    assert_connection_close(&response_d);

    harness.shutdown();
}

#[test]
fn default_buffered_response_writes_head_and_body_once() {
    let harness = TestHarness::start();

    let response = scripted_request(harness.addr, b"GET /counter HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_status_and_body(&response, "200", "0");

    let trace = harness.shutdown();
    let tcp_writes = count_tcp_write_completions(&trace);
    assert_eq!(
        tcp_writes, 1,
        "default no-metrics buffered response should write head+body once; trace had {tcp_writes} tcp writes",
    );
}

#[test]
fn large_buffered_response_keeps_head_and_body_split() {
    let harness = TestHarness::start();
    let body = vec![b'x'; 4097];
    let mut request = format!(
        "POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(&body);

    let response = scripted_request(harness.addr, &request);
    let expected = std::str::from_utf8(&body).expect("ascii body");
    assert_status_and_body(&response, "200", expected);

    let trace = harness.shutdown();
    let tcp_writes = count_tcp_write_completions(&trace);
    assert_eq!(
        tcp_writes, 2,
        "large no-metrics buffered response should avoid coalescing into a copied mega-buffer; trace had {tcp_writes} tcp writes",
    );
}

#[test]
fn second_start_does_not_rebind_or_leak_listener() {
    let harness = TestHarness::start();
    let runtime = harness.runtime_handle();
    let duplicate = runtime
        .call_blocking(
            harness.listener_address(),
            HttpListenerMsg::Start,
            Duration::from_secs(2),
        )
        .expect("duplicate Start call returns");
    assert_eq!(
        duplicate,
        CallOutcome::Replied(Err(HttpStartupError::AlreadyStarted))
    );

    // Give the second Start a chance to run and any (incorrect)
    // second tcp_bind to land in the trace.
    std::thread::sleep(Duration::from_millis(100));

    let trace = harness.shutdown();
    let tcp_bind_completions = trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::TcpBind,
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        tcp_bind_completions, 1,
        "exactly one tcp_bind should have completed (got {tcp_bind_completions})",
    );
}

#[test]
fn startup_bind_error_is_returned_to_caller() {
    let occupied = TcpListener::bind("127.0.0.1:0").expect("bind guard socket");
    let bind_addr = occupied.local_addr().expect("guard local addr");
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let service = runtime
        .register_with_capacity::<Counter, Infallible>(Counter::default(), 16)
        .expect("register counter");
    let listener = runtime
        .register_with_capacity::<HttpListener<TestShard>, _>(
            HttpListener::new(
                bind_addr,
                service,
                HttpLimits::default(),
                Duration::from_secs(2),
                16,
            ),
            8,
        )
        .expect("register listener");

    let startup = runtime
        .call_blocking(listener, HttpListenerMsg::Start, Duration::from_secs(2))
        .expect("startup call returns");
    match startup {
        CallOutcome::Replied(Err(HttpStartupError::Bind { .. })) => {}
        other => panic!("expected typed bind error, got {other:?}"),
    }
    drop(occupied);
    runtime.shutdown().expect("shutdown");
}
