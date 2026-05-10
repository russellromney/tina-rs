//! Tokio reference. Bounded `mpsc` for jobs, a `oneshot` for
//! shutdown, a consumer task with `tokio::select!` between the two.
//! Drain semantics are composed in user code: after shutdown, keep
//! recv'ing until the channel is empty.

use std::time::Duration;

use tokio::runtime::Builder;
use tokio::sync::{mpsc, oneshot};

use crate::{BURST_JOBS, JOB_WORK_MS, QUEUE_CAPACITY, Report};

pub fn run() -> anyhow::Result<Report> {
    let rt = Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let (tx, mut rx) = mpsc::channel::<u32>(QUEUE_CAPACITY);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
        let mut report = Report::default();

        let consumer = tokio::spawn(async move {
            let mut consumer_report = ConsumerReport::default();
            let mut shutdown_seen = false;
            loop {
                if shutdown_seen {
                    // Drain mode: keep pulling until the channel is
                    // empty. `try_recv` is the right primitive; it
                    // does not park the task.
                    match rx.try_recv() {
                        Ok(_) => {
                            tokio::time::sleep(Duration::from_millis(JOB_WORK_MS)).await;
                            consumer_report.processed += 1;
                        }
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => break,
                    }
                } else {
                    tokio::select! {
                        biased;
                        item = rx.recv() => {
                            match item {
                                Some(_) => {
                                    tokio::time::sleep(Duration::from_millis(JOB_WORK_MS)).await;
                                    consumer_report.processed += 1;
                                }
                                None => break,
                            }
                        }
                        _ = &mut shutdown_rx => {
                            shutdown_seen = true;
                            consumer_report.shutdown_observed = true;
                        }
                    }
                }
            }
            consumer_report
        });

        // Producer burst.
        for n in 0..BURST_JOBS {
            match tx.try_send(n) {
                Ok(()) => report.items_admitted += 1,
                Err(mpsc::error::TrySendError::Full(_)) => report.items_full += 1,
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    return Err(anyhow::anyhow!("consumer closed during burst"));
                }
            }
        }

        // Signal shutdown. Keep `tx` alive so that the consumer's
        // `shutdown_rx` arm wins the next select round (otherwise
        // dropping `tx` here makes `recv()` return `None` first, the
        // consumer breaks via the channel-closed path, and the
        // explicit shutdown signal is lost).
        let _ = shutdown_tx.send(());

        let consumer_report = consumer
            .await
            .map_err(|e| anyhow::anyhow!("consumer task: {e}"))?;
        drop(tx);
        report.items_processed = consumer_report.processed;
        report.shutdown_observed = consumer_report.shutdown_observed;
        report.exit_clean = true;
        Ok(report)
    })
}

#[derive(Default)]
struct ConsumerReport {
    processed: u32,
    shutdown_observed: bool,
}
