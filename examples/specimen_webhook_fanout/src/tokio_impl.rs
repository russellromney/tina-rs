use std::time::Duration;

use tokio::runtime::Builder;
use tokio::task::JoinSet;

use crate::Report;
use crate::upstream::{self, Upstream};

const PER_CALL_TIMEOUT: Duration = Duration::from_millis(150);

pub fn run() -> anyhow::Result<Report> {
    let upstream = upstream::spawn(&upstream::workload())?;
    let result = run_inner(&upstream);
    upstream.stop();
    result
}

fn run_inner(upstream: &Upstream) -> anyhow::Result<Report> {
    let rt = Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(PER_CALL_TIMEOUT)
            .build()
            .expect("reqwest client");
        let mut joins: JoinSet<Outcome> = JoinSet::new();
        for addr in &upstream.addrs {
            let url = format!("http://{addr}/hook");
            let client = client.clone();
            joins.spawn(async move {
                match client.get(&url).send().await {
                    Ok(resp) if resp.status().is_success() => Outcome::Delivered,
                    Ok(resp) if resp.status().as_u16() == 503 => Outcome::Unavailable,
                    Ok(_) => Outcome::Other,
                    Err(e) if e.is_timeout() => Outcome::Timeout,
                    Err(_) => Outcome::Other,
                }
            });
        }
        let mut report = Report::default();
        while let Some(joined) = joins.join_next().await {
            match joined.map_err(|e| anyhow::anyhow!("dispatch task: {e}"))? {
                Outcome::Delivered => report.delivered += 1,
                Outcome::Unavailable => report.server_unavailable += 1,
                Outcome::Timeout => report.timed_out += 1,
                Outcome::Other => report.other += 1,
            }
        }
        report.exit_clean = true;
        Ok(report)
    })
}

enum Outcome {
    Delivered,
    Unavailable,
    Timeout,
    Other,
}
