//! Tokio reference: hand-rolled supervisor loop. `tokio::spawn` the
//! worker; if it panics, `JoinError::is_panic()` says so; respawn
//! with the same channel and the same shared counters. The
//! `Arc<AtomicU32>` counters and `Arc<Mutex<Receiver>>` survive the
//! panic because they live in the supervisor scope, not the worker.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::runtime::Builder;
use tokio::sync::{Mutex, mpsc};

use crate::{Job, Report, job_script};

pub fn run() -> anyhow::Result<Report> {
    let runtime = Builder::new_current_thread().enable_time().build()?;
    runtime.block_on(async_run())
}

async fn async_run() -> anyhow::Result<Report> {
    let (tx, rx) = mpsc::channel::<Job>(16);
    for job in job_script() {
        tx.send(job).await?;
    }
    drop(tx);

    let rx = Arc::new(Mutex::new(rx));
    let processed = Arc::new(AtomicU32::new(0));
    let poisoned = Arc::new(AtomicU32::new(0));
    let mut restarts: u32 = 0;

    loop {
        let rx_for_task = Arc::clone(&rx);
        let processed_for_task = Arc::clone(&processed);
        let poisoned_for_task = Arc::clone(&poisoned);

        let handle = tokio::spawn(async move {
            loop {
                let job = {
                    let mut guard = rx_for_task.lock().await;
                    guard.recv().await
                };
                let Some(job) = job else {
                    return;
                };
                match job {
                    Job::Work(_) => {
                        processed_for_task.fetch_add(1, Ordering::Relaxed);
                    }
                    Job::Poison => {
                        poisoned_for_task.fetch_add(1, Ordering::Relaxed);
                        panic!("supervised worker hit a poison job");
                    }
                }
            }
        });

        match handle.await {
            Ok(()) => break,
            Err(join_error) if join_error.is_panic() => {
                restarts += 1;
            }
            Err(other) => {
                anyhow::bail!("unexpected join error: {other:?}");
            }
        }
    }

    Ok(Report {
        processed: processed.load(Ordering::Relaxed),
        poisoned: poisoned.load(Ordering::Relaxed),
        restarts,
        exit_clean: true,
    })
}
