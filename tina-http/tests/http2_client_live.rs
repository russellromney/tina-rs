//! Live interop proofs for the native HTTP/2 client against the
//! in-tree Tina HTTP/2 server (Counter service). This file covers the
//! happy paths a well-behaved server exercises:
//! - h2c GET round-trip → typed `Replied` with status + body
//! - h2c POST round-trip → body echoed byte-for-byte via `/echo`
//! - multiple sequential streams share one connection isolate (report
//!   confirms `opened == closed`, zero protocol errors)
//! - response body over the client cap → typed `BodyTooLarge`
//! - in-window POST through the outbound flow-control pacer
//! - `Http2Target::Tls` returns typed `TlsAlpnMismatch` (ALPN rail
//!   deferred), without touching the TLS rail
//!
//! Adversarial / concurrency / flow-control-under-window coverage —
//! server RST_STREAM, GOAWAY, malformed frames, *concurrent* streams
//! not crossing replies, `Full` admission under a peer concurrency
//! cap, caller cancel, and a 128 KB upload paced through real
//! WINDOW_UPDATE round trips — lives in `http2_client_adversarial.rs`,
//! which dials a hand-rolled misbehaving/foreign HTTP/2 peer.
//!
//! DST replay coverage and the native gRPC client are separate slices.

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use common::{Counter, TestShard};
use http::Method;
use tina::prelude::*;
use tina_http::{
    AlpnProtocols, Http2ClientConnection, Http2ClientLimits, Http2ClientMsg, Http2ClientOutcome,
    Http2ClientReply, Http2ClientRequest, Http2Limits, Http2Listener, Http2ListenerMsg,
    Http2ServerConfig, Http2Target,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig,
};

fn start_server() -> (
    ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
    Address<Http2ListenerMsg>,
    SocketAddr,
) {
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let counter = runtime
        .register_with_capacity::<Counter, Infallible>(Counter::default(), 16)
        .expect("register counter");
    let config = Http2ServerConfig {
        limits: Http2Limits::default(),
        service_call_timeout: Duration::from_secs(5),
        connection_mailbox_capacity: 16,
        listener_mailbox_capacity: 8,
    };
    let listener = runtime
        .register_with_capacity::<Http2Listener<TestShard>, _>(
            Http2Listener::<TestShard>::new("127.0.0.1:0".parse().unwrap(), counter, config),
            config.listener_mailbox_capacity,
        )
        .expect("register http2 listener");
    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener, Http2ListenerMsg::Start)
        .expect("start listener");
    let addr = bound
        .wait(Duration::from_secs(2))
        .expect("listener publishes bound address");
    (runtime, listener, addr)
}

fn make_client(
    runtime: &ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
    target: Http2Target,
    limits: Http2ClientLimits,
) -> Address<Http2ClientMsg, Http2ClientReply> {
    let client = runtime
        .register_with_capacity::<Http2ClientConnection<TestShard>, _>(
            Http2ClientConnection::<TestShard>::new(target, limits),
            32,
        )
        .expect("register http2 client");
    runtime
        .try_send(client, Http2ClientMsg::Begin)
        .expect("begin client");
    client
}

