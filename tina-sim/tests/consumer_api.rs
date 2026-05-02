use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tina::{
    Address, Context, Effect, Isolate, Outbound, RestartBudget, RestartPolicy,
    RestartableChildDefinition, Shard, ShardId,
};
use tina_runtime::{
    CallCompletionRejectedReason, CallError, CallInput, CallKind, CallOutcome, CallOutput,
    ListenerId, RuntimeCall, RuntimeEvent, RuntimeEventKind, SendOutcome, StreamId, call,
    send_observed,
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
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<ConnectionMsg>;
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
    Effect::Call(RuntimeCall::new(
        CallInput::TcpRead {
            stream,
            max_len: 64,
        },
        |result| match result {
            CallOutput::TcpRead { bytes } => ConnectionMsg::ReadCompleted(bytes),
            CallOutput::Failed(_) => ConnectionMsg::Failed,
            other => panic!("unexpected read result {other:?}"),
        },
    ))
}

fn write_call(stream: StreamId, bytes: Vec<u8>) -> Effect<EchoConnection> {
    Effect::Call(RuntimeCall::new(
        CallInput::TcpWrite { stream, bytes },
        |result| match result {
            CallOutput::TcpWrote { count } => ConnectionMsg::WriteCompleted { count },
            CallOutput::Failed(_) => ConnectionMsg::Failed,
            other => panic!("unexpected write result {other:?}"),
        },
    ))
}

