//! Tina-vs-Tokio: a tiny WebSocket room. Two clients connect, each
//! publishes one message, both should see both. The Tina side keeps
//! the subscriber list inside a `Room` isolate and crosses to axum
//! through the blessed `tina_tokio_bridge` lifecycle.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

pub mod tina_impl;
pub mod tokio_impl;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub alpha_inbox: Vec<String>,
    pub bravo_inbox: Vec<String>,
}

impl Report {
    pub fn assert_expected(&self) {
        let alpha: BTreeSet<&str> = self.alpha_inbox.iter().map(String::as_str).collect();
        let bravo: BTreeSet<&str> = self.bravo_inbox.iter().map(String::as_str).collect();
        let expected: BTreeSet<&str> = ["from-alpha", "from-bravo"].into_iter().collect();
        assert_eq!(alpha, expected, "alpha should see both broadcasts");
        assert_eq!(bravo, expected, "bravo should see both broadcasts");
    }
}

/// Two clients (alpha, bravo) connect, each publishes one text
/// message, each reads until two text messages have arrived or the
/// deadline elapses.
///
/// Publishing starts only after BOTH subscriptions are observable: each
/// server acknowledges a landed subscription with a Ping control frame
/// (see `tina_impl`/`tokio_impl`), and the driver waits for both acks
/// under a deadline instead of sleeping a fixed delay.
pub async fn run_room_clients(addr: SocketAddr) -> Report {
    let url = format!("ws://{addr}/ws");
    let (alpha_socket, _) = connect_async(&url).await.expect("alpha connect");
    let (bravo_socket, _) = connect_async(&url).await.expect("bravo connect");

    let alpha_socket = await_subscribed(alpha_socket, "alpha").await;
    let bravo_socket = await_subscribed(bravo_socket, "bravo").await;

    let alpha = tokio::spawn(client_session(alpha_socket, "from-alpha".to_string()));
    let bravo = tokio::spawn(client_session(bravo_socket, "from-bravo".to_string()));

    Report {
        alpha_inbox: alpha.await.expect("alpha task"),
        bravo_inbox: bravo.await.expect("bravo task"),
    }
}

/// Reads until the server's post-subscription Ping arrives or the
/// deadline elapses. The Ping is a control frame: it is never published
/// to the room and never enters the pinned text transcript.
async fn await_subscribed<S>(mut socket: S, peer: &str) -> S
where
    S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error>
        + StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    let subscribed = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(message) = socket.next().await {
            match message {
                Ok(Message::Ping(_)) => return true,
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
        false
    })
    .await;
    assert!(
        matches!(subscribed, Ok(true)),
        "{peer} subscription was not acknowledged before the deadline"
    );
    socket
}

async fn client_session<S>(mut socket: S, outgoing: String) -> Vec<String>
where
    S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error>
        + StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    socket
        .send(Message::Text(outgoing.into()))
        .await
        .expect("ws send");

    let mut inbox = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while inbox.len() < 2 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, socket.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => inbox.push(text.to_string()),
            Ok(Some(Ok(Message::Close(_)))) => break,
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(_))) | Ok(None) | Err(_) => break,
        }
    }

    let _ = socket.send(Message::Close(None)).await;
    inbox
}
