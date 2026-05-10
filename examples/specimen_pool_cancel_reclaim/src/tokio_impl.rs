//! Tokio side. Uses `tokio::sync::Semaphore` as the bounded pool, and
//! aborts each waiter task to model `cancel_call`. Captures the same
//! invariants the tina side does.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinSet;

use crate::{Report, WAITERS};

pub fn run() -> anyhow::Result<Report> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()?;

    let report = runtime.block_on(async move {
        let sem = Arc::new(Semaphore::new(1));
        let release_notify = Arc::new(Notify::new());

        // Prime: take the only permit.
        let prime = sem.clone().acquire_owned().await.expect("permit");

        // Waiter wave: spawn WAITERS tasks each trying to acquire.
        let mut waiters: JoinSet<()> = JoinSet::new();
        for _ in 0..WAITERS {
            let sem = sem.clone();
            waiters.spawn(async move {
                // This will park because the only permit is held.
                let _permit = sem.acquire_owned().await;
                // Sit on the permit until told to drop.
                std::future::pending::<()>().await;
            });
        }

        // Give the waiters a moment to park.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Cancel: abort all waiters.
        let cancelled_signal = waiters.len();
        waiters.abort_all();
        while waiters.join_next().await.is_some() {}

        // Retry wave: spawn another set; each tries acquire and reports.
        let mut retried_full = 0usize;
        let mut retried_resourced = 0usize;
        let mut retries: JoinSet<RetryOutcome> = JoinSet::new();
        for _ in 0..WAITERS {
            let sem = sem.clone();
            let notify = release_notify.clone();
            retries.spawn(async move {
                // try_acquire would surface "Full" semantics — but
                // tokio's Semaphore has no waiter cap; it's always
                // willing to park. Use try_acquire_owned to mirror
                // the pool's max_waiters behaviour.
                match sem.clone().try_acquire_owned() {
                    Ok(permit) => {
                        // Got it on first try (the prime hasn't released
                        // yet). Hold it until release_notify fires, then
                        // drop.
                        notify.notified().await;
                        drop(permit);
                        RetryOutcome::Resourced
                    }
                    Err(_) => {
                        // Park.
                        match sem.acquire_owned().await {
                            Ok(permit) => {
                                drop(permit);
                                RetryOutcome::Resourced
                            }
                            Err(_) => RetryOutcome::Closed,
                        }
                    }
                }
            });
        }

        let retried_admitted = WAITERS;

        tokio::time::sleep(Duration::from_millis(20)).await;

        // Release prime to unblock retries.
        drop(prime);
        release_notify.notify_waiters();

        while let Some(res) = retries.join_next().await {
            if let Ok(outcome) = res {
                match outcome {
                    RetryOutcome::Resourced => retried_resourced += 1,
                    RetryOutcome::Closed => retried_full += 1,
                }
            }
        }

        Report {
            cancelled: cancelled_signal,
            retried_admitted,
            retried_full,
            retried_resourced,
            // Tokio's `Semaphore` exposes neither live waiters nor
            // a high water mark. Capacity discovery here needs
            // hand instrumentation. Leave the capacity fields
            // empty so the tina/tokio diff shows the gap.
            waiters_high_water: 0,
            waiters_max: 0,
            discovery_line: String::new(),
            exit_clean: true,
        }
    });

    Ok(report)
}

enum RetryOutcome {
    Resourced,
    Closed,
}
