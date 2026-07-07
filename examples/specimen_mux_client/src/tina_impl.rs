//! Tina: a single `MuxClient` isolate owns the connection. The
//! parser, the read buffer, and the arrival log all live behind the
//! same mailbox — no shared `HashMap`, no oneshot per request, no
//! `Arc<Mutex<...>>` for the result. The isolate publishes its final
//! `Vec<u32>` of arrivals via `stop_with` (the typed stop-result path) and the
//! host receives it through `observe_result`.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::thread;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    DefaultThreadedMailboxFactory, StreamId, TcpConnectReply, TcpReadReply, TcpStreamCloseReply,
    TcpWriteReply, ThreadedRuntime, tcp_close_stream, tcp_connect, tcp_read, tcp_write,
};

use crate::{REQUEST_IDS, Report, spawn_responder};

#[derive(Debug, Clone)]
enum MuxMsg {
    Begin,
    Connected(TcpConnectReply),
    Wrote(TcpWriteReply),
    Read(TcpReadReply),
    Closed(TcpStreamCloseReply),
}

/// State that's empty/derivable at spawn. The host configures
/// `target` and `expected`; everything else (including the arrival
/// log) is `Default`.
#[derive(Debug, Default)]
struct ClientState {
    stream: Option<StreamId>,
    read_buf: Vec<u8>,
    arrivals: Vec<u32>,
}

struct MuxClient {
    target: SocketAddr,
    expected: usize,
    state: ClientState,
}

#[tina_runtime::isolate(message = MuxMsg)]
impl MuxClient {
    fn handle(&mut self, msg: MuxMsg, _ctx: &mut Context<'_, SingleShard, Self::Reply>) -> Effect<Self> {
        match msg {
            MuxMsg::Begin => tcp_connect(self.target).then(MuxMsg::Connected),
            MuxMsg::Connected(Ok((stream, _local, _peer))) => {
                self.state.stream = Some(stream);
                let mut payload = Vec::new();
                for &id in &REQUEST_IDS {
                    payload.extend_from_slice(format!("REQ {id}\n").as_bytes());
                }
                tcp_write(stream, payload).then(MuxMsg::Wrote)
            }
            MuxMsg::Wrote(Ok(_)) => {
                let stream = self.state.stream.expect("stream set after connect");
                tcp_read(stream, 4096).then(MuxMsg::Read)
            }
            MuxMsg::Read(Ok(bytes)) => {
                self.state.read_buf.extend_from_slice(&bytes);
                while let Some(idx) = self.state.read_buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = self.state.read_buf.drain(..=idx).collect();
                    let trimmed = std::str::from_utf8(&line[..line.len() - 1])
                        .expect("line utf8")
                        .trim();
                    if let Some(rest) = trimmed.strip_prefix("RESP ") {
                        let id: u32 = rest.parse().expect("RESP <id>");
                        self.state.arrivals.push(id);
                    }
                }
                let stream = self.state.stream.expect("stream set after connect");
                if self.state.arrivals.len() >= self.expected {
                    tcp_close_stream(stream).then(MuxMsg::Closed)
                } else {
                    tcp_read(stream, 4096).then(MuxMsg::Read)
                }
            }
            MuxMsg::Closed(Ok(())) => stop_with(std::mem::take(&mut self.state.arrivals)),
            MuxMsg::Connected(Err(_))
            | MuxMsg::Wrote(Err(_))
            | MuxMsg::Read(Err(_))
            | MuxMsg::Closed(Err(_)) => stop_with(std::mem::take(&mut self.state.arrivals)),
        }
    }
}

pub fn run() -> anyhow::Result<Report> {
    // The responder lives in a Tokio runtime on a dedicated thread;
    // we hand the address back to the Tina-driven client via std mpsc.
    let (server_addr_tx, server_addr_rx) = std::sync::mpsc::sync_channel::<SocketAddr>(1);
    let (server_done_tx, server_done_rx) = tokio::sync::oneshot::channel::<()>();
    let server_thread = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("server runtime");
        runtime.block_on(async move {
            let (addr, handle) = spawn_responder().await.expect("responder spawn");
            server_addr_tx.send(addr).expect("publish server addr");
            let _ = server_done_rx.await;
            handle.abort();
            let _ = handle.await;
        });
    });
    let target = server_addr_rx.recv()?;

    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);
    let address = runtime
        .register_with_capacity::<_, Infallible>(
            MuxClient {
                target,
                expected: REQUEST_IDS.len(),
                state: ClientState::default(),
            },
            16,
        )
        .map_err(|e| anyhow::anyhow!("register mux client: {e:?}"))?;

    let result = runtime
        .observe_result::<Vec<u32>, _, _>(address)
        .map_err(|e| anyhow::anyhow!("register result waiter: {e:?}"))?;
    runtime
        .try_send(address, MuxMsg::Begin)
        .map_err(|e| anyhow::anyhow!("kick mux client: {e:?}"))?;
    let arrival_order = result
        .wait(Duration::from_secs(3))
        .map_err(|e| anyhow::anyhow!("mux client finishes with arrivals: {e:?}"))?;

    let _ = runtime.shutdown();
    let _ = server_done_tx.send(());
    let _ = server_thread.join();

    Ok(Report { arrival_order })
}
