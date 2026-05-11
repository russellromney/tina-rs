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

async fn probe_ok() -> Result<(), &'static str> {
    time::sleep(Duration::from_millis(10)).await;
    Ok(())
}

async fn db_ping() -> Result<(), &'static str> {
    time::sleep(Duration::from_millis(10)).await;
    Ok(())
}

fn final_reply() -> String {
    String::from("ready")
}

pub async fn run(config: RunConfig) -> anyhow::Result<RunReport> {
    let mut replies = Vec::new();

    probe_ok()
        .await
        .map_err(|e| anyhow::anyhow!("probe failed: {}", e))?;
    db_ping()
        .await
        .map_err(|e| anyhow::anyhow!("db ping failed: {}", e))?;
    replies.push(final_reply());

    time::sleep(Duration::from_millis(config.probe_delay_ms)).await;
    time::sleep(Duration::from_millis(config.db_delay_ms)).await;

    Ok(RunReport { replies })
}
