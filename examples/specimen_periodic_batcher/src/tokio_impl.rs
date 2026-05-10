//! Tokio reference. Canonical `tokio::select!` shape.
//!
//! Producer pushes items into a bounded `mpsc`. Batcher loops on
//! `select!`:
//!
//! - one arm receives the next item;
//! - the other arm wakes on `tokio::time::sleep_until(deadline)`.
//!
//! When the buffer reaches `BATCH_SIZE` or the timer fires with a
//! non-empty buffer, flush.

use std::time::{Duration, Instant};

use tokio::runtime::Builder;
use tokio::sync::mpsc;

use crate::{
    BATCH_INTERVAL_MS, BATCH_SIZE, PRODUCER_GAP_MS, Report, TOTAL_ITEMS, TRAILING_PAUSE_MS,
};

pub fn run() -> anyhow::Result<Report> {
    let rt = Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let (tx, mut rx) = mpsc::channel::<u32>(64);

        let consumer = tokio::spawn(async move {
            let interval = Duration::from_millis(BATCH_INTERVAL_MS);
            let mut buffer: Vec<u32> = Vec::with_capacity(BATCH_SIZE);
            let mut report = Report::default();
            // Deadline is "now + interval" only when the buffer has at
            // least one item. Otherwise wait forever.
            let mut deadline: Option<Instant> = None;

            loop {
                tokio::select! {
                    biased;
                    item = rx.recv() => {
                        match item {
                            Some(item) => {
                                report.items_seen += 1;
                                if buffer.is_empty() {
                                    deadline = Some(Instant::now() + interval);
                                }
                                buffer.push(item);
                                if buffer.len() >= BATCH_SIZE {
                                    buffer.clear();
                                    deadline = None;
                                    report.size_flushes += 1;
                                }
                            }
                            None => {
                                if !buffer.is_empty() {
                                    buffer.clear();
                                    report.timer_flushes += 1;
                                }
                                break;
                            }
                        }
                    }
                    _ = wait_until(deadline) => {
                        if !buffer.is_empty() {
                            buffer.clear();
                            deadline = None;
                            report.timer_flushes += 1;
                        }
                    }
                }
            }
            report
        });

        // Producer: 10 items at PRODUCER_GAP_MS, then the trailing
        // pause, then the last 2 items.
        for n in 0..(TOTAL_ITEMS - 2) {
            tx.send(n).await.expect("send");
            tokio::time::sleep(Duration::from_millis(PRODUCER_GAP_MS)).await;
        }
        tokio::time::sleep(Duration::from_millis(TRAILING_PAUSE_MS)).await;
        for n in (TOTAL_ITEMS - 2)..TOTAL_ITEMS {
            tx.send(n).await.expect("send");
            tokio::time::sleep(Duration::from_millis(PRODUCER_GAP_MS)).await;
        }
        // Give the trailing batch room for its timer flush before
        // closing the channel.
        tokio::time::sleep(Duration::from_millis(BATCH_INTERVAL_MS * 3)).await;
        drop(tx);

        let mut report = consumer
            .await
            .map_err(|e| anyhow::anyhow!("consumer: {e}"))?;
        report.exit_clean = true;
        Ok(report)
    })
}

async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
        // No buffered items: wait forever for the recv arm to wake us.
        None => std::future::pending::<()>().await,
    }
}
