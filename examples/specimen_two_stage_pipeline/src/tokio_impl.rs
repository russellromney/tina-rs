use tokio::runtime::Builder;
use tokio::task::JoinSet;

use crate::{REQUESTS, Report, Stage, classify};

pub fn run() -> anyhow::Result<Report> {
    let rt = Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let mut joins: JoinSet<Stage> = JoinSet::new();
        for i in 0..REQUESTS {
            joins.spawn(async move {
                let parsed = match parse(i).await {
                    Ok(p) => p,
                    Err(()) => return Stage::ParseFailure,
                };
                let validated = match validate(parsed).await {
                    Ok(v) => v,
                    Err(()) => return Stage::ValidateFailure,
                };
                let _ = execute(validated).await;
                Stage::Ok
            });
        }
        let mut r = Report {
            requests: REQUESTS,
            ..Default::default()
        };
        while let Some(joined) = joins.join_next().await {
            match joined.map_err(|e| anyhow::anyhow!("task: {e}"))? {
                Stage::Ok => r.completed += 1,
                Stage::ParseFailure => r.parse_failed += 1,
                Stage::ValidateFailure => r.validate_failed += 1,
            }
        }
        r.exit_clean = true;
        Ok(r)
    })
}

async fn parse(i: usize) -> Result<usize, ()> {
    match classify(i) {
        Stage::ParseFailure => Err(()),
        _ => Ok(i),
    }
}

async fn validate(i: usize) -> Result<usize, ()> {
    match classify(i) {
        Stage::ValidateFailure => Err(()),
        _ => Ok(i),
    }
}

async fn execute(_i: usize) -> Result<(), ()> {
    Ok(())
}
