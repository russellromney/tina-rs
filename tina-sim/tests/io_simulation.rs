use std::cell::RefCell;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use tina::{
    Address, Context, Effect, Isolate, IsolateId, Outbound, RestartBudget, RestartPolicy,
    RestartableChildDefinition, Shard, ShardId,
};
use tina_runtime::{
    CallCompletionRejectedReason, CallError, CallInput, CallKind, CallOutput, ListenerId,
    RuntimeCall, RuntimeEvent, RuntimeEventKind, StreamId,
};
use tina_sim::{
    Checker, CheckerDecision, FaultConfig, ObservedPeerOutput, ScriptedListenerConfig,
    ScriptedPeerConfig, ScriptedTcpConfig, Simulator, SimulatorConfig, TcpCompletionFaultMode,
};
use tina_supervisor::SupervisorConfig;

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(81)
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
struct Connection {
    stream: StreamId,
    max_chunk: usize,
    pending_write: Vec<u8>,
}

impl Isolate for Connection {
    type Message = ConnectionMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<ConnectionMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ConnectionMsg::Start => read_call(self.stream, self.max_chunk),
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
                    read_call(self.stream, self.max_chunk)
                } else {
                    self.pending_write.drain(..count);
                    write_call(self.stream, self.pending_write.clone())
                }
            }
            ConnectionMsg::StreamClosed | ConnectionMsg::Failed => Effect::Stop,
        }
    }
}

fn read_call(stream: StreamId, max_len: usize) -> Effect<Connection> {
    Effect::Call(RuntimeCall::new(
        CallInput::TcpRead { stream, max_len },
        |result| match result {
            CallOutput::TcpRead { bytes } => ConnectionMsg::ReadCompleted(bytes),
            CallOutput::Failed(_) => ConnectionMsg::Failed,
            other => panic!("unexpected read result {other:?}"),
        },
    ))
}

fn write_call(stream: StreamId, bytes: Vec<u8>) -> Effect<Connection> {
    Effect::Call(RuntimeCall::new(
        CallInput::TcpWrite { stream, bytes },
        |result| match result {
            CallOutput::TcpWrote { count } => ConnectionMsg::WriteCompleted { count },
            CallOutput::Failed(_) => ConnectionMsg::Failed,
            other => panic!("unexpected write result {other:?}"),
        },
    ))
}

fn close_call(stream: StreamId) -> Effect<Connection> {
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
        addr: SocketAddr,
    },
    ReArmAccept,
    CloseListener,
    ListenerClosed,
    Accepted {
        stream: StreamId,
    },
    Failed,
}

#[derive(Debug)]
struct Listener {
    bind_addr: SocketAddr,
    bound_addr_slot: Arc<Mutex<Option<SocketAddr>>>,
    max_chunk: usize,
    target_accepts: usize,
    accepted_count: usize,
    self_addr: Option<Address<ListenerMsg>>,
    listener: Option<ListenerId>,
}

