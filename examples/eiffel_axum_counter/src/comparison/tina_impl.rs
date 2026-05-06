use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use tina::prelude::*;
use tina::{Mailbox, TrySendError};
use tina_runtime::{MailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig};
use tina_tokio_bridge::{BridgeHandle, BridgeRequest};
use tokio::net::TcpListener as TokioTcpListener;

use super::{SideReport, scripted_client};

#[derive(Debug, Clone, Copy)]
struct CounterShard;

impl Shard for CounterShard {
    fn id(&self) -> ShardId {
        ShardId::new(73)
    }
}

struct CounterMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    closed: Rc<Cell<bool>>,
}

impl<T> CounterMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Rc::new(RefCell::new(VecDeque::new())),
            closed: Rc::new(Cell::new(false)),
        }
    }
}

impl<T> Mailbox<T> for CounterMailbox<T> {
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
struct CounterMailboxFactory;

impl MailboxFactory for CounterMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(CounterMailbox::new(capacity))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CounterRequest {
    Read,
    Increment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CounterReply {
    value: u64,
}

#[derive(Debug)]
struct Counter {
    value: u64,
}

#[tina::isolate(
    message = BridgeRequest<CounterRequest, CounterReply>,
    shard = CounterShard
)]
impl Counter {
    fn handle(
        &mut self,
        msg: BridgeRequest<CounterRequest, CounterReply>,
        _ctx: &mut Context<'_, CounterShard>,
    ) -> Effect<Self> {
        let (request, responder) = msg.into_parts();
        match request {
            CounterRequest::Read => {}
            CounterRequest::Increment => self.value += 1,
        }
        let _ = responder.respond(CounterReply { value: self.value });
        noop()
    }
}

type CounterBridge =
    BridgeHandle<CounterRequest, CounterReply, CounterShard, CounterMailboxFactory, ()>;

async fn read_counter(State(bridge): State<CounterBridge>) -> (StatusCode, String) {
    match bridge.call(CounterRequest::Read).await {
        Ok(reply) => (StatusCode::OK, reply.value.to_string()),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, format!("{error:?}")),
    }
}

async fn increment_counter(State(bridge): State<CounterBridge>) -> (StatusCode, String) {
    match bridge.call(CounterRequest::Increment).await {
        Ok(reply) => (StatusCode::OK, reply.value.to_string()),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, format!("{error:?}")),
    }
}

pub(crate) fn run() -> SideReport {
    let runtime = Arc::new(ThreadedRuntime::with_config(
        CounterShard,
        CounterMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 16,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    ));
    let address = runtime
        .register_with_capacity::<Counter, Infallible>(Counter { value: 0 }, 16)
        .expect("register counter isolate");
    let bridge = BridgeHandle::new(Arc::clone(&runtime), address, Duration::from_secs(2));

    let tokio_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    let report = tokio_runtime.block_on(async move {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind tokio listener");
        let addr = listener.local_addr().expect("tokio listener local addr");

        let app = Router::new()
            .route("/counter", get(read_counter))
            .route("/counter/increment", post(increment_counter))
            .with_state(bridge);

        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let report = tokio::task::spawn_blocking(move || scripted_client(addr))
            .await
            .expect("client task");

        server.abort();
        let _ = server.await;
        report
    });

    drop(tokio_runtime);

    // Drain the runtime so the embedded thread shuts down cleanly.
    match Arc::try_unwrap(runtime) {
        Ok(runtime) => {
            let _ = runtime.shutdown();
        }
        Err(still_shared) => drop(still_shared),
    }

    report
}
