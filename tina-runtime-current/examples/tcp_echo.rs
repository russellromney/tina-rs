//! Runnable TCP echo demonstration.
//!
//! Mirrors the workload shape of `tests/tcp_echo.rs`: a listener isolate
//! is the supervised parent, the accepted connection becomes a
//! restartable child handler isolate, and the new runtime-owned call
//! contract drives bind/accept/read/write/close. The example doubles as
//! a smoke test by asserting on its own outcomes — per the package's
//! "do not let the echo example become the only proof" guardrail, the
//! semantic surface still lives in the integration test and the
//! focused call-dispatch tests.
//!
//! Run with:
//! ```bash
//! cargo run -p tina-runtime-current --example tcp_echo
//! ```
//!
//! Binds to a hardcoded high loopback port (the runtime does not
//! perform ephemeral-port discovery on this Betelgeuse rev — see
//! `io_backend.rs` "Honest scope") and drives one client connection
//! through the live runtime in the same process. If the port is already
//! in use on the host the example fails loudly through the runtime's
//! `TcpBind` failure path.

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
    Context, Effect, Isolate, Mailbox, RestartBudget, RestartPolicy, RestartableSpawnSpec,
    SendMessage, Shard, ShardId, TrySendError,
};
use tina_runtime_current::{
    CallKind, CallRequest, CallResult, CurrentCall, CurrentRuntime, ListenerId, MailboxFactory,
    RuntimeEvent, RuntimeEventKind, StreamId,
};
use tina_supervisor::SupervisorConfig;

#[derive(Debug, Default)]
struct ExampleShard;

impl Shard for ExampleShard {
    fn id(&self) -> ShardId {
        ShardId::new(1)
    }
}

struct ExampleMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    closed: Rc<Cell<bool>>,
}

impl<T> ExampleMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Rc::new(RefCell::new(VecDeque::new())),
            closed: Rc::new(Cell::new(false)),
        }
    }
}

impl<T> Mailbox<T> for ExampleMailbox<T> {
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
struct ExampleMailboxFactory;

impl MailboxFactory for ExampleMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(ExampleMailbox::new(capacity))
    }
}

type BoundAddr = Arc<Mutex<Option<SocketAddr>>>;

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
    type Shard = ExampleShard;

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
    type Shard = ExampleShard;

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

fn step_until<F>(
    runtime: &mut CurrentRuntime<ExampleShard, ExampleMailboxFactory>,
    timeout: Duration,
    label: &str,
    predicate: F,
) where
    F: Fn(&CurrentRuntime<ExampleShard, ExampleMailboxFactory>) -> bool,
{
    let deadline = Instant::now() + timeout;
    while !predicate(runtime) {
        if Instant::now() > deadline {
            panic!("step_until({label}): predicate not satisfied within timeout");
        }
        runtime.step();
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
    assert!(dispatched, "expected dispatch for {kind:?}");
    let completed = trace.iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCompleted { call_kind, .. } if call_kind == kind
        )
    });
    assert!(completed, "expected completion for {kind:?}");
}

/// Hardcoded high loopback port for this example. Distinct from the
/// integration test port (48721) so the two can be run side by side
/// without colliding. If this port is already in use on the host, the
/// example fails loudly through the runtime's `TcpBind` failure path.
const ECHO_EXAMPLE_PORT: u16 = 48722;

fn main() {
    let bind_addr: SocketAddr = format!("127.0.0.1:{ECHO_EXAMPLE_PORT}")
        .parse()
        .expect("loopback parse");
    let bound: BoundAddr = Arc::new(Mutex::new(None));

    let mut runtime = CurrentRuntime::new(ExampleShard, ExampleMailboxFactory);

    let listener_addr = runtime.register(
        Listener {
            bind_addr,
            bound_addr_slot: Arc::clone(&bound),
            max_chunk: 256,
        },
        ExampleMailbox::new(8),
    );

    runtime.supervise(
        listener_addr,
        SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(4)),
    );

    runtime
        .try_send(listener_addr, ListenerMsg::Bootstrap)
        .expect("bootstrap accepts");

    step_until(&mut runtime, Duration::from_secs(2), "bind", |_| {
        bound.lock().expect("mutex").is_some()
    });
    let local_addr = bound
        .lock()
        .expect("mutex")
        .expect("listener published address");
    println!("listening on {local_addr}");

    let payload: Vec<u8> = b"hello from the tina-rs echo example".to_vec();
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

    let pump_deadline = Instant::now() + Duration::from_secs(5);
    while !*done.lock().expect("mutex") {
        runtime.step();
        if Instant::now() > pump_deadline {
            panic!("example timed out waiting for client");
        }
        thread::sleep(Duration::from_millis(2));
    }

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

    println!(
        "echoed {} bytes through the runtime call contract; trace had {} events",
        received.len(),
        trace.len()
    );
}
