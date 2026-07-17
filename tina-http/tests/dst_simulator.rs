//! Isolate-level DST for the HTTP listener + connection state machine.
//!
//! Drives `HttpListener` + a `Counter` service through `tina_sim::Simulator`
//! with `ScriptedTcpConfig`. Asserts:
//!
//! - same seed + same script → byte-identical trace fingerprint
//! - different seed → different fingerprint
//! - covers parse-good, parse-bad, slow body (multi-chunk inbound),
//!   service-full (capacity-1 mailbox + concurrent peers), and
//!   shutdown-mid-request scenarios
//! - one named seed for an interesting interleaving is checked in for
//!   regression catch
//!
//! The simulator owns the scripted TCP layer end-to-end: the
//! `bind_addr` we declare on the listener must match what
//! `ScriptedListenerConfig` exposes; the simulator routes peer bytes to
//! the connection isolate the listener spawns per accept.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use http::{Method, StatusCode};
use tina::CallContext;
use tina::ShardId;
use tina::prelude::*;
use tina_http::{HttpLimits, HttpListener, HttpListenerMsg, HttpRequest, HttpResponse};
use tina_runtime::stable_trace_hash;
use tina_sim::{
    FaultConfig, FaultMode, LocalSendFaultMode, ScriptedListenerConfig, ScriptedPeerConfig,
    ScriptedTcpConfig, Simulator, SimulatorConfig,
};

/// Test shard distinct from anything used in real-runtime tests.
const SIM_SHARD_ID: u32 = 117;

#[derive(Debug, Default)]
struct SimShard;

impl Shard for SimShard {
    fn id(&self) -> ShardId {
        ShardId::new(SIM_SHARD_ID)
    }
}

/// Sync-reply counter service. Covers `/counter` and `/echo`.
#[derive(Debug, Default)]
struct Counter {
    value: u64,
}

impl Isolate for Counter {
    tina::isolate_types! {
        message: HttpRequest,
        reply: HttpResponse,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        io: tina_runtime::RuntimeCall<HttpRequest>,
        shard: SimShard,
    }

    fn handle(
        &mut self,
        request: HttpRequest,
        _ctx: &mut Context<'_, SimShard, Self::Reply>,
    ) -> Effect<Self> {
        reply(self.response_for(request))
    }

    fn handle_call(&mut self, request: HttpRequest, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(self.response_for(request))
    }
}

impl Counter {
    fn response_for(&mut self, request: HttpRequest) -> HttpResponse {
        match (request.method.clone(), request.path.as_str()) {
            (Method::GET, "/counter") => HttpResponse::text(self.value.to_string()),
            (Method::POST, "/counter") => {
                self.value += 1;
                HttpResponse::text(self.value.to_string())
            }
            _ => HttpResponse::with_status(StatusCode::NOT_FOUND),
        }
    }
}

/// Builds a SimulatorConfig with a single scripted listener at
/// `bind_addr` and the given peer configs.
fn build_config(
    seed: u64,
    bind_addr: SocketAddr,
    peers: Vec<ScriptedPeerConfig>,
) -> SimulatorConfig {
    SimulatorConfig {
        seed,
        tcp: ScriptedTcpConfig {
            pending_completion_capacity: 64,
            listeners: vec![ScriptedListenerConfig {
                bind_addr,
                local_addr: bind_addr,
                backlog_capacity: 16,
                peers,
            }],
        },
        ..Default::default()
    }
}

/// Constructs a peer with a single inbound chunk + standard caps.
fn one_chunk_peer(
    peer_addr: SocketAddr,
    accept_after_step: u64,
    bytes: Vec<u8>,
) -> ScriptedPeerConfig {
    ScriptedPeerConfig {
        accept_after_step,
        peer_addr,
        inbound_chunks: vec![bytes],
        inbound_capacity: 64 * 1024,
        read_chunk_cap: None,
        write_cap: 64 * 1024,
        output_capacity: 64 * 1024,
    }
}