fn close_call(stream: StreamId) -> Effect<EchoConnection> {
    Effect::Call(RuntimeCall::new(
        CallInput::TcpStreamClose { stream },
        |result| match result {
            CallOutput::TcpStreamClosed => ConnectionMsg::StreamClosed,
            CallOutput::Failed(_) => ConnectionMsg::Failed,
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
    type Send = Outbound<ListenerMsg>;
    type Spawn = RestartableChildDefinition<EchoConnection>;
    type Call = RuntimeCall<ListenerMsg>;
    type Shard = ConsumerShard;

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ListenerMsg::Bootstrap => {
                self.self_addr = Some(ctx.me());
                let addr = self.bind_addr;
                Effect::Call(RuntimeCall::new(
                    CallInput::TcpBind { addr },
                    |result| match result {
                        CallOutput::TcpBound {
                            listener,
                            local_addr,
                        } => ListenerMsg::Bound {
                            listener,
                            local_addr,
                        },
                        CallOutput::Failed(_) => ListenerMsg::Failed,
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
                Effect::Call(RuntimeCall::new(
                    CallInput::TcpAccept { listener },
                    |result| match result {
                        CallOutput::TcpAccepted { stream, .. } => ListenerMsg::Accepted { stream },
                        CallOutput::Failed(_) => ListenerMsg::Failed,
                        other => panic!("unexpected accept result {other:?}"),
                    },
                ))
            }
            ListenerMsg::ReArmAccept => {
                let listener = self.listener.expect("listener stored before re-arm");
                Effect::Call(RuntimeCall::new(
                    CallInput::TcpAccept { listener },
                    |result| match result {
                        CallOutput::TcpAccepted { stream, .. } => ListenerMsg::Accepted { stream },
                        CallOutput::Failed(_) => ListenerMsg::Failed,
                        other => panic!("unexpected accept result {other:?}"),
                    },
                ))
            }
            ListenerMsg::Accepted { stream } => {
                self.accepted += 1;
                let spawn = Effect::Spawn(
                    RestartableChildDefinition::new(
                        move || EchoConnection {
                            stream,
                            pending_write: Vec::new(),
                        },
                        8,
                    )
                    .with_initial_message(|| ConnectionMsg::Start),
                );
                let self_addr = self.self_addr.expect("listener captured its own address");
                let follow_up = if self.accepted < self.target_accepts {
                    ListenerMsg::ReArmAccept
                } else {
                    ListenerMsg::CloseListener
                };
                Effect::Batch(vec![
                    spawn,
                    Effect::Send(Outbound::new(self_addr, follow_up)),
                ])
            }
            ListenerMsg::CloseListener => {
                let listener = self.listener.expect("listener stored before close");
                Effect::Call(RuntimeCall::new(
                    CallInput::TcpListenerClose { listener },
                    |result| match result {
                        CallOutput::TcpListenerClosed => ListenerMsg::ListenerClosed,
                        CallOutput::Failed(_) => ListenerMsg::Failed,
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

fn count_call_failed(trace: &[RuntimeEvent], kind: CallKind, reason: CallError) -> usize {
    trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallFailed { call_kind, reason: found, .. }
                    if call_kind == kind && found == reason
            )
        })
        .count()
}

fn count_call_completion_rejected(
    trace: &[RuntimeEvent],
    kind: CallKind,
    reason: CallCompletionRejectedReason,
) -> usize {
    trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompletionRejected { call_kind, reason: found, .. }
                    if call_kind == kind && found == reason
            )
        })
        .count()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObservedTargetMsg {
    Work,
    Stop,
}

#[derive(Debug)]
struct ObservedTarget;

impl Isolate for ObservedTarget {
    type Message = ObservedTargetMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<ObservedTargetMsg>;
    type Shard = ConsumerShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ObservedTargetMsg::Work => Effect::Noop,
            ObservedTargetMsg::Stop => Effect::Stop,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ObservedSenderMsg {
    Start(Address<ObservedTargetMsg>),
    SendFinished(SendOutcome),
}

#[derive(Debug)]
struct ObservedSender {
    outcomes: Arc<Mutex<Vec<SendOutcome>>>,
}

impl Isolate for ObservedSender {
    type Message = ObservedSenderMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<ObservedSenderMsg>;
    type Shard = ConsumerShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ObservedSenderMsg::Start(target) => send_observed(target, ObservedTargetMsg::Work)
                .reply(ObservedSenderMsg::SendFinished),
            ObservedSenderMsg::SendFinished(outcome) => {
                self.outcomes
                    .lock()
                    .expect("observed outcomes mutex")
                    .push(outcome);
                Effect::Noop
            }
        }
    }
}

#[test]
fn downstream_consumer_can_replay_observed_send_outcomes() {
    for (target_capacity, stop_first, expected) in [
        (1, false, SendOutcome::Accepted),
        (0, false, SendOutcome::Full),
        (1, true, SendOutcome::Closed),
    ] {
        let (outcomes, first) = run_observed_send_scenario(target_capacity, stop_first);
        let (replayed_outcomes, replayed) = run_observed_send_scenario(target_capacity, stop_first);

        assert_eq!(outcomes.as_slice(), [expected]);
        assert_eq!(replayed_outcomes, outcomes);
        assert_eq!(
            count_call_completed(first.event_record(), CallKind::ObservedSend),
            1
        );
        assert_eq!(first.event_record(), replayed.event_record());
    }
}

fn run_observed_send_scenario(
    target_capacity: usize,
    stop_first: bool,
) -> (Vec<SendOutcome>, tina_sim::ReplayArtifact) {
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let mut sim = Simulator::new(ConsumerShard, SimulatorConfig::default());
    let target = sim.register_with_mailbox_capacity(ObservedTarget, target_capacity);
    let sender = sim.register_with_mailbox_capacity(
        ObservedSender {
            outcomes: Arc::clone(&outcomes),
        },
        8,
    );

    if stop_first {
        sim.try_send(target, ObservedTargetMsg::Stop)
            .expect("stop target");
        sim.run_until_quiescent();
    }

    sim.try_send(sender, ObservedSenderMsg::Start(target))
        .expect("start observed send");
    sim.run_until_quiescent();

    (
        outcomes.lock().expect("observed outcomes mutex").clone(),
        sim.replay_artifact(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkerReply(&'static str);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerRequest {
    ReplyNow,
    DoNotReply,
    Stop,
}

#[derive(Debug)]
struct ReplyWorker;

impl Isolate for ReplyWorker {
    type Message = WorkerRequest;
    type Reply = WorkerReply;
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<WorkerRequest>;
    type Shard = ConsumerShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            WorkerRequest::ReplyNow => Effect::Reply(WorkerReply("pong")),
            WorkerRequest::DoNotReply => Effect::Noop,
            WorkerRequest::Stop => Effect::Stop,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CallerMsg {
    Start(Address<WorkerRequest>, WorkerRequest),
    StartAndStop(Address<WorkerRequest>),
    Returned(CallOutcome<WorkerReply>),
}

#[derive(Debug)]
struct CallerWorker {
    outcomes: Arc<Mutex<Vec<CallOutcome<WorkerReply>>>>,
}

impl Isolate for CallerWorker {
    type Message = CallerMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<CallerMsg>;
    type Shard = ConsumerShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            CallerMsg::Start(target, request) => call::<WorkerRequest, WorkerReply>(
                target,
                request,
                std::time::Duration::from_millis(5),
            )
            .reply(CallerMsg::Returned),
            CallerMsg::StartAndStop(target) => Effect::Batch(vec![
                call::<WorkerRequest, WorkerReply>(
                    target,
                    WorkerRequest::ReplyNow,
                    std::time::Duration::from_millis(5),
                )
                .reply(CallerMsg::Returned),
                Effect::Stop,
            ]),
            CallerMsg::Returned(outcome) => {
                self.outcomes
                    .lock()
                    .expect("call outcomes mutex")
                    .push(outcome);
                Effect::Noop
            }
        }
    }
}

#[test]
fn downstream_consumer_can_replay_isolate_call_reply_full_closed_timeout() {
    for (target_capacity, stop_first, request, expected, failed) in [
        (
            1,
            false,
            WorkerRequest::ReplyNow,
            CallOutcome::Replied(WorkerReply("pong")),
            None,
        ),
        (
            0,
            false,
            WorkerRequest::ReplyNow,
            CallOutcome::Full,
            Some(CallError::TargetFull),
        ),
        (
            1,
            true,
            WorkerRequest::ReplyNow,
            CallOutcome::Closed,
            Some(CallError::TargetClosed),
        ),
        (
            1,
            false,
            WorkerRequest::DoNotReply,
            CallOutcome::Timeout,
            Some(CallError::Timeout),
        ),
    ] {
        let (outcomes, first) = run_isolate_call_scenario(target_capacity, stop_first, request);
        let (replayed_outcomes, replayed) =
            run_isolate_call_scenario(target_capacity, stop_first, request);

        assert_eq!(outcomes.as_slice(), [expected]);
        assert_eq!(replayed_outcomes, outcomes);
        assert_eq!(first.event_record(), replayed.event_record());
        if failed.is_none() {
            assert_eq!(
                count_call_completed(first.event_record(), CallKind::IsolateCall),
                1
            );
        }
        if let Some(reason) = failed {
            assert_eq!(
                count_call_failed(first.event_record(), CallKind::IsolateCall, reason),
                1
            );
        }
    }
}

#[test]
fn downstream_consumer_replays_isolate_call_completion_rejected_when_requester_stops() {
    let (outcomes, first) = run_stopped_requester_isolate_call_scenario();
    let (replayed_outcomes, replayed) = run_stopped_requester_isolate_call_scenario();

    assert!(outcomes.is_empty());
    assert_eq!(replayed_outcomes, outcomes);
    assert_eq!(first.event_record(), replayed.event_record());
    assert_eq!(
        count_call_completion_rejected(
            first.event_record(),
            CallKind::IsolateCall,
            CallCompletionRejectedReason::RequesterClosed,
        ),
        1
    );
}

fn run_stopped_requester_isolate_call_scenario()
-> (Vec<CallOutcome<WorkerReply>>, tina_sim::ReplayArtifact) {
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let mut sim = Simulator::new(ConsumerShard, SimulatorConfig::default());
    let target = sim.register_with_mailbox_capacity(ReplyWorker, 8);
    let caller = sim.register_with_mailbox_capacity(
        CallerWorker {
            outcomes: Arc::clone(&outcomes),
        },
        8,
    );

    sim.try_send(caller, CallerMsg::StartAndStop(target))
        .expect("start call then stop");
    sim.run_until_quiescent();

    (
        outcomes.lock().expect("call outcomes mutex").clone(),
        sim.replay_artifact(),
    )
}

fn run_isolate_call_scenario(
    target_capacity: usize,
    stop_first: bool,
    request: WorkerRequest,
) -> (Vec<CallOutcome<WorkerReply>>, tina_sim::ReplayArtifact) {
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let mut sim = Simulator::new(ConsumerShard, SimulatorConfig::default());
    let target = sim.register_with_mailbox_capacity(ReplyWorker, target_capacity);
    let caller = sim.register_with_mailbox_capacity(
        CallerWorker {
            outcomes: Arc::clone(&outcomes),
        },
        8,
    );

    if stop_first {
        sim.try_send(target, WorkerRequest::Stop)
            .expect("stop target");
        sim.run_until_quiescent();
    }

    sim.try_send(caller, CallerMsg::Start(target, request))
        .expect("start isolate call");
    sim.run_until_quiescent();

    (
        outcomes.lock().expect("call outcomes mutex").clone(),
        sim.replay_artifact(),
    )
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