#[test]
fn h2c_get_round_trip_returns_typed_replied_outcome() {
    let (runtime, listener, addr) = start_server();
    let target = Http2Target::H2c {
        authority: "test".into(),
        addr,
    };
    let client = make_client(&runtime, target, Http2ClientLimits::default());

    let outcome = runtime
        .call_blocking(
            client,
            Http2ClientMsg::Submit(Http2ClientRequest::get("/counter")),
            Duration::from_secs(5),
        )
        .expect("call returns outcome");
    match outcome {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::Replied(response),
            ..
        }) => {
            assert_eq!(response.status.as_u16(), 200, "status from response");
            assert!(!response.body.is_empty(), "response body must be non-empty");
        }
        other => panic!("expected Replied, got {other:?}"),
    }

    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn h2c_post_body_is_echoed_back_byte_for_byte() {
    // POSTs the body to the test Counter's `/echo` endpoint, which
    // returns the buffered request body unchanged. Proves the DATA
    // frame round-tripped end-to-end (HEADERS + DATA + END_STREAM on
    // request; HEADERS + DATA + END_STREAM on response), not just that
    // the server returned a 200.
    let (runtime, listener, addr) = start_server();
    let target = Http2Target::H2c {
        authority: "test".into(),
        addr,
    };
    let client = make_client(&runtime, target, Http2ClientLimits::default());

    let body = b"hello-from-tina-client".to_vec();
    let mut req = Http2ClientRequest::post("/echo", body.clone());
    req.headers.insert(
        "content-length",
        http::HeaderValue::from_str(&body.len().to_string()).unwrap(),
    );
    let outcome = runtime
        .call_blocking(client, Http2ClientMsg::Submit(req), Duration::from_secs(5))
        .expect("call returns outcome");
    match outcome {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::Replied(response),
            ..
        }) => {
            assert_eq!(response.status.as_u16(), 200);
            assert_eq!(
                response.body, body,
                "/echo must echo the exact bytes the client sent"
            );
        }
        other => panic!("expected Replied, got {other:?}"),
    }

    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn h2c_multiple_streams_share_one_client_connection() {
    // First-form reuse: "one connection isolate carries many admitted
    // streams." Three sequential GETs over the same client isolate
    // should all succeed and the per-connection report should show
    // opened_streams == 3 and closed_streams == 3.
    let (runtime, listener, addr) = start_server();
    let target = Http2Target::H2c {
        authority: "test".into(),
        addr,
    };
    let client = make_client(&runtime, target, Http2ClientLimits::default());

    for _ in 0..3 {
        let outcome = runtime
            .call_blocking(
                client,
                Http2ClientMsg::Submit(Http2ClientRequest::get("/counter")),
                Duration::from_secs(5),
            )
            .expect("call returns outcome");
        match outcome {
            CallOutcome::Replied(Http2ClientReply::Outcome {
                outcome: Http2ClientOutcome::Replied(response),
                ..
            }) => {
                assert_eq!(response.status.as_u16(), 200);
            }
            other => panic!("expected Replied, got {other:?}"),
        }
    }

    let report = runtime
        .call_blocking(client, Http2ClientMsg::Report, Duration::from_secs(2))
        .expect("report returns");
    match report {
        CallOutcome::Replied(Http2ClientReply::Report(report)) => {
            assert_eq!(report.opened_streams, 3, "three streams admitted");
            assert_eq!(report.closed_streams, 3, "three streams closed cleanly");
            assert_eq!(report.protocol_errors, 0, "no protocol errors");
            assert_eq!(report.reset_streams, 0, "no inbound resets");
        }
        other => panic!("expected Report, got {other:?}"),
    }

    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn response_body_above_cap_returns_typed_body_too_large() {
    // The plan: "oversized received message is `ResourceExhausted` /
    // typed cap failure before unbounded allocation." For raw HTTP/2
    // we map that to `Http2ProtocolError::BodyTooLarge { cap_bytes }`.
    // POST 4 KB to /echo with a 1 KB response cap; client RSTs and
    // returns the typed error, NOT `HeadersTooLarge`.
    let (runtime, listener, addr) = start_server();
    let target = Http2Target::H2c {
        authority: "test".into(),
        addr,
    };
    let limits = Http2ClientLimits {
        max_response_body_bytes: 1024,
        ..Http2ClientLimits::default()
    };
    let client = make_client(&runtime, target, limits);

    let body = vec![b'x'; 4096];
    let mut req = Http2ClientRequest::post("/echo", body.clone());
    req.headers.insert(
        "content-length",
        http::HeaderValue::from_str(&body.len().to_string()).unwrap(),
    );
    let outcome = runtime
        .call_blocking(client, Http2ClientMsg::Submit(req), Duration::from_secs(5))
        .expect("call returns outcome");
    match outcome {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome:
                Http2ClientOutcome::ProtocolError(tina_http::Http2ProtocolError::BodyTooLarge {
                    cap_bytes,
                }),
            ..
        }) => {
            assert_eq!(cap_bytes, 1024);
        }
        other => panic!("expected ProtocolError(BodyTooLarge), got {other:?}"),
    }

    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn h2c_post_in_window_body_round_trips_against_real_server() {
    // In-window outbound proof against the real Tina server: a 32 KB
    // POST fits inside the 65535-byte default window, so it round-trips
    // without needing WINDOW_UPDATE. This guards against the
    // outbound-flow-control pacer regressing the common in-window case.
    //
    // The "actually park and resume on WINDOW_UPDATE" proof (a 128 KB
    // upload through real window-update round trips) lives in
    // `http2_client_adversarial.rs::large_upload_paces_through_real_window_updates`,
    // which uses a hand-rolled peer that drains and credits
    // incrementally — the in-tree server cannot host that proof because
    // its *response* path parks the whole body until it fits the window
    // (see the KNOWN LIMITATION in `http2/server.rs`).
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let counter = runtime
        .register_with_capacity::<Counter, Infallible>(Counter::default(), 16)
        .expect("register counter");
    let limits = Http2Limits {
        max_body_bytes: 256 * 1024,
        max_response_body_bytes: 256 * 1024,
        ..Http2Limits::default()
    };
    let config = Http2ServerConfig {
        limits,
        service_call_timeout: Duration::from_secs(5),
        connection_mailbox_capacity: 16,
        listener_mailbox_capacity: 8,
    };
    let listener = runtime
        .register_with_capacity::<Http2Listener<TestShard>, _>(
            Http2Listener::<TestShard>::new("127.0.0.1:0".parse().unwrap(), counter, config),
            config.listener_mailbox_capacity,
        )
        .expect("register http2 listener");
    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener, Http2ListenerMsg::Start)
        .expect("start listener");
    let addr = bound
        .wait(Duration::from_secs(2))
        .expect("listener publishes bound address");

    let target = Http2Target::H2c {
        authority: "test".into(),
        addr,
    };
    let client_limits = Http2ClientLimits {
        max_response_body_bytes: 256 * 1024,
        ..Http2ClientLimits::default()
    };
    let client = make_client(&runtime, target, client_limits);

    // 32 KB body — half the 65535-byte default window, so the upload
    // fits in one window without needing WINDOW_UPDATE.
    let body = vec![b'a'; 32 * 1024];
    let mut req = Http2ClientRequest::post("/counter", body.clone());
    req.headers.insert(
        "content-length",
        http::HeaderValue::from_str(&body.len().to_string()).unwrap(),
    );
    let outcome = runtime
        .call_blocking(client, Http2ClientMsg::Submit(req), Duration::from_secs(15))
        .expect("call returns outcome");
    match outcome {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::Replied(response),
            ..
        }) => {
            assert_eq!(response.status.as_u16(), 200);
        }
        other => panic!("expected Replied, got {other:?}"),
    }

    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
}

