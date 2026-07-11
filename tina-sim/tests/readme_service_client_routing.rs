//! DST proof for the service-client continuation shape documented in chapter 15.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tina::prelude::*;
use tina_runtime::{
    CallError, CallKind, RuntimeEventKind, StreamId, tcp_connect, tcp_read, tcp_write,
};
use tina_sim::{
    FaultConfig, ScriptedListenerConfig, ScriptedPeerConfig, ScriptedTcpConfig, Simulator,
    SimulatorConfig, TcpCompletionFaultMode, dst::InvariantSuite,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Observation {
    request_id: u64,
    stream: StreamId,
    bytes: Vec<u8>,
}

type Observations = Arc<Mutex<Vec<Observation>>>;
type RunResult = (Vec<Observation>, Vec<tina_runtime::RuntimeEvent>);

#[derive(Debug)]
enum ClientMsg {
    Start {
        request_id: u64,
        payload: Vec<u8>,
    },
    Connected {
        request_id: u64,
        payload: Vec<u8>,
        result: Result<(StreamId, SocketAddr, SocketAddr), CallError>,
    },
    Wrote {
        request_id: u64,
        stream: StreamId,
        result: Result<usize, CallError>,
    },
    Read {
        request_id: u64,
        stream: StreamId,
        result: Result<Vec<u8>, CallError>,
    },
}

#[derive(Debug)]
struct Client {
    target: SocketAddr,
    observed: Observations,
}

#[tina_runtime::isolate(message = ClientMsg)]
impl Client {
    fn handle(
        &mut self,
        msg: ClientMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ClientMsg::Start {
                request_id,
                payload,
            } => tcp_connect(self.target).then(move |result| ClientMsg::Connected {
                request_id,
                payload,
                result,
            }),
            ClientMsg::Connected {
                request_id,
                payload,
                result: Ok((stream, _, _)),
            } => tcp_write(stream, payload).then(move |result| ClientMsg::Wrote {
                request_id,
                stream,
                result,
            }),
            ClientMsg::Wrote {
                request_id,
                stream,
                result: Ok(_),
            } => tcp_read(stream, 16).then(move |result| ClientMsg::Read {
                request_id,
                stream,
                result,
            }),
            ClientMsg::Read {
                request_id,
                stream,
                result: Ok(bytes),
            } => {
                self.observed
                    .lock()
                    .expect("client observations")
                    .push(Observation {
                        request_id,
                        stream,
                        bytes,
                    });
                noop()
            }
            ClientMsg::Connected { result: Err(_), .. }
            | ClientMsg::Wrote { result: Err(_), .. }
            | ClientMsg::Read { result: Err(_), .. } => stop(),
        }
    }
}

fn addr(port: u16) -> SocketAddr {
    format!("127.0.0.1:{port}").parse().expect("loopback")
}

fn peer(peer_addr: SocketAddr, response: &[u8]) -> ScriptedPeerConfig {
    ScriptedPeerConfig {
        accept_after_step: 0,
        peer_addr,
        inbound_chunks: vec![response.to_vec()],
        inbound_capacity: response.len(),
        read_chunk_cap: None,
        write_cap: 16,
        output_capacity: 16,
    }
}

fn run(seed: u64) -> RunResult {
    let target = addr(44_100);
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut sim = Simulator::new(
        SingleShard,
        SimulatorConfig {
            seed,
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                    one_in: 1,
                    steps: 2,
                },
                ..FaultConfig::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 16,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: target,
                    local_addr: target,
                    backlog_capacity: 2,
                    peers: vec![peer(addr(61_001), b"one"), peer(addr(61_002), b"two")],
                }],
            },
            ..SimulatorConfig::default()
        },
    );
    let client = sim.register_with_mailbox_capacity(
        Client {
            target,
            observed: Arc::clone(&observed),
        },
        8,
    );

    sim.try_send(
        client,
        ClientMsg::Start {
            request_id: 1,
            payload: b"req-1".to_vec(),
        },
    )
    .expect("first request fits");
    sim.try_send(
        client,
        ClientMsg::Start {
            request_id: 2,
            payload: b"req-2".to_vec(),
        },
    )
    .expect("second request fits");
    sim.run_until_quiescent();

    let mut observations = observed.lock().expect("client observations").clone();
    observations.sort_by_key(|observation| observation.request_id);
    let first_connect_completion = sim
        .trace()
        .iter()
        .position(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::TcpConnect,
                    ..
                }
            )
        })
        .expect("at least one TCP connect completed");
    let connects_dispatched_before_first_completion = sim.trace()[..first_connect_completion]
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallDispatchAttempted {
                    call_kind: CallKind::TcpConnect,
                    ..
                }
            )
        })
        .count();
    assert_eq!(connects_dispatched_before_first_completion, 2);
    InvariantSuite::standard().assert(sim.trace());
    (observations, sim.trace().to_vec())
}

#[test]
fn overlapping_connections_keep_each_response_with_its_request() {
    let (observations, _) = run(17);
    assert_eq!(observations.len(), 2);
    assert_eq!(observations[0].request_id, 1);
    assert_eq!(observations[0].bytes, b"one");
    assert_eq!(observations[1].request_id, 2);
    assert_eq!(observations[1].bytes, b"two");
    assert_ne!(observations[0].stream, observations[1].stream);
}

#[test]
fn overlapping_connection_routing_replays_byte_for_byte() {
    let (first_observations, first_trace) = run(17);
    let (second_observations, second_trace) = run(17);
    assert_eq!(first_observations, second_observations);
    assert_eq!(first_trace, second_trace);
}
