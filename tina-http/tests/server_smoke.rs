//! End-to-end happy-path smoke tests for the native HTTP/1.1 server.
//!
//! Spins up a `ThreadedRuntime` with one shard, registers the shared
//! [`Counter`] service from [`common`], and makes real-TCP requests
//! against an `HttpListener` bound to a loopback ephemeral port.
//!
//! Bad-input cases live in `server_bad_input.rs`. Overload, large-body,
//! and graceful-shutdown cases live in `server_pressure.rs`.

mod common;

use common::{TestHarness, assert_status_and_body, assert_status_starts_with, scripted_request};

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
