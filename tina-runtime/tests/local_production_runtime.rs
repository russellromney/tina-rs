#![feature(allocator_api)]

use std::alloc::Global;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::rc::Rc;
use std::sync::{Arc, Barrier, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use betelgeuse::IOLoop;
use betelgeuse::io::simulated::{SimulatedConfig, SimulatedDelay, SimulatedIO};
use tina::{Address, Mailbox, RestartBudget, RestartPolicy, TrySendError, prelude::*};
use tina_runtime::{
    BetelgeuseRuntime, BetelgeuseRuntimeConfig, CallCompletionRejectedReason, CallError, CallKind,
    CallOutcome, ListenerId, MailboxFactory, RuntimeEvent, RuntimeEventKind, SendOutcome,
    SendRejectedReason, StreamId, call, send_observed, sleep, tcp_accept, tcp_bind,
    tcp_close_listener, tcp_close_stream, tcp_read, tcp_write,
};
use tina_supervisor::SupervisorConfig;

#[derive(Debug, Default)]
struct LocalShard;

impl Shard for LocalShard {
    fn id(&self) -> ShardId {
        ShardId::new(30)
    }
}

struct LocalMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    closed: Rc<Cell<bool>>,
}

impl<T> LocalMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Rc::new(RefCell::new(VecDeque::new())),
            closed: Rc::new(Cell::new(false)),
        }
    }
}

impl<T> Mailbox<T> for LocalMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if self.closed.get() {
            return Err(TrySendError::Closed(message));
        }

        let mut queue = self.queue.borrow_mut();
        if queue.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }

        queue.push_back(message);
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        self.queue.borrow_mut().pop_front()
    }

    fn close(&self) {
        self.closed.set(true);
    }
}

#[derive(Debug, Clone, Copy)]
struct LocalMailboxFactory;

impl MailboxFactory for LocalMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(LocalMailbox::new(capacity))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerObservation {
    WorkerBooted,
    WorkerReplied,
    WorkerFull,
    WorkerTimeout,
    StaleClosed,
}

