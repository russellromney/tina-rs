use std::time::Duration;
use tokio::time;

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub probe_delay_ms: u64,
    pub db_delay_ms: u64,
}

pub struct RunReport {
    pub replies: Vec<String>,
}

/// Per-step readiness deadline, matching the Tina side's call timeout.
const DEADLINE_MS: u64 = 50;

async fn probe(delay_ms: u64) -> Result<(), &'static str> {
    time::sleep(Duration::from_millis(delay_ms)).await;
    if delay_ms <= DEADLINE_MS {
        Ok(())
    } else {
        Err("probe slow")
    }
}

async fn db_ping(delay_ms: u64) -> Result<(), &'static str> {
    time::sleep(Duration::from_millis(delay_ms)).await;
    if delay_ms <= DEADLINE_MS {
        Ok(())
    } else {
        Err("db slow")
    }
}

pub async fn run(config: RunConfig) -> anyhow::Result<RunReport> {
    // Same readiness shape as the Tina side: probe, then db, each with a
    // deadline; a slow dependency means not_ready.
    let ready = match probe(config.probe_delay_ms).await {
        Ok(()) => db_ping(config.db_delay_ms).await.is_ok(),
        Err(_) => false,
    };
    Ok(RunReport {
        replies: vec![String::from(if ready { "ready" } else { "not_ready" })],
    })
}
