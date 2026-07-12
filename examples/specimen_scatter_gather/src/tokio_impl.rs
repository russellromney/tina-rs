//! Tokio reference: workers as `tokio::spawn` tasks behind `mpsc`,
//! a coordinator task that spawns one fan-out sub-task per client
//! query, and an `oneshot` per query for the aggregate.
//!
//! Idiomatic Tokio. The coordinator's in-flight queries are bounded
//! only by the coordinator-inbound `mpsc::channel` capacity; each
//! admitted query gets a sub-task plus a `HashMap`-free oneshot,
//! and Tokio's runtime caps total parallelism implicitly via thread
//! count and FD limits. There is no named cap on "queries the
//! coordinator is currently coordinating."

use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Builder;
use tokio::sync::{mpsc, oneshot};

use crate::{CLIENTS, MAX_IN_FLIGHT, Report, WORKERS, expected_aggregate};

#[derive(Debug)]
struct WorkerReq {
    payload: u64,
    reply: oneshot::Sender<u64>,
}

#[derive(Debug)]
struct CoordReq {
    payload: u64,
    reply: oneshot::Sender<u64>,
}

pub fn run() -> anyhow::Result<Report> {
    let runtime = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()?;
    Ok(runtime.block_on(driver()))
}

async fn driver() -> Report {
    // Spawn the workers.
    let mut worker_txs: Vec<mpsc::Sender<WorkerReq>> = Vec::with_capacity(WORKERS);
    for w in 0..WORKERS as u64 {
        let (tx, mut rx) = mpsc::channel::<WorkerReq>(8);
        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                let _ = req.reply.send(req.payload.wrapping_add(w));
            }
        });
        worker_txs.push(tx);
    }
    let workers = Arc::new(worker_txs);

    // Spawn the coordinator. Bounded inbox; each admitted query
    // becomes its own fan-out sub-task. The coordinator does not
    // hold a HashMap<RequestId, oneshot::Sender>: every query owns
    // its own oneshot. That avoids one anti-pattern but the
    // sub-task spawn itself has no named cap.
    let (coord_tx, mut coord_rx) = mpsc::channel::<CoordReq>(MAX_IN_FLIGHT);
    {
        let workers = Arc::clone(&workers);
        tokio::spawn(async move {
            while let Some(query) = coord_rx.recv().await {
                let workers = Arc::clone(&workers);
                tokio::spawn(async move {
                    let mut waits = Vec::with_capacity(WORKERS);
                    for tx in workers.iter() {
                        let (rtx, rrx) = oneshot::channel();
                        if tx
                            .send(WorkerReq {
                                payload: query.payload,
                                reply: rtx,
                            })
                            .await
                            .is_err()
                        {
                            let _ = query.reply.send(0);
                            return;
                        }
                        waits.push(rrx);
                    }
                    let mut sum = 0u64;
                    for w in waits {
                        match w.await {
                            Ok(v) => sum = sum.wrapping_add(v),
                            Err(_) => return, // worker dropped: caller times out below
                        }
                    }
                    let _ = query.reply.send(sum);
                });
            }
        });
    }

    // Drive CLIENTS queries concurrently. Each client awaits its
    // own oneshot with a deadline; report the aggregate.
    let mut tasks = Vec::with_capacity(CLIENTS);
    for c in 0..CLIENTS as u64 {
        let coord = coord_tx.clone();
        let payload = c.wrapping_mul(7).wrapping_add(11);
        tasks.push(tokio::spawn(async move {
            let (rtx, rrx) = oneshot::channel();
            if coord
                .send(CoordReq {
                    payload,
                    reply: rtx,
                })
                .await
                .is_err()
            {
                return (payload, None);
            }
            let answer = tokio::time::timeout(Duration::from_secs(2), rrx).await;
            match answer {
                Ok(Ok(sum)) => (payload, Some(sum)),
                _ => (payload, None),
            }
        }));
    }
    drop(coord_tx);

    let mut report = Report {
        clients: CLIENTS,
        workers: WORKERS,
        ..Default::default()
    };
    for task in tasks {
        match task.await {
            Ok((payload, Some(sum))) if sum == expected_aggregate(payload) => {
                report.aggregates_correct += 1;
            }
            Ok((_, Some(_))) => report.aggregates_wrong += 1,
            _ => report.failed += 1,
        }
    }
    report.exit_clean = true;
    report
}