type Observations = Arc<Mutex<Vec<ServerObservation>>>;
type BoundAddr = Arc<Mutex<Option<SocketAddr>>>;
type WorkerAddresses = Arc<Mutex<Vec<Address<WorkerMsg, WorkerReply>>>>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkerMsg {
    Boot,
    Echo(Vec<u8>),
    NoReply,
    Panic,
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
                    .lock()
                    .expect("observations mutex")
                    .push(ServerObservation::WorkerBooted);
                self.addresses
                    .lock()
                    .expect("worker address mutex")
                    .push(ctx.me().with_reply::<WorkerReply>());
                noop()
            }
            WorkerMsg::Echo(bytes) => reply(WorkerReply(bytes)),
            WorkerMsg::NoReply => noop(),
            WorkerMsg::Panic => panic!("test worker panic"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum WorkerParentMsg {
    Spawn,
}

#[derive(Debug)]
struct WorkerParent {
    observations: Observations,
    addresses: WorkerAddresses,
}

#[tina_runtime::isolate(
    message = WorkerParentMsg,
    spawn = RestartableChildDefinition<Worker>,
    shard = LocalShard
)]
impl WorkerParent {
    fn handle(&mut self, msg: WorkerParentMsg, _ctx: &mut Context<'_, LocalShard>) -> Effect<Self> {
        match msg {
            WorkerParentMsg::Spawn => {
                let observations = Arc::clone(&self.observations);
                let addresses = Arc::clone(&self.addresses);
                spawn(
                    RestartableChildDefinition::new(
                        move || Worker {
                            observations: Arc::clone(&observations),
                            addresses: Arc::clone(&addresses),
                        },
                        1,
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

#[tina_runtime::isolate(
    message = ConnectionMsg,
    send = Outbound<Infallible>,
    shard = LocalShard
)]
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
                    batch(vec![
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
                        self.observations
                            .lock()
                            .expect("observations mutex")
                            .push(ServerObservation::WorkerReplied);
                        if self.mode == Some(RequestMode::Full) {
                            return noop();
                        }
                        bytes
                    }
                    CallOutcome::Full => {
                        self.observations
                            .lock()
                            .expect("observations mutex")
                            .push(ServerObservation::WorkerFull);
                        b"worker-full".to_vec()
                    }
                    CallOutcome::Closed => b"worker-closed".to_vec(),
                    CallOutcome::Timeout => {
                        self.observations
                            .lock()
                            .expect("observations mutex")
                            .push(ServerObservation::WorkerTimeout);
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
    Bound {
        listener: ListenerId,
        addr: SocketAddr,
    },
    AcceptNext,
    Accepted {
        stream: StreamId,
    },
    Close,
    Closed,
    Failed,
}

#[derive(Debug)]
struct Listener {
    bind_addr: SocketAddr,
    bound_addr: BoundAddr,
    worker: Address<WorkerMsg, WorkerReply>,
    observations: Observations,
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
                    Ok((listener, addr)) => ListenerMsg::Bound { listener, addr },
                    Err(_) => ListenerMsg::Failed,
                })
            }
            ListenerMsg::Bound { listener, addr } => {
                self.listener = Some(listener);
                *self.bound_addr.lock().expect("bound address mutex") = Some(addr);
                accept(listener)
            }
            ListenerMsg::AcceptNext => accept(self.listener.expect("listener stored")),
            ListenerMsg::Accepted { stream } => {
                self.accepted += 1;
                let worker = self.worker;
                let observations = Arc::clone(&self.observations);
                let child = spawn(
                    RestartableChildDefinition::new(
                        move || Connection {
                            stream,
                            worker,
                            observations: Arc::clone(&observations),
                            mode: None,
                            pending_write: Vec::new(),
                            response_started: false,
                        },
                        8,
                    )
                    .with_initial_message(|| ConnectionMsg::Begin),
                );
                let follow_up = if self.accepted < self.target_accepts {
                    ListenerMsg::AcceptNext
                } else {
                    ListenerMsg::Close
                };
                batch(vec![child, ctx.send_self(follow_up)])
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

#[derive(Debug, Clone)]
enum LongWorkMsg {
    Start {
        worker: Address<WorkerMsg, WorkerReply>,
    },
    Slept,
    WorkerReturned,
}

#[derive(Debug)]
struct LongWork;

#[tina_runtime::isolate(message = LongWorkMsg, shard = LocalShard)]
impl LongWork {
    fn handle(&mut self, msg: LongWorkMsg, _ctx: &mut Context<'_, LocalShard>) -> Effect<Self> {
        match msg {
            LongWorkMsg::Start { worker } => batch(vec![
                sleep(Duration::from_secs(60)).reply(|_| LongWorkMsg::Slept),
                call(worker, WorkerMsg::NoReply, Duration::from_secs(60))
                    .reply(|_| LongWorkMsg::WorkerReturned),
            ]),
            LongWorkMsg::Slept | LongWorkMsg::WorkerReturned => noop(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum StaleProbeMsg {
    Probe(Address<WorkerMsg, WorkerReply>),
    Observed(SendOutcome),
}

#[derive(Debug)]
struct StaleProbe {
    observations: Observations,
}

#[tina_runtime::isolate(message = StaleProbeMsg, shard = LocalShard)]
impl StaleProbe {
    fn handle(&mut self, msg: StaleProbeMsg, _ctx: &mut Context<'_, LocalShard>) -> Effect<Self> {
        match msg {
            StaleProbeMsg::Probe(target) => {
                send_observed(target, WorkerMsg::Echo(b"stale".to_vec()))
                    .reply(StaleProbeMsg::Observed)
            }
            StaleProbeMsg::Observed(SendOutcome::Closed) => {
                self.observations
                    .lock()
                    .expect("observations mutex")
                    .push(ServerObservation::StaleClosed);
                noop()
            }
            StaleProbeMsg::Observed(SendOutcome::Accepted | SendOutcome::Full) => {
                panic!("stale worker address must reject as closed")
            }
        }
    }
}

struct ClientRun {
    output: Arc<Mutex<Vec<u8>>>,
    handle: JoinHandle<()>,
}

fn spawn_native_client(
    local_addr: SocketAddr,
    payload: &'static [u8],
    barrier: Arc<Barrier>,
) -> ClientRun {
    let output = Arc::new(Mutex::new(Vec::new()));
    let output_for_client = Arc::clone(&output);
    let handle = thread::spawn(move || {
        let mut stream = TcpStream::connect(local_addr).expect("connect to tina listener");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");
        barrier.wait();
        stream.write_all(payload).expect("client write");
        stream
            .shutdown(std::net::Shutdown::Write)
            .expect("client write shutdown");

        let mut received = Vec::new();
        let mut buf = [0u8; 128];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(count) => received.extend_from_slice(&buf[..count]),
                Err(error) => panic!("client read failed: {error}"),
            }
        }
        *output_for_client.lock().expect("output mutex") = received;
    });
    ClientRun { output, handle }
}

fn wait_until<F>(timeout: Duration, label: &str, mut predicate: F)
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while !predicate() {
        if Instant::now() > deadline {
            panic!("wait_until({label}) timed out");
        }
        thread::yield_now();
    }
}

fn count_kind(trace: &[RuntimeEvent], predicate: impl Fn(RuntimeEventKind) -> bool) -> usize {
    trace.iter().filter(|event| predicate(event.kind())).count()
}

fn start_supervised_worker(
    runtime: &BetelgeuseRuntime<LocalShard, LocalMailboxFactory>,
    observations: &Observations,
    addresses: &WorkerAddresses,
) -> (Address<WorkerParentMsg>, Address<WorkerMsg, WorkerReply>) {
    let parent = runtime
        .register_with_capacity::<WorkerParent, _>(
            WorkerParent {
                observations: Arc::clone(observations),
                addresses: Arc::clone(addresses),
            },
            8,
        )
        .expect("worker parent register accepts");
    runtime
        .supervise(
            parent,
            SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(4)),
        )
        .expect("supervise accepts");
    runtime
        .try_send(parent, WorkerParentMsg::Spawn)
        .expect("spawn handoff accepts");
    wait_until(Duration::from_secs(2), "worker boot", || {
        !addresses.lock().expect("worker address mutex").is_empty()
    });
    let worker = addresses.lock().expect("worker address mutex")[0];
    (parent, worker)
}

#[test]
fn live_local_server_routes_tcp_through_bounded_worker_pressure() {
    let observations = Arc::new(Mutex::new(Vec::new()));
    let worker_addresses = Arc::new(Mutex::new(Vec::new()));
    let bound_addr = Arc::new(Mutex::new(None));
    let runtime = BetelgeuseRuntime::with_config(
        LocalShard,
        LocalMailboxFactory,
        BetelgeuseRuntimeConfig {
            command_capacity: 16,
            idle_wait: Duration::from_millis(1),
        },
    );
    let (_parent, worker) = start_supervised_worker(&runtime, &observations, &worker_addresses);
    let listener = runtime
        .register_with_capacity::<Listener, _>(
            Listener {
                bind_addr: "127.0.0.1:0".parse().expect("loopback parse"),
                bound_addr: Arc::clone(&bound_addr),
                worker,
                observations: Arc::clone(&observations),
                listener: None,
                accepted: 0,
                target_accepts: 3,
            },
            8,
        )
        .expect("listener register accepts");

    runtime
        .try_send(listener, ListenerMsg::Start)
        .expect("listener start handoff accepts");
    wait_until(Duration::from_secs(2), "listener bound", || {
        bound_addr.lock().expect("bound addr mutex").is_some()
    });

    let local_addr = bound_addr
        .lock()
        .expect("bound addr mutex")
        .expect("listener bound addr");
    let barrier = Arc::new(Barrier::new(3));
    let clients = [
        spawn_native_client(local_addr, b"echo:alpha", Arc::clone(&barrier)),
        spawn_native_client(local_addr, b"full", Arc::clone(&barrier)),
        spawn_native_client(local_addr, b"timeout", Arc::clone(&barrier)),
    ];

    for client in &clients {
        wait_until(Duration::from_secs(5), "client response", || {
            !client
                .output
                .lock()
                .expect("client output mutex")
                .is_empty()
        });
    }

    let mut outputs = Vec::new();
    for client in clients {
        client.handle.join().expect("client thread joins");
        outputs.push(client.output.lock().expect("client output mutex").clone());
    }
    outputs.sort();
    assert_eq!(
        outputs,
        vec![
            b"alpha".to_vec(),
            b"worker-full".to_vec(),
            b"worker-timeout".to_vec()
        ]
    );

    wait_until(Duration::from_secs(2), "listener stopped", || {
        let trace = runtime.trace().expect("trace query succeeds");
        trace.iter().any(|event| {
            event.isolate() == listener.isolate()
                && matches!(event.kind(), RuntimeEventKind::IsolateStopped)
        }) && !runtime
            .has_in_flight_calls()
            .expect("in-flight query succeeds")
    });

    let trace = runtime.shutdown().expect("runtime shutdown succeeds");
    let observations = observations.lock().expect("observations mutex").clone();
    assert!(observations.contains(&ServerObservation::WorkerBooted));
    assert!(observations.contains(&ServerObservation::WorkerReplied));
    assert!(observations.contains(&ServerObservation::WorkerFull));
    assert!(observations.contains(&ServerObservation::WorkerTimeout));
    assert!(
        count_kind(&trace, |kind| matches!(
            kind,
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::IsolateCall,
                reason: CallError::TargetFull,
                ..
            }
        )) >= 1,
        "native live client ordering may create more than one full outcome, but bounded pressure must be visible"
    );
    assert_eq!(
        count_kind(&trace, |kind| matches!(
            kind,
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::IsolateCall,
                reason: CallError::Timeout,
                ..
            }
        )),
        1
    );
    assert_eq!(
        count_kind(&trace, |kind| matches!(
            kind,
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::TcpAccept,
                ..
            }
        )),
        3
    );
}

#[test]
fn simulated_io_local_server_keeps_partial_slow_peer_semantics_through_threaded_runtime() {
    let observations = Arc::new(Mutex::new(Vec::new()));
    let worker_addresses = Arc::new(Mutex::new(Vec::new()));
    let bound_addr = Arc::new(Mutex::new(None));
    let simulated_io = SimulatedIO::with_config(SimulatedConfig {
        seed: 30,
        completion_delay: SimulatedDelay::Every {
            one_in: 1,
            steps: 1,
        },
        max_send_chunk: Some(2),
    });
    let io_for_worker = simulated_io.clone();
    let runtime = BetelgeuseRuntime::with_config_and_io_loop_factory(
        LocalShard,
        LocalMailboxFactory,
        BetelgeuseRuntimeConfig {
            command_capacity: 16,
            idle_wait: Duration::from_millis(1),
        },
        move || io_for_worker.loop_handle(Global),
    );
    let (_parent, worker) = start_supervised_worker(&runtime, &observations, &worker_addresses);
    let listener = runtime
        .register_with_capacity::<Listener, _>(
            Listener {
                bind_addr: "127.0.0.1:0".parse().expect("loopback parse"),
                bound_addr: Arc::clone(&bound_addr),
                worker,
                observations: Arc::clone(&observations),
                listener: None,
                accepted: 0,
                target_accepts: 2,
            },
            8,
        )
        .expect("listener register accepts");

    runtime
        .try_send(listener, ListenerMsg::Start)
        .expect("listener start handoff accepts");
    wait_until(Duration::from_secs(2), "simulated listener bound", || {
        bound_addr.lock().expect("bound addr mutex").is_some()
    });
    let local_addr = bound_addr
        .lock()
        .expect("bound addr mutex")
        .expect("listener bound addr");

    let echo_peer = simulated_io
        .connect(local_addr, b"echo:abcdef".to_vec())
        .expect("simulated echo peer connects");
    let full_peer = simulated_io
        .connect(local_addr, b"full".to_vec())
        .expect("simulated full peer connects");

    wait_until(Duration::from_secs(3), "simulated peers finish", || {
        echo_peer.output() == b"abcdef" && full_peer.output() == b"worker-full"
    });
    wait_until(Duration::from_secs(2), "simulated listener stopped", || {
        let trace = runtime.trace().expect("trace query succeeds");
        trace.iter().any(|event| {
            event.isolate() == listener.isolate()
                && matches!(event.kind(), RuntimeEventKind::IsolateStopped)
        }) && !runtime
            .has_in_flight_calls()
            .expect("in-flight query succeeds")
    });

    let trace = runtime.shutdown().expect("runtime shutdown succeeds");
    assert_eq!(echo_peer.output(), b"abcdef");
    assert_eq!(full_peer.output(), b"worker-full");
    assert!(
        count_kind(&trace, |kind| matches!(
            kind,
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::TcpWrite,
                ..
            }
        )) > 2,
        "partial send limit should force more than one write completion per response"
    );
    assert!(
        observations
            .lock()
            .expect("observations mutex")
            .contains(&ServerObservation::WorkerFull)
    );
}

