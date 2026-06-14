//! Protocol-chaos regression suite.
//!
//! These tests are the credible-proof rung for protocol bad-peer behaviour.
//! They run the hermetic WebSocket compliance corpus, the HTTP/2 and gRPC
//! probes, the WebSocket byte-replay save/shrink workflow, and the live-replay
//! protocol-fact bridge end to end through the public API.

use std::net::{SocketAddr, TcpListener};
use std::thread;
use std::time::{Duration, Instant};

use tina_proof_harness::bad_peer::{self, BadPeerScenario};
use tina_proof_harness::byte_replay::{ByteReplayDirection, ProtocolByteReplayCase};
use tina_proof_harness::grpc::{GrpcOutcome, grpc_probe_suite};
use tina_proof_harness::http2::{Http2Outcome, http2_probe_suite};
use tina_proof_harness::protocol_chaos::{
    PeerAction, ProtocolChaosFamily, ProtocolChaosReport, TerminalAction,
};
use tina_proof_harness::websocket::{AppMessage, WebSocketLimits, client_frame, compliance_corpus};

use tina::capacity::{CapacityMode, CapacitySurfaceReport};
use tina_runtime::{
    GrpcStatusCode, Http2ResetReason, ProtocolConnectionId, ProtocolFact, WebSocketCloseReason,
    WebSocketSessionId,
};
use tina_sim::dst::{
    LiveReplayCapture, LiveReplayFact, LiveReplayReport, ProtocolReplayMismatch, ReplayCase,
    ReplayConfig, ReplayReport, TraceProjectionError, UnsupportedProtocolFact,
    capture_overload_run, check_captured_replay, classify_protocol_facts, overload_capacity_fact,
    read_saved_replay_case, replay_overload_bug, save_overload_bug, write_saved_replay_case,
};

const OPCODE_TEXT: u8 = 0x1;

#[test]
fn websocket_compliance_corpus_holds() {
    for case in compliance_corpus() {
        let report = case.check().unwrap_or_else(|mismatch| panic!("{mismatch}"));
        // The derived chaos expectation matches the produced report too.
        case.expectation()
            .check(&report)
            .unwrap_or_else(|mismatch| panic!("chaos expectation for {}: {mismatch}", case.name));
    }
}

#[test]
fn valid_fragmented_text_reaches_app_once_and_invalid_never_does() {
    let corpus = compliance_corpus();
    let valid = corpus
        .iter()
        .find(|c| c.name == "ws_valid_fragmented_text")
        .expect("case");
    let run = valid.run();
    assert_eq!(
        run.app_messages,
        vec![AppMessage::Text("Hello".to_owned())],
        "valid fragmented text must reassemble to one app message"
    );

    let invalid = corpus
        .iter()
        .find(|c| c.name == "ws_invalid_utf8_across_fragments")
        .expect("case");
    let run = invalid.run();
    assert!(
        run.app_messages.is_empty(),
        "invalid UTF-8 must never reach app code"
    );
    assert_eq!(
        run.close,
        Some((Some(1007), WebSocketCloseReason::ProtocolError))
    );
}

#[test]
fn http2_probes_map_to_typed_facts() {
    let suite = http2_probe_suite();
    assert!(suite.len() >= 6, "expected the full HTTP/2 probe suite");
    for probe in suite {
        let report = probe
            .check()
            .unwrap_or_else(|mismatch| panic!("{mismatch}"));
        assert!(
            !report.protocol_facts.is_empty(),
            "{}: malformed frames must map to a typed fact, not just 'closed'",
            probe.name
        );
    }

    // The frame-size probe is a typed FrameSizeError reset, not a bare close.
    let frame_size = http2_probe_suite()
        .into_iter()
        .find(|p| p.name == "h2_invalid_frame_size")
        .expect("probe");
    assert_eq!(
        frame_size.run().1,
        Http2Outcome::StreamReset(Http2ResetReason::FrameSizeError)
    );
}

