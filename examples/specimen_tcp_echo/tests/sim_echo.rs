//! Deterministic simulator proof for the echo connection.
//!
//! The SAME `EchoListener` and `EchoConnection` isolates that run live
//! over a real socket are registered here into `tina_sim::Simulator`
//! and driven by a scripted TCP peer. From a fixed seed the run is
//! byte-for-byte reproducible: the peer output, the event record, and
//! the trace hash all replay identically.

use std::net::SocketAddr;

use tina::prelude::*;
use tina_runtime::{CallKind, RuntimeEvent, RuntimeEventKind, stable_trace_hash};
use tina_sim::{
    ReplayArtifact, ScriptedListenerConfig, ScriptedPeerConfig, ScriptedTcpConfig, Simulator,
    SimulatorConfig,
};

use specimen_tcp_echo::{EchoListener, EchoListenerMsg, MAX_CHUNK};

// Saved golden. Refresh only after a conscious trace-shape review (a
// new RuntimeEventKind variant, or an intentional isolate-logic
// change). A silent drift here is a real defect.
const SAVED_TRACE_HASH: u64 = 0x5ea0_694c_5c3f_7682;
const SEED: u64 = 7;

fn bind_addr() -> SocketAddr {
    "127.0.0.1:0".parse().expect("bind addr")
}

fn local_addr(port: u16) -> SocketAddr {
    format!("127.0.0.1:{port}").parse().expect("local addr")
}

fn peer_addr(port: u16) -> SocketAddr {
    format!("127.0.0.1:{port}").parse().expect("peer addr")
}

/// Registers the shared `EchoListener` into a simulator whose single
/// scripted peer delivers `payload`, then runs to quiescence.
fn run_echo_sim(payload: &[u8]) -> ReplayArtifact {
    let mut sim = Simulator::new(
        SingleShard,
        SimulatorConfig {
            seed: SEED,
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 32,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(41000),
                    backlog_capacity: 1,
                    peers: vec![ScriptedPeerConfig {
                        accept_after_step: 1,
                        peer_addr: peer_addr(51001),
                        inbound_capacity: payload.len(),
                        inbound_chunks: vec![payload.to_vec()],
                        read_chunk_cap: None,
                        write_cap: payload.len(),
                        output_capacity: 1024,
                    }],
                }],
            },
            ..SimulatorConfig::default()
        },
    );

    let listener = sim.register(EchoListener::new(bind_addr(), 1));
    sim.try_send(listener, EchoListenerMsg::Start)
        .expect("start listener");
    sim.run_until_quiescent();

    // MAX_CHUNK is the read size the live isolate uses; assert the sim
    // exercises the same chunk contract rather than a shrunk one.
    assert!(payload.len() <= MAX_CHUNK, "payload must fit one read");
    sim.replay_artifact()
}

fn count_call_completed(trace: &[RuntimeEvent], kind: CallKind) -> usize {
    trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted { call_kind, .. } if call_kind == kind
            )
        })
        .count()
}

fn peer_output(artifact: &ReplayArtifact) -> Vec<Vec<u8>> {
    artifact
        .observed_peer_output()
        .iter()
        .map(|output| output.bytes().to_vec())
        .collect()
}

#[test]
fn sim_echo_round_trips_payload() {
    let payload = b"hello from simulated tcp".to_vec();
    let artifact = run_echo_sim(&payload);

    assert_eq!(
        peer_output(&artifact),
        vec![payload.clone()],
        "the peer must observe its bytes echoed back",
    );
    let record = artifact.event_record();
    assert_eq!(count_call_completed(record, CallKind::TcpBind), 1);
    assert_eq!(count_call_completed(record, CallKind::TcpAccept), 1);
    assert!(count_call_completed(record, CallKind::TcpRead) >= 2);
    assert_eq!(count_call_completed(record, CallKind::TcpWrite), 1);
    assert_eq!(count_call_completed(record, CallKind::TcpStreamClose), 1);
    assert_eq!(count_call_completed(record, CallKind::TcpListenerClose), 1);
    assert_eq!(
        record
            .iter()
            .filter(|event| matches!(event.kind(), RuntimeEventKind::Spawned { .. }))
            .count(),
        1,
        "exactly one connection isolate is spawned",
    );
}

#[test]
fn sim_echo_replays_byte_for_byte() {
    let payload = b"replay me".to_vec();
    let first = run_echo_sim(&payload);
    let second = run_echo_sim(&payload);
    assert_eq!(
        first.event_record(),
        second.event_record(),
        "same seed must produce the identical event record",
    );
    assert_eq!(
        first.observed_peer_output(),
        second.observed_peer_output(),
        "same seed must produce the identical peer output",
    );
}

#[test]
fn sim_echo_matches_golden_trace_hash() {
    let payload = b"hello from simulated tcp".to_vec();
    let artifact = run_echo_sim(&payload);
    assert_eq!(
        stable_trace_hash(artifact.event_record().iter()),
        SAVED_TRACE_HASH,
        "trace shape drifted from the saved golden; review before refreshing",
    );
}
