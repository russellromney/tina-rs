//! Tina-vs-Tokio: a multiplexed *client* against a single TCP
//! responder. Each side issues three requests on one connection;
//! the responder delays high-id replies less than low-id replies, so
//! `RESP 3` arrives before `RESP 2` arrives before `RESP 1`. That
//! out-of-order arrival is the proof that real multiplexing
//! happened.
//!
//! Same workload, two implementations. Read [`tokio_impl`] and
//! [`tina_impl`] top-to-bottom; the README compares feel.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub mod tina_impl;
pub mod tokio_impl;

/// Request ids the client submits, in submission order.
pub const REQUEST_IDS: [u32; 3] = [1, 2, 3];

/// What each side observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Ids in the order their `RESP <id>` line was seen on the wire.
    pub arrival_order: Vec<u32>,
}

/// Spawns a single-connection responder on a fresh tokio runtime.
/// Returns the bound address and a join handle for clean shutdown.
///
/// The responder reads `REQ <id>\n` lines, schedules a write of
/// `RESP <id>\n` after a per-id delay (`id=1 → 30 ms`,
/// `id=2 → 20 ms`, `id=3 → 10 ms`), and uses one shared writer so
/// replies interleave freely. This is the *test target*, not part of
/// either side's implementation — both sides connect to it as
/// clients.
pub async fn spawn_responder() -> anyhow::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    let handle = tokio::spawn(async move {
        let (stream, _peer) = listener.accept().await.expect("responder accept");
        let (read_half, write_half) = stream.into_split();
        let writer = std::sync::Arc::new(tokio::sync::Mutex::new(write_half));

        let mut reader = tokio::io::BufReader::new(read_half);
        let mut line = String::new();
        let mut tasks = Vec::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            let id: u32 = line
                .trim()
                .strip_prefix("REQ ")
                .and_then(|rest| rest.parse().ok())
                .expect("REQ <id>");

            let writer = std::sync::Arc::clone(&writer);
            tasks.push(tokio::spawn(async move {
                let delay = Duration::from_millis((40 - u64::from(id) * 10).max(5));
                tokio::time::sleep(delay).await;
                let mut guard = writer.lock().await;
                guard
                    .write_all(format!("RESP {id}\n").as_bytes())
                    .await
                    .expect("responder write");
            }));
        }
        for task in tasks {
            task.await.expect("responder request task");
        }
    });
    Ok((addr, handle))
}