#[test]
fn local_server_supervision_restarts_worker_and_rejects_stale_address() {
    let observations = Arc::new(Mutex::new(Vec::new()));
    let worker_addresses = Arc::new(Mutex::new(Vec::new()));
    let runtime = BetelgeuseRuntime::with_config(
        LocalShard,
        LocalMailboxFactory,
        BetelgeuseRuntimeConfig {
            command_capacity: 16,
            idle_wait: Duration::from_millis(1),
        },
    );
    let (_parent, first_worker) =
        start_supervised_worker(&runtime, &observations, &worker_addresses);

    runtime
        .try_send(first_worker, WorkerMsg::Panic)
        .expect("panic handoff accepts");
    wait_until(Duration::from_secs(2), "worker restarted", || {
        worker_addresses.lock().expect("worker address mutex").len() >= 2
    });
    let replacement = worker_addresses.lock().expect("worker address mutex")[1];
    assert_ne!(first_worker.isolate(), replacement.isolate());

    let probe = runtime
        .register_with_capacity::<StaleProbe, _>(
            StaleProbe {
                observations: Arc::clone(&observations),
            },
            8,
        )
        .expect("stale probe register accepts");
    runtime
        .try_send(probe, StaleProbeMsg::Probe(first_worker))
        .expect("stale probe handoff accepts");
    wait_until(Duration::from_secs(2), "stale rejection observed", || {
        observations
            .lock()
            .expect("observations mutex")
            .contains(&ServerObservation::StaleClosed)
    });
    runtime
        .send_and_observe(replacement, WorkerMsg::Echo(b"fresh".to_vec()))
        .expect("replacement worker accepts observed send");

    let trace = runtime.shutdown().expect("runtime shutdown succeeds");
    assert!(trace.iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::RestartChildCompleted {
                old_isolate,
                new_isolate,
                ..
            } if old_isolate == first_worker.isolate() && new_isolate == replacement.isolate()
        )
    }));
    assert!(trace.iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::SendRejected {
                target_isolate,
                target_generation,
                reason: SendRejectedReason::Closed,
                ..
            } if target_isolate == first_worker.isolate()
                && target_generation == first_worker.generation()
        )
    }));
}