// Real h2/TLS coverage (happy path with `h2` selected, plus ALPN
// mismatch) lives in `http2_client_tls_live.rs`, which stands up a
// rustls + HTTP/2 server peer. This file is the h2c suite.

#[test]
fn tls_target_route_key_distinguishes_from_h2c_route_key() {
    // Reuse keying: TLS and h2c with the same authority must not share a
    // pool entry. This is a unit-shape proof, but it pins the route key
    // shape that pool work in Phase 119 will read.
    let h2c = Http2Target::H2c {
        authority: "x".into(),
        addr: "127.0.0.1:8080".parse().unwrap(),
    };
    let tls = Http2Target::Tls {
        authority: "x".into(),
        addr: "127.0.0.1:8443".parse().unwrap(),
        server_name: "x".into(),
        trust_roots: vec![vec![0_u8; 4]],
        alpn: AlpnProtocols::h2(),
    };
    assert_ne!(h2c.route_key(), tls.route_key());
}

#[test]
fn request_method_and_path_round_trip_through_targets() {
    // Compile/shape proof: Http2ClientRequest helpers produce GET/POST
    // requests with the right method, and Http2Target accessors return
    // the wire authority unchanged.
    let req = Http2ClientRequest::get("/health");
    assert_eq!(req.method, Method::GET);
    assert_eq!(req.path, "/health");
    let target = Http2Target::H2c {
        authority: "service.local".into(),
        addr: "127.0.0.1:9090".parse().unwrap(),
    };
    assert_eq!(target.authority(), "service.local");
    assert!(!target.is_tls());
}
