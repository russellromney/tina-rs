use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tina::{
    Address, Context, Effect, Isolate, RestartBudget, RestartPolicy, RestartableSpawnSpec,
    SendMessage, Shard, ShardId,
};
use tina_runtime_current::{
    CallKind, CallRequest, CallResult, CurrentCall, ListenerId, RuntimeEvent, RuntimeEventKind,
    StreamId,
};
use tina_sim::{
    ObservedPeerOutput, ScriptedListenerConfig, ScriptedPeerConfig, ScriptedTcpConfig, Simulator,
    SimulatorConfig,
};
use tina_supervisor::SupervisorConfig;

#[derive(Debug, Default)]
struct ConsumerShard;

impl Shard for ConsumerShard {
    fn id(&self) -> ShardId {
        ShardId::new(91)
    }
}

#[derive(Debug, Clone)]
enum ConnectionMsg {
    Start,
    ReadCompleted(Vec<u8>),
    WriteCompleted { count: usize },
    StreamClosed,
    Failed,
}

#[derive(Debug)]
struct EchoConnection {
    stream: StreamId,
    pending_write: Vec<u8>,
}

impl Isolate for EchoConnection {
    type Message = ConnectionMsg;
    type Reply = ();
    type Send = SendMessage<Infallible>;
    type Spawn = Infallible;
    type Call = CurrentCall<ConnectionMsg>;
    type Shard = ConsumerShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ConnectionMsg::Start => read_call(self.stream),
            ConnectionMsg::ReadCompleted(bytes) => {
                if bytes.is_empty() {
                    close_call(self.stream)
                } else {
                    self.pending_write = bytes;
                    write_call(self.stream, self.pending_write.clone())
                }
            }
            ConnectionMsg::WriteCompleted { count } => {
                if count >= self.pending_write.len() {
                    self.pending_write.clear();
                    read_call(self.stream)
                } else {
                    self.pending_write.drain(..count);
                    write_call(self.stream, self.pending_write.clone())
                }
            }
            ConnectionMsg::StreamClosed | ConnectionMsg::Failed => Effect::Stop,
        }
    }
}

fn read_call(stream: StreamId) -> Effect<EchoConnection> {
    Effect::Call(CurrentCall::new(
        CallRequest::TcpRead {
            stream,
            max_len: 64,
        },
        |result| match result {
            CallResult::TcpRead { bytes } => ConnectionMsg::ReadCompleted(bytes),
            CallResult::Failed(_) => ConnectionMsg::Failed,
            other => panic!("unexpected read result {other:?}"),
        },
    ))
}

fn write_call(stream: StreamId, bytes: Vec<u8>) -> Effect<EchoConnection> {
    Effect::Call(CurrentCall::new(
        CallRequest::TcpWrite { stream, bytes },
        |result| match result {
            CallResult::TcpWrote { count } => ConnectionMsg::WriteCompleted { count },
            CallResult::Failed(_) => ConnectionMsg::Failed,
            other => panic!("unexpected write result {other:?}"),
        },
    ))
}

fn close_call(stream: StreamId) -> Effect<EchoConnection> {
    Effect::Call(CurrentCall::new(
        CallRequest::TcpStreamClose { stream },
        |result| match result {
            CallResult::TcpStreamClosed => ConnectionMsg::StreamClosed,
            CallResult::Failed(_) => ConnectionMsg::Failed,
            other => panic!("unexpected close result {other:?}"),
        },
    ))
}

#[derive(Debug, Clone)]
enum ListenerMsg {
    Bootstrap,
    Bound {
        listener: ListenerId,
        local_addr: SocketAddr,
    },
    ReArmAccept,
    Accepted {
        stream: StreamId,
    },
    CloseListener,
    ListenerClosed,
    Failed,
}

