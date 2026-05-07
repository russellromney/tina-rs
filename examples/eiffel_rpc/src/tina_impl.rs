//! Tina framed RPC, typed server via the `#[service]` macro.
//!
//! `Connection::tiny_pressure()` enforces `max_in_flight = 1` per
//! connection. The first request grabs the slot, gets a `Reply`.
//! The next N-1 arrive while that one is in flight and come back as
//! wire `Error(Full)` — overload becomes a frame, not a stuck queue.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tina::{Mailbox, TrySendError, prelude::*};
use tina_rpc::{
    Connection, ConnectionConfig, ConnectionInit, ConnectionMsg, Dispatch, Frame, FrameError,
    FrameKind, FrameLimits, Json, LENGTH_PREFIX_SIZE, PayloadLimits, Registry, RegistryMsg,
    RouterReply, SingleService, decode_body, encode, parse_length_prefix, service,
};
use tina_runtime::{
    CallError, ListenerId, MailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig, tcp_accept,
    tcp_bind, tcp_close_listener,
};

use crate::{Report, RunConfig};

#[derive(Debug, Default)]
struct EiffelShard;

impl Shard for EiffelShard {
    fn id(&self) -> ShardId {
        ShardId::new(73)
    }
}

// -------------------------------------------------------------------
// Typed echo service. The `#[service]` macro emits a dispatcher and
// JSON encode/decode for each method — the server-side service body
// is just the user's `impl`.
// -------------------------------------------------------------------

#[service]
trait Echo {
    fn ping(&mut self, payload: Vec<u8>) -> Vec<u8>;
}

struct EchoState;

impl Echo for EchoState {
    fn ping(&mut self, payload: Vec<u8>) -> Vec<u8> {
        payload
    }
}

// -------------------------------------------------------------------
// Mailbox + factory boilerplate (shared with other eiffel examples).
// -------------------------------------------------------------------

struct EMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    closed: Rc<Cell<bool>>,
}

impl<T> EMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Rc::new(RefCell::new(VecDeque::new())),
            closed: Rc::new(Cell::new(false)),
        }
    }
}

impl<T> Mailbox<T> for EMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }
    fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        if self.closed.get() {
            return Err(TrySendError::Closed(msg));
        }
        let mut q = self.queue.borrow_mut();
        if q.len() >= self.capacity {
            return Err(TrySendError::Full(msg));
        }
        q.push_back(msg);
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
struct EFactory;

impl MailboxFactory for EFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(EMailbox::new(capacity))
    }
}

// -------------------------------------------------------------------
// Listener: bind, accept once, spawn one Connection isolate with
// `tiny_pressure` (max_in_flight = 1), then close the listener.
// -------------------------------------------------------------------

#[derive(Debug, Clone)]
enum ListenerMsg {
    Start,
    Bound(Result<(ListenerId, SocketAddr), CallError>),
    Accepted(Result<(tina_runtime::StreamId, SocketAddr), CallError>),
    ListenerClosed,
}

struct Listener {
    bind_addr: SocketAddr,
    bound_addr: Arc<Mutex<Option<SocketAddr>>>,
    router: Address<RegistryMsg, RouterReply>,
    listener_id: Option<ListenerId>,
}

#[tina_runtime::isolate(
    message = ListenerMsg,
    spawn = ChildDefinition<Connection<EiffelShard>>,
    shard = EiffelShard,
)]
impl Listener {
    fn handle(&mut self, msg: ListenerMsg, _ctx: &mut Context<'_, EiffelShard>) -> Effect<Self> {
        match msg {
            ListenerMsg::Start => tcp_bind(self.bind_addr).reply(ListenerMsg::Bound),
            ListenerMsg::Bound(Ok((listener, local_addr))) => {
                self.listener_id = Some(listener);
                *self.bound_addr.lock().expect("bound addr mutex") = Some(local_addr);
                tcp_accept(listener).reply(ListenerMsg::Accepted)
            }
            ListenerMsg::Accepted(Ok((stream, _peer_addr))) => {
                let listener = self.listener_id.expect("listener set after bind");
                let connection = Connection::<EiffelShard>::new(
                    ConnectionInit::new(stream, self.router)
                        .with_config(ConnectionConfig::tiny_pressure()),
                );
                batch(vec![
                    spawn(
                        ChildDefinition::new(connection, 64)
                            .with_initial_message(ConnectionMsg::Begin),
                    ),
                    tcp_close_listener(listener).reply(|_| ListenerMsg::ListenerClosed),
                ])
            }
            ListenerMsg::ListenerClosed => stop(),
            ListenerMsg::Bound(Err(_)) | ListenerMsg::Accepted(Err(_)) => stop(),
        }
    }
}

// -------------------------------------------------------------------
// Run
// -------------------------------------------------------------------

