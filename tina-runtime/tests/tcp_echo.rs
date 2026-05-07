#![feature(allocator_api)]

//! Live TCP echo proof for `Runtime` plus the runtime-owned call
//! contract.
//!
//! Topology mirrors slice 011's reference shape: one listener isolate is
//! the supervised parent, and the accepted connection becomes a
//! restartable child handler isolate. The handler issues `TcpRead`,
//! `TcpWrite`, and `TcpStreamClose` calls through the new effect; the
//! listener issues `TcpBind` and `TcpAccept`.
//!
//! Constraints honored:
//!
//! - bind to `127.0.0.1:0` and rely on the runtime to report the actual
//!   bound address through `CallOutput::TcpBound { local_addr }`
//! - assertions cover both observed network behavior (echoed bytes) and
//!   trace evidence (call completions, child spawns, batch effects,
//!   listener close/stop)
//! - keep assertions per-stream and on event multiplicity; do not claim
//!   cross-stream ordering the runtime never promised

use std::alloc::Global;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Barrier, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use betelgeuse::io::simulated::{SimulatedConfig, SimulatedDelay, SimulatedIO};
use tina::{IsolateId, Mailbox, RestartBudget, RestartPolicy, TrySendError, prelude::*};
use tina_runtime::{
    CallCompletionRejectedReason, CallError, CallInput, CallKind, FileId, ListenerId,
    MailboxFactory, Runtime, RuntimeCall, RuntimeEvent, RuntimeEventKind, StreamId,
    ThreadedRuntime, ThreadedRuntimeConfig, ThreadedSendObservedError, ThreadedTrySendError,
    file_close, file_create, file_fsync, file_read, file_size, file_write, mkdir, tcp_accept,
    tcp_bind, tcp_close_listener, tcp_close_stream, tcp_connect, tcp_read, tcp_write,
};
use tina_supervisor::SupervisorConfig;

// ---------------------------------------------------------------------------
// Test infrastructure.
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(13)
    }
}

struct TestMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    closed: Rc<Cell<bool>>,
}

impl<T> TestMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Rc::new(RefCell::new(VecDeque::new())),
            closed: Rc::new(Cell::new(false)),
        }
    }
}

impl<T> Mailbox<T> for TestMailbox<T> {
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
struct TestMailboxFactory;

impl MailboxFactory for TestMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(TestMailbox::new(capacity))
    }
}

type BoundAddr = Arc<Mutex<Option<SocketAddr>>>;
type ClientObserved = Arc<Mutex<Option<Vec<u8>>>>;
type FileObserved = Arc<Mutex<Option<(u64, Vec<u8>)>>>;

// ---------------------------------------------------------------------------
// Connection handler isolate. One per accepted connection.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum ConnectionEvent {
    Begin,
    Read(Vec<u8>),
    Wrote(usize),
    Closed,
    IoFailed,
}

#[derive(Debug)]
struct Connection {
    stream: StreamId,
    max_chunk: usize,
    pending_write: Vec<u8>,
}

impl Isolate for Connection {
    tina::isolate_types! {
        message: ConnectionEvent,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<ConnectionEvent>,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ConnectionEvent::Begin => read_call(self.stream, self.max_chunk),
            ConnectionEvent::Read(bytes) => {
                if bytes.is_empty() {
                    close_call(self.stream)
                } else {
                    self.pending_write = bytes;
                    write_call(self.stream, self.pending_write.clone())
                }
            }
            ConnectionEvent::Wrote(count) => {
                if count >= self.pending_write.len() {
                    self.pending_write.clear();
                    read_call(self.stream, self.max_chunk)
                } else {
                    self.pending_write.drain(..count);
                    write_call(self.stream, self.pending_write.clone())
                }
            }
            ConnectionEvent::Closed | ConnectionEvent::IoFailed => stop(),
        }
    }
}

fn read_call(stream: StreamId, max_len: usize) -> Effect<Connection> {
    tcp_read(stream, max_len).reply(|result| match result {
        Ok(bytes) => ConnectionEvent::Read(bytes),
        Err(_) => ConnectionEvent::IoFailed,
    })
}

fn write_call(stream: StreamId, bytes: Vec<u8>) -> Effect<Connection> {
    tcp_write(stream, bytes).reply(|result| match result {
        Ok(count) => ConnectionEvent::Wrote(count),
        Err(_) => ConnectionEvent::IoFailed,
    })
}

fn close_call(stream: StreamId) -> Effect<Connection> {
    tcp_close_stream(stream).reply(|result| match result {
        Ok(()) => ConnectionEvent::Closed,
        Err(_) => ConnectionEvent::IoFailed,
    })
}

// ---------------------------------------------------------------------------
// Outbound client isolate. Tina owns the connect/write/read/close chain.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum OutboundClientEvent {
    Start(SocketAddr),
    Connected(Result<(StreamId, SocketAddr, SocketAddr), CallError>),
    Wrote(Result<usize, CallError>),
    Read(Result<Vec<u8>, CallError>),
    Closed(Result<(), CallError>),
}

#[derive(Debug)]
struct OutboundClient {
    payload: Vec<u8>,
    pending_write: Vec<u8>,
    stream: Option<StreamId>,
    observed: ClientObserved,
}