#[derive(Debug)]
struct EchoListener {
    bind_addr: SocketAddr,
    target_accepts: usize,
    accepted: usize,
    self_addr: Option<Address<ListenerMsg>>,
    listener: Option<ListenerId>,
    bound_addr: Arc<Mutex<Option<SocketAddr>>>,
}

impl Isolate for EchoListener {
    type Message = ListenerMsg;
    type Reply = ();
    type Send = SendMessage<ListenerMsg>;
    type Spawn = RestartableSpawnSpec<EchoConnection>;
    type Call = CurrentCall<ListenerMsg>;
    type Shard = ConsumerShard;

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ListenerMsg::Bootstrap => {
                self.self_addr = Some(ctx.current_address::<ListenerMsg>());
                let addr = self.bind_addr;
                Effect::Call(CurrentCall::new(
                    CallRequest::TcpBind { addr },
                    |result| match result {
                        CallResult::TcpBound {
                            listener,
                            local_addr,
                        } => ListenerMsg::Bound {
                            listener,
                            local_addr,
                        },
                        CallResult::Failed(_) => ListenerMsg::Failed,
                        other => panic!("unexpected bind result {other:?}"),
                    },
                ))
            }
            ListenerMsg::Bound {
                listener,
                local_addr,
            } => {
                self.listener = Some(listener);
                *self.bound_addr.lock().expect("bound addr mutex") = Some(local_addr);
                Effect::Call(CurrentCall::new(
                    CallRequest::TcpAccept { listener },
                    |result| match result {
                        CallResult::TcpAccepted { stream, .. } => ListenerMsg::Accepted { stream },
                        CallResult::Failed(_) => ListenerMsg::Failed,
                        other => panic!("unexpected accept result {other:?}"),
                    },
                ))
            }
            ListenerMsg::ReArmAccept => {
                let listener = self.listener.expect("listener stored before re-arm");
                Effect::Call(CurrentCall::new(
                    CallRequest::TcpAccept { listener },
                    |result| match result {
                        CallResult::TcpAccepted { stream, .. } => ListenerMsg::Accepted { stream },
                        CallResult::Failed(_) => ListenerMsg::Failed,
                        other => panic!("unexpected accept result {other:?}"),
                    },
                ))
            }
            ListenerMsg::Accepted { stream } => {
                self.accepted += 1;
                let spawn = Effect::Spawn(
                    RestartableSpawnSpec::new(
                        move || EchoConnection {
                            stream,
                            pending_write: Vec::new(),
                        },
                        8,
                    )
                    .with_bootstrap(|| ConnectionMsg::Start),
                );
                let self_addr = self.self_addr.expect("listener captured its own address");
                let follow_up = if self.accepted < self.target_accepts {
                    ListenerMsg::ReArmAccept
                } else {
                    ListenerMsg::CloseListener
                };
                Effect::Batch(vec![
                    spawn,
                    Effect::Send(SendMessage::new(self_addr, follow_up)),
                ])
            }
            ListenerMsg::CloseListener => {
                let listener = self.listener.expect("listener stored before close");
                Effect::Call(CurrentCall::new(
                    CallRequest::TcpListenerClose { listener },
                    |result| match result {
                        CallResult::TcpListenerClosed => ListenerMsg::ListenerClosed,
                        CallResult::Failed(_) => ListenerMsg::Failed,
                        other => panic!("unexpected listener close result {other:?}"),
                    },
                ))
            }
            ListenerMsg::ListenerClosed => Effect::Stop,
            ListenerMsg::Failed => Effect::Stop,
        }
    }
}

fn bind_addr() -> SocketAddr {
    "127.0.0.1:0".parse().expect("loopback bind addr")
}

fn local_addr(port: u16) -> SocketAddr {
    format!("127.0.0.1:{port}")
        .parse()
        .expect("loopback local addr")
}

fn peer_addr(port: u16) -> SocketAddr {
    format!("127.0.0.1:{port}")
        .parse()
        .expect("loopback peer addr")
}

