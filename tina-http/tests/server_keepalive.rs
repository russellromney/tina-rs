//! Integration tests for server-side HTTP/1.1 keep-alive.
//!
//! Each test stands up a real `HttpListener` + `Counter` service and
//! drives it from a hand-rolled `std::net::TcpStream` so the test can
//! reason about *one* TCP connection serving multiple requests
//! sequentially. The proof shape is: write request bytes, read
//! response bytes, repeat — without ever opening a second socket.

#![allow(dead_code)]

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use tina_http::HttpLimits;

use common::{HarnessConfig, TestHarness};

// ---------------------------------------------------------------------
// Wire helpers
// ---------------------------------------------------------------------

const KEEPALIVE_IDLE: Duration = Duration::from_millis(800);

fn keepalive_harness() -> TestHarness {
    TestHarness::start_with_config(HarnessConfig {
        limits: HttpLimits {
            keepalive_idle_timeout: Some(KEEPALIVE_IDLE),
            ..HttpLimits::default()
        },
        ..HarnessConfig::default()
    })
}

fn open_stream(harness: &TestHarness) -> TcpStream {
    let stream = TcpStream::connect_timeout(&harness.addr, Duration::from_secs(2))
        .expect("connect to keepalive server");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .expect("write timeout");
    stream
}

/// Write one request and read exactly one response head + body
/// (assuming `Content-Length` framing). Returns the raw response
/// bytes for asserts.
fn round_trip(stream: &mut TcpStream, request: &[u8]) -> Vec<u8> {
    stream.write_all(request).expect("write request");
    stream.flush().expect("flush");
    read_one_response(stream)
}

