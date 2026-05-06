use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallError, DefaultThreadedMailboxFactory, StreamId, ThreadedRuntime, ThreadedRuntimeConfig,
    tcp_close_stream, tcp_connect, tcp_read, tcp_write,
};

use super::{REQUEST_IDS, SideReport, spawn_responder};

type ArrivalLog = Arc<Mutex<Vec<u32>>>;

#[derive(Debug, Clone)]
#[allow(dead_code)]
enum MuxMsg {
    Begin,
    Connected(Result<(StreamId, SocketAddr, SocketAddr), CallError>),
    Wrote(Result<usize, CallError>),
    Read(Result<Vec<u8>, CallError>),
    Closed(Result<(), CallError>),
}

#[derive(Debug)]
struct MuxClient {
    target: SocketAddr,
    stream: Option<StreamId>,
    arrivals: ArrivalLog,
    pending: usize,
    read_buf: Vec<u8>,
}

#[tina_runtime::isolate(message = MuxMsg)]
impl MuxClient {
    fn handle(&mut self, msg: MuxMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            MuxMsg::Begin => tcp_connect(self.target).reply(MuxMsg::Connected),
            MuxMsg::Connected(Ok((stream, _local, _peer))) => {
                self.stream = Some(stream);
                let mut payload = Vec::new();
                for &id in &REQUEST_IDS {
                    payload.extend_from_slice(format!("REQ {id}\n").as_bytes());
                }
                tcp_write(stream, payload).reply(MuxMsg::Wrote)
            }
            MuxMsg::Connected(Err(_)) => stop(),
            MuxMsg::Wrote(Ok(_)) => {
                let stream = self.stream.expect("stream set after connect");
                tcp_read(stream, 4096).reply(MuxMsg::Read)
            }
            MuxMsg::Wrote(Err(_)) => stop(),
            MuxMsg::Read(Ok(bytes)) => {
                self.read_buf.extend_from_slice(&bytes);
                while let Some(idx) = self.read_buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = self.read_buf.drain(..=idx).collect();
                    let trimmed = std::str::from_utf8(&line[..line.len() - 1])
                        .expect("line is utf8")
                        .trim();
                    if let Some(rest) = trimmed.strip_prefix("RESP ") {
                        let id: u32 = rest.parse().expect("response id parses");
                        self.arrivals.lock().expect("arrivals lock").push(id);
                        self.pending = self.pending.saturating_sub(1);
                    }
                }

                if self.pending == 0 {
                    let stream = self.stream.expect("stream set after connect");
                    tcp_close_stream(stream).reply(MuxMsg::Closed)
                } else {
                    let stream = self.stream.expect("stream set after connect");
                    tcp_read(stream, 4096).reply(MuxMsg::Read)
                }
            }
            MuxMsg::Read(Err(_)) | MuxMsg::Closed(_) => stop(),
        }
    }
}

pub(crate) fn run() -> SideReport {
    // Server lives in a Tokio runtime on a dedicated thread; we hand the
    // address back to the Tina-driven client.
    let (server_addr_tx, server_addr_rx) = std::sync::mpsc::sync_channel::<SocketAddr>(1);
    let (server_done_tx, server_done_rx) = tokio::sync::oneshot::channel::<()>();
    let server_thread = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("server runtime");
        runtime.block_on(async move {
            let (addr, handle) = spawn_responder().await;
            server_addr_tx.send(addr).expect("publish server addr");
            let _ = server_done_rx.await;
            handle.abort();
            let _ = handle.await;
        });
    });

    let target = server_addr_rx.recv().expect("server address arrives");

    let arrivals: ArrivalLog = Arc::default();
    let runtime = ThreadedRuntime::with_config(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 32,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let address = runtime
        .register_with_capacity::<MuxClient, Infallible>(
            MuxClient {
                target,
                stream: None,
                arrivals: Arc::clone(&arrivals),
                pending: REQUEST_IDS.len(),
                read_buf: Vec::new(),
            },
            16,
        )
        .expect("register mux client");
    let client_done = runtime.observe_isolate_complete(address);
    runtime
        .try_send(address, MuxMsg::Begin)
        .expect("kick mux client");

    client_done
        .wait(Duration::from_secs(3))
        .expect("mux client finishes");

    let _ = runtime.shutdown();
    let _ = server_done_tx.send(());
    let _ = server_thread.join();

    let arrival_order = arrivals.lock().expect("arrivals lock").clone();
    SideReport {
        arrival_order,
        request_ids: REQUEST_IDS.to_vec(),
    }
}