/// Constructs a peer that drips bytes across multiple inbound chunks.
fn multi_chunk_peer(
    peer_addr: SocketAddr,
    accept_after_step: u64,
    chunks: Vec<Vec<u8>>,
) -> ScriptedPeerConfig {
    ScriptedPeerConfig {
        accept_after_step,
        peer_addr,
        inbound_chunks: chunks,
        inbound_capacity: 64 * 1024,
        read_chunk_cap: None,
        write_cap: 64 * 1024,
        output_capacity: 64 * 1024,
    }
}

/// Runs one simulation pass with the given config. Service is a
/// capacity-`service_capacity` Counter. Returns `(trace_fingerprint,
/// trace_event_count)`.
///
/// Steps with a hard budget so a runaway listener loop (which keeps
/// re-arming `tcp_accept`) cannot hang the test.
fn run_pass(
    bind_addr: SocketAddr,
    config: SimulatorConfig,
    service_capacity: usize,
) -> (u64, usize) {
    let mut sim = Simulator::new(SimShard, config);

    let counter = sim.register_with_mailbox_capacity(Counter::default(), service_capacity);

    let listener_isolate = HttpListener::<SimShard>::new(
        bind_addr,
        counter,
        HttpLimits::default(),
        Duration::from_secs(2),
        16,
    );
    let listener = sim.register_with_mailbox_capacity(listener_isolate, 8);

    sim.try_send(listener, HttpListenerMsg::Start)
        .expect("send Start");
    // Drive forward with a step budget. The listener never stops on
    // its own (it keeps re-arming `tcp_accept`), so we send Stop
    // after the request work has had time to drain.
    drive_steps(&mut sim, 256);
    sim.try_send(listener, HttpListenerMsg::Stop)
        .expect("send Stop");
    drive_steps(&mut sim, 256);

    let trace = sim.trace();
    (stable_trace_hash(trace.iter()), trace.len())
}

fn drive_steps(sim: &mut Simulator<SimShard>, budget: usize) {
    let mut idle_in_a_row = 0;
    for _ in 0..budget {
        let delivered = sim.step();
        if delivered == 0 {
            idle_in_a_row += 1;
            // Two consecutive idle steps means the simulator is
            // genuinely quiescent (one idle step might still have
            // virtual-time advancement pending).
            if idle_in_a_row >= 2 {
                break;
            }
        } else {
            idle_in_a_row = 0;
        }
    }
}

fn good_request_bytes() -> Vec<u8> {
    b"GET /counter HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".to_vec()
}

fn bad_request_bytes() -> Vec<u8> {
    b"GIBBERISH NOT-A-REQUEST\r\n\r\n".to_vec()
}

#[test]
fn same_seed_replays_identical_fingerprint_for_good_request() {
    let bind: SocketAddr = "127.0.0.1:9001".parse().unwrap();
    let peer: SocketAddr = "10.0.0.1:55001".parse().unwrap();
    let seed = 42;

    let config_a = build_config(
        seed,
        bind,
        vec![one_chunk_peer(peer, 0, good_request_bytes())],
    );
    let config_b = config_a.clone();

    let (hash_a, len_a) = run_pass(bind, config_a, 16);
    let (hash_b, len_b) = run_pass(bind, config_b, 16);

    assert_eq!(hash_a, hash_b, "same seed → same fingerprint");
    assert_eq!(len_a, len_b, "same seed → same trace length");
}

#[test]
fn different_seed_changes_fingerprint() {
    let bind: SocketAddr = "127.0.0.1:9002".parse().unwrap();
    let peer_a: SocketAddr = "10.0.0.1:55002".parse().unwrap();
    let peer_b: SocketAddr = "10.0.0.1:55012".parse().unwrap();

    // Seed only changes the trace when there is some seed-driven
    // perturbation in play. Use the same fault shape as
    // `specimen_replay_dst`: 1-in-3 timer-wake delays, 1-in-4 local-send
    // delays. With multi-peer concurrent traffic the seeded ordering
    // is observable.
    let make_config = |seed: u64| SimulatorConfig {
        seed,
        faults: FaultConfig {
            timer_wake: FaultMode::DelayBy {
                one_in: 3,
                by: Duration::from_millis(1),
            },
            local_send: LocalSendFaultMode::DelayByRounds {
                one_in: 4,
                rounds: 1,
            },
            ..Default::default()
        },
        tcp: ScriptedTcpConfig {
            pending_completion_capacity: 64,
            listeners: vec![ScriptedListenerConfig {
                bind_addr: bind,
                local_addr: bind,
                backlog_capacity: 16,
                peers: vec![
                    one_chunk_peer(peer_a, 0, good_request_bytes()),
                    one_chunk_peer(peer_b, 0, good_request_bytes()),
                ],
            }],
        },
        ..Default::default()
    };

    let (hash_a, _) = run_pass(bind, make_config(0), 1);
    let (hash_b, _) = run_pass(bind, make_config(2), 1);

    assert_ne!(hash_a, hash_b, "different seed → different fingerprint");
}

