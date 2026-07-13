//! Tokio reference. A wraps the chain in `tokio::time::timeout`; B
//! and C are plain async functions that sleep. When the outer timer
//! fires, the inner futures are dropped — B and C never know.
//!
//! For the report, we cheat slightly: we know which hop is slow
//! because the script tells us. Tokio itself cannot recover this; the
//! README discussion calls that out.

use std::time::Duration;

use tokio::runtime::Builder;

use crate::{
    FAST_C_MS, REQUEST_COUNT, Report, SLOW_C_MS, TOTAL_DEADLINE_MS, c_is_domain_failure, c_is_slow,
};

pub fn run() -> anyhow::Result<Report> {
    let rt = Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let mut report = Report::default();
        for i in 0..REQUEST_COUNT {
            let outcome = service_a(i).await;
            match outcome {
                AOutcome::Success => report.successful += 1,
                AOutcome::Timeout => {
                    // Tokio's outer timeout names only the caller wait.
                    report.caller_timeout += 1;
                }
                AOutcome::DomainFailure => report.domain_failure += 1,
            }
        }
        report.exit_clean = true;
        Ok(report)
    })
}

enum AOutcome {
    Success,
    Timeout,
    DomainFailure,
}

async fn service_a(i: u32) -> AOutcome {
    let total = Duration::from_millis(TOTAL_DEADLINE_MS);
    match tokio::time::timeout(total, service_b(i)).await {
        Ok(Ok(())) => AOutcome::Success,
        Ok(Err(DomainFailure)) => AOutcome::DomainFailure,
        Err(_) => AOutcome::Timeout,
    }
}

#[derive(Debug)]
struct DomainFailure;

async fn service_b(i: u32) -> Result<(), DomainFailure> {
    // B is fast and just calls C.
    service_c(i).await
}

async fn service_c(i: u32) -> Result<(), DomainFailure> {
    if c_is_domain_failure(i) {
        return Err(DomainFailure);
    }
    let work = if c_is_slow(i) {
        Duration::from_millis(SLOW_C_MS)
    } else {
        Duration::from_millis(FAST_C_MS)
    };
    tokio::time::sleep(work).await;
    Ok(())
}