impl Isolate for Listener {
    type Message = ListenerMsg;
    type Reply = ();
    type Send = Outbound<ListenerMsg>;
    type Spawn = RestartableChildDefinition<Connection>;
    type Call = RuntimeCall<ListenerMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ListenerMsg::Bootstrap => {
                self.self_addr = Some(ctx.me());
                let addr = self.bind_addr;
                Effect::Call(RuntimeCall::new(
                    CallInput::TcpBind { addr },
                    move |result| match result {
                        CallOutput::TcpBound {
                            listener,
                            local_addr,
                        } => ListenerMsg::Bound {
                            listener,
                            addr: local_addr,
                        },
                        CallOutput::Failed(_) => ListenerMsg::Failed,
                        other => panic!("unexpected bind result {other:?}"),
                    },
                ))
            }
            ListenerMsg::Bound { listener, addr } => {
                self.listener = Some(listener);
                *self.bound_addr_slot.lock().expect("bound addr mutex") = Some(addr);
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
                self.accepted_count += 1;
                let max_chunk = self.max_chunk;
                let spawn = Effect::Spawn(
                    RestartableChildDefinition::new(
                        move || Connection {
                            stream,
                            max_chunk,
                            pending_write: Vec::new(),
                        },
                        8,
                    )
                    .with_initial_message(|| ConnectionMsg::Start),
                );
                let self_addr = self.self_addr.expect("listener captured self address");
                let follow_up = if self.accepted_count < self.target_accepts {
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

#[derive(Debug, Clone)]
enum BinderMsg {
    StartBind,
    Bound {
        listener: ListenerId,
        addr: SocketAddr,
    },
}

#[derive(Debug)]
struct Binder {
    bind_addr: SocketAddr,
    listener_slot: Arc<Mutex<Option<ListenerId>>>,
    addr_slot: Arc<Mutex<Option<SocketAddr>>>,
}

impl Isolate for Binder {
    type Message = BinderMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<BinderMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            BinderMsg::StartBind => {
                let addr = self.bind_addr;
                Effect::Call(RuntimeCall::new(
                    CallInput::TcpBind { addr },
                    move |result| match result {
                        CallOutput::TcpBound {
                            listener,
                            local_addr,
                        } => BinderMsg::Bound {
                            listener,
                            addr: local_addr,
                        },
                        other => panic!("expected bind success, got {other:?}"),
                    },
                ))
            }
            BinderMsg::Bound { listener, addr } => {
                *self.listener_slot.lock().expect("listener mutex") = Some(listener);
                *self.addr_slot.lock().expect("addr mutex") = Some(addr);
                Effect::Noop
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeMsg {
    StartInvalidAccept,
    StartInvalidRead,
    InvalidResourceObserved,
}

#[derive(Debug)]
struct Probe {
    log: Rc<RefCell<Vec<ProbeMsg>>>,
}

impl Isolate for Probe {
    type Message = ProbeMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<ProbeMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ProbeMsg::StartInvalidAccept => Effect::Call(RuntimeCall::new(
                CallInput::TcpAccept {
                    listener: ListenerId::new(999),
                },
                |result| match result {
                    CallOutput::Failed(CallError::InvalidResource) => {
                        ProbeMsg::InvalidResourceObserved
                    }
                    other => panic!("expected invalid accept failure, got {other:?}"),
                },
            )),
            ProbeMsg::StartInvalidRead => Effect::Call(RuntimeCall::new(
                CallInput::TcpRead {
                    stream: StreamId::new(999),
                    max_len: 8,
                },
                |result| match result {
                    CallOutput::Failed(CallError::InvalidResource) => {
                        ProbeMsg::InvalidResourceObserved
                    }
                    other => panic!("expected invalid read failure, got {other:?}"),
                },
            )),
            ProbeMsg::InvalidResourceObserved => {
                self.log
                    .borrow_mut()
                    .push(ProbeMsg::InvalidResourceObserved);
                Effect::Noop
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaiterMsg {
    StartAccept,
    StopNow,
    Fill,
    Accepted,
    Failed,
}

#[derive(Debug)]
struct Waiter {
    listener: ListenerId,
    log: Rc<RefCell<Vec<WaiterMsg>>>,
}

impl Isolate for Waiter {
    type Message = WaiterMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<WaiterMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            WaiterMsg::StartAccept => Effect::Call(RuntimeCall::new(
                CallInput::TcpAccept {
                    listener: self.listener,
                },
                |result| match result {
                    CallOutput::TcpAccepted { .. } => WaiterMsg::Accepted,
                    CallOutput::Failed(_) => WaiterMsg::Failed,
                    other => panic!("unexpected accept result {other:?}"),
                },
            )),
            WaiterMsg::StopNow => Effect::Stop,
            WaiterMsg::Fill => Effect::Noop,
            WaiterMsg::Accepted => {
                self.log.borrow_mut().push(WaiterMsg::Accepted);
                Effect::Noop
            }
            WaiterMsg::Failed => {
                self.log.borrow_mut().push(WaiterMsg::Failed);
                Effect::Noop
            }
        }
    }
}

#[derive(Debug, Clone)]
enum PeerAwareAcceptMsg {
    StartAccept,
    Accepted {
        stream: StreamId,
        peer_addr: SocketAddr,
    },
    Failed,
}

#[derive(Debug)]
struct PeerAwareAcceptor {
    listener: ListenerId,
    stream_slot: Arc<Mutex<Option<StreamId>>>,
    peer_slot: Arc<Mutex<Option<SocketAddr>>>,
}

impl Isolate for PeerAwareAcceptor {
    type Message = PeerAwareAcceptMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<PeerAwareAcceptMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            PeerAwareAcceptMsg::StartAccept => Effect::Call(RuntimeCall::new(
                CallInput::TcpAccept {
                    listener: self.listener,
                },
                |result| match result {
                    CallOutput::TcpAccepted { stream, peer_addr } => {
                        PeerAwareAcceptMsg::Accepted { stream, peer_addr }
                    }
                    CallOutput::Failed(_) => PeerAwareAcceptMsg::Failed,
                    other => panic!("unexpected accept result {other:?}"),
                },
            )),
            PeerAwareAcceptMsg::Accepted { stream, peer_addr } => {
                *self.stream_slot.lock().expect("stream mutex") = Some(stream);
                *self.peer_slot.lock().expect("peer mutex") = Some(peer_addr);
                Effect::Noop
            }
            PeerAwareAcceptMsg::Failed => Effect::Noop,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReadProbeMsg {
    StartRead,
    ReadCompleted(Vec<u8>),
    Failed,
    StopNow,
}

#[derive(Debug)]
struct ReadProbe {
    stream: StreamId,
    max_len: usize,
    log: Rc<RefCell<Vec<ReadProbeMsg>>>,
}

impl Isolate for ReadProbe {
    type Message = ReadProbeMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<ReadProbeMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ReadProbeMsg::StartRead => Effect::Call(RuntimeCall::new(
                CallInput::TcpRead {
                    stream: self.stream,
                    max_len: self.max_len,
                },
                |result| match result {
                    CallOutput::TcpRead { bytes } => ReadProbeMsg::ReadCompleted(bytes),
                    CallOutput::Failed(_) => ReadProbeMsg::Failed,
                    other => panic!("unexpected read result {other:?}"),
                },
            )),
            ReadProbeMsg::ReadCompleted(bytes) => {
                self.log
                    .borrow_mut()
                    .push(ReadProbeMsg::ReadCompleted(bytes));
                Effect::Noop
            }
            ReadProbeMsg::Failed => {
                self.log.borrow_mut().push(ReadProbeMsg::Failed);
                Effect::Noop
            }
            ReadProbeMsg::StopNow => Effect::Stop,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WriteProbeMsg {
    StartWrite,
    Wrote(usize),
    Failed,
    StopNow,
}

#[derive(Debug)]
struct WriteProbe {
    stream: StreamId,
    bytes: Vec<u8>,
    log: Rc<RefCell<Vec<WriteProbeMsg>>>,
}

impl Isolate for WriteProbe {
    type Message = WriteProbeMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<WriteProbeMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            WriteProbeMsg::StartWrite => Effect::Call(RuntimeCall::new(
                CallInput::TcpWrite {
                    stream: self.stream,
                    bytes: self.bytes.clone(),
                },
                |result| match result {
                    CallOutput::TcpWrote { count } => WriteProbeMsg::Wrote(count),
                    CallOutput::Failed(_) => WriteProbeMsg::Failed,
                    other => panic!("unexpected write result {other:?}"),
                },
            )),
            WriteProbeMsg::Wrote(count) => {
                self.log.borrow_mut().push(WriteProbeMsg::Wrote(count));
                Effect::Noop
            }
            WriteProbeMsg::Failed => {
                self.log.borrow_mut().push(WriteProbeMsg::Failed);
                Effect::Noop
            }
            WriteProbeMsg::StopNow => Effect::Stop,
        }
    }
}

#[derive(Debug, Clone)]
enum CloserMsg {
    CloseListener,
    CloseStream,
    ListenerClosed,
    StreamClosed,
    Failed,
}

#[derive(Debug)]
struct ListenerCloser {
    listener: ListenerId,
}

impl Isolate for ListenerCloser {
    type Message = CloserMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<CloserMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            CloserMsg::CloseListener => Effect::Call(RuntimeCall::new(
                CallInput::TcpListenerClose {
                    listener: self.listener,
                },
                |result| match result {
                    CallOutput::TcpListenerClosed => CloserMsg::ListenerClosed,
                    CallOutput::Failed(_) => CloserMsg::Failed,
                    other => panic!("unexpected listener close result {other:?}"),
                },
            )),
            CloserMsg::CloseStream
            | CloserMsg::ListenerClosed
            | CloserMsg::StreamClosed
            | CloserMsg::Failed => Effect::Noop,
        }
    }
}

#[derive(Debug)]
struct StreamCloser {
    stream: StreamId,
}

impl Isolate for StreamCloser {
    type Message = CloserMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<CloserMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            CloserMsg::CloseStream => Effect::Call(RuntimeCall::new(
                CallInput::TcpStreamClose {
                    stream: self.stream,
                },
                |result| match result {
                    CallOutput::TcpStreamClosed => CloserMsg::StreamClosed,
                    CallOutput::Failed(_) => CloserMsg::Failed,
                    other => panic!("unexpected stream close result {other:?}"),
                },
            )),
            CloserMsg::CloseListener
            | CloserMsg::ListenerClosed
            | CloserMsg::StreamClosed
            | CloserMsg::Failed => Effect::Noop,
        }
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

fn count_spawned(trace: &[RuntimeEvent]) -> usize {
    trace
        .iter()
        .filter(|event| matches!(event.kind(), RuntimeEventKind::Spawned { .. }))
        .count()
}

fn completed_call_isolates(trace: &[RuntimeEvent], kind: CallKind) -> Vec<IsolateId> {
    trace
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::CallCompleted { call_kind, .. } if call_kind == kind => {
                Some(event.isolate())
            }
            _ => None,
        })
        .collect()
}

fn accept_completion_order_for_two_waiters(
    config: SimulatorConfig,
) -> (Vec<IsolateId>, IsolateId, IsolateId) {
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 8,
                listeners: vec![
                    ScriptedListenerConfig {
                        bind_addr: bind_addr(),
                        local_addr: local_addr(47000),
                        backlog_capacity: 1,
                        peers: vec![peer_script(
                            5,
                            peer_addr(59001),
                            vec![b"a".to_vec()],
                            None,
                            1,
                        )],
                    },
                    ScriptedListenerConfig {
                        bind_addr: local_addr(47001),
                        local_addr: local_addr(47001),
                        backlog_capacity: 1,
                        peers: vec![peer_script(
                            5,
                            peer_addr(59002),
                            vec![b"b".to_vec()],
                            None,
                            1,
                        )],
                    },
                ],
            },
            ..config
        },
    );
    let first_listener = Arc::new(Mutex::new(None));
    let first_addr = Arc::new(Mutex::new(None));
    let first_binder = sim.register(Binder {
        bind_addr: bind_addr(),
        listener_slot: Arc::clone(&first_listener),
        addr_slot: Arc::clone(&first_addr),
    });
    let second_listener = Arc::new(Mutex::new(None));
    let second_addr = Arc::new(Mutex::new(None));
    let second_binder = sim.register(Binder {
        bind_addr: local_addr(47001),
        listener_slot: Arc::clone(&second_listener),
        addr_slot: Arc::clone(&second_addr),
    });
    sim.try_send(first_binder, BinderMsg::StartBind).unwrap();
    sim.try_send(second_binder, BinderMsg::StartBind).unwrap();
    sim.step();
    sim.step();

    let first_waiter = sim.register(Waiter {
        listener: first_listener
            .lock()
            .expect("listener mutex")
            .expect("listener"),
        log: Rc::new(RefCell::new(Vec::new())),
    });
    let second_waiter = sim.register(Waiter {
        listener: second_listener
            .lock()
            .expect("listener mutex")
            .expect("listener"),
        log: Rc::new(RefCell::new(Vec::new())),
    });
    let first_waiter_id = first_waiter.isolate();
    let second_waiter_id = second_waiter.isolate();
    sim.try_send(first_waiter, WaiterMsg::StartAccept).unwrap();
    sim.try_send(second_waiter, WaiterMsg::StartAccept).unwrap();
    sim.run_until_quiescent();

    (
        completed_call_isolates(sim.trace(), CallKind::TcpAccept),
        first_waiter_id,
        second_waiter_id,
    )
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
    let inbound_capacity = inbound_chunks.iter().map(Vec::len).sum();
    ScriptedPeerConfig {
        accept_after_step,
        peer_addr,
        inbound_capacity,
        inbound_chunks,
        read_chunk_cap,
        write_cap,
        output_capacity: 1024,
    }
}