#[test]
fn grpc_probes_return_typed_outcomes() {
    let missing = grpc_probe_suite()
        .into_iter()
        .find(|p| p.name == "grpc_missing_status")
        .expect("probe");
    assert_eq!(missing.run().outcome, GrpcOutcome::MissingStatus);

    let oversized = grpc_probe_suite()
        .into_iter()
        .find(|p| p.name == "grpc_oversized_message")
        .expect("probe");
    assert_eq!(oversized.run().outcome, GrpcOutcome::MessageTooLarge);
}

#[test]
fn reconnect_storm_does_not_leak_sessions_or_bytes() {
    let (addr, server) = accept_n_listener(3);
    let outcome = bad_peer::run(
        "storm",
        addr,
        Duration::from_secs(1),
        BadPeerScenario::ReconnectStorm { count: 3 },
    );
    let _ = server.join();

    // Every connection is accounted for; no bytes were sent or queued.
    assert_eq!(
        outcome.connects_ok + outcome.connects_failed,
        3,
        "every reconnect attempt must be accounted for: {outcome:?}"
    );
    assert_eq!(outcome.bytes_sent, 0, "storm queues no bytes: {outcome:?}");
    assert_eq!(outcome.bytes_read, 0, "storm reads no bytes: {outcome:?}");

    let report = ProtocolChaosReport::from_bad_peer(
        "reconnect_storm",
        ProtocolChaosFamily::Tcp,
        PeerAction::ReconnectStorm,
        &outcome,
    );
    assert_eq!(report.bytes_written, 0);
    assert!(report.protocol_facts.is_empty());
}

#[test]
fn stalled_peer_produces_a_visible_typed_report() {
    let (addr, server) = read_and_close_listener();
    let outcome = bad_peer::run(
        "stalled_writer",
        addr,
        Duration::from_secs(1),
        BadPeerScenario::StalledWriter {
            first_chunk: b"GET / HTTP/1.1\r\n".to_vec(),
            rest: b"Host: x\r\nConnection: close\r\n\r\n".to_vec(),
            stall_for: Duration::from_millis(60),
        },
    );
    let _ = server.join();

    let report = ProtocolChaosReport::from_bad_peer(
        "stalled_writer",
        ProtocolChaosFamily::Http1,
        PeerAction::Stalled,
        &outcome,
    );
    // The stall is visible in the typed report, not buried in a log line, and
    // the terminal side's action is recorded.
    assert!(
        report.elapsed >= Duration::from_millis(50),
        "stall must be visible in the report: {}",
        report.summary_line()
    );
    assert_ne!(
        report.terminal_action,
        TerminalAction::None,
        "a stalled peer must record a terminal action: {}",
        report.summary_line()
    );
}

#[test]
fn byte_replay_reproduces_and_shrinks_one_saved_bad_frame_case() {
    // A valid ping, a valid text frame, the bad frame (reserved bits set),
    // then trailing junk the close should make irrelevant.
    let mut bad = client_frame(true, OPCODE_TEXT, b"x");
    bad[0] |= 0x40; // RSV1 set.
    let chunks = vec![
        client_frame(true, 0x9, b"hb"),
        client_frame(true, OPCODE_TEXT, b"ok"),
        bad,
        b"trailing".to_vec(),
    ];
    let case = ProtocolByteReplayCase::capture(
        "ws_reserved_bits_regression",
        ByteReplayDirection::ClientToServer,
        WebSocketLimits::default(),
        chunks,
    );

    // Round-trips through a saved file and still reproduces.
    let path =
        std::env::temp_dir().join(format!("tina-proto-regression-{}.case", std::process::id()));
    case.save(&path).expect("save");
    let loaded = ProtocolByteReplayCase::load(&path, "ws_reserved_bits_regression").expect("load");
    let _ = std::fs::remove_file(&path);
    loaded.replay().expect("saved case reproduces");

    // Shrinks to a smaller chunk set that still closes with a protocol error,
    // and the smaller case is self-consistent.
    let shrink = loaded
        .shrink(|report| report.close == Some((Some(1002), WebSocketCloseReason::ProtocolError)))
        .expect("original reproduces");
    assert!(shrink.shrunk_len < shrink.original_len);
    shrink.shrunk_case.replay().expect("shrunk case reproduces");
}

