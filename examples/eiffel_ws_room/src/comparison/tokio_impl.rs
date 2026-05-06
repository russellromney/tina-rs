use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener as TokioTcpListener;
use tokio::sync::broadcast;

use super::{SideReport, run_room_clients};

#[derive(Clone)]
struct RoomState {
    sender: broadcast::Sender<String>,
}

async fn ws_upgrade(State(room): State<RoomState>, upgrade: WebSocketUpgrade) -> impl IntoResponse {
    upgrade.on_upgrade(move |socket| handle_socket(socket, room))
}

async fn handle_socket(socket: WebSocket, room: RoomState) {
    let (mut write, mut read) = socket.split();
    let mut subscription = room.sender.subscribe();

    let writer = tokio::spawn(async move {
        while let Ok(text) = subscription.recv().await {
            if write.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(message)) = read.next().await {
        match message {
            Message::Text(text) => {
                let _ = room.sender.send(text.to_string());
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    writer.abort();
    let _ = writer.await;
}

pub(crate) fn run() -> SideReport {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    runtime.block_on(async move {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind tokio listener");
        let addr = listener.local_addr().expect("tokio listener local addr");

        let (sender, _) = broadcast::channel::<String>(64);
        let app = Router::new()
            .route("/ws", get(ws_upgrade))
            .with_state(RoomState { sender });

        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let report = run_room_clients(addr).await;

        server.abort();
        let _ = server.await;
        report
    })
}
