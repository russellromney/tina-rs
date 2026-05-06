//! Bad-input integration tests for the native HTTP server.
//!
//! Each test sends a deliberately broken request over real TCP and
//! asserts the connection isolate produces the typed status code
//! [`RequestParseError`] documented in `tina_http::types`, then closes
//! the connection cleanly.
//!
//! Coverage maps to phase 048's rock 12 (bad input suite):
//!
//! - malformed request line -> `400 Bad Request`
//! - oversized header section -> `431 Request Header Fields Too Large`
//! - unsupported transfer encoding -> `501 Not Implemented`
//! - oversized `Content-Length` body -> `413 Payload Too Large`
//! - absolute-form request target -> `400 Bad Request`
//! - peer closes mid-request -> connection closes without response

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use common::{TestHarness, scripted_request};

#[test]
fn malformed_request_line_returns_400() {
    let harness = TestHarness::start();
    let response = scripted_request(harness.addr, b"GIBBERISH NOT-A-REQUEST\r\n\r\n");
    common::assert_status_starts_with(&response, "400");
    harness.shutdown();
}

#[test]
fn oversized_header_section_is_rejected_either_with_431_or_connection_reset() {
    // The connection isolate's parser fails with HeadersTooLarge once the
    // accumulated header bytes exceed `HttpLimits::max_header_bytes` (16
    // KiB by default). It then writes a 431 response and closes the
    // stream. If the client is still mid-write at that moment, the
    // kernel may convert the close into a TCP RST and the client's
    // `read_to_end` will surface as `ConnectionReset` rather than a
    // clean response. Both outcomes are valid evidence that the server
    // rejected the oversized headers; only "200 OK" or a hang would be
    // a bug.
    //
    // Future work (drain-before-close, tracked against 048b): the
    // connection isolate should consume any remaining inbound body
    // bytes before closing so error responses always reach the client
    // cleanly.
    let harness = TestHarness::start();
    let huge_value = "x".repeat(20 * 1024);
    let request = format!("GET / HTTP/1.1\r\nX-Huge: {huge_value}\r\n\r\n");

    let mut stream =
        TcpStream::connect_timeout(&harness.addr, Duration::from_secs(2)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let _ = stream.write_all(request.as_bytes());
    let _ = stream.flush();
    let mut response = Vec::new();
    let read_outcome = stream.read_to_end(&mut response);

    match read_outcome {
        Ok(_) => {
            // Server wrote a clean response before closing. Must be 431.
            common::assert_status_starts_with(&response, "431");
        }
        Err(error)
            if error.kind() == std::io::ErrorKind::ConnectionReset
                || error.kind() == std::io::ErrorKind::BrokenPipe =>
        {
            // Server closed the connection forcibly (RST) before the
            // response could be fully drained by the client. Acceptable
            // in 048a — the server still rejected the input cleanly on
            // its side. Drain-before-close is 048b future work.
        }
        Err(other) => panic!("unexpected client read error: {other:?}"),
    }

    harness.shutdown();
}

#[test]
fn chunked_transfer_encoding_returns_501() {
    let harness = TestHarness::start();
    let request =
        b"POST /upload HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n";
    let response = scripted_request(harness.addr, request);
    common::assert_status_starts_with(&response, "501");
    harness.shutdown();
}

#[test]
fn oversized_content_length_returns_413() {
    let harness = TestHarness::start();
    let huge = 10 * 1024 * 1024; // 10 MiB exceeds default 1 MiB body limit
    let request = format!("POST /upload HTTP/1.1\r\nHost: x\r\nContent-Length: {huge}\r\n\r\n");
    let response = scripted_request(harness.addr, request.as_bytes());
    common::assert_status_starts_with(&response, "413");
    harness.shutdown();
}

#[test]
fn absolute_form_request_target_returns_400() {
    let harness = TestHarness::start();
    let response = scripted_request(
        harness.addr,
        b"GET http://example.com/path HTTP/1.1\r\nHost: x\r\n\r\n",
    );
    common::assert_status_starts_with(&response, "400");
    harness.shutdown();
}

#[test]
fn peer_close_mid_request_does_not_panic_or_hang() {
    let harness = TestHarness::start();

    // Connect, send a partial request, then close immediately. The
    // connection isolate must close the stream cleanly without waiting
    // forever for the rest. We assert by timing out the runtime
    // shutdown.
    let mut stream =
        TcpStream::connect_timeout(&harness.addr, Duration::from_secs(2)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("read timeout");
    stream
        .write_all(b"GET /counter HTTP/1.1\r\nHost: ")
        .expect("write partial");
    stream.flush().expect("flush");
    drop(stream);

    // Issue a follow-up well-formed request; if the listener is still
    // running, it should be served correctly. This is the smoking
    // proof that a mid-request peer close did not corrupt the listener.
    let response = scripted_request(harness.addr, b"GET /counter HTTP/1.1\r\nHost: x\r\n\r\n");
    common::assert_status_starts_with(&response, "200");

    harness.shutdown();
}

#[allow(dead_code)]
fn fully_read(stream: &mut TcpStream) -> Vec<u8> {
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    response
}