fn peer_script(
    accept_after_step: u64,
    peer_addr: SocketAddr,
    inbound_chunks: Vec<Vec<u8>>,
    read_chunk_cap: Option<usize>,
    write_cap: usize,
) -> ScriptedPeerConfig {
    ScriptedPeerConfig {
        accept_after_step,
        peer_addr,
        inbound_capacity: inbound_chunks.iter().map(Vec::len).sum(),
        inbound_chunks,
        read_chunk_cap,
        write_cap,
        output_capacity: 1024,
    }
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

fn run_consumer_workload(config: SimulatorConfig) -> tina_sim::ReplayArtifact {
    let bound_addr = Arc::new(Mutex::new(None));
    let mut sim = Simulator::new(ConsumerShard, config);
    let listener = sim.register(EchoListener {
        bind_addr: bind_addr(),
        target_accepts: 2,
        accepted: 0,
        self_addr: None,
        listener: None,
        bound_addr: Arc::clone(&bound_addr),
    });
    sim.supervise(
        listener,
        SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(4)),
    );
    sim.try_send(listener, ListenerMsg::Bootstrap).unwrap();
    sim.run_until_quiescent();

    assert_eq!(
        *bound_addr.lock().expect("bound addr mutex"),
        Some(local_addr(50000))
    );
    sim.replay_artifact()
}

#[test]
fn downstream_consumer_can_run_scripted_tcp_echo_end_to_end() {
    let artifact = run_consumer_workload(SimulatorConfig {
        tcp: ScriptedTcpConfig {
            pending_completion_capacity: 16,
            listeners: vec![ScriptedListenerConfig {
                bind_addr: bind_addr(),
                local_addr: local_addr(50000),
                backlog_capacity: 2,
                peers: vec![
                    peer_script(1, peer_addr(60001), vec![b"alpha".to_vec()], Some(2), 2),
                    peer_script(5, peer_addr(60002), vec![b"beta".to_vec()], Some(2), 2),
                ],
            }],
        },
        ..Default::default()
    });

    let observed = artifact
        .observed_peer_output()
        .iter()
        .map(ObservedPeerOutput::bytes)
        .collect::<Vec<_>>();
    assert_eq!(observed, vec![b"alpha".as_slice(), b"beta".as_slice()]);
    assert_eq!(
        count_call_completed(artifact.event_record(), CallKind::TcpBind),
        1
    );
    assert_eq!(
        count_call_completed(artifact.event_record(), CallKind::TcpAccept),
        2
    );
    assert_eq!(
        count_call_completed(artifact.event_record(), CallKind::TcpListenerClose),
        1
    );
    assert_eq!(
        count_call_completed(artifact.event_record(), CallKind::TcpStreamClose),
        2
    );
    assert!(
        artifact
            .event_record()
            .iter()
            .any(|event| matches!(event.kind(), RuntimeEventKind::Spawned { .. }))
    );
}

#[test]
fn downstream_consumer_can_replay_from_saved_config() {
    let config = SimulatorConfig {
        seed: 44,
        tcp: ScriptedTcpConfig {
            pending_completion_capacity: 16,
            listeners: vec![ScriptedListenerConfig {
                bind_addr: bind_addr(),
                local_addr: local_addr(50000),
                backlog_capacity: 2,
                peers: vec![
                    peer_script(1, peer_addr(60101), vec![b"re".to_vec()], Some(1), 1),
                    peer_script(4, peer_addr(60102), vec![b"play".to_vec()], Some(2), 2),
                ],
            }],
        },
        ..Default::default()
    };

    let first = run_consumer_workload(config);
    let replayed = run_consumer_workload(first.config().clone());
    assert_eq!(first.event_record(), replayed.event_record());
    assert_eq!(
        first.observed_peer_output(),
        replayed.observed_peer_output()
    );
    assert_eq!(first.final_time(), replayed.final_time());
}
