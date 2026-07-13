//! Tokio reference. JoinSet + abort_all is the canonical mid-flight
//! cancellation pattern. The driver awaits on `join_next()` until
//! the cancel timer fires, then aborts every outstanding task.

use std::time::Duration;

use tokio::runtime::Builder;
use tokio::task::JoinSet;

use crate::{CANCEL_AFTER_MS, FANOUT, Report, WORK_MS};

pub fn run() -> anyhow::Result<Report> {
    let rt = Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let mut joins: JoinSet<u32> = JoinSet::new();
        for i in 0..FANOUT {
            joins.spawn(async move {
                tokio::time::sleep(Duration::from_millis(WORK_MS)).await;
                i
            });
        }

        let cancel = tokio::time::sleep(Duration::from_millis(CANCEL_AFTER_MS));
        tokio::pin!(cancel);

        let mut report = Report::default();
        loop {
            tokio::select! {
                biased;
                _ = &mut cancel, if !report.cancel_observed => {
                    joins.abort_all();
                    report.cancel_observed = true;
                }
                joined = joins.join_next() => {
                    match joined {
                        Some(Ok(_)) => report.replies_before_cancel += 1,
                        Some(Err(e)) if e.is_cancelled() => {
                            // Aborted task; nothing to count.
                        }
                        Some(Err(e)) => return Err(anyhow::anyhow!("worker task: {e}")),
                        None => break,
                    }
                }
            }
        }
        report.settlement_complete = true;
        report.exit_clean = true;
        Ok(report)
    })
}
