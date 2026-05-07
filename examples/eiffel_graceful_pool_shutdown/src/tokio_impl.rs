use std::time::Duration;

use tokio::runtime::Builder;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

use crate::{CALLERS, Report, SHUTDOWN_AFTER_MS, WORK_MS, WORKERS};

type Job = (usize, oneshot::Sender<()>);

pub fn run() -> anyhow::Result<Report> {
    let rt = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(async {
        let (tx, rx) = mpsc::channel::<Job>(64);
        let rx = std::sync::Arc::new(tokio::sync::Mutex::new(rx));

        let mut workers = JoinSet::new();
        for _ in 0..WORKERS {
            let rx = std::sync::Arc::clone(&rx);
            workers.spawn(async move {
                loop {
                    let job = {
                        let mut guard = rx.lock().await;
                        guard.recv().await
                    };
                    match job {
                        Some((_id, reply)) => {
                            tokio::time::sleep(Duration::from_millis(WORK_MS)).await;
                            let _ = reply.send(());
                        }
                        None => return,
                    }
                }
            });
        }

        let mut driver_set: JoinSet<Outcome> = JoinSet::new();
        for i in 0..CALLERS {
            let tx = tx.clone();
            driver_set.spawn(async move {
                let (rtx, rrx) = oneshot::channel::<()>();
                if tx.send((i, rtx)).await.is_err() {
                    return Outcome::Closed;
                }
                match rrx.await {
                    Ok(()) => Outcome::Completed,
                    Err(_) => Outcome::Closed,
                }
            });
        }
        drop(tx);

        // After a brief delay, abort the workers. In-flight callers
        // see their oneshot dropped → `Outcome::Closed`. Buffered
        // jobs in the channel still hold their reply oneshots, so
        // we drop the shared receiver too — that releases the
        // remaining oneshots and unblocks the queued callers.
        tokio::time::sleep(Duration::from_millis(SHUTDOWN_AFTER_MS)).await;
        workers.abort_all();
        while workers.join_next().await.is_some() {}
        drop(rx);

        let mut r = Report {
            callers: CALLERS,
            ..Default::default()
        };
        while let Some(joined) = driver_set.join_next().await {
            match joined {
                Ok(Outcome::Completed) => r.completed += 1,
                Ok(Outcome::Closed) => r.closed += 1,
                Err(_) => r.failed += 1,
            }
        }
        r.exit_clean = true;
        Ok(r)
    })
}

enum Outcome {
    Completed,
    Closed,
}
