//! Tokio reference. Bounded `tokio::sync::mpsc` channel as the queue,
//! a consumer task that processes one job per `RATE_WINDOW`.
//!
//! Producer pushes jobs with `Sender::try_send`. Past the channel
//! capacity (plus the slot currently held by the consumer) it gets
//! `TrySendError::Full`. The producer counts admitted vs full and
//! finishes once it has pushed [`BURST_JOBS`] times.
//!
//! Consumer awaits the next job, sleeps `RATE_WINDOW`, and counts.

use std::time::Duration;

use tokio::runtime::Builder;
use tokio::sync::mpsc;

use crate::{BURST_JOBS, QUEUE_CAPACITY, RATE_WINDOW_MS, Report};

pub fn run() -> anyhow::Result<Report> {
    let rt = Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let (tx, mut rx) = mpsc::channel::<u32>(QUEUE_CAPACITY);
        let mut report = Report::default();

        let consumer = tokio::spawn(async move {
            let mut processed = 0u32;
            while let Some(_job) = rx.recv().await {
                tokio::time::sleep(Duration::from_millis(RATE_WINDOW_MS)).await;
                processed += 1;
            }
            processed
        });

        for n in 0..BURST_JOBS {
            match tx.try_send(n) {
                Ok(()) => report.jobs_admitted += 1,
                Err(mpsc::error::TrySendError::Full(_)) => report.jobs_full += 1,
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    return Err(anyhow::anyhow!("consumer closed unexpectedly"));
                }
            }
        }
        drop(tx);

        let processed = consumer
            .await
            .map_err(|e| anyhow::anyhow!("consumer task: {e}"))?;
        report.jobs_processed = processed;
        report.exit_clean = true;
        Ok(report)
    })
}