#[test]
fn live_replay_can_save_websocket_http2_grpc_protocol_facts() {
    // WebSocket facts from the corpus.
    let ws_run = compliance_corpus()
        .into_iter()
        .find(|c| c.name == "ws_invalid_utf8_across_fragments")
        .expect("case")
        .run();
    assert_save_and_replay("ws_live_facts", ws_run.facts.clone());

    // HTTP/2 facts from a probe.
    let (h2_facts, _) = http2_probe_suite()
        .into_iter()
        .find(|p| p.name == "h2_rst_stream_during_body")
        .expect("probe")
        .run();
    assert_save_and_replay("h2_live_facts", h2_facts.clone());

    // gRPC facts from a probe.
    let grpc_facts = grpc_probe_suite()
        .into_iter()
        .find(|p| p.name == "grpc_oversized_message")
        .expect("probe")
        .run()
        .facts;
    assert_save_and_replay("grpc_live_facts", grpc_facts);
}

#[test]
fn mixed_protocol_and_capacity_capture_fails_if_either_diverges() {
    let protocol = LiveReplayFact::protocol(ws_closed(1007, WebSocketCloseReason::ProtocolError));
    let capacity = LiveReplayFact::capacity_surface(&CapacitySurfaceReport::weighted(
        "ws.outbound.queue",
        CapacityMode::Fixed,
        16,
        0,
        9,
        1,
        "frames",
    ));
    let (capture, candidate) =
        capture_with_facts("mixed_capture", vec![protocol.clone(), capacity.clone()]);

    // Both reproduced: clean.
    check_captured_replay(
        &capture,
        &candidate,
        runner(vec![protocol.clone(), capacity.clone()]),
    )
    .expect("both families replay");
    // Drop the protocol fact: fail.
    check_captured_replay(&capture, &candidate, runner(vec![capacity.clone()]))
        .expect_err("dropping the protocol family fails the capture");
    // Drop the capacity fact: fail.
    check_captured_replay(&capture, &candidate, runner(vec![protocol]))
        .expect_err("dropping the capacity family fails the capture");
}

#[test]
fn overload_bugbox_saves_protocol_and_capacity_facts_in_one_capture() {
    // A WebSocket slow-peer close under bounded broadcast pressure: the close
    // is a protocol fact, the broadcast queue is a capacity fact. One capture
    // carries both and replays only when both reproduce.
    let surface =
        CapacitySurfaceReport::count("ws.room.broadcast", CapacityMode::Fixed, 4, 0, 4, 2);
    let capacity = overload_capacity_fact(&surface).expect("bounded overload fact");
    let protocol = LiveReplayFact::protocol(ProtocolFact::WebSocketSlowPeerClosed {
        session: WebSocketSessionId::new(1),
        queued_frames: 4,
        queued_bytes: 4096,
    });

    let capture = capture_overload_run::<&'static str>("ws_overload_with_protocol")
        .with_seed(1)
        .with_config(ReplayConfig::new().with_mailbox("room", 4))
        .with_scenario("ws slow peer under broadcast pressure")
        .with_history(vec!["broadcast", "drain"])
        .with_invariant("bounded queue and typed slow-peer close")
        .with_trace(&[])
        .with_fact(capacity.clone())
        .with_fact(protocol.clone())
        .finish()
        .expect("capture builds");
    let candidate = capture.to_replay_case();

    let path =
        std::env::temp_dir().join(format!("tina-proto-overload-{}.case", std::process::id()));
    let saved = save_overload_bug(&path, &capture, |op| (*op).to_owned()).expect("save bugbox");
    assert!(saved.replay_hint.contains("replay_overload_bug"));
    let _ = std::fs::remove_file(&path);

    // Both families reproduced: replay succeeds.
    replay_overload_bug(
        &capture,
        &candidate,
        runner(vec![capacity.clone(), protocol.clone()]),
    )
    .expect("both families replay");
    // Drop the protocol fact: the whole capture fails closed.
    replay_overload_bug(&capture, &candidate, runner(vec![capacity]))
        .expect_err("dropping the protocol fact fails the bugbox");
}

