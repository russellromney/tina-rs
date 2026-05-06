use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tina::{Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    CallError, ListenerId, MailboxFactory, RuntimeEvent, RuntimeEventKind, SendOutcome,
    SendRejectedReason, StreamId, ThreadedRuntime, ThreadedRuntimeConfig, send_observed,
    tcp_accept, tcp_bind, tcp_close_listener, tcp_close_stream, tcp_read, tcp_write,
};

use super::{
    ComparisonConfig, SideReport, format_response, parse_burst, parse_response, real_tcp_client,
};

#[derive(Debug, Default)]
struct EiffelShard;

impl Shard for EiffelShard {
    fn id(&self) -> ShardId {
        ShardId::new(71)
    }
}

struct EiffelMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    closed: Rc<Cell<bool>>,
}

impl<T> EiffelMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Rc::new(RefCell::new(VecDeque::new())),
            closed: Rc::new(Cell::new(false)),
        }
    }
}

impl<T> Mailbox<T> for EiffelMailbox<T> {
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
struct EiffelMailboxFactory;

impl MailboxFactory for EiffelMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(EiffelMailbox::new(capacity))
    }
}

type BoundAddr = Arc<Mutex<Option<SocketAddr>>>;
type SlowClientEvents = Arc<Mutex<Vec<usize>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlowClientMsg {
    Deliver(usize),
}

#[derive(Debug)]
struct SlowClient {
    events: SlowClientEvents,
}

#[tina_runtime::isolate(message = SlowClientMsg, shard = EiffelShard)]
impl SlowClient {
    fn handle(&mut self, msg: SlowClientMsg, _ctx: &mut Context<'_, EiffelShard>) -> Effect<Self> {
        match msg {
            SlowClientMsg::Deliver(index) => {
                self.events
                    .lock()
                    .expect("slow client events mutex")
                    .push(index);
                noop()
            }
        }
    }
}

#[derive(Debug, Clone)]
enum ConnectionMsg {
    Begin,
    Read(Vec<u8>),
    Observed(SendOutcome),
    Wrote(usize),
    Closed,
    IoFailed,
}

#[derive(Debug)]
struct Connection {
    stream: StreamId,
    slow_client: Address<SlowClientMsg>,
    requested_burst: usize,
    observed: usize,
    accepted: usize,
    full: usize,
    closed: usize,
}

#[tina_runtime::isolate(
    message = ConnectionMsg,
    send = Outbound<SlowClientMsg>,
    shard = EiffelShard
)]
impl Connection {
    fn handle(&mut self, msg: ConnectionMsg, _ctx: &mut Context<'_, EiffelShard>) -> Effect<Self> {
        match msg {
            ConnectionMsg::Begin => tcp_read(self.stream, 32).reply(|result| match result {
                Ok(bytes) => ConnectionMsg::Read(bytes),
                Err(_) => ConnectionMsg::IoFailed,
            }),
            ConnectionMsg::Read(bytes) => {
                self.requested_burst = parse_burst(&bytes);
                batch(
                    (0..self.requested_burst)
                        .map(|index| {
                            send_observed(self.slow_client, SlowClientMsg::Deliver(index))
                                .reply(ConnectionMsg::Observed)
                        })
                        .collect::<Vec<_>>(),
                )
            }
            ConnectionMsg::Observed(outcome) => {
                self.observed += 1;
                if outcome.is_accepted() {
                    self.accepted += 1;
                } else if outcome.is_full() {
                    self.full += 1;
                } else {
                    debug_assert!(outcome.is_closed());
                    self.closed += 1;
                }
                if self.observed < self.requested_burst {
                    return noop();
                }

                let response =
                    format_response(self.accepted, self.full, self.closed, self.accepted, 0);
                tcp_write(self.stream, response).reply(|result| match result {
                    Ok(count) => ConnectionMsg::Wrote(count),
                    Err(_) => ConnectionMsg::IoFailed,
                })
            }
            ConnectionMsg::Wrote(_count) => {
                tcp_close_stream(self.stream).reply(|result| match result {
                    Ok(()) => ConnectionMsg::Closed,
                    Err(_) => ConnectionMsg::IoFailed,
                })
            }
            ConnectionMsg::Closed | ConnectionMsg::IoFailed => stop(),
        }
    }
}

#[derive(Debug, Clone)]
enum ListenerMsg {
    Start,
    Bound(Result<(ListenerId, SocketAddr), CallError>),
    Accepted(Result<(StreamId, SocketAddr), CallError>),
    ListenerClosed(Result<(), CallError>),
}

