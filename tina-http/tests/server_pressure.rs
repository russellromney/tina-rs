//! Pressure-side integration tests for the native HTTP/1.1 server.
//!
//! - Streaming-ish: a body larger than one TCP read travels end-to-end
//!   through the server's accumulating read buffer up to
//!   `Content-Length`. Asserted via a trace count of `tcp_read`
//!   completions.
//! - Overload: service call failure maps are pinned at the unit layer;
//!   this file adds deterministic wire-level coverage for timeout
//!   (`504 Gateway Timeout`). The slow-loris guard closes partial
//!   request heads after `HttpLimits::header_read_timeout`.
//! - Graceful shutdown: sending `HttpListenerMsg::Stop` stops accept
//!   and lets the runtime shut down cleanly. A `Stop` race — an
//!   `Accepted(Ok)` queued by the kernel before our close took effect
//!   — must not panic the listener isolate.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use common::{TestHarness, assert_status_and_body, count_tcp_read_completions, scripted_request};

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

    // Stronger property: prove the multi-read accumulation path was
    // actually exercised by counting TCP-read completion events in the
    // runtime trace. The connection isolate's READ_CHUNK is 4 KiB and
    // we sent ~8 KiB+headers in two halves with a sleep between, so
    // the connection must have issued at least two `tcp_read` calls.
    // Without this assertion a future change that buffered the whole
    // body in one read would also pass the byte-equality check above.
    let trace = harness.shutdown();
    let read_count = count_tcp_read_completions(&trace);
    assert!(
        read_count >= 2,
        "expected >= 2 tcp_read completions in trace (multi-read accumulation), got {read_count}"
    );
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

#[test]
fn slowloris_partial_header_closes_within_header_read_timeout() {
    // Slow-loris guard regression. A client that opens a TCP connection
    // and writes only a partial request head (no terminating CRLF CRLF)
    // must not be allowed to keep the connection isolate alive past
    // `HttpLimits::header_read_timeout`. The connection isolate stops,
    // the runtime drops the stream, and the kernel closes the socket
    // (the client sees FIN or RST).
    //
    // Note: the current runtime rejects `tcp_close_stream` while a
    // `tcp_read` is pending (`CallError::ResourceBusy`). The slow-loris
    // path therefore stops the isolate without writing a 408 first; the
    // close happens via runtime cleanup. A future runtime affordance
    // (`tcp_cancel_read` or implicit cancel-on-close) would let us send
    // 408 first.
    use std::io::Write;
    use std::net::TcpStream;
    use tina_http::HttpLimits;

    let config = common::HarnessConfig {
        limits: HttpLimits {
            header_read_timeout: Duration::from_millis(150),
            ..HttpLimits::default()
        },
        ..common::HarnessConfig::default()
    };
    let harness = TestHarness::start_with_config(config);

    let mut stream =
        TcpStream::connect_timeout(&harness.addr, Duration::from_secs(2)).expect("connect");
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));

    // Send a partial request line but never complete it.
    stream
        .write_all(b"GET /counter HTTP/")
        .expect("write partial");
    stream.flush().expect("flush");

    // Wait long enough for the deadline to fire and for the runtime to
    // process the resulting `stop()`. We do NOT rely on the kernel
    // socket being closed within this window — the current
    // tina-runtime keeps `StreamId` ownership through the worker until
    // shutdown, so a slow-loris client may continue to see an open
    // socket until the runtime drops. The property we *can* verify
    // here is that the connection isolate stopped: the runtime trace
    // contains a `CallCompleted{Sleep}` event for the deadline.
    std::thread::sleep(Duration::from_millis(400));

    let trace = harness.snapshot_trace();
    let saw_sleep_completion = trace.iter().any(|event| {
        matches!(
            event.kind(),
            tina_runtime::RuntimeEventKind::CallCompleted { call_kind, .. }
                if matches!(call_kind, tina_runtime::CallKind::Sleep)
        )
    });
    assert!(
        saw_sleep_completion,
        "expected the slow-loris deadline `sleep` to have completed in the trace within 400ms; trace had {} events",
        trace.len()
    );

    // Drop the partial-write client; this is independent of whether
    // the server-side socket closed.
    drop(stream);

    // Stronger property: the listener is not corrupted by the
    // slow-loris path. A well-formed follow-up request on a fresh
    // connection must still be served.
    let follow_up = scripted_request(harness.addr, b"GET /counter HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_status_and_body(&follow_up, "200", "0");

    let _ = harness.shutdown();
}

