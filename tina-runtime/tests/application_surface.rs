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
use tina::{
    Address, CallRejectedReason, Mailbox, RestartBudget, RestartPolicy, TrySendError, prelude::*,
};
use tina_runtime::{
    CallCompletionRejectedReason, CallError, CallKind, CallOutcome, ListenerId, MailboxFactory,
    Runtime, RuntimeEvent, RuntimeEventKind, SendOutcome, SendRejectedReason, StreamId,
    ThreadedRuntime, ThreadedRuntimeConfig, call, send_observed, sleep, tcp_accept, tcp_bind,
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
    fn is_empty(&self) -> bool {
        self.queue.borrow().is_empty()
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
type WorkerOutcomes = Arc<Mutex<Vec<CallOutcome<WorkerReply>>>>;
type AuditLog = Arc<Mutex<Vec<u64>>>;
type SessionSnapshots = Arc<Mutex<Vec<Vec<u64>>>>;

#[derive(Debug, Clone, Copy)]
struct ServiceCapacities {
    command_queue: usize,
    parent_mailbox: usize,
    worker_mailbox: usize,
    listener_mailbox: usize,
    connection_mailbox: usize,
    utility_mailbox: usize,
}

impl Default for ServiceCapacities {
    fn default() -> Self {
        Self {
            command_queue: 16,
            parent_mailbox: 8,
            worker_mailbox: 1,
            listener_mailbox: 8,
            connection_mailbox: 8,
            utility_mailbox: 8,
        }
    }
}

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
    held_requests: Vec<RequestContext<WorkerReply>>,
}

#[tina_runtime::isolate(
    message = WorkerMsg,
    reply = WorkerReply,
    shard = LocalShard
)]
impl Worker {
    fn handle(
        &mut self,
        msg: WorkerMsg,
        ctx: &mut Context<'_, LocalShard, Self::Reply>,
    ) -> Effect<Self> {
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

    fn handle_call(&mut self, msg: WorkerMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            WorkerMsg::Boot => call.reject(CallRejectedReason::UnsupportedMessage),
            WorkerMsg::Echo(bytes) => call.reply(WorkerReply(bytes)),
            WorkerMsg::NoReply => {
                self.held_requests.push(call.into_request_context());
                noop()
            }
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
    capacities: ServiceCapacities,
}

#[tina_runtime::isolate(
    message = WorkerParentMsg,
    spawn = RestartableChildDefinition<Worker>,
    shard = LocalShard
)]
impl WorkerParent {
    fn handle(
        &mut self,
        msg: WorkerParentMsg,
        _ctx: &mut Context<'_, LocalShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            WorkerParentMsg::Spawn => {
                let observations = Arc::clone(&self.observations);
                let addresses = Arc::clone(&self.addresses);
                spawn(
                    RestartableChildDefinition::new(
                        move || Worker {
                            observations: Arc::clone(&observations),
                            addresses: Arc::clone(&addresses),
                            held_requests: Vec::new(),
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

#[tina_runtime::isolate(
    message = ConnectionMsg,
    send = Outbound<Infallible>,
    shard = LocalShard
)]
impl Connection {
    fn handle(
        &mut self,
        msg: ConnectionMsg,
        _ctx: &mut Context<'_, LocalShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ConnectionMsg::Begin => tcp_read(self.stream, 256).then(|result| match result {
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
                    .then(ConnectionMsg::WorkerReturned)
                }
                (RequestMode::Full, _) => {
                    self.mode = Some(RequestMode::Full);
                    batch([
                        call(
                            self.worker,
                            WorkerMsg::Echo(b"accepted-before-full".to_vec()),
                            Duration::from_millis(250),
                        )
                        .then(ConnectionMsg::WorkerReturned),
                        call(
                            self.worker,
                            WorkerMsg::Echo(b"must-reject".to_vec()),
                            Duration::from_millis(250),
                        )
                        .then(ConnectionMsg::WorkerReturned),
                    ])
                }
                (RequestMode::Timeout, _) => {
                    self.mode = Some(RequestMode::Timeout);
                    call(self.worker, WorkerMsg::NoReply, Duration::from_millis(20))
                        .then(ConnectionMsg::WorkerReturned)
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
                    CallOutcome::Rejected(_) => b"worker-rejected".to_vec(),
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
    tcp_write(stream, bytes).then(|result| match result {
        Ok(count) => ConnectionMsg::Wrote(count),
        Err(_) => ConnectionMsg::Failed,
    })
}

fn close_stream(stream: StreamId) -> Effect<Connection> {
    tcp_close_stream(stream).then(|result| match result {
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
    fn handle(
        &mut self,
        msg: ListenerMsg,
        ctx: &mut Context<'_, LocalShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ListenerMsg::Start => {
                let bind_addr = self.bind_addr;
                tcp_bind(bind_addr).then(|result| match result {
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
                tcp_close_listener(listener).then(|result| match result {
                    Ok(()) => ListenerMsg::Closed,
                    Err(_) => ListenerMsg::Failed,
                })
            }
            ListenerMsg::Closed | ListenerMsg::Failed => stop(),
        }
    }
}

fn accept(listener: ListenerId) -> Effect<Listener> {
    tcp_accept(listener).then(|result| match result {
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
    fn handle(
        &mut self,
        msg: LongWorkMsg,
        _ctx: &mut Context<'_, LocalShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            LongWorkMsg::Start { worker } => batch([
                sleep(Duration::from_secs(60)).then(|_| LongWorkMsg::Slept),
                call(worker, WorkerMsg::NoReply, Duration::from_secs(60))
                    .then(|_| LongWorkMsg::WorkerReturned),
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
    fn handle(
        &mut self,
        msg: StaleProbeMsg,
        _ctx: &mut Context<'_, LocalShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            StaleProbeMsg::Probe(target) => {
                send_observed(target, WorkerMsg::Echo(b"stale".to_vec()))
                    .then(StaleProbeMsg::Observed)
            }
            StaleProbeMsg::Observed(SendOutcome::Closed) => {
                self.observations
                    .lock()
                    .expect("observations mutex")
                    .push(ServerObservation::StaleClosed);
                noop()
            }
            StaleProbeMsg::Observed(
                SendOutcome::Accepted | SendOutcome::Full | SendOutcome::ForeignSystem { .. },
            ) => {
                panic!("stale worker address must reject as closed")
            }
        }
    }
}

#[derive(Debug, Clone)]
enum RouterMsg {
    Start,
    WorkerReturned(CallOutcome<WorkerReply>),
}

#[derive(Debug)]
struct Router {
    worker: Address<WorkerMsg, WorkerReply>,
    outcomes: WorkerOutcomes,
}

#[tina_runtime::isolate(message = RouterMsg, shard = LocalShard)]
impl Router {
    fn handle(
        &mut self,
        msg: RouterMsg,
        _ctx: &mut Context<'_, LocalShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            RouterMsg::Start => batch([
                call(
                    self.worker,
                    WorkerMsg::Echo(b"first".to_vec()),
                    Duration::from_millis(250),
                )
                .then(RouterMsg::WorkerReturned),
                call(
                    self.worker,
                    WorkerMsg::Echo(b"second".to_vec()),
                    Duration::from_millis(250),
                )
                .then(RouterMsg::WorkerReturned),
            ]),
            RouterMsg::WorkerReturned(outcome) => {
                self.outcomes.lock().expect("outcomes mutex").push(outcome);
                noop()
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum SessionAuditMsg {
    Joined(u64),
}

#[derive(Debug)]
struct SessionAudit {
    log: AuditLog,
}

#[tina_runtime::isolate(message = SessionAuditMsg, shard = LocalShard)]
impl SessionAudit {
    fn handle(
        &mut self,
        msg: SessionAuditMsg,
        _ctx: &mut Context<'_, LocalShard, Self::Reply>,
    ) -> Effect<Self> {
        let SessionAuditMsg::Joined(user_id) = msg;
        self.log.lock().expect("audit log mutex").push(user_id);
        noop()
    }
}

#[derive(Debug, Clone, Copy)]
enum SessionMsg {
    Connected(u64),
    Snapshot,
}

#[derive(Debug)]
struct Session {
    audit: Address<SessionAuditMsg>,
    users: Vec<u64>,
    snapshots: SessionSnapshots,
}

#[tina_runtime::isolate(
    message = SessionMsg,
    send = Outbound<SessionAuditMsg>,
    shard = LocalShard
)]
impl Session {
    fn handle(
        &mut self,
        msg: SessionMsg,
        _ctx: &mut Context<'_, LocalShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SessionMsg::Connected(user_id) => {
                self.users.push(user_id);
                send(self.audit, SessionAuditMsg::Joined(user_id))
            }
            SessionMsg::Snapshot => {
                self.snapshots
                    .lock()
                    .expect("snapshots mutex")
                    .push(self.users.clone());
                noop()
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

fn step_until<F>(
    runtime: &mut Runtime<LocalShard, LocalMailboxFactory>,
    timeout: Duration,
    label: &str,
    predicate: F,
) where
    F: Fn(&Runtime<LocalShard, LocalMailboxFactory>) -> bool,
{
    let deadline = Instant::now() + timeout;
    while !predicate(runtime) {
        if Instant::now() > deadline {
            panic!(
                "step_until({label}) timed out; trace = {:#?}",
                runtime.trace()
            );
        }
        runtime.step();
        thread::sleep(Duration::from_millis(1));
    }
}

fn runtime_config(capacities: ServiceCapacities) -> ThreadedRuntimeConfig {
    ThreadedRuntimeConfig {
        command_capacity: capacities.command_queue,
        idle_wait: Duration::from_millis(1),
        ..Default::default()
    }
}

fn count_kind(trace: &[RuntimeEvent], predicate: impl Fn(RuntimeEventKind) -> bool) -> usize {
    trace.iter().filter(|event| predicate(event.kind())).count()
}

fn assert_event_count(
    trace: &[RuntimeEvent],
    label: &str,
    expected: usize,
    predicate: impl Fn(RuntimeEventKind) -> bool,
) {
    let count = count_kind(trace, predicate);
    assert_eq!(count, expected, "{label}; trace = {trace:#?}");
}

fn assert_event_count_at_least(
    trace: &[RuntimeEvent],
    label: &str,
    minimum: usize,
    predicate: impl Fn(RuntimeEventKind) -> bool,
) {
    let count = count_kind(trace, predicate);
    assert!(
        count >= minimum,
        "{label}; expected at least {minimum}, got {count}; trace = {trace:#?}"
    );
}

fn assert_event_exists(
    trace: &[RuntimeEvent],
    label: &str,
    predicate: impl Fn(RuntimeEventKind) -> bool,
) {
    assert_event_count_at_least(trace, label, 1, predicate);
}

fn assert_observed(observations: &[ServerObservation], expected: ServerObservation) {
    assert!(
        observations.contains(&expected),
        "missing {expected:?}; observations = {observations:?}"
    );
}

fn assert_listener_stopped_and_idle(
    runtime: &ThreadedRuntime<LocalShard, LocalMailboxFactory>,
    listener: Address<ListenerMsg>,
    label: &str,
) {
    wait_until(Duration::from_secs(2), label, || {
        let trace = runtime.complete_trace().expect("trace query succeeds");
        trace.iter().any(|event| {
            event.isolate() == listener.isolate()
                && matches!(event.kind(), RuntimeEventKind::IsolateStopped)
        }) && !runtime
            .has_in_flight_calls()
            .expect("in-flight query succeeds")
    });
}

fn assert_dispatch_attempts_have_terminal_outcomes(trace: &[RuntimeEvent]) {
    let send_attempts = count_kind(trace, |kind| {
        matches!(kind, RuntimeEventKind::SendDispatchAttempted { .. })
    });
    let send_outcomes = count_kind(trace, |kind| {
        matches!(
            kind,
            RuntimeEventKind::SendAccepted { .. } | RuntimeEventKind::SendRejected { .. }
        )
    });
    assert_eq!(
        send_attempts, send_outcomes,
        "every send dispatch attempt needs one visible accepted/rejected outcome; trace = {trace:#?}"
    );

    let call_attempts = count_kind(trace, |kind| {
        matches!(kind, RuntimeEventKind::CallDispatchAttempted { .. })
    });
    let call_outcomes = count_kind(trace, |kind| {
        matches!(
            kind,
            RuntimeEventKind::CallCompleted { .. }
                | RuntimeEventKind::CallFailed { .. }
                | RuntimeEventKind::CallCompletionRejected { .. }
                | RuntimeEventKind::CallCancelled { .. }
        )
    });
    assert_eq!(
        call_attempts, call_outcomes,
        "every call dispatch attempt needs one visible terminal outcome; trace = {trace:#?}"
    );
}

fn start_supervised_worker(
    runtime: &ThreadedRuntime<LocalShard, LocalMailboxFactory>,
    observations: &Observations,
    addresses: &WorkerAddresses,
    capacities: ServiceCapacities,
) -> (Address<WorkerParentMsg>, Address<WorkerMsg, WorkerReply>) {
    let parent = runtime
        .register_with_capacity::<WorkerParent, _>(
            WorkerParent {
                observations: Arc::clone(observations),
                addresses: Arc::clone(addresses),
                capacities,
            },
            capacities.parent_mailbox,
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

fn start_supervised_worker_explicit(
    runtime: &mut Runtime<LocalShard, LocalMailboxFactory>,
    observations: &Observations,
    addresses: &WorkerAddresses,
    capacities: ServiceCapacities,
) -> (Address<WorkerParentMsg>, Address<WorkerMsg, WorkerReply>) {
    let parent = runtime.register_with_capacity::<WorkerParent, _>(
        WorkerParent {
            observations: Arc::clone(observations),
            addresses: Arc::clone(addresses),
            capacities,
        },
        capacities.parent_mailbox,
    );
    runtime.supervise(
        parent,
        SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(4)),
    );
    runtime
        .try_send(parent, WorkerParentMsg::Spawn)
        .expect("spawn handoff accepts");
    step_until(
        runtime,
        Duration::from_secs(2),
        "explicit worker boot",
        |_| !addresses.lock().expect("worker address mutex").is_empty(),
    );
    let worker = addresses.lock().expect("worker address mutex")[0];
    (parent, worker)
}

#[test]
fn live_local_server_routes_tcp_through_bounded_worker_pressure() {
    let capacities = ServiceCapacities::default();
    let observations = Arc::new(Mutex::new(Vec::new()));
    let worker_addresses = Arc::new(Mutex::new(Vec::new()));
    let bound_addr = Arc::new(Mutex::new(None));
    let runtime =
        ThreadedRuntime::with_config(LocalShard, LocalMailboxFactory, runtime_config(capacities));
    let (_parent, worker) =
        start_supervised_worker(&runtime, &observations, &worker_addresses, capacities);
    let listener = runtime
        .register_with_capacity::<Listener, _>(
            Listener {
                bind_addr: "127.0.0.1:0".parse().expect("loopback parse"),
                bound_addr: Arc::clone(&bound_addr),
                worker,
                observations: Arc::clone(&observations),
                capacities,
                listener: None,
                accepted: 0,
                target_accepts: 3,
            },
            capacities.listener_mailbox,
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
    assert!(
        outputs.iter().all(|output| output == b"alpha"
            || output == b"worker-full"
            || output == b"worker-timeout"),
        "bounded worker pressure should surface as full/timeout responses: {outputs:?}"
    );
    assert!(
        outputs.iter().any(|output| output == b"worker-full"),
        "at least one native client should observe bounded worker pressure"
    );

    assert_listener_stopped_and_idle(&runtime, listener, "listener stopped");

    let trace = runtime.shutdown().expect("runtime shutdown succeeds");
    assert_dispatch_attempts_have_terminal_outcomes(&trace);
    let observations = observations.lock().expect("observations mutex").clone();
    assert_observed(&observations, ServerObservation::WorkerBooted);
    if outputs.iter().any(|output| output == b"alpha") {
        assert_observed(&observations, ServerObservation::WorkerReplied);
    }
    assert_observed(&observations, ServerObservation::WorkerFull);
    if outputs.iter().any(|output| output == b"worker-timeout") {
        assert_observed(&observations, ServerObservation::WorkerTimeout);
    }
    assert_event_count_at_least(
        &trace,
        "native live client ordering may create more than one full outcome, but bounded pressure must be visible",
        1,
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
    if outputs.iter().any(|output| output == b"worker-timeout") {
        assert_event_count(&trace, "timeout outcome stays singular", 1, |kind| {
            matches!(
                kind,
                RuntimeEventKind::CallFailed {
                    call_kind: CallKind::IsolateCall,
                    reason: CallError::Timeout,
                    ..
                }
            )
        });
    }
    assert_event_count(
        &trace,
        "all three native clients should complete accept",
        3,
        |kind| {
            matches!(
                kind,
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::TcpAccept,
                    ..
                }
            )
        },
    );
}

#[test]
fn explicit_step_application_surface_routes_simulated_tcp_with_visible_pressure() {
    let capacities = ServiceCapacities::default();
    let observations = Arc::new(Mutex::new(Vec::new()));
    let worker_addresses = Arc::new(Mutex::new(Vec::new()));
    let bound_addr = Arc::new(Mutex::new(None));
    let simulated_io = SimulatedIO::with_config(SimulatedConfig {
        seed: 32,
        completion_delay: SimulatedDelay::Every {
            one_in: 1,
            steps: 1,
        },
        max_send_chunk: Some(2),
    });
    let mut runtime = Runtime::with_betelgeuse_io_loop(
        LocalShard,
        LocalMailboxFactory,
        simulated_io.loop_handle(Global),
    );
    let (_parent, worker) = start_supervised_worker_explicit(
        &mut runtime,
        &observations,
        &worker_addresses,
        capacities,
    );
    let listener = runtime.register_with_capacity::<Listener, _>(
        Listener {
            bind_addr: "127.0.0.1:0".parse().expect("loopback parse"),
            bound_addr: Arc::clone(&bound_addr),
            worker,
            observations: Arc::clone(&observations),
            capacities,
            listener: None,
            accepted: 0,
            target_accepts: 2,
        },
        capacities.listener_mailbox,
    );

    runtime
        .try_send(listener, ListenerMsg::Start)
        .expect("listener start handoff accepts");
    step_until(
        &mut runtime,
        Duration::from_secs(2),
        "explicit bind",
        |_| bound_addr.lock().expect("bound addr mutex").is_some(),
    );
    let local_addr = bound_addr
        .lock()
        .expect("bound addr mutex")
        .expect("listener bound addr");
    let echo_peer = simulated_io
        .connect(local_addr, b"echo:explicit".to_vec())
        .expect("simulated echo peer connects");
    let full_peer = simulated_io
        .connect(local_addr, b"full".to_vec())
        .expect("simulated full peer connects");

    step_until(
        &mut runtime,
        Duration::from_secs(3),
        "explicit simulated peers finish",
        |runtime| {
            echo_peer.output() == b"explicit"
                && full_peer.output() == b"worker-full"
                && !runtime.has_in_flight_calls()
                && runtime.trace().iter().any(|event| {
                    event.isolate() == listener.isolate()
                        && matches!(event.kind(), RuntimeEventKind::IsolateStopped)
                })
        },
    );

    assert_eq!(echo_peer.output(), b"explicit");
    assert_eq!(full_peer.output(), b"worker-full");
    let trace = runtime.trace().to_vec();
    assert_dispatch_attempts_have_terminal_outcomes(&trace);
    assert_event_exists(
        &trace,
        "explicit runtime should surface TargetFull",
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
    assert_event_count_at_least(
        &trace,
        "explicit runtime should prove partial writes through repeated completions",
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
    let observations = observations.lock().expect("observations mutex").clone();
    assert_observed(&observations, ServerObservation::WorkerFull);
}

#[test]
fn application_surface_routes_bounded_worker_pressure_without_tcp() {
    let capacities = ServiceCapacities::default();
    let observations = Arc::new(Mutex::new(Vec::new()));
    let worker_addresses = Arc::new(Mutex::new(Vec::new()));
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = Runtime::new(LocalShard, LocalMailboxFactory);
    let (_parent, worker) = start_supervised_worker_explicit(
        &mut runtime,
        &observations,
        &worker_addresses,
        capacities,
    );
    let router = runtime.register_with_capacity::<Router, _>(
        Router {
            worker,
            outcomes: Arc::clone(&outcomes),
        },
        capacities.utility_mailbox,
    );

    runtime
        .try_send(router, RouterMsg::Start)
        .expect("router start accepts");
    step_until(
        &mut runtime,
        Duration::from_secs(2),
        "bounded router outcomes",
        |_| outcomes.lock().expect("outcomes mutex").len() == 2,
    );

    let outcomes = outcomes.lock().expect("outcomes mutex").clone();
    assert!(
        outcomes
            .iter()
            .any(|outcome| matches!(outcome, CallOutcome::Replied(WorkerReply(bytes)) if bytes == b"first")),
        "one worker call should complete successfully; outcomes = {outcomes:?}"
    );
    assert!(
        outcomes
            .iter()
            .any(|outcome| matches!(outcome, CallOutcome::Full)),
        "second worker call should surface bounded backpressure; outcomes = {outcomes:?}"
    );
    assert_dispatch_attempts_have_terminal_outcomes(runtime.trace());
    assert_event_exists(
        runtime.trace(),
        "router proof should trace worker mailbox full",
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

#[test]
fn application_surface_stateful_session_updates_state_and_audits_locally() {
    let capacities = ServiceCapacities::default();
    let audit_log = Arc::new(Mutex::new(Vec::new()));
    let snapshots = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = Runtime::new(LocalShard, LocalMailboxFactory);
    let audit = runtime.register_with_capacity::<SessionAudit, _>(
        SessionAudit {
            log: Arc::clone(&audit_log),
        },
        capacities.utility_mailbox,
    );
    let session = runtime.register_with_capacity::<Session, _>(
        Session {
            audit,
            users: Vec::new(),
            snapshots: Arc::clone(&snapshots),
        },
        capacities.utility_mailbox,
    );

    runtime
        .try_send(session, SessionMsg::Connected(7))
        .expect("first session event accepts");
    runtime
        .try_send(session, SessionMsg::Connected(9))
        .expect("second session event accepts");
    runtime
        .try_send(session, SessionMsg::Snapshot)
        .expect("snapshot event accepts");
    step_until(
        &mut runtime,
        Duration::from_secs(2),
        "session snapshot and audit",
        |_| {
            audit_log.lock().expect("audit log mutex").len() == 2
                && snapshots.lock().expect("snapshots mutex").len() == 1
        },
    );

    assert_eq!(
        audit_log.lock().expect("audit log mutex").as_slice(),
        [7, 9]
    );
    assert_eq!(
        snapshots.lock().expect("snapshots mutex").as_slice(),
        [vec![7, 9]]
    );
    assert_dispatch_attempts_have_terminal_outcomes(runtime.trace());
    assert_event_count(
        runtime.trace(),
        "two audit sends should be accepted",
        2,
        |kind| {
            matches!(
                kind,
                RuntimeEventKind::SendAccepted {
                    target_isolate,
                    ..
                } if target_isolate == audit.isolate()
            )
        },
    );
}

#[test]
fn simulated_io_local_server_keeps_partial_slow_peer_semantics_through_threaded_runtime() {
    let capacities = ServiceCapacities::default();
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
    let runtime = ThreadedRuntime::with_config_and_io_loop_factory(
        LocalShard,
        LocalMailboxFactory,
        runtime_config(capacities),
        move || io_for_worker.loop_handle(Global),
    );
    let (_parent, worker) =
        start_supervised_worker(&runtime, &observations, &worker_addresses, capacities);
    let listener = runtime
        .register_with_capacity::<Listener, _>(
            Listener {
                bind_addr: "127.0.0.1:0".parse().expect("loopback parse"),
                bound_addr: Arc::clone(&bound_addr),
                worker,
                observations: Arc::clone(&observations),
                capacities,
                listener: None,
                accepted: 0,
                target_accepts: 2,
            },
            capacities.listener_mailbox,
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
    assert_listener_stopped_and_idle(&runtime, listener, "simulated listener stopped");

    let trace = runtime.shutdown().expect("runtime shutdown succeeds");
    assert_dispatch_attempts_have_terminal_outcomes(&trace);
    assert_eq!(echo_peer.output(), b"abcdef");
    assert_eq!(full_peer.output(), b"worker-full");
    assert_event_count_at_least(
        &trace,
        "partial send limit should force more than one write completion per response",
        3,
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
    let observations = observations.lock().expect("observations mutex").clone();
    assert_observed(&observations, ServerObservation::WorkerFull);
}

#[test]
fn llama_supervised_worker_service_restarts_worker_and_rejects_stale_address() {
    let capacities = ServiceCapacities::default();
    let observations = Arc::new(Mutex::new(Vec::new()));
    let worker_addresses = Arc::new(Mutex::new(Vec::new()));
    let runtime =
        ThreadedRuntime::with_config(LocalShard, LocalMailboxFactory, runtime_config(capacities));
    let (_parent, first_worker) =
        start_supervised_worker(&runtime, &observations, &worker_addresses, capacities);

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
            capacities.utility_mailbox,
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
    assert_dispatch_attempts_have_terminal_outcomes(&trace);
    assert_event_exists(&trace, "worker restart should be visible", |kind| {
        matches!(
            kind,
            RuntimeEventKind::RestartChildCompleted {
                old_isolate,
                new_isolate,
                ..
            } if old_isolate == first_worker.isolate() && new_isolate == replacement.isolate()
        )
    });
    assert_event_exists(
        &trace,
        "stale worker address should reject closed",
        |kind| {
            matches!(
                kind,
                RuntimeEventKind::SendRejected {
                    target_isolate,
                    target_generation,
                    reason: SendRejectedReason::Closed,
                    ..
                } if target_isolate == first_worker.isolate()
                    && target_generation == first_worker.generation()
            )
        },
    );
}

#[test]
fn local_server_shutdown_cancels_pending_accept_read_timer_and_call_work() {
    let capacities = ServiceCapacities::default();
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
    let runtime = ThreadedRuntime::with_config_and_io_loop_factory(
        LocalShard,
        LocalMailboxFactory,
        runtime_config(capacities),
        move || io_for_worker.loop_handle(Global),
    );
    let (_parent, worker) =
        start_supervised_worker(&runtime, &observations, &worker_addresses, capacities);
    let listener = runtime
        .register_with_capacity::<Listener, _>(
            Listener {
                bind_addr: "127.0.0.1:0".parse().expect("loopback parse"),
                bound_addr: Arc::clone(&bound_addr),
                worker,
                observations: Arc::clone(&observations),
                capacities,
                listener: None,
                accepted: 0,
                target_accepts: 4,
            },
            capacities.listener_mailbox,
        )
        .expect("listener register accepts");
    let long_work = runtime
        .register_with_capacity::<LongWork, _>(LongWork, capacities.utility_mailbox)
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
    let _pending_read_peer = simulated_io
        .connect_open(local_addr, Vec::new())
        .expect("pending-read peer connects");

    runtime
        .try_send(long_work, LongWorkMsg::Start { worker })
        .expect("long work start handoff accepts");
    wait_until(Duration::from_secs(2), "server-shaped pending work", || {
        let trace = runtime.complete_trace().expect("trace query succeeds");
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

    let pre_shutdown_trace = runtime.complete_trace().expect("trace query succeeds");
    let pending_read_call = pre_shutdown_trace
        .iter()
        .find_map(|event| match event.kind() {
            RuntimeEventKind::CallDispatchAttempted {
                call_id,
                call_kind: CallKind::TcpRead,
            } => Some(call_id),
            _ => None,
        })
        .expect("the open simulated peer must dispatch one pending read");
    assert!(
        !pre_shutdown_trace.iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted { call_id, .. }
                    | RuntimeEventKind::CallFailed { call_id, .. }
                    | RuntimeEventKind::CallCompletionRejected { call_id, .. }
                    if call_id == pending_read_call
            )
        }),
        "the read call must still be pending before shutdown; trace = {pre_shutdown_trace:#?}"
    );

    let trace = runtime.shutdown().expect("runtime shutdown succeeds");
    assert_dispatch_attempts_have_terminal_outcomes(&trace);
    for call_kind in [CallKind::TcpAccept, CallKind::Sleep] {
        assert_event_exists(
            &trace,
            &format!("shutdown should reject pending {call_kind:?} completion"),
            |kind| {
                matches!(
                    kind,
                    RuntimeEventKind::CallCompletionRejected {
                        call_kind: found,
                        reason: CallCompletionRejectedReason::RequesterClosed,
                        ..
                    } if found == call_kind
                )
            },
        );
    }
    assert_event_count(
        &trace,
        "shutdown should reject the proven-pending TcpRead exactly once",
        1,
        |kind| {
            matches!(
                kind,
                RuntimeEventKind::CallCompletionRejected {
                    call_id,
                    call_kind: CallKind::TcpRead,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                } if call_id == pending_read_call
            )
        },
    );
    // Pending isolate-calls now settle as `CallCancelled { RuntimeStopped }`
    // at shutdown so observers can attribute the cause and so any caller
    // still holding a `CallHandle` sees `state() == Cancelled` (not a
    // forever-`Pending` lie).
    assert_event_exists(
        &trace,
        "shutdown should cancel pending isolate calls with RuntimeStopped",
        |kind| {
            matches!(
                kind,
                RuntimeEventKind::CallCancelled {
                    cause: tina::CancelCause::RuntimeStopped,
                    ..
                }
            )
        },
    );
}

#[test]
fn local_server_shutdown_cancels_pending_write_work() {
    let capacities = ServiceCapacities::default();
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
    let runtime = ThreadedRuntime::with_config_and_io_loop_factory(
        LocalShard,
        LocalMailboxFactory,
        runtime_config(capacities),
        move || io_for_worker.loop_handle(Global),
    );
    let (_parent, worker) =
        start_supervised_worker(&runtime, &observations, &worker_addresses, capacities);
    let listener = runtime
        .register_with_capacity::<Listener, _>(
            Listener {
                bind_addr: "127.0.0.1:0".parse().expect("loopback parse"),
                bound_addr: Arc::clone(&bound_addr),
                worker,
                observations: Arc::clone(&observations),
                capacities,
                listener: None,
                accepted: 0,
                target_accepts: 1,
            },
            capacities.listener_mailbox,
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
        let trace = runtime.complete_trace().expect("trace query succeeds");
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
    assert_dispatch_attempts_have_terminal_outcomes(&trace);
    for _ in 0..4 {
        simulated_io
            .step()
            .expect("external simulated I/O step after shutdown stays safe");
    }
    assert!(
        peer.output().is_empty(),
        "pending write should be canceled before simulated peer observes bytes"
    );
    assert_event_exists(&trace, "shutdown should reject pending write", |kind| {
        matches!(
            kind,
            RuntimeEventKind::CallCompletionRejected {
                call_kind: CallKind::TcpWrite,
                reason: CallCompletionRejectedReason::RequesterClosed,
                ..
            }
        )
    });
}
