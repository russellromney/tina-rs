use std::time::Duration;

use tokio::runtime::Builder;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::{COLD_WRITES_PER_SHARD, HOT_WRITES, PER_WRITE_MS, Report, SHARD_MAILBOX, SHARDS};

pub fn run() -> anyhow::Result<Report> {
    let rt = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(async {
        let mut txs = Vec::with_capacity(SHARDS as usize);
        let mut workers = JoinSet::new();
        for _ in 0..SHARDS {
            let (tx, mut rx) = mpsc::channel::<()>(SHARD_MAILBOX);
            txs.push(tx);
            workers.spawn(async move {
                while rx.recv().await.is_some() {
                    tokio::time::sleep(Duration::from_millis(PER_WRITE_MS)).await;
                }
            });
        }

        let mut report = Report::default();
        // Hot key: index 0.
        for _ in 0..HOT_WRITES {
            match txs[0].try_send(()) {
                Ok(()) => report.hot_admitted += 1,
                Err(mpsc::error::TrySendError::Full(_)) => report.hot_rejected += 1,
                Err(mpsc::error::TrySendError::Closed(_)) => report.hot_terminal += 1,
            }
        }
        // Cold keys: round-robin across the remaining shards.
        let cold_shards = (SHARDS - 1) as usize;
        for i in 0..(cold_shards * COLD_WRITES_PER_SHARD as usize) {
            let shard = 1 + (i % cold_shards);
            match txs[shard].try_send(()) {
                Ok(()) => report.cold_admitted += 1,
                Err(mpsc::error::TrySendError::Full(_)) => report.cold_rejected += 1,
                Err(mpsc::error::TrySendError::Closed(_)) => report.cold_terminal += 1,
            }
        }
        drop(txs);
        while workers.join_next().await.is_some() {}

        report.exit_clean = true;
        Ok(report)
    })
}
