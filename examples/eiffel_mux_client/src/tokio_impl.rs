//! Tokio reference: classic shared-state multiplexed client. A
//! reader task drains the socket and looks each parsed id up in an
//! `Arc<Mutex<HashMap<u32, oneshot::Sender<()>>>>`; the writer side
//! inserts into the map before each `write_all` and awaits the
//! oneshot. The `Arc<Mutex<HashMap<...>>>` is the price of admission
//! for "many in-flight requests on one connection" in vanilla
//! Tokio.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

use crate::{REQUEST_IDS, Report, spawn_responder};

pub fn run() -> anyhow::Result<Report> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async move {
        let (addr, server) = spawn_responder().await?;
        let stream = TcpStream::connect(addr).await?;
        let (read_half, mut write_half) = stream.into_split();

        let pending: Arc<Mutex<HashMap<u32, oneshot::Sender<()>>>> = Arc::default();
        let arrivals: Arc<Mutex<Vec<u32>>> = Arc::default();

        let reader_pending = Arc::clone(&pending);
        let reader_arrivals = Arc::clone(&arrivals);
        let reader = tokio::spawn(async move {
            let mut reader = BufReader::new(read_half);
            let mut line = String::new();
            loop {
                line.clear();
                let n = reader.read_line(&mut line).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                let id: u32 = line
                    .trim()
                    .strip_prefix("RESP ")
                    .and_then(|rest| rest.parse().ok())
                    .expect("RESP <id>");
                reader_arrivals.lock().expect("arrivals lock").push(id);
                if let Some(tx) = reader_pending.lock().expect("pending lock").remove(&id) {
                    let _ = tx.send(());
                }
            }
        });

        let mut waiters = Vec::new();
        for &id in &REQUEST_IDS {
            let (tx, rx) = oneshot::channel::<()>();
            pending.lock().expect("pending lock").insert(id, tx);
            write_half
                .write_all(format!("REQ {id}\n").as_bytes())
                .await?;
            waiters.push(rx);
        }
        for rx in waiters {
            rx.await?;
        }

        drop(write_half);
        let _ = reader.await;
        server.abort();
        let _ = server.await;

        let arrival_order = arrivals.lock().expect("arrivals lock").clone();
        Ok(Report { arrival_order })
    })
}