/// Read exactly one HTTP/1.1 response with `Content-Length` framing.
/// Stops as soon as the head + declared body have arrived; leaves any
/// later bytes in the kernel buffer for the next round trip.
fn read_one_response(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    // Read until we have the head terminator.
    let head_end = loop {
        if let Some(p) = find_double_crlf(&buf) {
            break p;
        }
        let mut chunk = [0u8; 1024];
        let n = stream.read(&mut chunk).expect("read response head");
        if n == 0 {
            panic!(
                "peer closed before sending a complete response head; got {} bytes: {:?}",
                buf.len(),
                String::from_utf8_lossy(&buf)
            );
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head_str = std::str::from_utf8(&buf[..head_end]).unwrap_or("");
    let cl = parse_content_length(head_str);
    let body_end = head_end + cl;
    while buf.len() < body_end {
        let mut chunk = [0u8; 1024];
        let n = stream.read(&mut chunk).expect("read response body");
        if n == 0 {
            panic!("peer closed before sending the full body");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    buf.truncate(body_end);
    buf
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

fn parse_content_length(head: &str) -> usize {
    for line in head.split("\r\n") {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            if let Ok(n) = rest.trim().parse::<usize>() {
                return n;
            }
        }
    }
    0
}

fn assert_status(response: &[u8], status: &str) {
    let text = std::str::from_utf8(response).expect("utf8");
    assert!(
        text.starts_with(&format!("HTTP/1.1 {status}")),
        "expected {status} at start of {text:?}"
    );
}

fn assert_body_ends_with(response: &[u8], body: &str) {
    let text = std::str::from_utf8(response).expect("utf8");
    assert!(
        text.ends_with(body),
        "expected body {body:?} at end of {text:?}"
    );
}

/// Read whatever is available within `timeout`; returns the bytes
/// (possibly empty) plus whether the peer closed (`Ok(0)`).
fn read_until_close(stream: &mut TcpStream, timeout: Duration) -> (Vec<u8>, bool) {
    stream
        .set_read_timeout(Some(timeout))
        .expect("set read timeout");
    let mut buf: Vec<u8> = Vec::new();
    let mut peer_closed = false;
    loop {
        let mut chunk = [0u8; 1024];
        match stream.read(&mut chunk) {
            Ok(0) => {
                peer_closed = true;
                break;
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break, // timeout / would-block
        }
    }
    (buf, peer_closed)
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[test]
fn three_sequential_requests_share_one_tcp_connection_when_keepalive_on() {
    let harness = keepalive_harness();
    let mut stream = open_stream(&harness);

    // POST x3, then GET — counter goes 1, 2, 3, then GET returns 3.
    for expected in ["1", "2", "3"] {
        let resp = round_trip(
            &mut stream,
            b"POST /counter HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
        );
        assert_status(&resp, "200");
        assert_body_ends_with(&resp, expected);
        // Server must NOT have written `Connection: close` — the
        // request had neither HTTP/1.0 nor an explicit close header.
        let text = std::str::from_utf8(&resp).expect("utf8");
        assert!(
            !text.contains("Connection: close"),
            "keepalive response should not include `Connection: close`; got {text:?}"
        );
    }
    let resp = round_trip(&mut stream, b"GET /counter HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_status(&resp, "200");
    assert_body_ends_with(&resp, "3");

    // Drop the stream; harness shutdown closes the listener.
    drop(stream);
    harness.shutdown();
}

#[test]
fn http_1_0_client_closes_after_one_response_even_when_keepalive_on() {
    let harness = keepalive_harness();
    let mut stream = open_stream(&harness);

    let resp = round_trip(&mut stream, b"GET /counter HTTP/1.0\r\nHost: x\r\n\r\n");
    assert_status(&resp, "200");
    let text = std::str::from_utf8(&resp).expect("utf8");
    assert!(
        text.contains("Connection: close"),
        "HTTP/1.0 response should signal close; got {text:?}"
    );

    // Server must close cleanly within the idle window.
    let (extra, closed) = read_until_close(&mut stream, Duration::from_millis(500));
    assert!(extra.is_empty(), "no extra bytes expected; got {extra:?}");
    assert!(closed, "server should close after HTTP/1.0 response");

    harness.shutdown();
}

#[test]
fn explicit_connection_close_header_closes_after_response() {
    let harness = keepalive_harness();
    let mut stream = open_stream(&harness);

    // First request with explicit close → response carries
    // `Connection: close` and the server closes the socket.
    let resp = round_trip(
        &mut stream,
        b"GET /counter HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert_status(&resp, "200");
    let text = std::str::from_utf8(&resp).expect("utf8");
    assert!(
        text.contains("Connection: close"),
        "server must echo close intent; got {text:?}"
    );

    let (extra, closed) = read_until_close(&mut stream, Duration::from_millis(500));
    assert!(extra.is_empty());
    assert!(closed, "server should close after `Connection: close`");

    harness.shutdown();
}

#[test]
fn idle_timeout_fires_and_listener_remains_healthy() {
    // The connection isolate's idle deadline fires after
    // `keepalive_idle_timeout` past the previous response. We
    // observe the deadline via the runtime trace (same precedent as
    // the existing slow-loris regression test) and verify that the
    // listener is uncorrupted by serving a follow-up request on a
    // fresh socket. A peer-observed FIN is not asserted because the
    // current runtime keeps `StreamId` ownership until shutdown
    // (see `slowloris_partial_header_closes_within_header_read_timeout`).
    let harness = keepalive_harness();
    let mut stream = open_stream(&harness);

    let resp = round_trip(&mut stream, b"GET /counter HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_status(&resp, "200");

    // Wait long enough for the idle deadline to expire and the
    // runtime to process the resulting `stop()`.
    std::thread::sleep(KEEPALIVE_IDLE + Duration::from_millis(400));

    let trace = harness.snapshot_trace();
    // Two `Sleep` completions are expected by now: the
    // header_read_timeout for request 1 (still pending — fires
    // later) and the keepalive_idle_timeout for the next-request
    // wait. Be permissive: assert at least one Sleep completion
    // fired.
    let sleep_completions = trace
        .iter()
        .filter(|e| {
            matches!(
                e.kind(),
                tina_runtime::RuntimeEventKind::CallCompleted { call_kind, .. }
                    if matches!(call_kind, tina_runtime::CallKind::Sleep)
            )
        })
        .count();
    assert!(
        sleep_completions >= 1,
        "expected at least one Sleep completion (idle deadline) within window; trace had {} events",
        trace.len()
    );

    drop(stream);

    // Listener is uncorrupted: a fresh request still gets served.
    let follow =
        TcpStream::connect_timeout(&harness.addr, Duration::from_secs(2)).expect("fresh connect");
    let mut follow = follow;
    follow
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let resp = round_trip(&mut follow, b"GET /counter HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_status(&resp, "200");

    harness.shutdown();
}

#[test]
fn default_limits_serve_one_request_then_close() {
    // Backwards-compat: HttpLimits::default() leaves
    // keepalive_idle_timeout = None, so the legacy one-request-per-
    // connection behaviour is preserved.
    let harness = TestHarness::start();
    let mut stream = open_stream(&harness);

    let resp = round_trip(&mut stream, b"GET /counter HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_status(&resp, "200");
    // Server forces close because keepalive is off.
    let text = std::str::from_utf8(&resp).expect("utf8");
    assert!(
        text.contains("Connection: close"),
        "default config should still emit close header; got {text:?}"
    );

    let (extra, closed) = read_until_close(&mut stream, Duration::from_millis(500));
    assert!(extra.is_empty());
    assert!(closed, "default config closes after one response");

    harness.shutdown();
}

#[test]
fn slow_loris_on_second_request_fires_deadline_via_trace() {
    // After the first response on a keepalive connection, a partial
    // second-request head must trigger the idle/header deadline and
    // stop the connection isolate. Observed via the runtime trace
    // (same precedent as `slowloris_partial_header_closes_within_header_read_timeout`
    // in server_pressure.rs — peer-side FIN is not asserted because
    // the runtime keeps `StreamId` ownership until shutdown).
    let harness = keepalive_harness();
    let mut stream = open_stream(&harness);

    let resp = round_trip(&mut stream, b"GET /counter HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_status(&resp, "200");

    // Send a partial second head (no terminating CRLF CRLF).
    stream
        .write_all(b"GET /counter HTTP/1.1\r\nHost: ")
        .expect("write partial");
    stream.flush().expect("flush partial");

    std::thread::sleep(KEEPALIVE_IDLE + Duration::from_millis(400));

    let trace = harness.snapshot_trace();
    let sleep_completions = trace
        .iter()
        .filter(|e| {
            matches!(
                e.kind(),
                tina_runtime::RuntimeEventKind::CallCompleted { call_kind, .. }
                    if matches!(call_kind, tina_runtime::CallKind::Sleep)
            )
        })
        .count();
    assert!(
        sleep_completions >= 1,
        "expected the slow-loris deadline `sleep` to have completed; trace had {} events",
        trace.len()
    );

    drop(stream);

    // Listener still serves a follow-up on a fresh connection.
    let mut fresh =
        TcpStream::connect_timeout(&harness.addr, Duration::from_secs(2)).expect("fresh connect");
    fresh
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let resp = round_trip(&mut fresh, b"GET /counter HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_status(&resp, "200");

    harness.shutdown();
}

#[test]
fn body_cap_still_enforced_on_subsequent_keepalive_request() {
    // Tight body cap. First request fits; second request claims a
    // body length over the cap and must be rejected without
    // dispatching to the service. The server then closes (parse
    // error forces will_close = true).
    let harness = TestHarness::start_with_config(HarnessConfig {
        limits: HttpLimits {
            max_body_bytes: 16,
            keepalive_idle_timeout: Some(KEEPALIVE_IDLE),
            ..HttpLimits::default()
        },
        ..HarnessConfig::default()
    });
    let mut stream = open_stream(&harness);

    // First request: in-budget body.
    let resp = round_trip(
        &mut stream,
        b"POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello",
    );
    assert_status(&resp, "200");

    // Second request: body declared over the cap.
    let resp = round_trip(
        &mut stream,
        b"POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 999\r\n\r\n",
    );
    assert_status(&resp, "413");

    // Server forces close after the parse error.
    let (extra, closed) = read_until_close(&mut stream, Duration::from_millis(500));
    assert!(extra.is_empty());
    assert!(closed, "server should close after parse-error response");

    harness.shutdown();
}

#[test]
fn keepalive_serves_mixed_get_and_post_with_bodies() {
    // GETs interleaved with POSTs (with bodies) on the same socket.
    // Proves the per-request reset (read_buf, parsed_head, head_len)
    // does not contaminate the next request.
    let harness = keepalive_harness();
    let mut stream = open_stream(&harness);

    let resp = round_trip(&mut stream, b"GET /counter HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_status(&resp, "200");
    assert_body_ends_with(&resp, "0");

    let resp = round_trip(
        &mut stream,
        b"POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 11\r\n\r\nhello world",
    );
    assert_status(&resp, "200");
    assert_body_ends_with(&resp, "hello world");

    let resp = round_trip(
        &mut stream,
        b"POST /counter HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
    );
    assert_status(&resp, "200");
    assert_body_ends_with(&resp, "1");

    let resp = round_trip(&mut stream, b"GET /counter HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_status(&resp, "200");
    assert_body_ends_with(&resp, "1");

    drop(stream);
    harness.shutdown();
}