#[test]
fn parse_bad_request_replays_deterministically() {
    let bind: SocketAddr = "127.0.0.1:9003".parse().unwrap();
    let peer: SocketAddr = "10.0.0.1:55003".parse().unwrap();
    let seed = 13;

    let cfg = build_config(
        seed,
        bind,
        vec![one_chunk_peer(peer, 0, bad_request_bytes())],
    );
    let cfg2 = cfg.clone();

    let (hash_a, _) = run_pass(bind, cfg, 16);
    let (hash_b, _) = run_pass(bind, cfg2, 16);

    assert_eq!(hash_a, hash_b, "bad-request scenario must be deterministic");
}

#[test]
fn slow_body_multichunk_inbound_replays_deterministically() {
    let bind: SocketAddr = "127.0.0.1:9004".parse().unwrap();
    let peer: SocketAddr = "10.0.0.1:55004".parse().unwrap();
    let seed = 27;

    // POST /counter with a 5-byte body sent across three inbound chunks.
    let chunks = vec![
        b"POST /counter HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\nConnection: close\r\n\r\n"
            .to_vec(),
        b"hel".to_vec(),
        b"lo".to_vec(),
    ];
    let cfg = build_config(seed, bind, vec![multi_chunk_peer(peer, 0, chunks.clone())]);
    let cfg2 = cfg.clone();

    let (hash_a, _) = run_pass(bind, cfg, 16);
    let (hash_b, _) = run_pass(bind, cfg2, 16);

    assert_eq!(
        hash_a, hash_b,
        "slow-body scenario must replay byte-identically"
    );
}

#[test]
fn shutdown_mid_request_replays_deterministically() {
    let bind: SocketAddr = "127.0.0.1:9005".parse().unwrap();
    let peer: SocketAddr = "10.0.0.1:55005".parse().unwrap();
    let seed = 91;

    // Peer that streams a request head only, then leaves the
    // connection idle. Listener will receive a Stop in the middle.
    let cfg = SimulatorConfig {
        seed,
        tcp: ScriptedTcpConfig {
            pending_completion_capacity: 64,
            listeners: vec![ScriptedListenerConfig {
                bind_addr: bind,
                local_addr: bind,
                backlog_capacity: 16,
                peers: vec![multi_chunk_peer(
                    peer,
                    0,
                    // Partial head, never completes.
                    vec![b"GET /counter HTTP/1.1\r\nHost:".to_vec()],
                )],
            }],
        },
        ..Default::default()
    };

    // Run this scenario with explicit Stop injection so the simulator's
    // event order includes the Stop ↔ Read-pending interleaving.
    let mut sim_a = Simulator::new(SimShard, cfg.clone());
    let counter_a = sim_a.register_with_mailbox_capacity(Counter::default(), 16);
    let listener_a = sim_a.register_with_mailbox_capacity(
        HttpListener::<SimShard>::new(
            bind,
            counter_a,
            HttpLimits::default(),
            Duration::from_secs(2),
            16,
        ),
        8,
    );
    sim_a.try_send(listener_a, HttpListenerMsg::Start).unwrap();
    drive_steps(&mut sim_a, 64);
    sim_a.try_send(listener_a, HttpListenerMsg::Stop).unwrap();
    drive_steps(&mut sim_a, 256);
    let hash_a = stable_trace_hash(sim_a.trace().iter());

    let mut sim_b = Simulator::new(SimShard, cfg);
    let counter_b = sim_b.register_with_mailbox_capacity(Counter::default(), 16);
    let listener_b = sim_b.register_with_mailbox_capacity(
        HttpListener::<SimShard>::new(
            bind,
            counter_b,
            HttpLimits::default(),
            Duration::from_secs(2),
            16,
        ),
        8,
    );
    sim_b.try_send(listener_b, HttpListenerMsg::Start).unwrap();
    drive_steps(&mut sim_b, 64);
    sim_b.try_send(listener_b, HttpListenerMsg::Stop).unwrap();
    drive_steps(&mut sim_b, 256);
    let hash_b = stable_trace_hash(sim_b.trace().iter());

    assert_eq!(
        hash_a, hash_b,
        "shutdown-mid-request must replay deterministically"
    );
}