#[derive(Debug)]
struct Listener {
    bind_addr: SocketAddr,
    bound_addr: BoundAddr,
    slow_client: Address<SlowClientMsg>,
    listener: Option<ListenerId>,
}

#[tina_runtime::isolate(
    message = ListenerMsg,
    spawn = ChildDefinition<Connection>,
    shard = EiffelShard
)]
impl Listener {
    fn handle(&mut self, msg: ListenerMsg, _ctx: &mut Context<'_, EiffelShard>) -> Effect<Self> {
        match msg {
            ListenerMsg::Start => tcp_bind(self.bind_addr).reply(ListenerMsg::Bound),
            ListenerMsg::Bound(Ok((listener, local_addr))) => {
                self.listener = Some(listener);
                *self.bound_addr.lock().expect("bound addr mutex") = Some(local_addr);
                tcp_accept(listener).reply(ListenerMsg::Accepted)
            }
            ListenerMsg::Accepted(Ok((stream, _peer_addr))) => {
                let listener = self.listener.expect("listener set after bind");
                batch(vec![
                    spawn(
                        ChildDefinition::new(
                            Connection {
                                stream,
                                slow_client: self.slow_client,
                                requested_burst: 0,
                                observed: 0,
                                accepted: 0,
                                full: 0,
                                closed: 0,
                            },
                            1024,
                        )
                        .with_initial_message(ConnectionMsg::Begin),
                    ),
                    tcp_close_listener(listener).reply(ListenerMsg::ListenerClosed),
                ])
            }
            ListenerMsg::ListenerClosed(Ok(())) => stop(),
            ListenerMsg::Bound(Err(_))
            | ListenerMsg::Accepted(Err(_))
            | ListenerMsg::ListenerClosed(Err(_)) => stop(),
        }
    }
}

pub(crate) fn run(config: ComparisonConfig) -> SideReport {
    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("loopback parse");
    let bound_addr: BoundAddr = Arc::new(Mutex::new(None));
    let slow_events: SlowClientEvents = Arc::new(Mutex::new(Vec::new()));

    let runtime = ThreadedRuntime::with_config(
        EiffelShard,
        EiffelMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 16,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let slow_client = runtime
        .register_with_capacity::<SlowClient, Infallible>(
            SlowClient {
                events: Arc::clone(&slow_events),
            },
            config.slow_client_capacity,
        )
        .expect("register slow client");

    let listener = runtime
        .register_with_capacity::<Listener, Infallible>(
            Listener {
                bind_addr,
                bound_addr: Arc::clone(&bound_addr),
                slow_client,
                listener: None,
            },
            8,
        )
        .expect("register listener");

    runtime
        .try_send(listener, ListenerMsg::Start)
        .expect("start listener");

    wait_until(Duration::from_secs(3), "listener bind", || {
        bound_addr.lock().expect("bound addr mutex").is_some()
    });

    let addr = bound_addr
        .lock()
        .expect("bound addr mutex")
        .expect("listener published address");
    let client = thread::spawn(move || real_tcp_client(addr, config.burst));

    wait_until(Duration::from_secs(3), "connection pressure", || {
        let trace = runtime.complete_trace().expect("trace available");
        trace.iter().any(tcp_stream_close_completed) && trace.iter().any(send_rejected_full)
    });

    let response = client.join().expect("client thread");

    wait_until(Duration::from_secs(3), "slow client delivery", || {
        slow_events.lock().expect("slow events mutex").len() == config.slow_client_capacity
    });

    let trace = runtime.shutdown().expect("runtime shutdown");
    parse_response(
        response,
        trace.iter().any(real_tcp_call_completed),
        trace.iter().any(send_rejected_full),
    )
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

fn send_rejected_full(event: &RuntimeEvent) -> bool {
    matches!(
        event.kind(),
        RuntimeEventKind::SendRejected {
            reason: SendRejectedReason::Full,
            ..
        }
    )
}

fn real_tcp_call_completed(event: &RuntimeEvent) -> bool {
    matches!(
        event.kind(),
        RuntimeEventKind::CallCompleted { call_kind, .. }
            if matches!(
                call_kind,
                tina_runtime::CallKind::TcpBind
                    | tina_runtime::CallKind::TcpAccept
                    | tina_runtime::CallKind::TcpRead
                    | tina_runtime::CallKind::TcpWrite
                    | tina_runtime::CallKind::TcpStreamClose
                    | tina_runtime::CallKind::TcpListenerClose
            )
    )
}

fn tcp_stream_close_completed(event: &RuntimeEvent) -> bool {
    matches!(
        event.kind(),
        RuntimeEventKind::CallCompleted { call_kind, .. }
            if matches!(call_kind, tina_runtime::CallKind::TcpStreamClose)
    )
}
