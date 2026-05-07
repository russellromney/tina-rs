use std::time::Duration;

use tokio::runtime::Builder;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

use crate::{CLIENTS, Report, WORKERS, expected_for};

type Job = (u64, oneshot::Sender<u64>);

pub fn run() -> anyhow::Result<Report> {
    let rt = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(async {
        let mut worker_txs = Vec::with_capacity(WORKERS);
        let mut worker_set = JoinSet::new();
        for w in 0..WORKERS as u64 {
            let (tx, mut rx) = mpsc::channel::<Job>(8);
            worker_txs.push(tx);
            let work = Duration::from_millis(5 + (w * 7) % 20);
            worker_set.spawn(async move {
                while let Some((payload, reply)) = rx.recv().await {
                    tokio::time::sleep(work).await;
                    let _ = reply.send(payload + w);
                }
            });
        }

        let mut driver_set = JoinSet::new();
        for i in 0..CLIENTS {
            let payload = (i as u64).wrapping_mul(11) + 1;
            let worker_idx = i % WORKERS;
            let tx = worker_txs[worker_idx].clone();
            driver_set.spawn(async move {
                let (reply_tx, reply_rx) = oneshot::channel::<u64>();
                tx.send((payload, reply_tx))
                    .await
                    .map_err(|_| anyhow::anyhow!("worker channel closed"))?;
                let got = reply_rx.await?;
                let want = expected_for(payload, worker_idx as u64);
                Ok::<_, anyhow::Error>((got, want))
            });
        }

        let mut report = Report {
            clients: CLIENTS,
            ..Default::default()
        };
        while let Some(joined) = driver_set.join_next().await {
            match joined.map_err(|e| anyhow::anyhow!("driver task: {e}"))? {
                Ok((got, want)) if got == want => report.correct_replies += 1,
                Ok(_) => report.wrong_replies += 1,
                Err(_) => report.failed += 1,
            }
        }
        drop(worker_txs);
        while worker_set.join_next().await.is_some() {}

        report.exit_clean = true;
        Ok(report)
    })
}