impl Isolate for OutboundClient {
    tina::isolate_types! {
        message: OutboundClientEvent,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<OutboundClientEvent>,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            OutboundClientEvent::Start(addr) => {
                tcp_connect(addr).reply(OutboundClientEvent::Connected)
            }
            OutboundClientEvent::Connected(Ok((stream, _local_addr, _peer_addr))) => {
                self.stream = Some(stream);
                self.pending_write = self.payload.clone();
                tcp_write(stream, self.pending_write.clone()).reply(OutboundClientEvent::Wrote)
            }
            OutboundClientEvent::Wrote(Ok(count)) => {
                let stream = self.stream.expect("stream set after connect");
                if count >= self.pending_write.len() {
                    self.pending_write.clear();
                    tcp_read(stream, 4).reply(OutboundClientEvent::Read)
                } else {
                    self.pending_write.drain(..count);
                    tcp_write(stream, self.pending_write.clone()).reply(OutboundClientEvent::Wrote)
                }
            }
            OutboundClientEvent::Read(Ok(bytes)) => {
                *self.observed.lock().expect("observed mutex") = Some(bytes);
                let stream = self.stream.expect("stream set after connect");
                tcp_close_stream(stream).reply(OutboundClientEvent::Closed)
            }
            OutboundClientEvent::Closed(Ok(())) => stop(),
            OutboundClientEvent::Connected(Err(_))
            | OutboundClientEvent::Wrote(Err(_))
            | OutboundClientEvent::Read(Err(_))
            | OutboundClientEvent::Closed(Err(_)) => stop(),
        }
    }
}

// ---------------------------------------------------------------------------
// File client isolate. Tina owns open/write/fsync/size/read/close.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum FileClientEvent {
    Start { dir: PathBuf, path: PathBuf },
    DirectoryMade(Result<(), CallError>, PathBuf),
    Opened(Result<FileId, CallError>),
    Wrote(Result<usize, CallError>),
    Synced(Result<(), CallError>),
    Sized(Result<u64, CallError>),
    Read(Result<Vec<u8>, CallError>),
    Closed(Result<(), CallError>),
}

#[derive(Debug)]
struct FileClient {
    payload: Vec<u8>,
    file: Option<FileId>,
    size: Option<u64>,
    observed: FileObserved,
}

impl Isolate for FileClient {
    tina::isolate_types! {
        message: FileClientEvent,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<FileClientEvent>,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            FileClientEvent::Start { dir, path } => {
                mkdir(dir, 0o755).reply(move |result| FileClientEvent::DirectoryMade(result, path))
            }
            FileClientEvent::DirectoryMade(Ok(()), path) => {
                file_create(path).reply(FileClientEvent::Opened)
            }
            FileClientEvent::Opened(Ok(file)) => {
                self.file = Some(file);
                file_write(file, self.payload.clone()).reply(FileClientEvent::Wrote)
            }
            FileClientEvent::Wrote(Ok(_)) => {
                let file = self.file.expect("file set after open");
                file_fsync(file).reply(FileClientEvent::Synced)
            }
            FileClientEvent::Synced(Ok(())) => {
                let file = self.file.expect("file set after open");
                file_size(file).reply(FileClientEvent::Sized)
            }
            FileClientEvent::Sized(Ok(size)) => {
                self.size = Some(size);
                let file = self.file.expect("file set after open");
                file_read(file, size as usize).reply(FileClientEvent::Read)
            }
            FileClientEvent::Read(Ok(bytes)) => {
                let size = self.size.expect("size set before read");
                *self.observed.lock().expect("file observed mutex") = Some((size, bytes));
                let file = self.file.expect("file set after open");
                file_close(file).reply(FileClientEvent::Closed)
            }
            FileClientEvent::Closed(Ok(())) => stop(),
            FileClientEvent::DirectoryMade(Err(_), _)
            | FileClientEvent::Opened(Err(_))
            | FileClientEvent::Wrote(Err(_))
            | FileClientEvent::Synced(Err(_))
            | FileClientEvent::Sized(Err(_))
            | FileClientEvent::Read(Err(_))
            | FileClientEvent::Closed(Err(_)) => stop(),
        }
    }
}

// ---------------------------------------------------------------------------
// Listener isolate. Start → TcpBind → publish bound addr → TcpAccept →
// spawn restartable connection child. The supervised-parent shape mirrors
// slice 011's reference workload.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum ListenerEvent {
    Start,
    Bound {
        listener: ListenerId,
        addr: SocketAddr,
    },
    AcceptNext,
    Close,
    Closed,
    Accepted {
        stream: StreamId,
    },
    IoFailed,
}

#[derive(Debug)]
struct Listener {
    bind_addr: SocketAddr,
    bound_addr_slot: BoundAddr,
    max_chunk: usize,
    target_accepts: usize,
    accepted_count: usize,
    listener: Option<ListenerId>,
}

