use std::cell::RefCell;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::rc::Rc;
use std::time::Duration;

use tina::{Address, RestartableChildDefinition, prelude::*};
use tina_runtime::{
    CallError, CallKind, CallOutcome, ListenerId, RuntimeEventKind, StreamId, call, tcp_accept,
    tcp_bind, tcp_close_listener, tcp_close_stream, tcp_read, tcp_write,
};
use tina_sim::{
    FaultConfig, ObservedPeerOutput, ScriptedListenerConfig, ScriptedPeerConfig, ScriptedTcpConfig,
    Simulator, SimulatorConfig, TcpCompletionFaultMode,
};

#[derive(Debug, Default)]
struct LocalShard;

impl Shard for LocalShard {
    fn id(&self) -> ShardId {
        ShardId::new(130)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Observation {
    WorkerBooted,
    Replied,
    Full,
    Timeout,
}

type Observations = Rc<RefCell<Vec<Observation>>>;
type WorkerAddresses = Rc<RefCell<Vec<Address<WorkerMsg, WorkerReply>>>>;

#[derive(Debug, Clone, Copy)]
struct ServiceCapacities {
    parent_mailbox: usize,
    worker_mailbox: usize,
    listener_mailbox: usize,
    connection_mailbox: usize,
    pending_tcp_completions: usize,
    listener_backlog: usize,
}

impl Default for ServiceCapacities {
    fn default() -> Self {
        Self {
            parent_mailbox: 8,
            worker_mailbox: 1,
            listener_mailbox: 8,
            connection_mailbox: 8,
            pending_tcp_completions: 32,
            listener_backlog: 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkerMsg {
    Boot,
    Echo(Vec<u8>),
    NoReply,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkerReply(Vec<u8>);

#[derive(Debug)]
struct Worker {
    observations: Observations,
    addresses: WorkerAddresses,
}

#[tina_runtime::isolate(
    message = WorkerMsg,
    reply = WorkerReply,
    shard = LocalShard
)]
impl Worker {
    fn handle(&mut self, msg: WorkerMsg, ctx: &mut Context<'_, LocalShard>) -> Effect<Self> {
        match msg {
            WorkerMsg::Boot => {
                self.observations
                    .borrow_mut()
                    .push(Observation::WorkerBooted);
                self.addresses
                    .borrow_mut()
                    .push(ctx.me().with_reply::<WorkerReply>());
                noop()
            }
            WorkerMsg::Echo(bytes) => reply(WorkerReply(bytes)),
            WorkerMsg::NoReply => noop(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ParentMsg {
    Spawn,
}

#[derive(Debug)]
struct Parent {
    observations: Observations,
    addresses: WorkerAddresses,
    capacities: ServiceCapacities,
}

#[tina_runtime::isolate(
    message = ParentMsg,
    spawn = RestartableChildDefinition<Worker>,
    shard = LocalShard
)]
impl Parent {
    fn handle(&mut self, msg: ParentMsg, _ctx: &mut Context<'_, LocalShard>) -> Effect<Self> {
        match msg {
            ParentMsg::Spawn => {
                let observations = Rc::clone(&self.observations);
                let addresses = Rc::clone(&self.addresses);
                spawn(
                    RestartableChildDefinition::new(
                        move || Worker {
                            observations: Rc::clone(&observations),
                            addresses: Rc::clone(&addresses),
                        },
                        self.capacities.worker_mailbox,
                    )
                    .with_initial_message(|| WorkerMsg::Boot),
                )
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestMode {
    Echo,
    Full,
    Timeout,
}

#[derive(Debug, Clone)]
enum ConnectionMsg {
    Begin,
    Read(Vec<u8>),
    WorkerReturned(CallOutcome<WorkerReply>),
    Wrote(usize),
    Closed,
    Failed,
}

#[derive(Debug)]
struct Connection {
    stream: StreamId,
    worker: Address<WorkerMsg, WorkerReply>,
    observations: Observations,
    mode: Option<RequestMode>,
    pending_write: Vec<u8>,
    response_started: bool,
}

#[tina_runtime::isolate(message = ConnectionMsg, shard = LocalShard)]
impl Connection {
    fn handle(&mut self, msg: ConnectionMsg, _ctx: &mut Context<'_, LocalShard>) -> Effect<Self> {
        match msg {
            ConnectionMsg::Begin => tcp_read(self.stream, 256).reply(|result| match result {
                Ok(bytes) => ConnectionMsg::Read(bytes),
                Err(_) => ConnectionMsg::Failed,
            }),
            ConnectionMsg::Read(bytes) if bytes.is_empty() => close_stream(self.stream),
            ConnectionMsg::Read(bytes) => match parse_request(&bytes) {
                (RequestMode::Echo, body) => {
                    self.mode = Some(RequestMode::Echo);
                    call(
                        self.worker,
                        WorkerMsg::Echo(body.to_vec()),
                        Duration::from_millis(250),
                    )
                    .reply(ConnectionMsg::WorkerReturned)
                }
                (RequestMode::Full, _) => {
                    self.mode = Some(RequestMode::Full);
                    batch([
                        call(
                            self.worker,
                            WorkerMsg::Echo(b"accepted-before-full".to_vec()),
                            Duration::from_millis(250),
                        )
                        .reply(ConnectionMsg::WorkerReturned),
                        call(
                            self.worker,
                            WorkerMsg::Echo(b"must-reject".to_vec()),
                            Duration::from_millis(250),
                        )
                        .reply(ConnectionMsg::WorkerReturned),
                    ])
                }
                (RequestMode::Timeout, _) => {
                    self.mode = Some(RequestMode::Timeout);
                    call(self.worker, WorkerMsg::NoReply, Duration::from_millis(20))
                        .reply(ConnectionMsg::WorkerReturned)
                }
            },
            ConnectionMsg::WorkerReturned(outcome) => {
                let response = match outcome {
                    CallOutcome::Replied(WorkerReply(bytes)) => {
                        self.observations.borrow_mut().push(Observation::Replied);
                        if self.mode == Some(RequestMode::Full) {
                            return noop();
                        }
                        bytes
                    }
                    CallOutcome::Full => {
                        self.observations.borrow_mut().push(Observation::Full);
                        b"worker-full".to_vec()
                    }
                    CallOutcome::Closed => b"worker-closed".to_vec(),
                    CallOutcome::Timeout => {
                        self.observations.borrow_mut().push(Observation::Timeout);
                        b"worker-timeout".to_vec()
                    }
                };

                if self.response_started {
                    return noop();
                }

                self.response_started = true;
                self.pending_write = response;
                write_pending(self.stream, self.pending_write.clone())
            }
            ConnectionMsg::Wrote(count) => {
                if count == 0 {
                    stop()
                } else if count >= self.pending_write.len() {
                    self.pending_write.clear();
                    close_stream(self.stream)
                } else {
                    self.pending_write.drain(..count);
                    write_pending(self.stream, self.pending_write.clone())
                }
            }
            ConnectionMsg::Closed | ConnectionMsg::Failed => stop(),
        }
    }
}

fn parse_request(bytes: &[u8]) -> (RequestMode, &[u8]) {
    if let Some(body) = bytes.strip_prefix(b"echo:") {
        (RequestMode::Echo, body)
    } else if bytes == b"full" {
        (RequestMode::Full, bytes)
    } else if bytes == b"timeout" {
        (RequestMode::Timeout, bytes)
    } else {
        (RequestMode::Echo, bytes)
    }
}

fn write_pending(stream: StreamId, bytes: Vec<u8>) -> Effect<Connection> {
    tcp_write(stream, bytes).reply(|result| match result {
        Ok(count) => ConnectionMsg::Wrote(count),
        Err(_) => ConnectionMsg::Failed,
    })
}

fn close_stream(stream: StreamId) -> Effect<Connection> {
    tcp_close_stream(stream).reply(|result| match result {
        Ok(()) => ConnectionMsg::Closed,
        Err(_) => ConnectionMsg::Failed,
    })
}

#[derive(Debug, Clone)]
enum ListenerMsg {
    Start,
    Bound { listener: ListenerId },
    AcceptNext,
    Accepted { stream: StreamId },
    Close,
    Closed,
    Failed,
}

#[derive(Debug)]
struct Listener {
    bind_addr: SocketAddr,
    worker: Address<WorkerMsg, WorkerReply>,
    observations: Observations,
    capacities: ServiceCapacities,
    listener: Option<ListenerId>,
    accepted: usize,
    target_accepts: usize,
}

#[tina_runtime::isolate(
    message = ListenerMsg,
    send = Outbound<ListenerMsg>,
    spawn = RestartableChildDefinition<Connection>,
    shard = LocalShard
)]
impl Listener {
    fn handle(&mut self, msg: ListenerMsg, ctx: &mut Context<'_, LocalShard>) -> Effect<Self> {
        match msg {
            ListenerMsg::Start => {
                let bind_addr = self.bind_addr;
                tcp_bind(bind_addr).reply(|result| match result {
                    Ok((listener, _addr)) => ListenerMsg::Bound { listener },
                    Err(_) => ListenerMsg::Failed,
                })
            }
            ListenerMsg::Bound { listener } => {
                self.listener = Some(listener);
                accept(listener)
            }
            ListenerMsg::AcceptNext => accept(self.listener.expect("listener stored")),
            ListenerMsg::Accepted { stream } => {
                self.accepted += 1;
                let worker = self.worker;
                let observations = Rc::clone(&self.observations);
                let child = spawn(
                    RestartableChildDefinition::new(
                        move || Connection {
                            stream,
                            worker,
                            observations: Rc::clone(&observations),
                            mode: None,
                            pending_write: Vec::new(),
                            response_started: false,
                        },
                        self.capacities.connection_mailbox,
                    )
                    .with_initial_message(|| ConnectionMsg::Begin),
                );
                let follow_up = if self.accepted < self.target_accepts {
                    ListenerMsg::AcceptNext
                } else {
                    ListenerMsg::Close
                };
                batch([child, ctx.send_self(follow_up)])
            }
            ListenerMsg::Close => {
                let listener = self.listener.expect("listener stored before close");
                tcp_close_listener(listener).reply(|result| match result {
                    Ok(()) => ListenerMsg::Closed,
                    Err(_) => ListenerMsg::Failed,
                })
            }
            ListenerMsg::Closed | ListenerMsg::Failed => stop(),
        }
    }
}

fn accept(listener: ListenerId) -> Effect<Listener> {
    tcp_accept(listener).reply(|result| match result {
        Ok((stream, _peer_addr)) => ListenerMsg::Accepted { stream },
        Err(_) => ListenerMsg::Failed,
    })
}

fn bind_addr() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 41030)
}

fn local_addr() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 51030)
}

fn peer_addr(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

fn peer(
    port: u16,
    inbound: &'static [u8],
    read_chunk_cap: Option<usize>,
    write_cap: usize,
    output_capacity: usize,
) -> ScriptedPeerConfig {
    ScriptedPeerConfig {
        accept_after_step: 1,
        peer_addr: peer_addr(port),
        inbound_chunks: vec![inbound.to_vec()],
        inbound_capacity: inbound.len(),
        read_chunk_cap,
        write_cap,
        output_capacity,
    }
}

fn assert_event_exists(
    event_kinds: &[RuntimeEventKind],
    label: &str,
    predicate: impl Fn(RuntimeEventKind) -> bool,
) {
    assert!(
        event_kinds.iter().copied().any(predicate),
        "{label}; event_kinds = {event_kinds:#?}"
    );
}

fn assert_event_count_at_least(
    event_kinds: &[RuntimeEventKind],
    label: &str,
    minimum: usize,
    predicate: impl Fn(RuntimeEventKind) -> bool,
) {
    let count = event_kinds
        .iter()
        .copied()
        .filter(|kind| predicate(*kind))
        .count();
    assert!(
        count >= minimum,
        "{label}; expected at least {minimum}, got {count}; event_kinds = {event_kinds:#?}"
    );
}

fn assert_dispatch_attempts_have_terminal_outcomes(event_kinds: &[RuntimeEventKind]) {
    let send_attempts = event_kinds
        .iter()
        .filter(|kind| matches!(kind, RuntimeEventKind::SendDispatchAttempted { .. }))
        .count();
    let send_outcomes = event_kinds
        .iter()
        .filter(|kind| {
            matches!(
                kind,
                RuntimeEventKind::SendAccepted { .. } | RuntimeEventKind::SendRejected { .. }
            )
        })
        .count();
    assert_eq!(
        send_attempts, send_outcomes,
        "every send dispatch attempt needs one visible accepted/rejected outcome"
    );

    let call_attempts = event_kinds
        .iter()
        .filter(|kind| matches!(kind, RuntimeEventKind::CallDispatchAttempted { .. }))
        .count();
    let call_outcomes = event_kinds
        .iter()
        .filter(|kind| {
            matches!(
                kind,
                RuntimeEventKind::CallCompleted { .. }
                    | RuntimeEventKind::CallFailed { .. }
                    | RuntimeEventKind::CallCompletionRejected { .. }
            )
        })
        .count();
    assert_eq!(
        call_attempts, call_outcomes,
        "every call dispatch attempt needs one visible terminal outcome"
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LocalServerOracleCost {
    parent_deliveries: usize,
    server_deliveries: usize,
    event_count: usize,
    tcp_write_completions: usize,
    isolate_call_failures: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalServerOracleRun {
    observations: Vec<Observation>,
    outputs: Vec<Vec<u8>>,
    event_kinds: Vec<RuntimeEventKind>,
    cost: LocalServerOracleCost,
}

fn run_local_server_oracle(seed: u64) -> LocalServerOracleRun {
    let capacities = ServiceCapacities::default();
    let observations = Rc::new(RefCell::new(Vec::new()));
    let worker_addresses = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(
        LocalShard,
        SimulatorConfig {
            seed,
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                    one_in: 2,
                    steps: 1,
                },
                ..Default::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: capacities.pending_tcp_completions,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(),
                    backlog_capacity: capacities.listener_backlog,
                    peers: vec![
                        peer(61031, b"echo:alpha", None, 2, b"alpha".len()),
                        peer(61032, b"full", None, 2, b"worker-full".len()),
                        peer(61033, b"timeout", None, 2, b"worker-timeout".len()),
                    ],
                }],
            },
            storage: Default::default(),
            ..Default::default()
        },
    );
    let parent = sim.register_with_mailbox_capacity(
        Parent {
            observations: Rc::clone(&observations),
            addresses: Rc::clone(&worker_addresses),
            capacities,
        },
        capacities.parent_mailbox,
    );
    sim.try_send(parent, ParentMsg::Spawn).unwrap();
    let parent_deliveries = sim.run_until_quiescent();
    let worker = worker_addresses.borrow()[0];
    let listener = sim.register_with_mailbox_capacity(
        Listener {
            bind_addr: bind_addr(),
            worker,
            observations: Rc::clone(&observations),
            capacities,
            listener: None,
            accepted: 0,
            target_accepts: 3,
        },
        capacities.listener_mailbox,
    );
    sim.try_send(listener, ListenerMsg::Start).unwrap();
    let server_deliveries = sim.run_until_quiescent();

    let outputs = sim
        .replay_artifact()
        .observed_peer_output()
        .iter()
        .map(ObservedPeerOutput::bytes)
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    let event_kinds = sim
        .trace()
        .iter()
        .map(|event| event.kind())
        .collect::<Vec<_>>();
    let cost = LocalServerOracleCost {
        parent_deliveries,
        server_deliveries,
        event_count: event_kinds.len(),
        tcp_write_completions: event_kinds
            .iter()
            .filter(|kind| {
                matches!(
                    kind,
                    RuntimeEventKind::CallCompleted {
                        call_kind: CallKind::TcpWrite,
                        ..
                    }
                )
            })
            .count(),
        isolate_call_failures: event_kinds
            .iter()
            .filter(|kind| {
                matches!(
                    kind,
                    RuntimeEventKind::CallFailed {
                        call_kind: CallKind::IsolateCall,
                        ..
                    }
                )
            })
            .count(),
    };
    LocalServerOracleRun {
        observations: observations.borrow().clone(),
        outputs,
        event_kinds,
        cost,
    }
}

#[test]
fn llama_sim_dst_parity_service_replays_bounded_worker_pressure_and_partial_writes() {
    let first = run_local_server_oracle(30);
    let replayed = run_local_server_oracle(30);

    assert_eq!(first, replayed);
    assert_dispatch_attempts_have_terminal_outcomes(&first.event_kinds);
    assert_eq!(
        first.cost,
        LocalServerOracleCost {
            parent_deliveries: 2,
            server_deliveries: 41,
            event_count: 220,
            tcp_write_completions: 16,
            isolate_call_failures: 2,
        },
        "local-production oracle operation counts changed; update the cost model"
    );
    assert_eq!(
        first.outputs,
        vec![
            b"alpha".to_vec(),
            b"worker-full".to_vec(),
            b"worker-timeout".to_vec()
        ]
    );
    assert!(first.observations.contains(&Observation::WorkerBooted));
    assert!(first.observations.contains(&Observation::Replied));
    assert!(first.observations.contains(&Observation::Full));
    assert!(first.observations.contains(&Observation::Timeout));
    assert_event_exists(
        &first.event_kinds,
        "bounded worker overload must be visible as TargetFull",
        |kind| {
            matches!(
                kind,
                RuntimeEventKind::CallFailed {
                    call_kind: CallKind::IsolateCall,
                    reason: CallError::TargetFull,
                    ..
                }
            )
        },
    );
    assert_event_exists(
        &first.event_kinds,
        "mandatory call deadline must be visible as Timeout",
        |kind| {
            matches!(
                kind,
                RuntimeEventKind::CallFailed {
                    call_kind: CallKind::IsolateCall,
                    reason: CallError::Timeout,
                    ..
                }
            )
        },
    );
    assert_event_count_at_least(
        &first.event_kinds,
        "write_cap should force partial writes in the oracle workload",
        4,
        |kind| {
            matches!(
                kind,
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::TcpWrite,
                    ..
                }
            )
        },
    );
}

#[test]
fn application_surface_oracle_replays_under_non_default_seed() {
    let first = run_local_server_oracle(91);
    let replayed = run_local_server_oracle(91);

    assert_eq!(first, replayed);
    assert_dispatch_attempts_have_terminal_outcomes(&first.event_kinds);
    assert_eq!(
        first.outputs,
        vec![
            b"alpha".to_vec(),
            b"worker-full".to_vec(),
            b"worker-timeout".to_vec()
        ]
    );
    assert_event_exists(
        &first.event_kinds,
        "non-default seed still preserves visible bounded pressure",
        |kind| {
            matches!(
                kind,
                RuntimeEventKind::CallFailed {
                    call_kind: CallKind::IsolateCall,
                    reason: CallError::TargetFull,
                    ..
                }
            )
        },
    );
}