fn run_echo_scenario(
    peers: Vec<ScriptedPeerConfig>,
    max_chunk: usize,
    config: SimulatorConfig,
) -> (ReplayArtifactData, IsolateId) {
    let bound = Arc::new(Mutex::new(None));
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 32,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(41000),
                    backlog_capacity: peers.len().max(1),
                    peers,
                }],
            },
            ..config
        },
    );

    let listener = sim.register(Listener {
        bind_addr: bind_addr(),
        bound_addr_slot: Arc::clone(&bound),
        max_chunk,
        target_accepts: sim.config().tcp.listeners[0].peers.len(),
        accepted_count: 0,
        self_addr: None,
        listener: None,
    });
    sim.supervise(
        listener,
        SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(4)),
    );
    sim.try_send(listener, ListenerMsg::Bootstrap).unwrap();
    sim.run_until_quiescent();

    (
        ReplayArtifactData {
            artifact: sim.replay_artifact(),
            bound_addr: *bound.lock().expect("bound mutex"),
        },
        listener.isolate(),
    )
}

fn bind_listener(
    sim: &mut Simulator<TestShard>,
    bind_addr: SocketAddr,
) -> (ListenerId, SocketAddr) {
    let listener_slot = Arc::new(Mutex::new(None));
    let addr_slot = Arc::new(Mutex::new(None));
    let binder = sim.register(Binder {
        bind_addr,
        listener_slot: Arc::clone(&listener_slot),
        addr_slot: Arc::clone(&addr_slot),
    });
    sim.try_send(binder, BinderMsg::StartBind).unwrap();
    sim.step();
    sim.step();
    (
        listener_slot
            .lock()
            .expect("listener mutex")
            .expect("listener"),
        addr_slot.lock().expect("addr mutex").expect("local addr"),
    )
}

