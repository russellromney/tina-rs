use std::time::{Duration, Instant};

use tokio::runtime::Builder;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

use crate::{BATCH_SIZE, BATCH_TIMEOUT_MS, CALLERS, Report, SUBMISSION_CAPACITY};

type Job = (u64, oneshot::Sender<u64>);

pub fn run() -> anyhow::Result<Report> {
    let rt = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(async {
        let (tx, mut rx) = mpsc::channel::<Job>(SUBMISSION_CAPACITY);

        let batcher = tokio::spawn(async move {
            let mut items: Vec<u64> = Vec::with_capacity(BATCH_SIZE);
            let mut replies: Vec<oneshot::Sender<u64>> = Vec::with_capacity(BATCH_SIZE);
            let mut size_flushes = 0usize;
            let mut timer_flushes = 0usize;
            let mut deadline: Option<Instant> = None;

            loop {
                let next_deadline = deadline;
                let timer = async move {
                    match next_deadline {
                        Some(at) => {
                            tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::pin!(timer);
                tokio::select! {
                    biased;
                    msg = rx.recv() => {
                        match msg {
                            Some((item, reply)) => {
                                if items.is_empty() {
                                    deadline = Some(Instant::now() + Duration::from_millis(BATCH_TIMEOUT_MS));
                                }
                                items.push(item);
                                replies.push(reply);
                                if items.len() >= BATCH_SIZE {
                                    size_flushes += 1;
                                    flush(&mut items, &mut replies);
                                    deadline = None;
                                }
                            }
                            None => {
                                if !items.is_empty() {
                                    timer_flushes += 1;
                                    flush(&mut items, &mut replies);
                                }
                                break;
                            }
                        }
                    }
                    _ = &mut timer => {
                        if !items.is_empty() {
                            timer_flushes += 1;
                            flush(&mut items, &mut replies);
                        }
                        deadline = None;
                    }
                }
            }
            (size_flushes, timer_flushes)
        });

        let mut driver_set: JoinSet<bool> = JoinSet::new();
        for i in 0..CALLERS as u64 {
            let item = i + 1;
            let tx = tx.clone();
            driver_set.spawn(async move {
                let (rtx, rrx) = oneshot::channel::<u64>();
                if tx.send((item, rtx)).await.is_err() {
                    return false;
                }
                rrx.await.is_ok()
            });
        }
        drop(tx);

        let mut report = Report {
            callers: CALLERS,
            ..Default::default()
        };
        while let Some(joined) = driver_set.join_next().await {
            match joined {
                Ok(true) => report.successes += 1,
                Ok(false) => report.failed += 1,
                Err(_) => report.failed += 1,
            }
        }

        let (size_flushes, timer_flushes) = batcher
            .await
            .map_err(|e| anyhow::anyhow!("batcher task: {e}"))?;
        report.batches_size_flushed = size_flushes;
        report.batches_timer_flushed = timer_flushes;
        report.exit_clean = true;
        Ok(report)
    })
}

fn flush(items: &mut Vec<u64>, replies: &mut Vec<oneshot::Sender<u64>>) {
    let total: u64 = items.iter().sum();
    items.clear();
    for reply in replies.drain(..) {
        let _ = reply.send(total);
    }
}
