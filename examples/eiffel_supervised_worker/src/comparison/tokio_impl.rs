use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::runtime::Builder;
use tokio::sync::Mutex;
use tokio::sync::mpsc;

use super::{Job, SideReport, job_script};

pub(crate) fn run() -> SideReport {
    let runtime = Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("tokio current_thread runtime");
    runtime.block_on(async_run())
}

async fn async_run() -> SideReport {
    let (tx, rx) = mpsc::channel::<Job>(16);
    for job in job_script() {
        tx.send(job).await.expect("send job");
    }
    drop(tx);

    let rx = Arc::new(Mutex::new(rx));
    let processed = Arc::new(AtomicU32::new(0));
    let poisoned = Arc::new(AtomicU32::new(0));
    let mut restarts: u32 = 0;

    // Manual supervisor loop: respawn the worker every time it panics, until
    // the channel is drained. Tokio gives us the panic via `JoinError`, but
    // every other property (channel-survives-panic, counter-survives-panic,
    // shared budget) is hand-rolled here.
    loop {
        let rx_for_task = Arc::clone(&rx);
        let processed_for_task = Arc::clone(&processed);
        let poisoned_for_task = Arc::clone(&poisoned);

        let handle = tokio::spawn(async move {
            let rx = rx_for_task;
            let processed = processed_for_task;
            let poisoned = poisoned_for_task;
            loop {
                let job = {
                    let mut guard = rx.lock().await;
                    guard.recv().await
                };
                let Some(job) = job else {
                    return WorkerExit::Drained;
                };
                match job {
                    Job::Work(_) => {
                        processed.fetch_add(1, Ordering::Relaxed);
                    }
                    Job::Poison => {
                        poisoned.fetch_add(1, Ordering::Relaxed);
                        panic!("supervised worker hit a poison job");
                    }
                }
            }
        });

        match handle.await {
            Ok(WorkerExit::Drained) => break,
            Err(join_error) if join_error.is_panic() => {
                restarts += 1;
            }
            Err(other) => {
                eprintln!("unexpected join error: {other:?}");
                return SideReport {
                    processed: processed.load(Ordering::Relaxed),
                    poisoned: poisoned.load(Ordering::Relaxed),
                    restarts,
                    exit_clean: false,
                };
            }
        }
    }

    SideReport {
        processed: processed.load(Ordering::Relaxed),
        poisoned: poisoned.load(Ordering::Relaxed),
        restarts,
        exit_clean: true,
    }
}

enum WorkerExit {
    Drained,
}
