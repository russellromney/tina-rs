//! Tina side: a `Room` isolate owns the subscriber list. axum sits
//! in a Tokio runtime and reaches the room through the blessed
//! `tina_tokio_bridge` lifecycle. Per WebSocket connection: one
//! `Subscribe` call to register the client's mpsc sender, then one
//! `Publish` call per inbound message.

use std::convert::Infallible;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use tina::prelude::*;
use tina_runtime::{DefaultThreadedMailboxFactory, LocalSystem};
use tina_tokio_bridge::{BridgeHost, BridgeRequest};
use tina_tower_bridge::{Service, TinaService, TinaTowerService};
use tokio::net::TcpListener as TokioTcpListener;
use tokio::sync::mpsc;

use crate::{Report, run_room_clients};

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
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
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

    // The Subscribe reply means the sender is registered in the Room
    // isolate; acknowledge to the client with a Ping control frame so
    // the driver can observe the landed subscription before publishing.
    if write
        .send(AxumMessage::Ping(Vec::new().into()))
        .await
        .is_err()
    {
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
    match writer.await {
        Err(error) if error.is_cancelled() => {}
        Ok(()) => {}
        Err(error) => panic!("room writer task failed: {error}"),
    }
}

pub fn run() -> Result<Report, Box<dyn std::error::Error>> {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
        .ingress_capacity(32)
        .idle_wait(Duration::from_millis(1))
        .try_build()?;
    let mut host = BridgeHost::from_app(app);
    let bridge = host
        .register_bridge::<Room, RoomRequest, RoomReply, Infallible>(
            Room::default(),
            32,
            Duration::from_secs(2),
        )
        .map_err(|error| std::io::Error::other(format!("register room bridge: {error:?}")))?;
    let svc: RoomService = TinaTowerService::new(bridge);

    let tokio_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let report: Result<Report, Box<dyn std::error::Error>> = tokio_runtime.block_on(async move {
        let listener = TokioTcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;

        let app = Router::new().route("/ws", get(ws_upgrade)).with_state(svc);

        let server = tokio::spawn(async move { axum::serve(listener, app).await });

        let report = run_room_clients(addr).await;

        server.abort();
        match server.await {
            Err(error) if error.is_cancelled() => {}
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error.into()),
            Err(error) => return Err(error.into()),
        }
        Ok(report)
    });
    let report = report?;

    drop(tokio_runtime);

    let shutdown = host
        .drain_and_shutdown(Duration::from_secs(2))
        .map_err(|error| std::io::Error::other(format!("shut down bridge host: {error:?}")))?;
    if !shutdown.drained_within_timeout {
        return Err(std::io::Error::other(format!(
            "bridge host still had {} handles after drain timeout",
            shutdown.outstanding_handles_at_shutdown
        ))
        .into());
    }

    Ok(report)
}