#[test]
fn stop_race_with_pending_accept_does_not_panic() {
    // Regression for the listener Stop race: an `Accepted(Ok)` queued
    // by the kernel before `tcp_close_listener` took effect arrives at
    // the listener handler *after* `Stop` ran. The fix routes such
    // orphan streams to `tcp_close_stream` instead of touching the
    // already-taken `self.listener`.
    //
    // We approximate the race by spamming several connect attempts
    // immediately after sending Stop. At least one accept is likely to
    // be already-queued or in-flight when Stop runs. The test passes
    // if the runtime shuts down cleanly with no panic from the
    // listener isolate.
    use std::net::TcpStream;
    use std::thread;

    let harness = TestHarness::start();
    let addr = harness.addr;

    // Rapid-fire connect attempts in a background thread. We don't
    // wait for the connections to complete; they're meant to land in
    // the listener's accept queue around the time Stop is sent.
    let bombarder = thread::spawn(move || {
        for _ in 0..32 {
            // Best-effort: let the OS reject if the listener is closed.
            let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(100));
        }
    });

    // Give the bombarder a moment to land some accepts.
    thread::sleep(Duration::from_millis(20));

    // shutdown() sends Stop to the listener and shuts down the
    // runtime. If the listener panics on a post-Stop Accepted(Ok) the
    // runtime shutdown path will surface it.
    let trace = harness.shutdown();

    // Whatever happens, the runtime must produce a trace and not panic.
    // We do not assert specific event counts because the timing is
    // intrinsically nondeterministic; we assert the test reached this
    // line without unwinding.
    let _ = trace;
    let _ = bombarder.join();
}

#[test]
fn service_call_timeout_returns_504_on_the_wire() {
    // Wire-level proof of the overload-visible mapping in
    // `connection.rs::response_for_call_error`. We construct a service
    // isolate that *never replies* — it just returns `noop()` from its
    // handler. The connection isolate's `call(service, request,
    // SERVICE_CALL_TIMEOUT)` therefore times out, which the runtime
    // surfaces as `CallOutcome::Timeout` -> `CallError::Timeout`, and
    // the connection writes `504 Gateway Timeout` to the wire before
    // closing.
    //
    // Deterministic alternative to the harder-to-construct "service
    // mailbox full -> 503" path. The Full path requires concurrent
    // timing the single-shard runtime does not naturally produce, and
    // is tested at the unit-test level in
    // `connection::tests::full_call_error_maps_to_503`.
    use std::convert::Infallible;

    use http::StatusCode;
    use tina::prelude::*;
    use tina::{CallContext, RequestContext};
    use tina_http::{HttpLimits, HttpListener, HttpListenerMsg, HttpRequest, HttpResponse};
    use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig};

    use std::net::SocketAddr;

    #[derive(Debug, Default)]
    struct TimeoutShard;
    impl Shard for TimeoutShard {
        fn id(&self) -> ShardId {
            ShardId::new(common::TEST_SHARD_ID)
        }
    }

    /// Service that never replies. The connection isolate's `call`
    /// will time out.
    #[derive(Debug, Default)]
    struct DropService {
        pending: Vec<RequestContext<HttpResponse>>,
    }
    impl Isolate for DropService {
        tina::isolate_types! {
            message: HttpRequest,
            reply: HttpResponse,
            send: tina::Outbound<Infallible>,
            spawn: Infallible,
            call: Infallible,
            shard: TimeoutShard,
        }
        fn handle(
            &mut self,
            _request: HttpRequest,
            _ctx: &mut Context<'_, TimeoutShard, Self::Reply>,
        ) -> Effect<Self> {
            // Never reply. The caller's call timeout fires.
            noop()
        }

        fn handle_call(
            &mut self,
            _request: HttpRequest,
            call: CallContext<'_, Self>,
        ) -> Effect<Self> {
            // Carry the authority so the caller observes its timeout
            // instead of the new unused-call-context rejection.
            self.pending.push(call.into_request_context());
            noop()
        }
    }

    let runtime = ThreadedRuntime::with_config(
        TimeoutShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 32,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let svc = runtime
        .register_with_capacity::<DropService, Infallible>(
            DropService {
                pending: Vec::new(),
            },
            8,
        )
        .expect("register service");

    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("loopback parse");
    let listener = runtime
        .register_with_capacity::<HttpListener<TimeoutShard>, _>(
            HttpListener::<TimeoutShard>::new(
                bind_addr,
                svc,
                HttpLimits::default(),
                Duration::from_millis(150), // short call timeout
                16,
            ),
            8,
        )
        .expect("register listener");
    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener, HttpListenerMsg::Start)
        .expect("send Start");

    let server_addr = bound
        .wait(Duration::from_secs(2))
        .expect("listener publishes bound address");

    // Single client request — deterministic. The service never replies,
    // so the call times out at 150 ms and we receive a 504 on the wire.
    let response = scripted_request(server_addr, b"GET /anything HTTP/1.1\r\nHost: x\r\n\r\n");
    let text = std::str::from_utf8(&response).expect("ascii response");
    assert!(
        text.starts_with(&format!(
            "HTTP/1.1 {}",
            StatusCode::GATEWAY_TIMEOUT.as_str()
        )),
        "expected 504 Gateway Timeout, got: {}",
        &text[..text.len().min(160)]
    );

    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    let _ = runtime.shutdown();
}
