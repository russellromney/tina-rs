//! Live TCP echo proof for `CurrentRuntime` plus the runtime-owned call
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
//! - bind to a hardcoded high loopback port. The runtime does not
//!   support ephemeral-port discovery on this Betelgeuse rev (see
//!   `io_backend.rs` "Honest scope"), so a port-0 bind would yield a
//!   dishonest `local_addr`. Tests pick a concrete port instead. If the
//!   port is already in use on the host, this test fails loudly — that
//!   is the honest signal.
//! - assertions cover both observed network behavior (echoed bytes) and
//!   trace evidence (each call kind on the path appears with a
//!   `CallCompleted` event)
//! - one client connection only; multi-connection workloads would need
//!   an ingress-driven re-arm and are out of scope for the first echo
//!   proof per the package plan

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tina::{
    Context, Effect, Isolate, IsolateId, Mailbox, RestartBudget, RestartPolicy,
    RestartableSpawnSpec, SendMessage, Shard, ShardId, TrySendError,
};
use tina_runtime_current::{
    CallKind, CallRequest, CallResult, CurrentCall, CurrentRuntime, ListenerId, MailboxFactory,
    RuntimeEvent, RuntimeEventKind, StreamId,
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
    type Send = SendMessage<Infallible>;
    type Spawn = Infallible;
    type Call = CurrentCall<ConnectionMsg>;
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
    Effect::Call(CurrentCall::new(
        CallRequest::TcpRead { stream, max_len },
        |result| match result {
            CallResult::TcpRead { bytes } => ConnectionMsg::ReadCompleted(bytes),
            CallResult::Failed(_) => ConnectionMsg::Failed,
            other => panic!("unexpected read result {other:?}"),
        },
    ))
}

fn write_call(stream: StreamId, bytes: Vec<u8>) -> Effect<Connection> {
    Effect::Call(CurrentCall::new(
        CallRequest::TcpWrite { stream, bytes },
        |result| match result {
            CallResult::TcpWrote { count } => ConnectionMsg::WriteCompleted { count },
            CallResult::Failed(_) => ConnectionMsg::Failed,
            other => panic!("unexpected write result {other:?}"),
        },
    ))
}

fn close_call(stream: StreamId) -> Effect<Connection> {
    Effect::Call(CurrentCall::new(
        CallRequest::TcpStreamClose { stream },
        |result| match result {
            CallResult::TcpStreamClosed => ConnectionMsg::StreamClosed,
            CallResult::Failed(_) => ConnectionMsg::Failed,
            other => panic!("unexpected close result {other:?}"),
        },
    ))
}

// ---------------------------------------------------------------------------
// Listener isolate. Bootstrap → TcpBind → publish bound addr → TcpAccept →
// spawn restartable connection child. The supervised-parent shape mirrors
// slice 011's reference workload.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum ListenerMsg {
    Bootstrap,
    Bound {
        listener: ListenerId,
        addr: SocketAddr,
    },
    Accepted {
        stream: StreamId,
    },
    Failed,
}

#[derive(Debug)]
struct Listener {
    bind_addr: SocketAddr,
    bound_addr_slot: BoundAddr,
    max_chunk: usize,
}

impl Isolate for Listener {
    type Message = ListenerMsg;
    type Reply = ();
    type Send = SendMessage<Infallible>;
    type Spawn = RestartableSpawnSpec<Connection>;
    type Call = CurrentCall<ListenerMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ListenerMsg::Bootstrap => {
                let addr = self.bind_addr;
                Effect::Call(CurrentCall::new(
                    CallRequest::TcpBind { addr },
                    move |result| match result {
                        CallResult::TcpBound {
                            listener,
                            local_addr,
                        } => ListenerMsg::Bound {
                            listener,
                            addr: local_addr,
                        },
                        CallResult::Failed(_) => ListenerMsg::Failed,
                        other => panic!("unexpected bind result {other:?}"),
                    },
                ))
            }
            ListenerMsg::Bound { listener, addr } => {
                *self
                    .bound_addr_slot
                    .lock()
                    .expect("bound addr mutex never poisoned") = Some(addr);
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
                let max_chunk = self.max_chunk;
                Effect::Spawn(
                    RestartableSpawnSpec::new(
                        move || Connection {
                            stream,
                            max_chunk,
                            pending_write: Vec::new(),
                        },
                        8,
                    )
                    .with_bootstrap(|| ConnectionMsg::Start),
                )
            }
            ListenerMsg::Failed => Effect::Stop,
        }
    }
}

// ---------------------------------------------------------------------------
// Drive helpers.
// ---------------------------------------------------------------------------

fn step_until<F>(
    runtime: &mut CurrentRuntime<TestShard, TestMailboxFactory>,
    timeout: Duration,
    label: &str,
    predicate: F,
) where
    F: Fn(&CurrentRuntime<TestShard, TestMailboxFactory>) -> bool,
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

