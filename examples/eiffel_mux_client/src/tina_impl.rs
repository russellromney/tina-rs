//! Tina: a single `MuxClient` isolate owns the connection. The
//! parser, the read buffer, and the pending counter all live behind
//! the same mailbox — no shared `HashMap`, no oneshot per request.
//! Out-of-order arrival just works: the runtime delivers `tcp_read`
//! replies as bytes land, and the handler walks complete lines.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallError, DefaultThreadedMailboxFactory, StreamId, ThreadedRuntime, tcp_close_stream,
    tcp_connect, tcp_read, tcp_write,
};

use crate::{REQUEST_IDS, Report, spawn_responder};

/// Arrival log shared with the host so it can read the result after
/// the client isolate stops. App-specific data the runtime can't
/// know about; FINDINGS.md tracks it under "app-specific facts still
/// need ordinary state."
type ArrivalLog = Arc<Mutex<Vec<u32>>>;

#[derive(Debug, Clone)]
enum MuxMsg {
    Begin,
    Connected(Result<(StreamId, SocketAddr, SocketAddr), CallError>),
    Wrote(Result<usize, CallError>),
    Read(Result<Vec<u8>, CallError>),
    Closed(Result<(), CallError>),
}

/// State that's empty/derivable at spawn. The host configures
/// `target` and the shared `arrivals` log; everything else is
/// `Default` and the spawn site reads `state: ClientState::default()`.
#[derive(Debug, Default)]
struct ClientState {
    stream: Option<StreamId>,
    read_buf: Vec<u8>,
}

struct MuxClient {
    target: SocketAddr,
    arrivals: ArrivalLog,
    expected: usize,
    state: ClientState,
}

#[tina_runtime::isolate(message = MuxMsg)]
impl MuxClient {
    fn handle(&mut self, msg: MuxMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            MuxMsg::Begin => tcp_connect(self.target).reply(MuxMsg::Connected),
            MuxMsg::Connected(Ok((stream, _local, _peer))) => {
                self.state.stream = Some(stream);
                let mut payload = Vec::new();
                for &id in &REQUEST_IDS {
                    payload.extend_from_slice(format!("REQ {id}\n").as_bytes());
                }
                tcp_write(stream, payload).reply(MuxMsg::Wrote)
            }
            MuxMsg::Wrote(Ok(_)) => {
                let stream = self.state.stream.expect("stream set after connect");
                tcp_read(stream, 4096).reply(MuxMsg::Read)
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
                        self.arrivals.lock().expect("arrivals lock").push(id);
                    }
                }
                let received = self.arrivals.lock().expect("arrivals lock").len();
                let stream = self.state.stream.expect("stream set after connect");
                if received >= self.expected {
                    tcp_close_stream(stream).reply(MuxMsg::Closed)
                } else {
                    tcp_read(stream, 4096).reply(MuxMsg::Read)
                }
            }
            MuxMsg::Closed(Ok(())) => stop(),
            MuxMsg::Connected(Err(_))
            | MuxMsg::Wrote(Err(_))
            | MuxMsg::Read(Err(_))
            | MuxMsg::Closed(Err(_)) => stop(),
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

    let arrivals: ArrivalLog = Arc::default();
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);
    let address = runtime
        .register_with_capacity::<_, Infallible>(
            MuxClient {
                target,
                arrivals: Arc::clone(&arrivals),
                expected: REQUEST_IDS.len(),
                state: ClientState::default(),
            },
            16,
        )
        .map_err(|e| anyhow::anyhow!("register mux client: {e:?}"))?;

    let client_done = runtime.observe_isolate_complete(address);
    runtime
        .try_send(address, MuxMsg::Begin)
        .map_err(|e| anyhow::anyhow!("kick mux client: {e:?}"))?;
    client_done
        .wait(Duration::from_secs(3))
        .map_err(|e| anyhow::anyhow!("mux client finishes: {e:?}"))?;

    let _ = runtime.shutdown();
    let _ = server_done_tx.send(());
    let _ = server_thread.join();

    let arrival_order = arrivals.lock().expect("arrivals lock").clone();
    Ok(Report { arrival_order })
}