pub fn run(config: RunConfig) -> anyhow::Result<Report> {
    let runtime = ThreadedRuntime::with_config(
        EiffelShard,
        EFactory,
        ThreadedRuntimeConfig {
            command_capacity: 32,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    // 1. Typed dispatch: the `#[service]` macro emits `EchoService`
    //    with a JSON-tuple decoder for each method's args and a JSON
    //    encoder for each return type.
    let dispatch: Dispatch<EchoState, Json, EiffelShard> =
        EchoService::dispatch::<EchoState, EiffelShard>(EchoState, PayloadLimits::default());
    let service = runtime
        .register_with_capacity::<SingleService<Dispatch<EchoState, Json, EiffelShard>, EiffelShard>, Infallible>(
            SingleService::new(dispatch),
            16,
        )
        .map_err(|e| anyhow::anyhow!("register service: {e:?}"))?;

    // 2. Registry mapping wire service name to the dispatch isolate.
    let registry_state = Registry::<EiffelShard>::builder()
        .service("echo", service)
        .build();
    let registry = runtime
        .register_with_capacity::<Registry<EiffelShard>, Infallible>(registry_state, 16)
        .map_err(|e| anyhow::anyhow!("register registry: {e:?}"))?;

    // 3. Listener that binds, accepts once, and spawns the Connection
    //    isolate that enforces the in-flight cap on the wire.
    let bind_addr: SocketAddr = "127.0.0.1:0".parse()?;
    let bound_addr: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));
    let listener = runtime
        .register_with_capacity::<Listener, Infallible>(
            Listener {
                bind_addr,
                bound_addr: Arc::clone(&bound_addr),
                router: registry,
                listener_id: None,
            },
            8,
        )
        .map_err(|e| anyhow::anyhow!("register listener: {e:?}"))?;

    runtime
        .try_send(listener, ListenerMsg::Start)
        .map_err(|e| anyhow::anyhow!("start listener: {e:?}"))?;

    wait_until(Duration::from_secs(3), "listener bind", || {
        bound_addr.lock().expect("bound addr mutex").is_some()
    })?;
    let addr = bound_addr
        .lock()
        .expect("bound addr mutex")
        .expect("listener published address");

    // 4. Drive the burst from a blocking std::net client. The client
    //    encodes args as a JSON tuple `[<payload_bytes>]` to match the
    //    macro's decoder.
    let burst = config.burst;
    let report = thread::spawn(move || drive_client(addr, burst))
        .join()
        .map_err(|_| anyhow::anyhow!("client thread panicked"))??;

    let _ = runtime.shutdown();
    Ok(report)
}

/// Open one TCP connection, write the whole burst, read replies,
/// classify into a `Report`.
fn drive_client(addr: SocketAddr, burst: usize) -> anyhow::Result<Report> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    // The macro's args decoder for `fn ping(payload: Vec<u8>)` is
    // `(Vec<u8>,)` — a JSON array with one element. Encode that
    // tuple once and reuse for every frame in the burst.
    let payload = serde_json::to_vec(&(b"hi".to_vec(),))?;

    let limits = FrameLimits::default();
    let mut request_bytes = Vec::new();
    for id in 1..=burst as u64 {
        let frame = Frame::request(id, "echo", "ping", payload.clone());
        request_bytes.extend(encode(&frame, &limits)?);
    }
    stream.write_all(&request_bytes)?;

    let mut report = Report::default();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while report.total() < burst {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => {
                report.other += burst - report.total();
                return Ok(report);
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        loop {
            if buf.len() < LENGTH_PREFIX_SIZE {
                break;
            }
            let mut prefix = [0u8; LENGTH_PREFIX_SIZE];
            prefix.copy_from_slice(&buf[..LENGTH_PREFIX_SIZE]);
            let body_len = match parse_length_prefix(prefix, &limits) {
                Ok(len) => len,
                Err(_) => {
                    report.other += 1;
                    return Ok(report);
                }
            };
            let total = LENGTH_PREFIX_SIZE + body_len;
            if buf.len() < total {
                break;
            }
            let body = buf[LENGTH_PREFIX_SIZE..total].to_vec();
            buf.drain(..total);
            match decode_body(&body) {
                Ok(frame) => match (frame.kind, frame.error) {
                    (FrameKind::Reply, _) => report.ok += 1,
                    (FrameKind::Error, Some(FrameError::Full)) => report.full += 1,
                    _ => report.other += 1,
                },
                Err(_) => report.other += 1,
            }
        }
    }
    Ok(report)
}

fn wait_until<F>(timeout: Duration, label: &str, mut predicate: F) -> anyhow::Result<()>
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while !predicate() {
        if Instant::now() > deadline {
            anyhow::bail!("wait_until({label}) timed out");
        }
        thread::yield_now();
    }
    Ok(())
}