#[test]
fn protocol_fact_mismatch_distinguishes_diverged_from_unsupported() {
    let live = [
        ws_closed(1007, WebSocketCloseReason::ProtocolError),
        h2_reset(),
    ];
    let sim = [ws_closed(1007, WebSocketCloseReason::ProtocolError)];

    // Without an unsupported note, the missing HTTP/2 reset is a real divergence.
    let diverged = classify_protocol_facts(&live, &sim, &[]);
    assert_eq!(diverged.len(), 1);
    assert!(matches!(
        diverged[0],
        ProtocolReplayMismatch::Diverged {
            live_only: true,
            ..
        }
    ));

    // Naming it unsupported turns it into a coverage gap, not a bug.
    let unsupported = [UnsupportedProtocolFact::new(
        h2_reset(),
        "kernel RST timing is live-only",
    )];
    let gap = classify_protocol_facts(&live, &sim, &unsupported);
    assert_eq!(gap.len(), 1);
    assert!(matches!(
        gap[0],
        ProtocolReplayMismatch::UnsupportedProtocolFact { .. }
    ));
}

#[test]
fn saved_capture_live_fact_lines_name_the_family() {
    let facts = vec![
        LiveReplayFact::protocol(ws_closed(1000, WebSocketCloseReason::Normal)),
        LiveReplayFact::protocol(h2_reset()),
        LiveReplayFact::protocol(grpc_status()),
    ];
    let (capture, _) = capture_with_facts("saved_family_lines", facts);
    let path = std::env::temp_dir().join(format!(
        "tina-proto-family-lines-{}.case",
        std::process::id()
    ));
    write_saved_replay_case(&path, &capture, |op| (*op).to_owned()).expect("save");
    let body = std::fs::read_to_string(&path).expect("read");
    assert!(body.contains("protocol WebSocket"), "{body}");
    assert!(body.contains("protocol Http2"), "{body}");
    assert!(body.contains("protocol Grpc"), "{body}");

    // The saved live-fact lines survive a read back, family token intact —
    // even though protocol-fact debug bodies and capacity facts both carry
    // characters the line format also uses.
    let saved = read_saved_replay_case::<&'static str, _>(&path, |text| match text {
        "run" => Ok("run"),
        other => Err(format!("unexpected op {other:?}")),
    })
    .expect("read back");
    let _ = std::fs::remove_file(&path);
    assert_eq!(saved.live_facts.len(), 3, "{:?}", saved.live_facts);
    assert!(
        saved
            .live_facts
            .iter()
            .any(|f| f.contains("protocol WebSocket"))
    );
    assert!(
        saved
            .live_facts
            .iter()
            .any(|f| f.contains("protocol Http2"))
    );
    assert!(saved.live_facts.iter().any(|f| f.contains("protocol Grpc")));
}

#[test]
fn print_typed_protocol_chaos_reports() {
    // Visible with `--nocapture` (see `make proof-bad-peer`): one typed report
    // line per protocol-chaos case. This is the typed-report surface, not a
    // log scrape.
    for case in compliance_corpus() {
        if let Ok(report) = case.check() {
            println!("{}", report.summary_line());
        }
    }
    for probe in http2_probe_suite() {
        if let Ok(report) = probe.check() {
            println!("{}", report.summary_line());
        }
    }
    for probe in grpc_probe_suite() {
        if let Ok(report) = probe.check() {
            println!("{}", report.summary_line());
        }
    }
}

