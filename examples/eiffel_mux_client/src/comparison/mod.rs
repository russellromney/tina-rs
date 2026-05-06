use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::{TcpListener as TokioTcpListener, TcpStream as TokioTcpStream};

mod tina_impl;
mod tokio_impl;

pub(crate) const REQUEST_IDS: [u32; 3] = [1, 2, 3];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideReport {
    /// IDs in the order their `RESP <id>` line was observed.
    pub arrival_order: Vec<u32>,
    /// IDs that were submitted (always [1, 2, 3]).
    pub request_ids: Vec<u32>,
}

impl SideReport {
    pub fn assert_expected(&self) {
        assert_eq!(
            self.request_ids,
            REQUEST_IDS.to_vec(),
            "submitted IDs should be [1, 2, 3]"
        );
        let saw: BTreeSet<u32> = self.arrival_order.iter().copied().collect();
        let expected: BTreeSet<u32> = REQUEST_IDS.iter().copied().collect();
        assert_eq!(saw, expected, "must observe all three responses");
        assert_eq!(
            self.arrival_order.first(),
            Some(&3),
            "id=3 has the shortest server delay so it should arrive first \
             (this is the property that proves multiplexing actually happened)"
        );
    }
}

pub fn run_tokio_side() -> SideReport {
    tokio_impl::run()
}

pub fn run_tina_side() -> SideReport {
    tina_impl::run()
}

/// Spawns a single-connection responder that returns "RESP <id>\n" for every
/// "REQ <id>\n" line, with id-dependent delay so high-id replies overtake
/// low-id replies (id=1 → 30ms, id=2 → 20ms, id=3 → 10ms). Returns the bound
/// address and a join handle to await server completion.
pub(crate) async fn spawn_responder() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TokioTcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind responder listener");
    let addr = listener.local_addr().expect("responder local addr");

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
            let text = line.trim();
            let id: u32 = text
                .strip_prefix("REQ ")
                .and_then(|rest| rest.parse().ok())
                .unwrap_or_else(|| panic!("responder received unexpected line {text:?}"));

            let writer = std::sync::Arc::clone(&writer);
            tasks.push(tokio::spawn(async move {
                let delay = Duration::from_millis((40 - id as u64 * 10).max(5));
                tokio::time::sleep(delay).await;
                let mut guard = writer.lock().await;
                guard
                    .write_all(format!("RESP {id}\n").as_bytes())
                    .await
                    .expect("responder write");
            }));
        }
        for task in tasks {
            let _ = task.await;
        }
    });
    (addr, handle)
}

/// Convenience: connect a Tokio TCP stream synchronously from inside an async
/// context.
pub(crate) async fn connect(addr: SocketAddr) -> TokioTcpStream {
    TokioTcpStream::connect(addr)
        .await
        .expect("client connects to responder")
}
