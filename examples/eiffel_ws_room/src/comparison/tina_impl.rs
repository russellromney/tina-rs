use std::convert::Infallible;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use tina::prelude::*;
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntimeConfig};
use tina_tokio_bridge::{BridgeHost, BridgeRequest};
use tina_tower_bridge::{Service, TinaService, TinaTowerService};
use tokio::net::TcpListener as TokioTcpListener;
use tokio::sync::mpsc;

use super::{SideReport, run_room_clients};

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

#[tina::isolate(message = BridgeRequest<RoomRequest, RoomReply>)]
impl Room {
    fn handle(
        &mut self,
        msg: BridgeRequest<RoomRequest, RoomReply>,
        _ctx: &mut Context<'_, SingleShard>,
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

type RoomService = TinaService<RoomRequest, RoomReply>;

async fn ws_upgrade(
    State(svc): State<RoomService>,
    upgrade: WebSocketUpgrade,
) -> impl IntoResponse {
    upgrade.on_upgrade(move |socket| handle_socket(socket, svc))
}

async fn handle_socket(socket: WebSocket, svc: RoomService) {
    let (mut write, mut read) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    let mut sub_svc = svc.clone();
    if sub_svc.call(RoomRequest::Subscribe(tx)).await.is_err() {
        return;
    }

    let writer = tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            if write.send(AxumMessage::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    let mut publish_svc = svc.clone();
    while let Some(Ok(message)) = read.next().await {
        match message {
            AxumMessage::Text(text) => {
                let _ = publish_svc
                    .call(RoomRequest::Publish(text.to_string()))
                    .await;
            }
            AxumMessage::Close(_) => break,
            _ => {}
        }
    }

    writer.abort();
    let _ = writer.await;
}

pub(crate) fn run() -> SideReport {
    let mut host = BridgeHost::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 32,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let bridge = host
        .register_bridge::<Room, RoomRequest, RoomReply, Infallible>(
            Room::default(),
            32,
            Duration::from_secs(2),
        )
        .expect("register room bridge");
    let svc: RoomService = TinaTowerService::new(bridge);

    let tokio_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    let report = tokio_runtime.block_on(async move {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind tokio listener");
        let addr = listener.local_addr().expect("tokio listener local addr");

        let app = Router::new().route("/ws", get(ws_upgrade)).with_state(svc);

        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let report = run_room_clients(addr).await;

        server.abort();
        let _ = server.await;
        report
    });

    drop(tokio_runtime);

    let _ = host
        .drain_and_shutdown(Duration::from_secs(2))
        .expect("bridge host drains and shuts down cleanly");

    report
}