fn accept_stream(sim: &mut Simulator<TestShard>, listener: ListenerId) -> (StreamId, SocketAddr) {
    let stream_slot = Arc::new(Mutex::new(None));
    let peer_slot = Arc::new(Mutex::new(None));
    let acceptor = sim.register(PeerAwareAcceptor {
        listener,
        stream_slot: Arc::clone(&stream_slot),
        peer_slot: Arc::clone(&peer_slot),
    });
    sim.try_send(acceptor, PeerAwareAcceptMsg::StartAccept)
        .unwrap();
    sim.run_until_quiescent();
    (
        stream_slot
            .lock()
            .expect("stream mutex")
            .expect("accepted stream"),
        peer_slot.lock().expect("peer mutex").expect("peer addr"),
    )
}

#[derive(Debug)]
struct ReplayArtifactData {
    artifact: tina_sim::ReplayArtifact,
    bound_addr: Option<SocketAddr>,
}

#[derive(Debug, Default)]
struct FirstAcceptOrderChecker {
    second_waiter: Option<IsolateId>,
    saw_accept: bool,
}

impl Checker for FirstAcceptOrderChecker {
    fn name(&self) -> &'static str {
        "first-accept-order-checker"
    }

    fn on_event(&mut self, event: &RuntimeEvent) -> CheckerDecision {
        match event.kind() {
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::TcpAccept,
                ..
            } if !self.saw_accept => {
                self.saw_accept = true;
                if self
                    .second_waiter
                    .is_some_and(|waiter| waiter == event.isolate())
                {
                    CheckerDecision::Fail("second waiter accepted before first".into())
                } else {
                    CheckerDecision::Continue
                }
            }
            _ => CheckerDecision::Continue,
        }
    }
}

#[test]
fn scripted_tcp_echo_round_trips_one_client_payload() {
    let payload = b"hello from simulated tcp".to_vec();
    let (run, listener_isolate) = run_echo_scenario(
        vec![peer_script(
            1,
            peer_addr(51001),
            vec![payload.clone()],
            None,
            payload.len(),
        )],
        256,
        SimulatorConfig::default(),
    );

    assert_eq!(run.bound_addr, Some(local_addr(41000)));
    assert_eq!(
        run.artifact
            .observed_peer_output()
            .iter()
            .map(ObservedPeerOutput::bytes)
            .collect::<Vec<_>>(),
        vec![payload.as_slice()]
    );
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpBind),
        1
    );
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpAccept),
        1
    );
    assert!(count_call_completed(run.artifact.event_record(), CallKind::TcpRead) >= 2);
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpWrite),
        1
    );
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpStreamClose),
        1
    );
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpListenerClose),
        1
    );
    assert_eq!(count_spawned(run.artifact.event_record()), 1);
    assert!(run.artifact.event_record().iter().any(|event| {
        event.isolate() == listener_isolate
            && matches!(event.kind(), RuntimeEventKind::IsolateStopped)
    }));
}

#[test]
fn scripted_tcp_echo_handles_overlap_and_partial_io() {
    let first = b"abcdef".to_vec();
    let second = b"uvwxyz".to_vec();
    let (run, _) = run_echo_scenario(
        vec![
            peer_script(1, peer_addr(51011), vec![first.clone()], Some(2), 2),
            peer_script(1, peer_addr(51012), vec![second.clone()], Some(2), 2),
        ],
        256,
        SimulatorConfig::default(),
    );

    let mut observed = run
        .artifact
        .observed_peer_output()
        .iter()
        .map(|output| output.bytes().to_vec())
        .collect::<Vec<_>>();
    observed.sort();
    let mut expected = vec![first, second];
    expected.sort();
    assert_eq!(observed, expected);
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpAccept),
        2
    );
    assert!(count_call_completed(run.artifact.event_record(), CallKind::TcpWrite) >= 4);
    assert!(count_call_completed(run.artifact.event_record(), CallKind::TcpRead) >= 6);
    assert_eq!(count_spawned(run.artifact.event_record()), 2);
}

#[test]
fn scripted_tcp_echo_handles_tangled_overlap_with_single_byte_drain() {
    let first = b"abcdef".to_vec();
    let second = b"uvwxyz".to_vec();
    let (run, _) = run_echo_scenario(
        vec![
            peer_script(
                1,
                peer_addr(51013),
                vec![b"ab".to_vec(), b"cd".to_vec(), b"ef".to_vec()],
                Some(1),
                1,
            ),
            peer_script(
                1,
                peer_addr(51014),
                vec![b"uv".to_vec(), b"wx".to_vec(), b"yz".to_vec()],
                Some(1),
                1,
            ),
        ],
        256,
        SimulatorConfig {
            seed: 0,
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::ReorderReady { one_in: 1 },
                ..Default::default()
            },
            ..Default::default()
        },
    );

    let mut observed = run
        .artifact
        .observed_peer_output()
        .iter()
        .map(|output| output.bytes().to_vec())
        .collect::<Vec<_>>();
    observed.sort();
    let mut expected = vec![first.clone(), second.clone()];
    expected.sort();
    assert_eq!(observed, expected);
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpAccept),
        2
    );
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpWrite),
        first.len() + second.len()
    );
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpRead),
        first.len() + second.len() + 2
    );
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpStreamClose),
        2
    );
    assert_eq!(count_spawned(run.artifact.event_record()), 2);
}

#[test]
fn scripted_tcp_echo_handles_sequential_multi_client_flow() {
    let payloads = vec![
        b"first-sequential".to_vec(),
        b"second-sequential".to_vec(),
        b"third-sequential".to_vec(),
    ];
    let (run, listener_isolate) = run_echo_scenario(
        vec![
            peer_script(1, peer_addr(51021), vec![payloads[0].clone()], Some(4), 3),
            peer_script(5, peer_addr(51022), vec![payloads[1].clone()], Some(4), 3),
            peer_script(9, peer_addr(51023), vec![payloads[2].clone()], Some(4), 3),
        ],
        256,
        SimulatorConfig::default(),
    );

    let observed = run
        .artifact
        .observed_peer_output()
        .iter()
        .map(|output| output.bytes().to_vec())
        .collect::<Vec<_>>();
    assert_eq!(observed, payloads);
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpBind),
        1
    );
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpAccept),
        3
    );
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpListenerClose),
        1
    );
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpStreamClose),
        3
    );
    assert_eq!(count_spawned(run.artifact.event_record()), 3);
    assert!(run.artifact.event_record().iter().any(|event| {
        event.isolate() == listener_isolate
            && matches!(event.kind(), RuntimeEventKind::IsolateStopped)
    }));
}