impl Isolate for Listener {
    tina::isolate_types! {
        message: ListenerEvent,
        reply: (),
        send: Outbound<ListenerEvent>,
        spawn: RestartableChildDefinition<Connection>,
        call: RuntimeCall<ListenerEvent>,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ListenerEvent::Start => {
                let addr = self.bind_addr;
                tcp_bind(addr).reply(move |result| match result {
                    Ok((listener, local_addr)) => ListenerEvent::Bound {
                        listener,
                        addr: local_addr,
                    },
                    Err(_) => ListenerEvent::IoFailed,
                })
            }
            ListenerEvent::Bound { listener, addr } => {
                self.listener = Some(listener);
                *self
                    .bound_addr_slot
                    .lock()
                    .expect("bound addr mutex never poisoned") = Some(addr);
                tcp_accept(listener).reply(|result| match result {
                    Ok((stream, _peer_addr)) => ListenerEvent::Accepted { stream },
                    Err(_) => ListenerEvent::IoFailed,
                })
            }
            ListenerEvent::AcceptNext => {
                let listener = self.listener.expect("listener stored before re-arm");
                tcp_accept(listener).reply(|result| match result {
                    Ok((stream, _peer_addr)) => ListenerEvent::Accepted { stream },
                    Err(_) => ListenerEvent::IoFailed,
                })
            }
            ListenerEvent::Accepted { stream } => {
                self.accepted_count += 1;
                let max_chunk = self.max_chunk;
                let child = spawn(
                    RestartableChildDefinition::new(
                        move || Connection {
                            stream,
                            max_chunk,
                            pending_write: Vec::new(),
                        },
                        8,
                    )
                    .with_initial_message(|| ConnectionEvent::Begin),
                );
                let follow_up = if self.accepted_count < self.target_accepts {
                    ListenerEvent::AcceptNext
                } else {
                    ListenerEvent::Close
                };
                batch([child, ctx.send_self(follow_up)])
            }
            ListenerEvent::Close => {
                let listener = self.listener.expect("listener stored before close");
                tcp_close_listener(listener).reply(|result| match result {
                    Ok(()) => ListenerEvent::Closed,
                    Err(_) => ListenerEvent::IoFailed,
                })
            }
            ListenerEvent::Closed => stop(),
            ListenerEvent::IoFailed => stop(),
        }
    }
}

// ---------------------------------------------------------------------------
// Drive helpers.
// ---------------------------------------------------------------------------

fn step_until<F>(
    runtime: &mut Runtime<TestShard, TestMailboxFactory>,
    timeout: Duration,
    label: &str,
    predicate: F,
) where
    F: Fn(&Runtime<TestShard, TestMailboxFactory>) -> bool,
{
    let deadline = Instant::now() + timeout;
    while !predicate(runtime) {
        if Instant::now() > deadline {
            panic!(
                "step_until({label}): predicate not satisfied within timeout; trace = {:#?}",
                runtime.trace()
            );
        }
        runtime.step();
        thread::sleep(Duration::from_millis(2));
    }
}

fn pump_runtime_until<F>(
    runtime: &mut Runtime<TestShard, TestMailboxFactory>,
    timeout: Duration,
    predicate: F,
) where
    F: Fn() -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        runtime.step();
        if predicate() {
            return;
        }
        if Instant::now() > deadline {
            panic!(
                "pump_runtime_until timed out waiting for condition; trace = {:#?}",
                runtime.trace()
            );
        }
        thread::sleep(Duration::from_millis(2));
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

fn count_handler_finished(trace: &[RuntimeEvent], effect: tina_runtime::EffectKind) -> usize {
    trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::HandlerFinished { effect: actual } if actual == effect
            )
        })
        .count()
}

fn tcp_semantic_counts(trace: &[RuntimeEvent]) -> Vec<(CallKind, usize)> {
    [
        CallKind::TcpBind,
        CallKind::TcpAccept,
        CallKind::TcpRead,
        CallKind::TcpWrite,
        CallKind::TcpStreamClose,
        CallKind::TcpListenerClose,
    ]
    .into_iter()
    .map(|kind| (kind, count_call_completed(trace, kind)))
    .collect()
}

struct ClientRun {
    done: Arc<Mutex<bool>>,
    echoed: Arc<Mutex<Vec<u8>>>,
    handle: JoinHandle<()>,
}

fn spawn_client(
    local_addr: SocketAddr,
    payload: Vec<u8>,
    barrier: Option<Arc<Barrier>>,
) -> ClientRun {
    let done = Arc::new(Mutex::new(false));
    let echoed: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let done_for_client = Arc::clone(&done);
    let echoed_for_client = Arc::clone(&echoed);

    let handle = thread::spawn(move || {
        let mut stream = TcpStream::connect(local_addr).expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(3))).ok();
        if let Some(barrier) = barrier {
            barrier.wait();
        }
        stream.write_all(&payload).expect("write");
        stream
            .shutdown(std::net::Shutdown::Write)
            .expect("write shutdown");

        let mut received = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => received.extend_from_slice(&buf[..n]),
                Err(error) => panic!("client read error: {error}"),
            }
        }

        *echoed_for_client.lock().expect("mutex") = received;
        *done_for_client.lock().expect("mutex") = true;
    });

    ClientRun {
        done,
        echoed,
        handle,
    }
}

