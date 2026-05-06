use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http::{Method, StatusCode};
use tina::{Mailbox, TrySendError, prelude::*};
use tina_http::{HttpLimits, HttpListener, HttpListenerMsg, HttpRequest, HttpResponse};
use tina_runtime::{MailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig};

use super::{SideReport, scripted_client};

/// Single shard for the Tina-side HTTP server in this comparison. Each
/// Eiffel comparison runs in its own process, so cross-comparison id
/// collisions are not a concern; the const just removes the
/// magic-number-per-file smell.
const TINA_SHARD_ID: u32 = 303;

#[derive(Debug, Default)]
struct AppShard;

impl Shard for AppShard {
    fn id(&self) -> ShardId {
        ShardId::new(TINA_SHARD_ID)
    }
}

struct AppMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    closed: Rc<Cell<bool>>,
}

impl<T> AppMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Rc::new(RefCell::new(VecDeque::new())),
            closed: Rc::new(Cell::new(false)),
        }
    }
}

impl<T> Mailbox<T> for AppMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if self.closed.get() {
            return Err(TrySendError::Closed(message));
        }
        let mut q = self.queue.borrow_mut();
        if q.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        q.push_back(message);
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
struct AppMailboxFactory;

impl MailboxFactory for AppMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(AppMailbox::new(capacity))
    }
}

#[derive(Debug, Default)]
struct Counter {
    value: u32,
}

impl Isolate for Counter {
    tina::isolate_types! {
        message: HttpRequest,
        reply: HttpResponse,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: Infallible,
        shard: AppShard,
    }

    fn handle(&mut self, request: HttpRequest, _ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        let response = match (request.method.clone(), request.path.as_str()) {
            (Method::GET, "/counter") => HttpResponse::text(self.value.to_string()),
            (Method::POST, "/counter") => {
                self.value += 1;
                HttpResponse::text(self.value.to_string())
            }
            _ => HttpResponse::with_status(StatusCode::NOT_FOUND),
        };
        reply(response)
    }
}

pub(crate) fn run() -> SideReport {
    let runtime = ThreadedRuntime::with_config(
        AppShard,
        AppMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let counter = runtime
        .register_with_capacity::<Counter, Infallible>(Counter::default(), 16)
        .expect("register counter");

    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("loopback parse");
    let bound_addr_slot = Arc::new(Mutex::new(None));

    let listener = runtime
        .register_with_capacity::<HttpListener<AppShard>, _>(
            HttpListener::<AppShard>::new(
                bind_addr,
                Arc::clone(&bound_addr_slot),
                counter,
                HttpLimits::default(),
                Duration::from_secs(2),
                16,
            ),
            8,
        )
        .expect("register listener");
    runtime
        .try_send(listener, HttpListenerMsg::Start)
        .expect("send Start");

    // Wait for the listener to publish its bound address.
    let server_addr = wait_for_bound_addr(&bound_addr_slot);

    let report = scripted_client(server_addr);

    runtime
        .try_send(listener, HttpListenerMsg::Stop)
        .expect("send Stop");
    let _ = runtime.shutdown().expect("runtime shutdown");

    report
}

fn wait_for_bound_addr(slot: &Arc<Mutex<Option<SocketAddr>>>) -> SocketAddr {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(addr) = *slot.lock().expect("bound addr lock") {
            return addr;
        }
        if std::time::Instant::now() > deadline {
            panic!("listener bind timed out");
        }
        std::thread::yield_now();
    }
}