#[test]
fn accept_reports_peer_addr_and_read_write_use_live_result_shapes() {
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                    one_in: 1,
                    steps: 1,
                },
                ..Default::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 8,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(48000),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        1,
                        peer_addr(58010),
                        vec![b"hello".to_vec()],
                        None,
                        3,
                    )],
                }],
            },
            ..Default::default()
        },
    );

    let (listener, bound_addr) = bind_listener(&mut sim, bind_addr());
    assert_eq!(bound_addr, local_addr(48000));

    let (stream, accepted_peer) = accept_stream(&mut sim, listener);
    assert_eq!(accepted_peer, peer_addr(58010));

    let read_log = Rc::new(RefCell::new(Vec::new()));
    let reader = sim.register(ReadProbe {
        stream,
        max_len: 16,
        log: Rc::clone(&read_log),
    });
    sim.try_send(reader, ReadProbeMsg::StartRead).unwrap();
    sim.run_until_quiescent();
    assert_eq!(
        *read_log.borrow(),
        vec![ReadProbeMsg::ReadCompleted(b"hello".to_vec())]
    );

    let write_log = Rc::new(RefCell::new(Vec::new()));
    let writer = sim.register(WriteProbe {
        stream,
        bytes: b"goodbye".to_vec(),
        log: Rc::clone(&write_log),
    });
    sim.try_send(writer, WriteProbeMsg::StartWrite).unwrap();
    sim.run_until_quiescent();
    assert_eq!(*write_log.borrow(), vec![WriteProbeMsg::Wrote(3)]);
    assert_eq!(
        sim.replay_artifact()
            .observed_peer_output()
            .iter()
            .map(ObservedPeerOutput::bytes)
            .collect::<Vec<_>>(),
        vec![b"goo".as_slice()]
    );
}

#[test]
fn invalid_tcp_resources_surface_failures() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(TestShard, SimulatorConfig::default());
    let probe = sim.register(Probe {
        log: Rc::clone(&log),
    });
    sim.try_send(probe, ProbeMsg::StartInvalidAccept).unwrap();
    sim.try_send(probe, ProbeMsg::StartInvalidRead).unwrap();
    sim.run_until_quiescent();

    assert_eq!(
        *log.borrow(),
        vec![
            ProbeMsg::InvalidResourceObserved,
            ProbeMsg::InvalidResourceObserved
        ]
    );
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::TcpAccept,
                reason: CallError::InvalidResource,
                ..
            }
        )
    }));
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::TcpRead,
                reason: CallError::InvalidResource,
                ..
            }
        )
    }));
}

#[test]
fn closed_stream_id_surfaces_invalid_resource() {
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                    one_in: 1,
                    steps: 1,
                },
                ..Default::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 8,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(48500),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        1,
                        peer_addr(58501),
                        vec![b"x".to_vec()],
                        None,
                        1,
                    )],
                }],
            },
            ..Default::default()
        },
    );

    let (listener, _) = bind_listener(&mut sim, bind_addr());
    let (stream, _) = accept_stream(&mut sim, listener);
    let closer = sim.register(StreamCloser { stream });
    sim.try_send(closer, CloserMsg::CloseStream).unwrap();
    sim.run_until_quiescent();

    let log = Rc::new(RefCell::new(Vec::new()));
    let reader = sim.register(ReadProbe {
        stream,
        max_len: 8,
        log: Rc::clone(&log),
    });
    sim.try_send(reader, ReadProbeMsg::StartRead).unwrap();
    sim.run_until_quiescent();

    assert_eq!(*log.borrow(), vec![ReadProbeMsg::Failed]);
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::TcpRead,
                reason: CallError::InvalidResource,
                ..
            }
        )
    }));
}

#[test]
fn pending_completion_capacity_exhaustion_surfaces_io_failure() {
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                    one_in: 1,
                    steps: 1,
                },
                ..Default::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 1,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(48550),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        1,
                        peer_addr(58550),
                        vec![b"chunk".to_vec()],
                        None,
                        8,
                    )],
                }],
            },
            ..Default::default()
        },
    );

    let (listener, _) = bind_listener(&mut sim, bind_addr());
    let (stream, _) = accept_stream(&mut sim, listener);

    let read_log = Rc::new(RefCell::new(Vec::new()));
    let reader_one = sim.register(ReadProbe {
        stream,
        max_len: 8,
        log: Rc::clone(&read_log),
    });
    let reader_two = sim.register(ReadProbe {
        stream,
        max_len: 8,
        log: Rc::clone(&read_log),
    });

    sim.try_send(reader_one, ReadProbeMsg::StartRead).unwrap();
    sim.try_send(reader_two, ReadProbeMsg::StartRead).unwrap();
    sim.run_until_quiescent();

    assert_eq!(
        *read_log.borrow(),
        vec![
            ReadProbeMsg::Failed,
            ReadProbeMsg::ReadCompleted(b"chunk".to_vec())
        ]
    );
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::TcpRead,
                reason: CallError::Io,
                ..
            }
        )
    }));
}