#[test]
fn service_full_with_concurrent_peers_replays_deterministically() {
    // Capacity-1 service mailbox + two peers arriving simultaneously
    // creates a service-full interleaving. We only assert determinism;
    // whether a 503 lands on the wire depends on internal scheduling
    // and is checked separately at the unit-test layer.
    let bind: SocketAddr = "127.0.0.1:9006".parse().unwrap();
    let peer_a: SocketAddr = "10.0.0.1:55006".parse().unwrap();
    let peer_b: SocketAddr = "10.0.0.1:55007".parse().unwrap();
    let seed = 31337;

    let make_peers = || {
        vec![
            one_chunk_peer(peer_a, 0, good_request_bytes()),
            one_chunk_peer(peer_b, 0, good_request_bytes()),
        ]
    };

    let cfg_a = build_config(seed, bind, make_peers());
    let cfg_b = build_config(seed, bind, make_peers());

    let (hash_a, _) = run_pass(bind, cfg_a, 1);
    let (hash_b, _) = run_pass(bind, cfg_b, 1);

    assert_eq!(
        hash_a, hash_b,
        "service-full + concurrent peers must replay deterministically"
    );
}

/// Saved seed for a multi-peer interleaving. The pinned fingerprint
/// below was captured under the listed config. If the trace shape
/// changes (intentional or otherwise) this assertion fails and forces
/// a conscious update — recapture by re-running with `--nocapture`
/// after an explicit "yes, the trace shape changed" review.
#[test]
fn saved_seed_interleaving_fingerprint_is_stable() {
    const SAVED_SEED: u64 = 0xBEEF_C0DE;
    /// Pinned trace fingerprint for `SAVED_SEED` + the config below.
    /// Bump only after reviewing why the trace shape changed.
    /// Last update: each fully buffered plain-TCP service call now spawns one
    /// observed peer monitor. The two peers add their bounded child lifecycle,
    /// terminal reservation, one-byte read, settlement, and disposal facts
    /// (81 -> 121). The monitor terminates once, and the same-seed rerun below
    /// still proves byte-identical replay.
    const EXPECTED_HASH: u64 = 10_434_183_390_251_076_748;
    /// Pinned trace event count for `SAVED_SEED` + the config below.
    const EXPECTED_LEN: usize = 121;

    let bind: SocketAddr = "127.0.0.1:9007".parse().unwrap();
    let peer_a: SocketAddr = "10.0.0.1:55008".parse().unwrap();
    let peer_b: SocketAddr = "10.0.0.1:55009".parse().unwrap();

    let make_cfg = || {
        build_config(
            SAVED_SEED,
            bind,
            vec![
                multi_chunk_peer(
                    peer_a,
                    0,
                    vec![
                        b"POST /counter HTTP/1.1\r\nHost: a\r\n".to_vec(),
                        b"Content-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                    ],
                ),
                one_chunk_peer(peer_b, 1, good_request_bytes()),
            ],
        )
    };

    let (hash, len) = run_pass(bind, make_cfg(), 16);
    // Pin against the saved fingerprint — this is what catches drift.
    assert_eq!(
        hash, EXPECTED_HASH,
        "saved-seed trace fingerprint changed; got {hash}, expected {EXPECTED_HASH}. \
         If the change is intentional, recapture with `cargo test --test dst_simulator \
         saved_seed -- --nocapture` and update EXPECTED_HASH + EXPECTED_LEN.",
    );
    assert_eq!(
        len, EXPECTED_LEN,
        "saved-seed trace length changed; got {len}, expected {EXPECTED_LEN}",
    );

    // Sanity: a second run with the same config must agree byte-for-
    // byte. (If this ever differs from the pinned value above, the
    // simulator itself has drifted.)
    let (hash2, len2) = run_pass(bind, make_cfg(), 16);
    assert_eq!(hash, hash2, "same-seed reruns must agree on fingerprint");
    assert_eq!(len, len2, "same-seed reruns must agree on trace length");
}

// ---------------------------------------------------------------
// Chunked-stream DST: register IterBodySource + a chunked service,
// drive a single GET /big-chunked, compare fingerprints across two
// passes with the same seed.
// ---------------------------------------------------------------

use tina_http::{IterBodySource, ResponseChunkMsg, ResponseChunkReply};

struct ChunkedDemoService {
    source: tina::Address<ResponseChunkMsg, ResponseChunkReply>,
}

impl Isolate for ChunkedDemoService {
    tina::isolate_types! {
        message: HttpRequest,
        reply: HttpResponse,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        io: tina_runtime::RuntimeCall<HttpRequest>,
        shard: SimShard,
    }
    fn handle(
        &mut self,
        request: HttpRequest,
        _ctx: &mut Context<'_, SimShard, Self::Reply>,
    ) -> Effect<Self> {
        reply(self.response_for(request))
    }

    fn handle_call(&mut self, request: HttpRequest, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(self.response_for(request))
    }
}

impl ChunkedDemoService {
    fn response_for(&self, request: HttpRequest) -> HttpResponse {
        if request.path == "/big-chunked" {
            HttpResponse::stream_chunked(StatusCode::OK, self.source)
        } else {
            HttpResponse::not_found()
        }
    }
}

fn run_chunked_pass(bind_addr: SocketAddr, config: SimulatorConfig) -> (u64, usize) {
    let mut sim = Simulator::new(SimShard, config);

    // Two-chunk source: enough to exercise pull/write/pull/write/eof
    // without needing many trace events.
    let chunks: Vec<Vec<u8>> = vec![b"first ".to_vec(), b"second".to_vec()];
    let source =
        sim.register_with_mailbox_capacity(IterBodySource::<SimShard>::new(chunks.into_iter()), 16);
    let service = sim.register_with_mailbox_capacity(ChunkedDemoService { source }, 16);
    let listener_isolate = HttpListener::<SimShard>::new(
        bind_addr,
        service,
        HttpLimits::default(),
        Duration::from_secs(2),
        16,
    );
    let listener = sim.register_with_mailbox_capacity(listener_isolate, 8);

    sim.try_send(listener, HttpListenerMsg::Start)
        .expect("send Start");
    drive_steps(&mut sim, 256);
    sim.try_send(listener, HttpListenerMsg::Stop)
        .expect("send Stop");
    drive_steps(&mut sim, 256);

    let trace = sim.trace();
    (stable_trace_hash(trace.iter()), trace.len())
}

fn chunked_request_bytes() -> Vec<u8> {
    b"GET /big-chunked HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".to_vec()
}

#[test]
fn chunked_response_same_seed_replays_identical_fingerprint() {
    let bind: SocketAddr = "127.0.0.1:9100".parse().unwrap();
    let peer: SocketAddr = "10.0.0.1:55100".parse().unwrap();
    let seed = 0xABCD_1234u64;

    let make_cfg = || {
        build_config(
            seed,
            bind,
            vec![one_chunk_peer(peer, 0, chunked_request_bytes())],
        )
    };

    let (hash_a, len_a) = run_chunked_pass(bind, make_cfg());
    let (hash_b, len_b) = run_chunked_pass(bind, make_cfg());

    assert_eq!(
        hash_a, hash_b,
        "same seed + same script → identical chunked-trace fingerprint"
    );
    assert_eq!(len_a, len_b, "same seed → identical event count");
    assert!(len_a > 0, "trace should not be empty");
}

// We don't ship a "different seed → different fingerprint" test
// for chunked: with no fault injection and a single peer the seed
// has nothing to perturb, so different seeds produce identical
// traces by design. The same-seed determinism property — which is
// the load-bearing one for replay — is proven by the test above.