#[test]
fn protocol_chaos_soak() {
    // Same corpus + probe semantics as the fast gate, repeated. The fast gate
    // runs one iteration; `make proof-soak` sets TINA_PROTOCOL_SOAK_ITERS high.
    let iters: u64 = std::env::var("TINA_PROTOCOL_SOAK_ITERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);
    let mut checked = 0u64;
    for _ in 0..iters {
        for case in compliance_corpus() {
            case.check().unwrap_or_else(|mismatch| panic!("{mismatch}"));
            checked += 1;
        }
        for probe in http2_probe_suite() {
            probe
                .check()
                .unwrap_or_else(|mismatch| panic!("{mismatch}"));
            checked += 1;
        }
        for probe in grpc_probe_suite() {
            probe
                .check()
                .unwrap_or_else(|mismatch| panic!("{mismatch}"));
            checked += 1;
        }
    }
    println!("protocol_chaos_soak iters={iters} cases_checked={checked}");
}

// --- helpers ---------------------------------------------------------------

fn ws_closed(code: u16, reason: WebSocketCloseReason) -> ProtocolFact {
    ProtocolFact::WebSocketSessionClosed {
        session: WebSocketSessionId::new(1),
        reason,
        code: Some(code),
    }
}

fn h2_reset() -> ProtocolFact {
    ProtocolFact::Http2StreamReset {
        connection: ProtocolConnectionId::new(1),
        stream: tina_runtime::Http2StreamId::new(3),
        direction: tina_runtime::ProtocolDirection::Inbound,
        reason: Http2ResetReason::Cancel,
    }
}

fn grpc_status() -> ProtocolFact {
    ProtocolFact::GrpcFinalStatusReceived {
        connection: ProtocolConnectionId::new(1),
        stream: tina_runtime::GrpcStreamId::new(1),
        status: GrpcStatusCode::ResourceExhausted,
    }
}

fn capture_with_facts(
    name: &'static str,
    facts: Vec<LiveReplayFact>,
) -> (LiveReplayCapture<&'static str>, ReplayCase<&'static str>) {
    let case = ReplayCase::new(
        name,
        1,
        ReplayConfig::new(),
        "protocol chaos capture",
        vec!["run"],
        "protocol facts reproduce in the simulator",
    );
    let capture = LiveReplayCapture::from_case_and_events(&case, "protocol chaos live", &[])
        .with_live_facts(facts);
    let candidate = capture.to_replay_case();
    (capture, candidate)
}

fn runner(
    facts: Vec<LiveReplayFact>,
) -> impl FnMut(&ReplayCase<&'static str>) -> Result<LiveReplayReport<&'static str>, TraceProjectionError>
{
    move |case: &ReplayCase<&'static str>| {
        Ok(
            LiveReplayReport::exact(ReplayReport::from_case_and_events(case, &[], "ok"))
                .with_live_facts(facts.clone()),
        )
    }
}

fn assert_save_and_replay(name: &'static str, facts: Vec<ProtocolFact>) {
    assert!(!facts.is_empty(), "{name}: expected at least one fact");
    let live: Vec<LiveReplayFact> = facts.into_iter().map(LiveReplayFact::protocol).collect();
    let (capture, candidate) = capture_with_facts(name, live.clone());
    check_captured_replay(&capture, &candidate, runner(live))
        .unwrap_or_else(|mismatch| panic!("{name}: protocol facts must replay: {mismatch}"));
}

/// Binds a listener that accepts one connection, drains its request, replies
/// with a tiny canned response, and closes — enough to give a stalled-writer
/// scenario a server that eventually closes.
fn read_and_close_listener() -> (SocketAddr, thread::JoinHandle<()>) {
    use std::io::{Read, Write};
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let handle = thread::spawn(move || {
        listener.set_nonblocking(false).expect("blocking listener");
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
            let _ = stream.flush();
            drop(stream);
        }
    });
    thread::sleep(Duration::from_millis(20));
    (addr, handle)
}

/// Binds a listener that accepts up to `count` connections then exits.
fn accept_n_listener(count: usize) -> (SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    listener.set_nonblocking(true).expect("non-blocking");
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut accepted = 0;
        while accepted < count && Instant::now() < deadline {
            match listener.accept() {
                Ok((stream, _)) => {
                    accepted += 1;
                    drop(stream);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
    thread::sleep(Duration::from_millis(20));
    (addr, handle)
}
