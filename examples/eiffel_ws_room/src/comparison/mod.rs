use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

mod tina_impl;
mod tokio_impl;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideReport {
    pub alpha_inbox: Vec<String>,
    pub bravo_inbox: Vec<String>,
}

impl SideReport {
    pub fn assert_expected(&self) {
        let alpha: BTreeSet<&str> = self.alpha_inbox.iter().map(String::as_str).collect();
        let bravo: BTreeSet<&str> = self.bravo_inbox.iter().map(String::as_str).collect();
        let expected: BTreeSet<&str> = ["from-alpha", "from-bravo"].into_iter().collect();
        assert_eq!(alpha, expected, "alpha should see both broadcasts");
        assert_eq!(bravo, expected, "bravo should see both broadcasts");
    }
}

pub fn run_tokio_side() -> SideReport {
    tokio_impl::run()
}

pub fn run_tina_side() -> SideReport {
    tina_impl::run()
}

/// Two clients, alpha and bravo, both connect to ws://addr/ws. Alpha sends
/// "from-alpha"; bravo sends "from-bravo". Each client reads frames until it
/// has accumulated two text messages or the read deadline expires.
pub(crate) async fn run_room_clients(addr: SocketAddr) -> SideReport {
    let url = format!("ws://{addr}/ws");
    let (alpha_socket, _) = connect_async(&url).await.expect("alpha connect");
    let (bravo_socket, _) = connect_async(&url).await.expect("bravo connect");

    // Wait briefly for both subscriptions to land before any client publishes.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let alpha = tokio::spawn(client_session(alpha_socket, "from-alpha".to_string()));
    let bravo = tokio::spawn(client_session(bravo_socket, "from-bravo".to_string()));

    let alpha_inbox = alpha.await.expect("alpha task");
    let bravo_inbox = bravo.await.expect("bravo task");

    SideReport {
        alpha_inbox,
        bravo_inbox,
    }
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