#[test]
fn local_server_shutdown_cancels_pending_accept_read_timer_and_call_work() {
    let observations = Arc::new(Mutex::new(Vec::new()));
    let worker_addresses = Arc::new(Mutex::new(Vec::new()));
    let bound_addr = Arc::new(Mutex::new(None));
    let simulated_io = SimulatedIO::with_config(SimulatedConfig {
        seed: 31,
        completion_delay: SimulatedDelay::Every {
            one_in: 1,
            steps: 2,
        },
        max_send_chunk: Some(2),
    });
    let io_for_worker = simulated_io.clone();
    let runtime = BetelgeuseRuntime::with_config_and_io_loop_factory(
        LocalShard,
        LocalMailboxFactory,
        BetelgeuseRuntimeConfig {
            command_capacity: 16,
            idle_wait: Duration::from_millis(1),
        },
        move || io_for_worker.loop_handle(Global),
    );
    let (_parent, worker) = start_supervised_worker(&runtime, &observations, &worker_addresses);
    let listener = runtime
        .register_with_capacity::<Listener, _>(
            Listener {
                bind_addr: "127.0.0.1:0".parse().expect("loopback parse"),
                bound_addr: Arc::clone(&bound_addr),
                worker,
                observations: Arc::clone(&observations),
                listener: None,
                accepted: 0,
                target_accepts: 4,
            },
            8,
        )
        .expect("listener register accepts");
    let long_work = runtime
        .register_with_capacity::<LongWork, _>(LongWork, 8)
        .expect("long work register accepts");

    runtime
        .try_send(listener, ListenerMsg::Start)
        .expect("listener start handoff accepts");
    wait_until(Duration::from_secs(2), "simulated listener bound", || {
        bound_addr.lock().expect("bound addr mutex").is_some()
    });
    let local_addr = bound_addr
        .lock()
        .expect("bound addr mutex")
        .expect("listener bound addr");
    let pending_read_peer = simulated_io
        .connect(local_addr, Vec::new())
        .expect("pending-read peer connects");
    pending_read_peer.push_input(&[]);

    runtime
        .try_send(long_work, LongWorkMsg::Start { worker })
        .expect("long work start handoff accepts");
    wait_until(Duration::from_secs(2), "server-shaped pending work", || {
        let trace = runtime.trace().expect("trace query succeeds");
        runtime
            .has_in_flight_calls()
            .expect("in-flight query succeeds")
            && trace.iter().any(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::CallDispatchAttempted {
                        call_kind: CallKind::TcpRead,
                        ..
                    }
                )
            })
            && trace
                .iter()
                .filter(|event| {
                    matches!(
                        event.kind(),
                        RuntimeEventKind::CallDispatchAttempted {
                            call_kind: CallKind::TcpAccept,
                            ..
                        }
                    )
                })
                .count()
                >= 2
    });

    let trace = runtime.shutdown().expect("runtime shutdown succeeds");
    for call_kind in [
        CallKind::TcpAccept,
        CallKind::TcpRead,
        CallKind::Sleep,
        CallKind::IsolateCall,
    ] {
        assert!(
            trace.iter().any(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::CallCompletionRejected {
                        call_kind: found,
                        reason: CallCompletionRejectedReason::RequesterClosed,
                        ..
                    } if found == call_kind
                )
            }),
            "shutdown should reject pending {call_kind:?} completion; trace = {trace:#?}"
        );
    }
}

