//! Pressure-side integration tests for the native HTTP/1.1 server.
//!
//! Covers phase 048 rocks:
//!
//! - rock 4 (streaming bodies, partial coverage in 048a): a body larger
//!   than one TCP read travels end-to-end through the server's
//!   accumulating read buffer up to `Content-Length`.
//! - rock 6 (graceful shutdown): sending `HttpListenerMsg::Stop` stops
//!   accept and lets the runtime shut down cleanly.
//!
//! The full overload story (rock 5) — `503 Service Unavailable` when the
//! service mailbox is full, `504 Gateway Timeout` when the call timeout
//! elapses — is unit-tested in `connection::tests` because constructing
//! a "slow service" in Tina's request/reply model requires either the
//! 048b connection pool primitive or a future delayed-reply primitive.
//! 048a covers the *mapping*: that `CallOutcome::{Full,Closed,Timeout}`
//! produce the right status codes. The wire-level integration test is
//! recorded as future work in 048a's PR description.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use common::{TestHarness, assert_status_and_body, scripted_request};

#[test]
fn body_larger_than_one_tcp_read_round_trips_through_post_echo() {
    // The connection isolate's read chunk is 4 KiB. We send an 8 KiB
    // body to force the connection to issue at least one extra
    // `tcp_read` to finish accumulating it before dispatching to the
    // service. The Counter service's `/echo` endpoint reflects the body
    // back; we check the round-trip byte for byte.
    let harness = TestHarness::start();

    let body_size = 8 * 1024;
    let body: Vec<u8> = (0..body_size).map(|i| (i % 251) as u8).collect();
    let mut request =
        format!("POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: {body_size}\r\n\r\n")
            .into_bytes();
    request.extend_from_slice(&body);

    let mut stream =
        TcpStream::connect_timeout(&harness.addr, Duration::from_secs(2)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");

    // Write the request in two halves with a pause to force an extra
    // server-side read. Even without the pause an 8 KiB body will not
    // arrive in one 4 KiB chunk.
    stream
        .write_all(&request[..request.len() / 2])
        .expect("first half");
    stream.flush().expect("flush");
    std::thread::sleep(Duration::from_millis(20));
    stream
        .write_all(&request[request.len() / 2..])
        .expect("second half");
    stream.flush().expect("flush");

    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");

    // The headers are ASCII; the body is binary. Find the CRLFCRLF
    // separator on the byte sequence directly rather than attempting to
    // UTF-8 decode the whole response.
    let separator = b"\r\n\r\n";
    let header_end = response
        .windows(separator.len())
        .position(|w| w == separator)
        .expect("response has CRLFCRLF");
    let header_bytes = &response[..header_end];
    let header_text = std::str::from_utf8(header_bytes).expect("headers are ASCII");
    assert!(
        header_text.starts_with("HTTP/1.1 200"),
        "expected 200, got headers: {header_text:?}"
    );

    let body_offset = header_end + separator.len();
    let body_in_response = &response[body_offset..];
    assert_eq!(
        body_in_response,
        &body[..],
        "echoed body must match request body byte-for-byte"
    );

    harness.shutdown();
}

#[test]
fn graceful_shutdown_stops_accept_and_completes_shutdown() {
    // Build a harness, make one well-formed request, send Stop, then
    // confirm the runtime shuts down without hanging. If
    // `harness.shutdown()` returns at all, the listener stopped
    // accepting and the runtime drained cleanly.
    let harness = TestHarness::start();

    let response = scripted_request(harness.addr, b"GET /counter HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_status_and_body(&response, "200", "0");

    harness.shutdown();
}
