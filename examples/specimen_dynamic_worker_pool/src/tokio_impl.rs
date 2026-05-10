//! Tokio reference. Standard `JoinSet` pattern: spawn N workers,
//! loop on `join_next()`, accumulate partials.

use tokio::runtime::Builder;
use tokio::task::JoinSet;

use crate::{Report, WORK_VALUES, WORKER_COUNT};

pub fn run() -> anyhow::Result<Report> {
    let rt = Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let chunk_size = WORK_VALUES.len() / WORKER_COUNT as usize;
        let mut joins: JoinSet<u64> = JoinSet::new();
        for i in 0..WORKER_COUNT {
            let start = i as usize * chunk_size;
            let end = start + chunk_size;
            let slice: Vec<u64> = WORK_VALUES[start..end].to_vec();
            joins.spawn(async move { slice.iter().copied().sum() });
        }

        let mut report = Report::default();
        while let Some(joined) = joins.join_next().await {
            let partial = joined.map_err(|e| anyhow::anyhow!("worker task: {e}"))?;
            report.results_collected += 1;
            report.total_sum += partial;
        }
        report.exit_clean = true;
        Ok(report)
    })
}