fn run_echo_scenario(
    payloads: Vec<Vec<u8>>,
    overlap: bool,
    timeout: Duration,
) -> (Vec<Vec<u8>>, Vec<RuntimeEvent>, IsolateId) {
    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("loopback parse");
    let bound: BoundAddr = Arc::new(Mutex::new(None));

    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);

    let listener_addr = runtime.register(
        Listener {
            bind_addr,
            bound_addr_slot: Arc::clone(&bound),
            max_chunk: 256,
            target_accepts: payloads.len(),
            accepted_count: 0,
            listener: None,
        },
        TestMailbox::new(8),
    );

    runtime.supervise(
        listener_addr,
        SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(4)),
    );

    runtime
        .try_send(listener_addr, ListenerEvent::Start)
        .expect("bootstrap accepts");

    step_until(&mut runtime, Duration::from_secs(2), "bind", |_| {
        bound.lock().expect("mutex").is_some()
    });

    let local_addr = bound
        .lock()
        .expect("mutex")
        .expect("listener published address");

    let clients = if overlap {
        let barrier = Arc::new(Barrier::new(payloads.len()));
        payloads
            .into_iter()
            .map(|payload| spawn_client(local_addr, payload, Some(Arc::clone(&barrier))))
            .collect::<Vec<_>>()
    } else {
        let mut clients = Vec::new();
        for payload in payloads {
            let client = spawn_client(local_addr, payload, None);
            pump_runtime_until(&mut runtime, timeout, || {
                *client.done.lock().expect("flag mutex")
            });
            clients.push(client);
        }
        clients
    };

    if overlap {
        pump_runtime_until(&mut runtime, timeout, || {
            clients
                .iter()
                .all(|client| *client.done.lock().expect("flag mutex"))
        });
    }

    step_until(&mut runtime, timeout, "listener stop", |runtime| {
        !runtime.has_in_flight_calls()
            && runtime.trace().iter().any(|event| {
                event.isolate() == listener_addr.isolate()
                    && matches!(event.kind(), RuntimeEventKind::IsolateStopped)
            })
    });

    let mut echoed = Vec::new();
    for client in clients {
        client.handle.join().expect("client thread");
        echoed.push(client.echoed.lock().expect("mutex").clone());
    }

    (echoed, runtime.trace().to_vec(), listener_addr.isolate())
}

fn wait_until<F>(timeout: Duration, label: &str, mut predicate: F)
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while !predicate() {
        if Instant::now() > deadline {
            panic!("wait_until({label}): predicate not satisfied within timeout");
        }
        thread::yield_now();
    }
}

fn run_betelgeuse_echo_scenario(
    payloads: Vec<Vec<u8>>,
    timeout: Duration,
) -> (Vec<Vec<u8>>, Vec<RuntimeEvent>, IsolateId) {
    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("loopback parse");
    let bound: BoundAddr = Arc::new(Mutex::new(None));

    let runtime = ThreadedRuntime::with_config(
        TestShard,
        TestMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 16,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let listener_addr = runtime
        .register_with_capacity::<Listener, _>(
            Listener {
                bind_addr,
                bound_addr_slot: Arc::clone(&bound),
                max_chunk: 256,
                target_accepts: payloads.len(),
                accepted_count: 0,
                listener: None,
            },
            8,
        )
        .expect("Betelgeuse register accepts");

    runtime
        .supervise(
            listener_addr,
            SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(4)),
        )
        .expect("Betelgeuse supervise accepts");

    runtime
        .try_send(listener_addr, ListenerEvent::Start)
        .expect("Betelgeuse bootstrap accepts");

    wait_until(timeout, "Betelgeuse bind", || {
        bound.lock().expect("mutex").is_some()
    });

    let local_addr = bound
        .lock()
        .expect("mutex")
        .expect("listener published address");

    let clients = payloads
        .into_iter()
        .map(|payload| spawn_client(local_addr, payload, None))
        .collect::<Vec<_>>();

    wait_until(timeout, "Betelgeuse clients", || {
        clients
            .iter()
            .all(|client| *client.done.lock().expect("flag mutex"))
    });

    wait_until(timeout, "Betelgeuse listener stop", || {
        let trace = runtime.complete_trace().expect("Betelgeuse trace");
        trace.iter().any(|event| {
            event.isolate() == listener_addr.isolate()
                && matches!(event.kind(), RuntimeEventKind::IsolateStopped)
        }) && !runtime
            .has_in_flight_calls()
            .expect("Betelgeuse in-flight check")
    });

    let mut echoed = Vec::new();
    for client in clients {
        client.handle.join().expect("client thread");
        echoed.push(client.echoed.lock().expect("mutex").clone());
    }

    let trace = runtime.shutdown().expect("Betelgeuse shutdown");
    (echoed, trace, listener_addr.isolate())
}

