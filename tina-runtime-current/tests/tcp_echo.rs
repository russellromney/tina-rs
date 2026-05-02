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

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::rc::Rc;
use std::sync::{Arc, Barrier, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tina::{IsolateId, Mailbox, RestartBudget, RestartPolicy, TrySendError, prelude::*};
use tina_runtime::{
    CallInput, CallKind, ListenerId, MailboxFactory, Runtime, RuntimeCall, RuntimeEvent,
    RuntimeEventKind, StreamId, ThreadedRuntime, ThreadedRuntimeConfig, ThreadedSendObservedError,
    ThreadedTrySendError, tcp_accept, tcp_bind, tcp_close_listener, tcp_close_stream, tcp_read,
    tcp_write,
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

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
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

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
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

fn run_threaded_echo_scenario(
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
        .expect("threaded register accepts");

    runtime
        .supervise(
            listener_addr,
            SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(4)),
        )
        .expect("threaded supervise accepts");

    runtime
        .try_send(listener_addr, ListenerEvent::Start)
        .expect("threaded bootstrap accepts");

    wait_until(timeout, "threaded bind", || {
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

    wait_until(timeout, "threaded clients", || {
        clients
            .iter()
            .all(|client| *client.done.lock().expect("flag mutex"))
    });

    wait_until(timeout, "threaded listener stop", || {
        let trace = runtime.trace().expect("threaded trace");
        trace.iter().any(|event| {
            event.isolate() == listener_addr.isolate()
                && matches!(event.kind(), RuntimeEventKind::IsolateStopped)
        }) && !runtime
            .has_in_flight_calls()
            .expect("threaded in-flight check")
    });

    let mut echoed = Vec::new();
    for client in clients {
        client.handle.join().expect("client thread");
        echoed.push(client.echoed.lock().expect("mutex").clone());
    }

    let trace = runtime.shutdown().expect("threaded shutdown");
    (echoed, trace, listener_addr.isolate())
}

// ---------------------------------------------------------------------------
// Main echo test.
// ---------------------------------------------------------------------------

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
        b"threaded reference payload one".to_vec(),
        b"threaded reference payload two".to_vec(),
    ];
    let (echoed, trace, listener_isolate) =
        run_threaded_echo_scenario(payloads.clone(), Duration::from_secs(10));
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
        .expect("threaded register accepts");

    runtime
        .try_send(listener_addr, ListenerEvent::IoFailed)
        .expect("stop message accepts");

    wait_until(Duration::from_secs(2), "threaded stop", || {
        let trace = runtime.trace().expect("threaded trace");
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

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
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
        .expect("threaded register accepts");

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
    wait_until(Duration::from_secs(2), "threaded ingress drain", || {
        let trace = runtime.trace().expect("threaded trace");
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
