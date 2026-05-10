//! End-to-end happy-path smoke tests for the native HTTP/1.1 server.
//!
//! Spins up a `ThreadedRuntime` with one shard, registers the shared
//! [`Counter`] service from [`common`], and makes real-TCP requests
//! against an `HttpListener` bound to a loopback ephemeral port.
//!
//! Bad-input cases live in `server_bad_input.rs`. Overload, large-body,
//! and graceful-shutdown cases live in `server_pressure.rs`.

mod common;

use std::time::Duration;

use common::{TestHarness, assert_status_and_body, assert_status_starts_with, scripted_request};
use tina_http::HttpListenerMsg;
use tina_runtime::{CallKind, RuntimeEventKind};

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
fn second_start_does_not_rebind_or_leak_listener() {
    // HttpListener's reply type is `()` so we can't surface a typed
    // AlreadyStarted error here; the contract is that the second
    // Start is a no-op and the trace shows exactly one TcpBind.
    let harness = TestHarness::start();
    // TestHarness already sent Start once during start(). Send a
    // second Start — should be a no-op.
    let runtime = harness.runtime_handle();
    runtime
        .try_send(harness.listener_address(), HttpListenerMsg::Start)
        .expect("send second Start");

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