#[test]
fn listener_close_fails_pending_accept() {
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                    one_in: 1,
                    steps: 1,
                },
                ..Default::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 8,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(42000),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        10,
                        peer_addr(52001),
                        vec![b"later".to_vec()],
                        None,
                        5,
                    )],
                }],
            },
            ..Default::default()
        },
    );

    let bound = Arc::new(Mutex::new(None));
    let listener = sim.register(Listener {
        bind_addr: bind_addr(),
        bound_addr_slot: Arc::clone(&bound),
        max_chunk: 32,
        target_accepts: 1,
        accepted_count: 0,
        self_addr: None,
        listener: None,
    });
    sim.try_send(listener, ListenerMsg::Bootstrap).unwrap();
    sim.step();
    sim.step();

    let listener_resource = ListenerId::new(1);
    let log = Rc::new(RefCell::new(Vec::new()));
    let waiter = sim.register(Waiter {
        listener: listener_resource,
        log: Rc::clone(&log),
    });
    let closer = sim.register(ListenerCloser {
        listener: listener_resource,
    });

    sim.try_send(waiter, WaiterMsg::StartAccept).unwrap();
    sim.step();
    sim.try_send(closer, CloserMsg::CloseListener).unwrap();
    sim.run_until_quiescent();

    assert_eq!(*log.borrow(), vec![WaiterMsg::Failed]);
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::TcpAccept,
                reason: CallError::InvalidResource,
                ..
            }
        )
    }));
}

#[test]
fn same_resource_fifo_is_preserved_under_tcp_delay_faults() {
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                    one_in: 2,
                    steps: 1,
                },
                ..Default::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 8,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(48575),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        1,
                        peer_addr(58575),
                        vec![b"first".to_vec(), b"second".to_vec()],
                        None,
                        8,
                    )],
                }],
            },
            ..Default::default()
        },
    );

    let (listener, _) = bind_listener(&mut sim, bind_addr());
    let (stream, _) = accept_stream(&mut sim, listener);

    let first_log = Rc::new(RefCell::new(Vec::new()));
    let second_log = Rc::new(RefCell::new(Vec::new()));
    let first_reader = sim.register(ReadProbe {
        stream,
        max_len: 5,
        log: Rc::clone(&first_log),
    });
    let second_reader = sim.register(ReadProbe {
        stream,
        max_len: 6,
        log: Rc::clone(&second_log),
    });

    sim.try_send(first_reader, ReadProbeMsg::StartRead).unwrap();
    sim.step();
    sim.try_send(second_reader, ReadProbeMsg::StartRead)
        .unwrap();
    sim.run_until_quiescent();

    assert_eq!(
        *first_log.borrow(),
        vec![ReadProbeMsg::ReadCompleted(b"first".to_vec())]
    );
    assert_eq!(
        *second_log.borrow(),
        vec![ReadProbeMsg::ReadCompleted(b"second".to_vec())]
    );

    let completed = completed_call_isolates(sim.trace(), CallKind::TcpRead);
    let first_index = completed
        .iter()
        .position(|isolate| *isolate == first_reader.isolate())
        .expect("first read completion");
    let second_index = completed
        .iter()
        .position(|isolate| *isolate == second_reader.isolate())
        .expect("second read completion");
    assert!(
        first_index < second_index,
        "same-stream read completions must preserve submission order under delay faults"
    );
}

#[test]
fn stream_close_fails_pending_read() {
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                    one_in: 1,
                    steps: 1,
                },
                ..Default::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 8,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(48600),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        1,
                        peer_addr(58601),
                        vec![b"soon".to_vec()],
                        None,
                        4,
                    )],
                }],
            },
            ..Default::default()
        },
    );

    let (listener, _) = bind_listener(&mut sim, bind_addr());
    let (stream, _) = accept_stream(&mut sim, listener);

    let read_log = Rc::new(RefCell::new(Vec::new()));
    let reader = sim.register(ReadProbe {
        stream,
        max_len: 8,
        log: Rc::clone(&read_log),
    });
    sim.try_send(reader, ReadProbeMsg::StartRead).unwrap();
    sim.step();

    let closer = sim.register(StreamCloser { stream });
    sim.try_send(closer, CloserMsg::CloseStream).unwrap();
    sim.run_until_quiescent();

    assert_eq!(*read_log.borrow(), vec![ReadProbeMsg::Failed]);
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::TcpRead,
                reason: CallError::InvalidResource,
                ..
            }
        )
    }));
}

#[test]
fn stream_close_fails_pending_write() {
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                    one_in: 1,
                    steps: 1,
                },
                ..Default::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 8,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(48700),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        1,
                        peer_addr(58701),
                        vec![b"x".to_vec()],
                        None,
                        8,
                    )],
                }],
            },
            ..Default::default()
        },
    );

    let (listener, _) = bind_listener(&mut sim, bind_addr());
    let (stream, _) = accept_stream(&mut sim, listener);

    let write_log = Rc::new(RefCell::new(Vec::new()));
    let writer = sim.register(WriteProbe {
        stream,
        bytes: b"payload".to_vec(),
        log: Rc::clone(&write_log),
    });
    sim.try_send(writer, WriteProbeMsg::StartWrite).unwrap();
    sim.step();

    let closer = sim.register(StreamCloser { stream });
    sim.try_send(closer, CloserMsg::CloseStream).unwrap();
    sim.run_until_quiescent();

    assert_eq!(*write_log.borrow(), vec![WriteProbeMsg::Failed]);
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::TcpWrite,
                reason: CallError::InvalidResource,
                ..
            }
        )
    }));
}

#[test]
fn stopped_requester_rejects_pending_accept_completion() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 8,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(43000),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        5,
                        peer_addr(53001),
                        vec![b"hi".to_vec()],
                        None,
                        2,
                    )],
                }],
            },
            ..Default::default()
        },
    );

    let listener_slot = Arc::new(Mutex::new(None));
    let addr_slot = Arc::new(Mutex::new(None));
    let binder = sim.register(Binder {
        bind_addr: bind_addr(),
        listener_slot: Arc::clone(&listener_slot),
        addr_slot: Arc::clone(&addr_slot),
    });
    sim.try_send(binder, BinderMsg::StartBind).unwrap();
    sim.step();
    sim.step();

    let listener = listener_slot
        .lock()
        .expect("listener mutex")
        .expect("listener");
    let waiter = sim.register(Waiter {
        listener,
        log: Rc::clone(&log),
    });
    sim.try_send(waiter, WaiterMsg::StartAccept).unwrap();
    sim.step();
    sim.try_send(waiter, WaiterMsg::StopNow).unwrap();
    sim.run_until_quiescent();

    assert!(log.borrow().is_empty());
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCompletionRejected {
                call_kind: CallKind::TcpAccept,
                reason: CallCompletionRejectedReason::RequesterClosed,
                ..
            }
        )
    }));
}

