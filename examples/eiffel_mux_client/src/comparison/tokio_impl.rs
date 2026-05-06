use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

use super::{REQUEST_IDS, SideReport, connect, spawn_responder};

pub(crate) fn run() -> SideReport {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    runtime.block_on(async move {
        let (addr, server) = spawn_responder().await;
        let stream = connect(addr).await;
        let (read_half, mut write_half) = stream.into_split();

        let pending: Arc<Mutex<HashMap<u32, oneshot::Sender<()>>>> = Arc::default();
        let arrival_order: Arc<Mutex<Vec<u32>>> = Arc::default();

        let reader_pending = Arc::clone(&pending);
        let reader_order = Arc::clone(&arrival_order);
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
                    .expect("client parses response id");
                reader_order.lock().expect("order lock").push(id);
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
                .await
                .expect("client write");
            waiters.push(rx);
        }

        for rx in waiters {
            rx.await.expect("response delivered");
        }

        drop(write_half);
        let _ = reader.await;
        server.abort();
        let _ = server.await;

        SideReport {
            arrival_order: arrival_order.lock().expect("order lock").clone(),
            request_ids: REQUEST_IDS.to_vec(),
        }
    })
}