fn run_simulated_betelgeuse_echo_scenario(payload: Vec<u8>) -> (Vec<u8>, Vec<RuntimeEvent>) {
    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("loopback parse");
    let bound: BoundAddr = Arc::new(Mutex::new(None));
    let simulated_io = SimulatedIO::with_config(SimulatedConfig {
        seed: 25,
        completion_delay: SimulatedDelay::Every {
            one_in: 1,
            steps: 1,
        },
        max_send_chunk: Some(3),
    });
    let mut runtime = Runtime::with_betelgeuse_io_loop(
        TestShard,
        TestMailboxFactory,
        simulated_io.loop_handle(Global),
    );

    let listener_addr = runtime.register(
        Listener {
            bind_addr,
            bound_addr_slot: Arc::clone(&bound),
            max_chunk: 256,
            target_accepts: 1,
            accepted_count: 0,
            listener: None,
        },
        TestMailbox::new(8),
    );

    runtime
        .try_send(listener_addr, ListenerEvent::Start)
        .expect("bootstrap accepts");

    step_until(
        &mut runtime,
        Duration::from_secs(2),
        "simulated bind",
        |_| bound.lock().expect("mutex").is_some(),
    );

    let local_addr = bound
        .lock()
        .expect("mutex")
        .expect("listener published address");
    let peer = simulated_io
        .connect(local_addr, payload)
        .expect("simulated peer connects to listener");

    step_until(
        &mut runtime,
        Duration::from_secs(2),
        "simulated echo close",
        |runtime| {
            !runtime.has_in_flight_calls()
                && runtime.trace().iter().any(|event| {
                    event.isolate() == listener_addr.isolate()
                        && matches!(event.kind(), RuntimeEventKind::IsolateStopped)
                })
        },
    );

    (peer.output(), runtime.trace().to_vec())
}

fn run_threaded_simulated_betelgeuse_echo_scenario(
    payload: Vec<u8>,
) -> (Vec<u8>, Vec<RuntimeEvent>) {
    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("loopback parse");
    let bound: BoundAddr = Arc::new(Mutex::new(None));
    let simulated_io = SimulatedIO::with_config(SimulatedConfig {
        seed: 26,
        completion_delay: SimulatedDelay::Every {
            one_in: 1,
            steps: 1,
        },
        max_send_chunk: Some(3),
    });
    let io_for_worker = simulated_io.clone();
    let runtime = ThreadedRuntime::with_config_and_io_loop_factory(
        TestShard,
        TestMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 16,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
        move || io_for_worker.loop_handle(Global),
    );

    let listener_addr = runtime
        .register_with_capacity::<Listener, _>(
            Listener {
                bind_addr,
                bound_addr_slot: Arc::clone(&bound),
                max_chunk: 256,
                target_accepts: 1,
                accepted_count: 0,
                listener: None,
            },
            8,
        )
        .expect("Betelgeuse register accepts");

    runtime
        .try_send(listener_addr, ListenerEvent::Start)
        .expect("bootstrap accepts");

    wait_until(Duration::from_secs(2), "threaded simulated bind", || {
        bound.lock().expect("mutex").is_some()
    });

    let local_addr = bound
        .lock()
        .expect("mutex")
        .expect("listener published address");
    let peer = simulated_io
        .connect(local_addr, payload)
        .expect("simulated peer connects to listener");

    wait_until(
        Duration::from_secs(2),
        "threaded simulated echo close",
        || {
            let trace = runtime.complete_trace().expect("trace query succeeds");
            trace.iter().any(|event| {
                event.isolate() == listener_addr.isolate()
                    && matches!(event.kind(), RuntimeEventKind::IsolateStopped)
            }) && !runtime
                .has_in_flight_calls()
                .expect("in-flight query succeeds")
        },
    );

    let output = peer.output();
    let trace = runtime.shutdown().expect("shutdown succeeds");
    (output, trace)
}

// ---------------------------------------------------------------------------
// Main echo test.
// ---------------------------------------------------------------------------

#[test]
fn tcp_connect_client_round_trips_against_local_server() {
    let server = TcpListener::bind("127.0.0.1:0").expect("bind local server");
    let server_addr = server.local_addr().expect("server local addr");
    let server_thread = thread::spawn(move || {
        let (mut stream, _) = server.accept().expect("server accepts tina client");
        let mut request = [0_u8; 4];
        stream
            .read_exact(&mut request)
            .expect("server reads request");
        assert_eq!(&request, b"ping");
        stream.write_all(b"pong").expect("server writes reply");
    });

    let observed: ClientObserved = Arc::new(Mutex::new(None));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let client = runtime.register(
        OutboundClient {
            payload: b"ping".to_vec(),
            pending_write: Vec::new(),
            stream: None,
            observed: Arc::clone(&observed),
        },
        TestMailbox::new(8),
    );

    runtime
        .try_send(client, OutboundClientEvent::Start(server_addr))
        .expect("client bootstrap accepts");

    step_until(
        &mut runtime,
        Duration::from_secs(5),
        "tcp connect client",
        |runtime| {
            observed.lock().expect("observed mutex").is_some()
                && !runtime.has_in_flight_calls()
                && runtime.trace().iter().any(|event| {
                    event.isolate() == client.isolate()
                        && matches!(event.kind(), RuntimeEventKind::IsolateStopped)
                })
        },
    );

    server_thread.join().expect("server thread joins");
    assert_eq!(
        observed.lock().expect("observed mutex").as_deref(),
        Some(&b"pong"[..])
    );
    assert_eq!(
        count_call_completed(runtime.trace(), CallKind::TcpConnect),
        1
    );
    assert_eq!(count_call_completed(runtime.trace(), CallKind::TcpWrite), 1);
    assert_eq!(count_call_completed(runtime.trace(), CallKind::TcpRead), 1);
    assert_eq!(
        count_call_completed(runtime.trace(), CallKind::TcpStreamClose),
        1
    );
}