#[test]
fn stopped_requester_rejects_pending_read_completion() {
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                    one_in: 1,
                    steps: 1,
                },
                ..Default::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 8,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(48800),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        1,
                        peer_addr(58801),
                        vec![b"later".to_vec()],
                        None,
                        5,
                    )],
                }],
            },
            ..Default::default()
        },
    );

    let (listener, _) = bind_listener(&mut sim, bind_addr());
    let (stream, _) = accept_stream(&mut sim, listener);

    let read_log = Rc::new(RefCell::new(Vec::new()));
    let reader = sim.register(ReadProbe {
        stream,
        max_len: 8,
        log: Rc::clone(&read_log),
    });
    sim.try_send(reader, ReadProbeMsg::StartRead).unwrap();
    sim.step();
    sim.try_send(reader, ReadProbeMsg::StopNow).unwrap();
    sim.run_until_quiescent();

    assert!(read_log.borrow().is_empty());
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCompletionRejected {
                call_kind: CallKind::TcpRead,
                reason: CallCompletionRejectedReason::RequesterClosed,
                ..
            }
        )
    }));
}

#[test]
fn stopped_requester_rejects_pending_write_completion() {
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                    one_in: 1,
                    steps: 1,
                },
                ..Default::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 8,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(48900),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        1,
                        peer_addr(58901),
                        vec![b"z".to_vec()],
                        None,
                        8,
                    )],
                }],
            },
            ..Default::default()
        },
    );

    let (listener, _) = bind_listener(&mut sim, bind_addr());
    let (stream, _) = accept_stream(&mut sim, listener);

    let write_log = Rc::new(RefCell::new(Vec::new()));
    let writer = sim.register(WriteProbe {
        stream,
        bytes: b"payload".to_vec(),
        log: Rc::clone(&write_log),
    });
    sim.try_send(writer, WriteProbeMsg::StartWrite).unwrap();
    sim.step();
    sim.try_send(writer, WriteProbeMsg::StopNow).unwrap();
    sim.run_until_quiescent();

    assert!(write_log.borrow().is_empty());
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCompletionRejected {
                call_kind: CallKind::TcpWrite,
                reason: CallCompletionRejectedReason::RequesterClosed,
                ..
            }
        )
    }));
}

#[test]
fn tcp_completion_rejected_when_requester_mailbox_is_full() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 8,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(44000),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        1,
                        peer_addr(54001),
                        vec![b"mail".to_vec()],
                        None,
                        4,
                    )],
                }],
            },
            ..Default::default()
        },
    );

    let listener_slot = Arc::new(Mutex::new(None));
    let addr_slot = Arc::new(Mutex::new(None));
    let binder = sim.register(Binder {
        bind_addr: bind_addr(),
        listener_slot: Arc::clone(&listener_slot),
        addr_slot: Arc::clone(&addr_slot),
    });
    sim.try_send(binder, BinderMsg::StartBind).unwrap();
    sim.step();
    sim.step();

    let listener = listener_slot
        .lock()
        .expect("listener mutex")
        .expect("listener");
    let waiter = sim.register_with_mailbox_capacity(
        Waiter {
            listener,
            log: Rc::clone(&log),
        },
        1,
    );
    sim.try_send(waiter, WaiterMsg::StartAccept).unwrap();
    sim.step();
    sim.try_send(waiter, WaiterMsg::Fill).unwrap();
    sim.run_until_quiescent();

    assert!(log.borrow().is_empty());
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCompletionRejected {
                call_kind: CallKind::TcpAccept,
                reason: CallCompletionRejectedReason::MailboxFull,
                ..
            }
        )
    }));
}

#[test]
fn tcp_replay_preserves_peer_output() {
    let config = SimulatorConfig {
        seed: 22,
        tcp: ScriptedTcpConfig {
            pending_completion_capacity: 32,
            listeners: vec![ScriptedListenerConfig {
                bind_addr: bind_addr(),
                local_addr: local_addr(45000),
                backlog_capacity: 1,
                peers: vec![peer_script(
                    1,
                    peer_addr(55001),
                    vec![b"replay me".to_vec()],
                    Some(3),
                    2,
                )],
            }],
        },
        ..Default::default()
    };
    let (first, _) = run_echo_scenario(
        vec![peer_script(
            1,
            peer_addr(55001),
            vec![b"replay me".to_vec()],
            Some(3),
            2,
        )],
        32,
        config.clone(),
    );
    let (replayed, _) = run_echo_scenario(
        vec![peer_script(
            1,
            peer_addr(55001),
            vec![b"replay me".to_vec()],
            Some(3),
            2,
        )],
        32,
        first.artifact.config().clone(),
    );
    assert_eq!(
        first.artifact.event_record(),
        replayed.artifact.event_record()
    );
    assert_eq!(
        first.artifact.observed_peer_output(),
        replayed.artifact.observed_peer_output()
    );
}

