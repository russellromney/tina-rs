use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use tina::prelude::*;
use tina::{Mailbox, TrySendError};
use tina_runtime::{MailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig};
use tina_tokio_bridge::{BridgeHandle, BridgeRequest};
use tokio::net::TcpListener as TokioTcpListener;
use tokio::sync::mpsc;

use super::{SideReport, run_room_clients};

#[derive(Debug, Clone, Copy)]
struct RoomShard;

impl Shard for RoomShard {
    fn id(&self) -> ShardId {
        ShardId::new(74)
    }
}

struct RoomMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    closed: Rc<Cell<bool>>,
}

impl<T> RoomMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Rc::new(RefCell::new(VecDeque::new())),
            closed: Rc::new(Cell::new(false)),
        }
    }
}

impl<T> Mailbox<T> for RoomMailbox<T> {
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
struct RoomMailboxFactory;

impl MailboxFactory for RoomMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(RoomMailbox::new(capacity))
    }
}

#[derive(Debug)]
enum RoomRequest {
    Subscribe(mpsc::UnboundedSender<String>),
    Publish(String),
}

#[derive(Debug, Clone)]
struct RoomReply;

#[derive(Debug, Default)]
struct Room {
    subscribers: Vec<mpsc::UnboundedSender<String>>,
}

#[tina::isolate(
    message = BridgeRequest<RoomRequest, RoomReply>,
    shard = RoomShard
)]
impl Room {
    fn handle(
        &mut self,
        msg: BridgeRequest<RoomRequest, RoomReply>,
        _ctx: &mut Context<'_, RoomShard>,
    ) -> Effect<Self> {
        let (request, responder) = msg.into_parts();
        match request {
            RoomRequest::Subscribe(tx) => {
                self.subscribers.push(tx);
            }
            RoomRequest::Publish(text) => {
                self.subscribers.retain(|tx| tx.send(text.clone()).is_ok());
            }
        }
        let _ = responder.respond(RoomReply);
        noop()
    }
}

type RoomBridge = BridgeHandle<RoomRequest, RoomReply, RoomShard, RoomMailboxFactory, ()>;

async fn ws_upgrade(
    State(bridge): State<RoomBridge>,
    upgrade: WebSocketUpgrade,
) -> impl IntoResponse {
    upgrade.on_upgrade(move |socket| handle_socket(socket, bridge))
}

async fn handle_socket(socket: WebSocket, bridge: RoomBridge) {
    let (mut write, mut read) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    if bridge.call(RoomRequest::Subscribe(tx)).await.is_err() {
        return;
    }

    let writer = tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            if write.send(AxumMessage::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(message)) = read.next().await {
        match message {
            AxumMessage::Text(text) => {
                let _ = bridge.call(RoomRequest::Publish(text.to_string())).await;
            }
            AxumMessage::Close(_) => break,
            _ => {}
        }
    }

    writer.abort();
    let _ = writer.await;
}

pub(crate) fn run() -> SideReport {
    let runtime = Arc::new(ThreadedRuntime::with_config(
        RoomShard,
        RoomMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 32,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    ));
    let address = runtime
        .register_with_capacity::<Room, Infallible>(Room::default(), 32)
        .expect("register room isolate");
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
            .route("/ws", get(ws_upgrade))
            .with_state(bridge);

        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let report = run_room_clients(addr).await;

        server.abort();
        let _ = server.await;
        report
    });

    drop(tokio_runtime);

    match Arc::try_unwrap(runtime) {
        Ok(runtime) => {
            let _ = runtime.shutdown();
        }
        Err(still_shared) => drop(still_shared),
    }

    report
}