#[test]
fn file_client_writes_syncs_reads_and_closes() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "tina-runtime-file-client-{}-{unique}",
        std::process::id()
    ));
    let path = dir.join("snapshot.txt");
    let payload = b"llama config snapshot\n".to_vec();
    let observed: FileObserved = Arc::new(Mutex::new(None));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let client = runtime.register(
        FileClient {
            payload: payload.clone(),
            file: None,
            size: None,
            observed: Arc::clone(&observed),
        },
        TestMailbox::new(8),
    );

    runtime
        .try_send(
            client,
            FileClientEvent::Start {
                dir: dir.clone(),
                path: path.clone(),
            },
        )
        .expect("file client bootstrap accepts");

    step_until(
        &mut runtime,
        Duration::from_secs(5),
        "file client",
        |runtime| {
            observed.lock().expect("file observed mutex").is_some()
                && !runtime.has_in_flight_calls()
                && runtime.trace().iter().any(|event| {
                    event.isolate() == client.isolate()
                        && matches!(event.kind(), RuntimeEventKind::IsolateStopped)
                })
        },
    );

    assert_eq!(
        *observed.lock().expect("file observed mutex"),
        Some((payload.len() as u64, payload.clone()))
    );
    assert_eq!(fs::read(&path).expect("read written file"), payload);
    assert_eq!(count_call_completed(runtime.trace(), CallKind::Mkdir), 1);
    assert_eq!(count_call_completed(runtime.trace(), CallKind::FileOpen), 1);
    assert_eq!(
        count_call_completed(runtime.trace(), CallKind::FileWriteAt),
        1
    );
    assert_eq!(
        count_call_completed(runtime.trace(), CallKind::FileFsync),
        1
    );
    assert_eq!(count_call_completed(runtime.trace(), CallKind::FileSize), 1);
    assert_eq!(
        count_call_completed(runtime.trace(), CallKind::FileReadAt),
        1
    );
    assert_eq!(
        count_call_completed(runtime.trace(), CallKind::FileClose),
        1
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn tcp_echo_round_trips_one_client_payload() {
    let payload = b"hello from a tina-rs echo client".to_vec();
    let (echoed, trace, listener_isolate) =
        run_echo_scenario(vec![payload.clone()], false, Duration::from_secs(10));

    assert_eq!(echoed, vec![payload]);
    assert_eq!(count_call_completed(&trace, CallKind::TcpBind), 1);
    assert_eq!(count_call_completed(&trace, CallKind::TcpAccept), 1);
    assert!(count_call_completed(&trace, CallKind::TcpRead) >= 2);
    assert_eq!(count_call_completed(&trace, CallKind::TcpWrite), 1);
    assert_eq!(count_call_completed(&trace, CallKind::TcpStreamClose), 1);
    assert_eq!(count_call_completed(&trace, CallKind::TcpListenerClose), 1);
    assert_eq!(count_spawned(&trace), 1);
    assert_eq!(
        count_handler_finished(&trace, tina_runtime::EffectKind::Batch),
        1
    );
    assert_eq!(
        trace
            .iter()
            .filter(|event| {
                event.isolate() == listener_isolate
                    && matches!(event.kind(), RuntimeEventKind::IsolateStopped)
            })
            .count(),
        1
    );
}

#[test]
fn tcp_echo_serves_multiple_sequential_clients_without_rebinding_listener() {
    let payloads = vec![
        b"first sequential payload".to_vec(),
        b"second sequential payload".to_vec(),
        b"third sequential payload".to_vec(),
    ];
    let (echoed, trace, listener_isolate) =
        run_echo_scenario(payloads.clone(), false, Duration::from_secs(10));

    assert_eq!(echoed, payloads);
    assert_eq!(count_call_completed(&trace, CallKind::TcpBind), 1);
    assert_eq!(count_call_completed(&trace, CallKind::TcpAccept), 3);
    assert_eq!(count_call_completed(&trace, CallKind::TcpStreamClose), 3);
    assert_eq!(count_call_completed(&trace, CallKind::TcpListenerClose), 1);
    assert_eq!(count_spawned(&trace), 3);
    assert_eq!(
        count_handler_finished(&trace, tina_runtime::EffectKind::Batch),
        3
    );
    assert_eq!(
        trace
            .iter()
            .filter(|event| {
                event.isolate() == listener_isolate
                    && matches!(event.kind(), RuntimeEventKind::IsolateStopped)
            })
            .count(),
        1
    );
}

#[test]
fn tcp_echo_handles_two_overlapping_clients_and_rearms_listener() {
    let payloads = vec![
        b"first overlapping payload".to_vec(),
        b"second overlapping payload".to_vec(),
    ];
    let (mut echoed, trace, _) = run_echo_scenario(payloads.clone(), true, Duration::from_secs(10));

    echoed.sort();
    let mut expected = payloads;
    expected.sort();
    assert_eq!(echoed, expected);
    assert_eq!(count_call_completed(&trace, CallKind::TcpBind), 1);
    assert_eq!(count_call_completed(&trace, CallKind::TcpAccept), 2);
    assert_eq!(count_call_completed(&trace, CallKind::TcpStreamClose), 2);
    assert_eq!(count_call_completed(&trace, CallKind::TcpListenerClose), 1);
    assert_eq!(count_spawned(&trace), 2);
    assert_eq!(
        count_handler_finished(&trace, tina_runtime::EffectKind::Batch),
        2
    );
}

#[test]
fn threaded_runtime_tcp_echo_round_trips_reference_workload() {
    let payloads = vec![
        b"Betelgeuse reference payload one".to_vec(),
        b"Betelgeuse reference payload two".to_vec(),
    ];
    let (echoed, trace, listener_isolate) =
        run_betelgeuse_echo_scenario(payloads.clone(), Duration::from_secs(10));
    let (oracle_echoed, oracle_trace, _) =
        run_echo_scenario(payloads.clone(), false, Duration::from_secs(10));

    assert_eq!(echoed, payloads);
    assert_eq!(echoed, oracle_echoed);
    assert_eq!(
        tcp_semantic_counts(&trace),
        tcp_semantic_counts(&oracle_trace)
    );
    assert_eq!(count_spawned(&trace), count_spawned(&oracle_trace));
    assert_eq!(count_call_completed(&trace, CallKind::TcpBind), 1);
    assert_eq!(count_call_completed(&trace, CallKind::TcpAccept), 2);
    assert!(count_call_completed(&trace, CallKind::TcpRead) >= 4);
    assert_eq!(count_call_completed(&trace, CallKind::TcpWrite), 2);
    assert_eq!(count_call_completed(&trace, CallKind::TcpStreamClose), 2);
    assert_eq!(count_call_completed(&trace, CallKind::TcpListenerClose), 1);
    assert_eq!(count_spawned(&trace), 2);
    assert_eq!(
        trace
            .iter()
            .filter(|event| {
                event.isolate() == listener_isolate
                    && matches!(event.kind(), RuntimeEventKind::IsolateStopped)
            })
            .count(),
        1
    );
}

#[test]
fn betelgeuse_simulated_io_drives_tina_tcp_echo_with_faulted_completion_shape() {
    let payload = b"simulated Betelgeuse payload".to_vec();
    let (echoed, trace) = run_simulated_betelgeuse_echo_scenario(payload.clone());
    let (second_echoed, second_trace) = run_simulated_betelgeuse_echo_scenario(payload.clone());

    assert_eq!(echoed, payload);
    assert_eq!(second_echoed, payload);
    assert_eq!(trace, second_trace);
    assert_eq!(count_call_completed(&trace, CallKind::TcpBind), 1);
    assert_eq!(count_call_completed(&trace, CallKind::TcpAccept), 1);
    assert!(count_call_completed(&trace, CallKind::TcpRead) >= 2);
    assert!(count_call_completed(&trace, CallKind::TcpWrite) > 1);
    assert_eq!(count_call_completed(&trace, CallKind::TcpStreamClose), 1);
    assert_eq!(count_call_completed(&trace, CallKind::TcpListenerClose), 1);
    assert_eq!(count_spawned(&trace), 1);
}

#[test]
fn threaded_runtime_can_run_over_simulated_io_backend() {
    let payload = b"threaded simulated Betelgeuse payload".to_vec();
    let (echoed, trace) = run_threaded_simulated_betelgeuse_echo_scenario(payload.clone());

    assert_eq!(echoed, payload);
    assert_eq!(count_call_completed(&trace, CallKind::TcpBind), 1);
    assert_eq!(count_call_completed(&trace, CallKind::TcpAccept), 1);
    assert!(count_call_completed(&trace, CallKind::TcpRead) >= 2);
    assert!(count_call_completed(&trace, CallKind::TcpWrite) > 1);
    assert_eq!(count_call_completed(&trace, CallKind::TcpStreamClose), 1);
    assert_eq!(count_call_completed(&trace, CallKind::TcpListenerClose), 1);
    assert_eq!(count_spawned(&trace), 1);
}

#[test]
fn threaded_runtime_surfaces_closed_mailbox_after_stop() {
    let runtime = ThreadedRuntime::new(TestShard, TestMailboxFactory);
    let listener_addr = runtime
        .register_with_capacity::<Listener, _>(
            Listener {
                bind_addr: "127.0.0.1:0".parse().expect("loopback parse"),
                bound_addr_slot: Arc::new(Mutex::new(None)),
                max_chunk: 256,
                target_accepts: 0,
                accepted_count: 0,
                listener: None,
            },
            1,
        )
        .expect("Betelgeuse register accepts");

    runtime
        .try_send(listener_addr, ListenerEvent::IoFailed)
        .expect("stop message accepts");

    wait_until(Duration::from_secs(2), "Betelgeuse stop", || {
        let trace = runtime.complete_trace().expect("Betelgeuse trace");
        trace.iter().any(|event| {
            event.isolate() == listener_addr.isolate()
                && matches!(event.kind(), RuntimeEventKind::IsolateStopped)
        })
    });

    assert_eq!(
        runtime.send_and_observe(listener_addr, ListenerEvent::Start),
        Err(ThreadedSendObservedError::MailboxClosed)
    );
}

#[test]
fn threaded_runtime_shutdown_rejects_outstanding_tcp_accept_completion() {
    let runtime = ThreadedRuntime::new(TestShard, TestMailboxFactory);
    let listener_addr = runtime
        .register_with_capacity::<Listener, _>(
            Listener {
                bind_addr: "127.0.0.1:0".parse().expect("loopback parse"),
                bound_addr_slot: Arc::new(Mutex::new(None)),
                max_chunk: 256,
                target_accepts: 1,
                accepted_count: 0,
                listener: None,
            },
            8,
        )
        .expect("Betelgeuse register accepts");

    runtime
        .try_send(listener_addr, ListenerEvent::Start)
        .expect("listener start handoff accepted");

    wait_until(Duration::from_secs(2), "Betelgeuse accept pending", || {
        let trace = runtime.complete_trace().expect("Betelgeuse trace");
        trace.iter().any(|event| {
            event.isolate() == listener_addr.isolate()
                && matches!(
                    event.kind(),
                    RuntimeEventKind::CallDispatchAttempted {
                        call_kind: CallKind::TcpAccept,
                        ..
                    }
                )
        }) && runtime
            .has_in_flight_calls()
            .expect("in-flight query succeeds")
    });

    let trace = runtime.shutdown().expect("Betelgeuse shutdown");
    assert!(trace.iter().any(|event| {
        event.isolate() == listener_addr.isolate()
            && matches!(
                event.kind(),
                RuntimeEventKind::CallCompletionRejected {
                    call_kind: CallKind::TcpAccept,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                    ..
                }
            )
    }));
}

#[derive(Debug, Clone)]
enum BlockingMsg {
    Park,
    Wake,
}

#[derive(Debug)]
struct BlockingIsolate {
    parked_tx: Option<mpsc::SyncSender<()>>,
    wake_rx: mpsc::Receiver<()>,
}

impl Isolate for BlockingIsolate {
    tina::isolate_types! {
        message: BlockingMsg,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<BlockingMsg>,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            BlockingMsg::Park => {
                if let Some(parked_tx) = self.parked_tx.take() {
                    parked_tx.send(()).expect("test observes parked handler");
                }
                self.wake_rx.recv().expect("test releases parked handler");
                noop()
            }
            BlockingMsg::Wake => noop(),
        }
    }
}