#[test]
fn different_seeds_diverge_under_tcp_delay_faults() {
    let delayed = SimulatorConfig {
        seed: 0,
        faults: FaultConfig {
            tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                one_in: 2,
                steps: 1,
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let preserved = SimulatorConfig {
        seed: 1,
        faults: delayed.faults,
        ..Default::default()
    };

    let (delayed_accept_order, delayed_first_waiter, delayed_second_waiter) =
        accept_completion_order_for_two_waiters(delayed);
    let (preserved_accept_order, preserved_first_waiter, preserved_second_waiter) =
        accept_completion_order_for_two_waiters(preserved);
    assert_eq!(delayed_accept_order.len(), 2);
    assert_eq!(preserved_accept_order.len(), 2);
    assert_eq!(
        delayed_accept_order,
        vec![delayed_second_waiter, delayed_first_waiter],
        "seed 0 should delay the first queued accept and let the second waiter complete first"
    );
    assert_eq!(
        preserved_accept_order,
        vec![preserved_first_waiter, preserved_second_waiter],
        "seed 1 should preserve first-in-first-out accept visibility under DelayBySteps"
    );
    assert_ne!(
        delayed_accept_order, preserved_accept_order,
        "different seeds should flip which waiter completes first under DelayBySteps"
    );
}

#[test]
fn different_seeds_diverge_under_tcp_ready_reordering() {
    let reordered = SimulatorConfig {
        seed: 0,
        faults: FaultConfig {
            tcp_completion: TcpCompletionFaultMode::ReorderReady { one_in: 2 },
            ..Default::default()
        },
        ..Default::default()
    };
    let baseline = SimulatorConfig {
        seed: 1,
        faults: reordered.faults,
        ..Default::default()
    };

    let (reordered_accept_order, reordered_first_waiter, reordered_second_waiter) =
        accept_completion_order_for_two_waiters(reordered);
    let (baseline_accept_order, baseline_first_waiter, baseline_second_waiter) =
        accept_completion_order_for_two_waiters(baseline);
    assert_eq!(reordered_accept_order.len(), 2);
    assert_eq!(baseline_accept_order.len(), 2);
    assert_eq!(
        reordered_accept_order,
        vec![reordered_second_waiter, reordered_first_waiter],
        "seed 0 should reorder tied ready accept completions so the second waiter wins first"
    );
    assert_eq!(
        baseline_accept_order,
        vec![baseline_first_waiter, baseline_second_waiter],
        "seed 1 should preserve request-order accept visibility when ReorderReady does not fire"
    );
    assert_ne!(
        reordered_accept_order, baseline_accept_order,
        "different seeds should flip which waiter completes first under ReorderReady"
    );
}

#[test]
fn tcp_checker_failure_replays_under_ready_reordering() {
    let config = SimulatorConfig {
        seed: 0,
        faults: FaultConfig {
            tcp_completion: TcpCompletionFaultMode::ReorderReady { one_in: 1 },
            ..Default::default()
        },
        tcp: ScriptedTcpConfig {
            pending_completion_capacity: 8,
            listeners: vec![
                ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(47000),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        5,
                        peer_addr(59001),
                        vec![b"a".to_vec()],
                        None,
                        1,
                    )],
                },
                ScriptedListenerConfig {
                    bind_addr: local_addr(47001),
                    local_addr: local_addr(47001),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        5,
                        peer_addr(59002),
                        vec![b"b".to_vec()],
                        None,
                        1,
                    )],
                },
            ],
        },
    };

    let mut sim = Simulator::new(TestShard, config);
    let first_listener = Arc::new(Mutex::new(None));
    let first_addr = Arc::new(Mutex::new(None));
    let first_binder = sim.register(Binder {
        bind_addr: bind_addr(),
        listener_slot: Arc::clone(&first_listener),
        addr_slot: Arc::clone(&first_addr),
    });
    let second_listener = Arc::new(Mutex::new(None));
    let second_addr = Arc::new(Mutex::new(None));
    let second_binder = sim.register(Binder {
        bind_addr: local_addr(47001),
        listener_slot: Arc::clone(&second_listener),
        addr_slot: Arc::clone(&second_addr),
    });
    sim.try_send(first_binder, BinderMsg::StartBind).unwrap();
    sim.try_send(second_binder, BinderMsg::StartBind).unwrap();
    sim.step();
    sim.step();

    let first_waiter = sim.register(Waiter {
        listener: first_listener
            .lock()
            .expect("listener mutex")
            .expect("listener"),
        log: Rc::new(RefCell::new(Vec::new())),
    });
    let second_waiter = sim.register(Waiter {
        listener: second_listener
            .lock()
            .expect("listener mutex")
            .expect("listener"),
        log: Rc::new(RefCell::new(Vec::new())),
    });
    sim.try_send(first_waiter, WaiterMsg::StartAccept).unwrap();
    sim.try_send(second_waiter, WaiterMsg::StartAccept).unwrap();

    let mut checker = FirstAcceptOrderChecker {
        second_waiter: Some(second_waiter.isolate()),
        saw_accept: false,
    };
    let failure = sim
        .run_until_quiescent_checked(&mut checker)
        .expect("reordered run should trip checker");
    let artifact = sim.replay_artifact();
    assert_eq!(artifact.checker_failure(), Some(&failure));

    let mut replay = Simulator::new(TestShard, artifact.config().clone());
    let first_listener = Arc::new(Mutex::new(None));
    let first_addr = Arc::new(Mutex::new(None));
    let first_binder = replay.register(Binder {
        bind_addr: bind_addr(),
        listener_slot: Arc::clone(&first_listener),
        addr_slot: Arc::clone(&first_addr),
    });
    let second_listener = Arc::new(Mutex::new(None));
    let second_addr = Arc::new(Mutex::new(None));
    let second_binder = replay.register(Binder {
        bind_addr: local_addr(47001),
        listener_slot: Arc::clone(&second_listener),
        addr_slot: Arc::clone(&second_addr),
    });
    replay.try_send(first_binder, BinderMsg::StartBind).unwrap();
    replay
        .try_send(second_binder, BinderMsg::StartBind)
        .unwrap();
    replay.step();
    replay.step();

    let first_waiter = replay.register(Waiter {
        listener: first_listener
            .lock()
            .expect("listener mutex")
            .expect("listener"),
        log: Rc::new(RefCell::new(Vec::new())),
    });
    let second_waiter = replay.register(Waiter {
        listener: second_listener
            .lock()
            .expect("listener mutex")
            .expect("listener"),
        log: Rc::new(RefCell::new(Vec::new())),
    });
    replay
        .try_send(first_waiter, WaiterMsg::StartAccept)
        .unwrap();
    replay
        .try_send(second_waiter, WaiterMsg::StartAccept)
        .unwrap();

    let mut checker = FirstAcceptOrderChecker {
        second_waiter: Some(second_waiter.isolate()),
        saw_accept: false,
    };
    let replayed_failure = replay
        .run_until_quiescent_checked(&mut checker)
        .expect("replay should trip same checker");
    let replayed = replay.replay_artifact();
    assert_eq!(artifact.event_record(), replayed.event_record());
    assert_eq!(
        artifact.observed_peer_output(),
        replayed.observed_peer_output()
    );
    assert_eq!(artifact.checker_failure(), replayed.checker_failure());
    assert_eq!(failure, replayed_failure);
}
