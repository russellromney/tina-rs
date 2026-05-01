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
//! cargo run -p tina-runtime --example tcp_echo
//! ```
//!
//! Binds to `127.0.0.1:0` and relies on the runtime to report the actual
//! bound address through `CallOutput::TcpBound { local_addr }`, then
//! accepts exactly three client connections, closes the listener
//! cleanly, and exits.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tina::{Mailbox, RestartBudget, RestartPolicy, TrySendError, prelude::*};
use tina_runtime::{
    CallKind, ListenerId, MailboxFactory, Runtime, RuntimeCall, RuntimeEvent, RuntimeEventKind,
    StreamId, tcp_accept, tcp_bind, tcp_close_listener, tcp_close_stream, tcp_read, tcp_write,
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
        shard: ExampleShard,
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
        shard: ExampleShard,
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

fn step_until<F>(
    runtime: &mut Runtime<ExampleShard, ExampleMailboxFactory>,
    timeout: Duration,
    label: &str,
    predicate: F,
) where
    F: Fn(&Runtime<ExampleShard, ExampleMailboxFactory>) -> bool,
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

struct ClientRun {
    done: Arc<Mutex<bool>>,
    echoed: Arc<Mutex<Vec<u8>>>,
    handle: JoinHandle<()>,
}

fn spawn_client(local_addr: SocketAddr, payload: Vec<u8>) -> ClientRun {
    let done = Arc::new(Mutex::new(false));
    let echoed: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let done_for_client = Arc::clone(&done);
    let echoed_for_client = Arc::clone(&echoed);

    let handle = thread::spawn(move || {
        let mut stream = TcpStream::connect(local_addr).expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(3))).ok();
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

fn main() {
    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("loopback parse");
    let bound: BoundAddr = Arc::new(Mutex::new(None));

    let mut runtime = Runtime::new(ExampleShard, ExampleMailboxFactory);

    let payloads = vec![
        b"hello from the tina-rs echo example".to_vec(),
        b"listener re-armed for a second client".to_vec(),
        b"third client proves graceful shutdown".to_vec(),
    ];

    let listener_addr = runtime.register(
        Listener {
            bind_addr,
            bound_addr_slot: Arc::clone(&bound),
            max_chunk: 256,
            target_accepts: payloads.len(),
            accepted_count: 0,
            listener: None,
        },
        ExampleMailbox::new(8),
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
    println!("listening on {local_addr}");

    let mut clients = Vec::new();
    for payload in payloads.clone() {
        let client = spawn_client(local_addr, payload);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !*client.done.lock().expect("mutex") {
            runtime.step();
            if Instant::now() > deadline {
                panic!("example timed out waiting for client completion");
            }
            thread::sleep(Duration::from_millis(2));
        }
        clients.push(client);
    }

    step_until(
        &mut runtime,
        Duration::from_secs(5),
        "listener stop",
        |runtime| {
            !runtime.has_in_flight_calls()
                && runtime.trace().iter().any(|event| {
                    event.isolate() == listener_addr.isolate()
                        && matches!(event.kind(), RuntimeEventKind::IsolateStopped)
                })
        },
    );

    let mut echoed = Vec::new();
    for client in clients {
        client.handle.join().expect("client thread");
        echoed.push(client.echoed.lock().expect("mutex").clone());
    }

    assert_eq!(echoed, payloads, "echoed bytes must match sent payloads");

    let trace = runtime.trace();
    assert_eq!(count_call_completed(trace, CallKind::TcpBind), 1);
    assert_eq!(count_call_completed(trace, CallKind::TcpAccept), 3);
    assert!(count_call_completed(trace, CallKind::TcpRead) >= 6);
    assert_eq!(count_call_completed(trace, CallKind::TcpWrite), 3);
    assert_eq!(count_call_completed(trace, CallKind::TcpStreamClose), 3);
    assert_eq!(count_call_completed(trace, CallKind::TcpListenerClose), 1);
    assert_eq!(count_spawned(trace), 3);
    assert_eq!(
        count_handler_finished(trace, tina_runtime::EffectKind::Batch),
        3
    );
    assert_eq!(
        trace
            .iter()
            .filter(|event| {
                event.isolate() == listener_addr.isolate()
                    && matches!(event.kind(), RuntimeEventKind::IsolateStopped)
            })
            .count(),
        1
    );

    println!(
        "echoed {} payloads through the runtime call contract; trace had {} events",
        echoed.len(),
        trace.len()
    );
}