#[test]
fn threaded_runtime_try_send_surfaces_ingress_full_without_blocking_on_worker() {
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        TestMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 1,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let (parked_tx, parked_rx) = mpsc::sync_channel(0);
    let (wake_tx, wake_rx) = mpsc::sync_channel(0);
    let blocking_addr = runtime
        .register_with_capacity::<BlockingIsolate, _>(
            BlockingIsolate {
                parked_tx: Some(parked_tx),
                wake_rx,
            },
            8,
        )
        .expect("Betelgeuse register accepts");

    runtime
        .try_send(blocking_addr, BlockingMsg::Park)
        .expect("park handoff accepted");
    parked_rx.recv().expect("worker reached parked handler");

    runtime
        .try_send(blocking_addr, BlockingMsg::Wake)
        .expect("first queued wake handoff accepted");
    assert_eq!(
        runtime.try_send(blocking_addr, BlockingMsg::Wake),
        Err(ThreadedTrySendError::IngressFull)
    );

    wake_tx.send(()).expect("release parked handler");
    wait_until(Duration::from_secs(2), "Betelgeuse ingress drain", || {
        let trace = runtime.complete_trace().expect("Betelgeuse trace");
        trace
            .iter()
            .filter(|event| event.isolate() == blocking_addr.isolate())
            .filter(|event| matches!(event.kind(), RuntimeEventKind::HandlerStarted))
            .count()
            >= 2
    });
}

#[test]
fn connection_retries_partial_write_before_reading_again() {
    let mut connection = Connection {
        stream: StreamId::new(7),
        max_chunk: 256,
        pending_write: Vec::new(),
    };
    let mut shard = TestShard;
    let mut ctx = Context::new(&mut shard, IsolateId::new(42));

    let effect = connection.handle(ConnectionEvent::Read(b"hello".to_vec()), &mut ctx);
    let Effect::Call(first_write) = effect else {
        panic!("expected first write call");
    };
    assert!(matches!(
        first_write.request(),
        CallInput::TcpWrite { bytes, .. } if bytes == b"hello"
    ));

    let effect = connection.handle(ConnectionEvent::Wrote(2), &mut ctx);
    let Effect::Call(second_write) = effect else {
        panic!("expected retry write call");
    };
    assert!(matches!(
        second_write.request(),
        CallInput::TcpWrite { bytes, .. } if bytes == b"llo"
    ));

    let effect = connection.handle(ConnectionEvent::Wrote(3), &mut ctx);
    let Effect::Call(next_read) = effect else {
        panic!("expected next read call");
    };
    assert!(matches!(next_read.request(), CallInput::TcpRead { .. }));
}
