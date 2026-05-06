//! Tina framed first-form: Connection isolate enforces an explicit
//! `max_in_flight = 1` per connection. Over-cap requests get a
//! server-reported wire `Error(Full)` frame immediately.

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
    CallError, ListenerId, MailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig, tcp_accept,
    tcp_bind, tcp_close_listener,
};
use tina_rpc::{
    Connection, ConnectionConfig, ConnectionInit, ConnectionMsg, Registry, RegistryMsg,
    RouterReply, ServiceCall, ServiceReply,
};

use super::{ComparisonConfig, SideReport, drive_client};

#[derive(Debug, Default)]
struct EiffelShard;

impl Shard for EiffelShard {
    fn id(&self) -> ShardId {
        ShardId::new(73)
    }
}

/// Tiny mailbox that the runtime constructs for each registered isolate.
/// Borrowed straight from `eiffel_real_io_chat`'s pattern.
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

/// Trivial echo service. The connection isolate's `max_in_flight = 1`
/// is what limits concurrency; the service itself replies immediately.
struct EchoService;

#[tina_runtime::isolate(
    message = ServiceCall,
    reply = ServiceReply,
    shard = EiffelShard,
)]
impl EchoService {
    fn handle(
        &mut self,
        msg: ServiceCall,
        _ctx: &mut Context<'_, EiffelShard>,
    ) -> Effect<Self> {
        reply(ServiceReply::Ok(msg.payload))
    }
}

/// Listener: tcp_bind, tcp_accept once, spawn a Connection isolate, then
/// close the listener. The comparison only needs to handle one client.
#[derive(Debug, Clone)]
enum ListenerMsg {
    Start,
    Bound(Result<(ListenerId, SocketAddr), CallError>),
    Accepted(Result<(tina_runtime::StreamId, SocketAddr), CallError>),
    ListenerClosed,
}

struct Listener {
    bind_addr: SocketAddr,
    bound_addr: BoundAddr,
    router: Address<RegistryMsg, RouterReply>,
    listener_id: Option<ListenerId>,
}

#[tina_runtime::isolate(
    message = ListenerMsg,
    spawn = ChildDefinition<Connection<EiffelShard>>,
    shard = EiffelShard,
)]
impl Listener {
    fn handle(
        &mut self,
        msg: ListenerMsg,
        _ctx: &mut Context<'_, EiffelShard>,
    ) -> Effect<Self> {
        match msg {
            ListenerMsg::Start => tcp_bind(self.bind_addr).reply(ListenerMsg::Bound),
            ListenerMsg::Bound(Ok((listener, local_addr))) => {
                self.listener_id = Some(listener);
                *self.bound_addr.lock().expect("bound addr mutex") = Some(local_addr);
                tcp_accept(listener).reply(ListenerMsg::Accepted)
            }
            ListenerMsg::Accepted(Ok((stream, _peer_addr))) => {
                let listener = self.listener_id.expect("listener set after bind");
                // `tiny_pressure` is the demo preset: max_in_flight=1
                // with a registry-dominant service_call_timeout, so
                // overload becomes wire-visible at small bursts.
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

pub(crate) fn run(config: ComparisonConfig) -> SideReport {
    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("loopback parse");
    let bound_addr: BoundAddr = Arc::new(Mutex::new(None));

    let runtime = ThreadedRuntime::with_config(
        EiffelShard,
        EiffelMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 32,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    // 1) Service: a single echo service.
    let service = runtime
        .register_with_capacity::<EchoService, Infallible>(EchoService, 16)
        .expect("register service");

    // 2) Registry that knows about the service.
    let registry_state = Registry::<EiffelShard>::builder()
        .service("echo", service)
        .build();
    let registry = runtime
        .register_with_capacity::<Registry<EiffelShard>, Infallible>(registry_state, 16)
        .expect("register registry");

    // 3) Listener that spawns Connection per accepted stream.
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

    // 4) Drive the framed RPC client in a separate thread (real TCP).
    let burst = config.burst;
    let client_thread = thread::spawn(move || drive_client(addr, burst));
    let report = client_thread.join().expect("client thread");

    let _ = runtime.shutdown();
    report
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