fn pump_runtime(
    runtime: &mut CurrentRuntime<TestShard, TestMailboxFactory>,
    flag: &Arc<Mutex<bool>>,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    loop {
        runtime.step();
        if *flag.lock().expect("flag mutex never poisoned") {
            return;
        }
        if Instant::now() > deadline {
            panic!(
                "pump_runtime timed out waiting for client thread; trace = {:#?}",
                runtime.trace()
            );
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn assert_call_path_completed(trace: &[RuntimeEvent], kind: CallKind) {
    let dispatched = trace.iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallDispatchAttempted { call_kind, .. } if call_kind == kind
        )
    });
    assert!(
        dispatched,
        "expected CallDispatchAttempted({kind:?}) in trace"
    );
    let completed = trace.iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCompleted { call_kind, .. } if call_kind == kind
        )
    });
    assert!(completed, "expected CallCompleted({kind:?}) in trace");
}

// ---------------------------------------------------------------------------
// Main echo test.
// ---------------------------------------------------------------------------

/// Hardcoded high loopback port for this integration test. Picked from
/// the registered range and unlikely to collide with normal services on
/// a developer machine. If it is already in use on the host the test
/// fails loudly through the runtime's `TcpBind` failure path, which is
/// the honest signal — the runtime cannot do ephemeral-port discovery
/// honestly without an upstream `IOSocket::local_addr` (see
/// `io_backend.rs`).
const ECHO_TEST_PORT: u16 = 48721;

#[test]
fn tcp_echo_round_trips_one_client_payload() {
    let bind_addr: SocketAddr = format!("127.0.0.1:{ECHO_TEST_PORT}")
        .parse()
        .expect("loopback parse");
    let bound: BoundAddr = Arc::new(Mutex::new(None));

    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);

    let listener_addr = runtime.register(
        Listener {
            bind_addr,
            bound_addr_slot: Arc::clone(&bound),
            max_chunk: 256,
        },
        TestMailbox::new(8),
    );

    runtime.supervise(
        listener_addr,
        SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(4)),
    );

    runtime
        .try_send(listener_addr, ListenerMsg::Bootstrap)
        .expect("bootstrap accepts");

    // Drive the runtime until the listener has published the bound addr.
    step_until(&mut runtime, Duration::from_secs(2), "bind", |_| {
        bound.lock().expect("mutex").is_some()
    });

    let local_addr = bound
        .lock()
        .expect("mutex")
        .expect("listener published address");

    // Client thread: connect, write, half-close, read echoed bytes.
    let payload: Vec<u8> = b"hello from a tina-rs echo client".to_vec();
    let payload_for_client = payload.clone();
    let done = Arc::new(Mutex::new(false));
    let echoed: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let echoed_for_client = Arc::clone(&echoed);
    let done_for_client = Arc::clone(&done);

    let client: JoinHandle<()> = thread::spawn(move || {
        let mut stream = TcpStream::connect(local_addr).expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(3))).ok();
        stream.write_all(&payload_for_client).expect("write");
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

    // Pump the runtime until the client thread reports completion.
    pump_runtime(&mut runtime, &done, Duration::from_secs(5));

    // Pump any stragglers so the trace is settled before we read it.
    let drain_deadline = Instant::now() + Duration::from_millis(500);
    while runtime.has_in_flight_calls() {
        runtime.step();
        if Instant::now() > drain_deadline {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }

    client.join().expect("client thread");

    let received = echoed.lock().expect("mutex").clone();
    assert_eq!(received, payload, "echoed bytes must match");

    let trace = runtime.trace();
    assert_call_path_completed(trace, CallKind::TcpBind);
    assert_call_path_completed(trace, CallKind::TcpAccept);
    assert_call_path_completed(trace, CallKind::TcpRead);
    assert_call_path_completed(trace, CallKind::TcpWrite);
    assert_call_path_completed(trace, CallKind::TcpStreamClose);
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

    let effect = connection.handle(ConnectionMsg::ReadCompleted(b"hello".to_vec()), &mut ctx);
    let Effect::Call(first_write) = effect else {
        panic!("expected first write call");
    };
    assert!(matches!(
        first_write.request(),
        CallRequest::TcpWrite { bytes, .. } if bytes == b"hello"
    ));

    let effect = connection.handle(ConnectionMsg::WriteCompleted { count: 2 }, &mut ctx);
    let Effect::Call(second_write) = effect else {
        panic!("expected retry write call");
    };
    assert!(matches!(
        second_write.request(),
        CallRequest::TcpWrite { bytes, .. } if bytes == b"llo"
    ));

    let effect = connection.handle(ConnectionMsg::WriteCompleted { count: 3 }, &mut ctx);
    let Effect::Call(next_read) = effect else {
        panic!("expected next read call");
    };
    assert!(matches!(next_read.request(), CallRequest::TcpRead { .. }));
}
