//! DST replay honesty for the native HTTP/2 client.
//!
//! The native `Http2ClientConnection` does real outbound socket I/O —
//! `tcp_connect` / `tcp_read` / `tcp_write` / `tcp_close`. The
//! deterministic simulator's replay op-alphabet models *app* operations,
//! not a remote peer's live socket completions, so a captured live client
//! run cannot be silently re-driven from the op history alone.
//!
//! Rather than a silent no-op or a fake replay, the capture carries a
//! typed [`tina_sim::dst::UnsupportedLiveFact`] naming the client socket
//! work, and `check_captured_replay` fails closed on it. This test pins
//! that contract and that the unsupported fact survives a saved-case
//! round trip.

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use common::{Counter, TestShard};
use tina::prelude::*;
use tina_http::{
    Http2ClientConnection, Http2ClientLimits, Http2ClientMsg, Http2ClientReply, Http2ClientRequest,
    Http2Limits, Http2Listener, Http2ListenerMsg, Http2ServerConfig, Http2Target,
};
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig};
use tina_sim::dst::{
    CapturedReplayChange, LiveReplayCapture, LiveReplayReport, ReplayConfig, TraceProjection,
    check_captured_replay, read_saved_replay_case, write_saved_replay_case,
};

/// App-level operation alphabet for the captured client run. These are
/// the *intentful* steps; the socket completions they cause are exactly
/// the live work the simulator cannot replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Http2ClientReplayOp {
    Begin,
    UnaryGet,
}

fn encode_op(op: &Http2ClientReplayOp) -> String {
    match op {
        Http2ClientReplayOp::Begin => "begin",
        Http2ClientReplayOp::UnaryGet => "unary-get",
    }
    .to_owned()
}

fn decode_op(text: &str) -> Result<Http2ClientReplayOp, String> {
    match text {
        "begin" => Ok(Http2ClientReplayOp::Begin),
        "unary-get" => Ok(Http2ClientReplayOp::UnaryGet),
        other => Err(format!("unknown op {other:?}")),
    }
}

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

#[test]
fn live_http2_client_socket_work_is_unsupported_under_dst_replay() {
    // 1. Run the native HTTP/2 client live: connect + one h2c GET.
    let (runtime, listener, addr) = start_server();
    let client = runtime
        .register_with_capacity::<Http2ClientConnection<TestShard>, _>(
            Http2ClientConnection::<TestShard>::new(
                Http2Target::H2c {
                    authority: "dst".into(),
                    addr,
                },
                Http2ClientLimits::default(),
            ),
            32,
        )
        .expect("register http2 client");
    runtime
        .try_send(client, Http2ClientMsg::Begin)
        .expect("begin client");
    let outcome = runtime
        .call_blocking(
            client,
            Http2ClientMsg::Submit(Http2ClientRequest::get("/counter")),
            Duration::from_secs(5),
        )
        .expect("call returns outcome");
    assert!(
        matches!(
            outcome,
            tina_runtime::CallOutcome::Replied(Http2ClientReply::Outcome { .. })
        ),
        "live client GET should reply",
    );
    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let trace = runtime.shutdown().expect("shutdown returns the trace");

    // 2. Capture the run, naming the client socket I/O as live work the
    //    replay op-alphabet does not model.
    let capture = LiveReplayCapture::from_events_with_options(
        "http2 client h2c unary live capture",
        116,
        ReplayConfig::new().with_mailbox("http2-client", 32),
        "live native HTTP/2 client GET over h2c with raw outbound socket I/O",
        vec![Http2ClientReplayOp::Begin, Http2ClientReplayOp::UnaryGet],
        "native client socket lifecycle is live work, not a replayable op alphabet",
        "tina-http ThreadedRuntime http2 client DST test",
        &trace,
        TraceProjection::Exact,
        Vec::new(),
    )
    .expect("exact projection accepts the live trace")
    .with_unsupported_fact(
        "native HTTP/2 client socket I/O",
        "tcp_connect/read/write/close completions are live sockets the replay op alphabet does not model",
    );

    // 3. The simulator must fail closed on the unsupported live fact —
    //    not silently no-op, not pretend the socket work replayed.
    let candidate = capture.to_replay_case();
    let mismatch = check_captured_replay::<_, (), _>(&capture, &candidate, |case| {
        LiveReplayReport::from_case_and_events(case, &trace, (), capture.projection.clone())
    })
    .expect_err("live client socket work must fail closed under replay");
    assert!(
        mismatch.includes(CapturedReplayChange::UnsupportedFact),
        "expected an UnsupportedFact change, got {:?}",
        mismatch.changes,
    );
    let rendered = mismatch.to_string();
    assert!(
        rendered.contains("native HTTP/2 client socket I/O"),
        "mismatch must name the unsupported live socket work: {rendered}",
    );

    // 4. The saved replay case round-trips, preserving the unsupported
    //    fact and the explicit op history.
    let path = std::env::temp_dir().join(format!(
        "tina-http2-client-dst-{}-{}.case",
        std::process::id(),
        capture.expected.trace_hash
    ));
    write_saved_replay_case(&path, &capture, encode_op).expect("write saved replay case");
    let saved = read_saved_replay_case(&path, decode_op).expect("read saved replay case");
    let _ = std::fs::remove_file(&path);
    assert_eq!(
        saved.unsupported_facts.len(),
        1,
        "one unsupported fact saved"
    );
    assert!(
        saved.unsupported_facts[0]
            .fact
            .contains("native HTTP/2 client socket I/O"),
        "saved unsupported fact preserved",
    );
    assert_eq!(
        saved.history,
        capture.history.operations(),
        "explicit op history round-trips",
    );
}