#[test]
fn local_server_shutdown_cancels_pending_write_work() {
    let observations = Arc::new(Mutex::new(Vec::new()));
    let worker_addresses = Arc::new(Mutex::new(Vec::new()));
    let bound_addr = Arc::new(Mutex::new(None));
    // This seed/frequency lets accept and read complete, then tombstones the
    // first write long enough for shutdown to cancel it deterministically.
    let simulated_io = SimulatedIO::with_config(SimulatedConfig {
        seed: 9,
        completion_delay: SimulatedDelay::Every {
            one_in: 2,
            steps: 1_000_000_000,
        },
        max_send_chunk: Some(2),
    });
    let io_for_worker = simulated_io.clone();
    let runtime = BetelgeuseRuntime::with_config_and_io_loop_factory(
        LocalShard,
        LocalMailboxFactory,
        BetelgeuseRuntimeConfig {
            command_capacity: 16,
            idle_wait: Duration::from_millis(1),
        },
        move || io_for_worker.loop_handle(Global),
    );
    let (_parent, worker) = start_supervised_worker(&runtime, &observations, &worker_addresses);
    let listener = runtime
        .register_with_capacity::<Listener, _>(
            Listener {
                bind_addr: "127.0.0.1:0".parse().expect("loopback parse"),
                bound_addr: Arc::clone(&bound_addr),
                worker,
                observations: Arc::clone(&observations),
                listener: None,
                accepted: 0,
                target_accepts: 1,
            },
            8,
        )
        .expect("listener register accepts");

    runtime
        .try_send(listener, ListenerMsg::Start)
        .expect("listener start handoff accepts");
    wait_until(Duration::from_secs(2), "simulated listener bound", || {
        bound_addr.lock().expect("bound addr mutex").is_some()
    });
    let local_addr = bound_addr
        .lock()
        .expect("bound addr mutex")
        .expect("listener bound addr");
    let peer = simulated_io
        .connect(local_addr, b"echo:write-shutdown".to_vec())
        .expect("write-shutdown peer connects");

    wait_until(Duration::from_secs(2), "pending write dispatched", || {
        let trace = runtime.trace().expect("trace query succeeds");
        runtime
            .has_in_flight_calls()
            .expect("in-flight query succeeds")
            && trace.iter().any(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::CallDispatchAttempted {
                        call_kind: CallKind::TcpWrite,
                        ..
                    }
                )
            })
    });

    let trace = runtime.shutdown().expect("runtime shutdown succeeds");
    for _ in 0..4 {
        simulated_io
            .step()
            .expect("external simulated I/O step after shutdown stays safe");
    }
    assert!(
        peer.output().is_empty(),
        "pending write should be canceled before simulated peer observes bytes"
    );
    assert!(trace.iter().any(|event| {
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
