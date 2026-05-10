//! Tokio reference: producer task + consumer task + a `select!` on
//! `tokio::signal::ctrl_c()`. Closing the producer's sender lets the
//! consumer drain and exit.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use tokio::runtime::Builder;
use tokio::sync::mpsc;
use tokio::time::sleep;

use crate::{ITEM_INTERVAL_MS, Report, SIGNAL_AFTER_MS, TOTAL_PLANNED_ITEMS};

pub fn run() -> anyhow::Result<Report> {
    let runtime = Builder::new_current_thread().enable_all().build()?;
    Ok(runtime.block_on(async_run()))
}

async fn async_run() -> Report {
    let (tx, mut rx) = mpsc::channel::<u32>(TOTAL_PLANNED_ITEMS as usize + 1);
    let signal_received = Arc::new(AtomicBool::new(false));
    let items_produced = Arc::new(AtomicU32::new(0));
    let items_processed = Arc::new(AtomicU32::new(0));

    // Producer: pushes items at intervals; stops on signal.
    let producer = tokio::spawn({
        let tx = tx.clone();
        let signal_received = Arc::clone(&signal_received);
        let items_produced = Arc::clone(&items_produced);
        async move {
            let signal_stream = tokio::signal::ctrl_c();
            tokio::pin!(signal_stream);
            for i in 0..TOTAL_PLANNED_ITEMS {
                tokio::select! {
                    _ = sleep(Duration::from_millis(ITEM_INTERVAL_MS)) => {
                        if tx.send(i).await.is_err() {
                            break;
                        }
                        items_produced.fetch_add(1, Ordering::Relaxed);
                    }
                    res = &mut signal_stream => {
                        if res.is_ok() {
                            signal_received.store(true, Ordering::Release);
                        }
                        break;
                    }
                }
            }
            // Closing the sender lets the consumer drain and exit.
            drop(tx);
        }
    });

    // External SIGINT trigger: simulate the operator sending Ctrl-C.
    tokio::spawn(async {
        sleep(Duration::from_millis(SIGNAL_AFTER_MS)).await;
        signal_hook::low_level::raise(signal_hook::consts::SIGINT).expect("raise SIGINT");
    });

    drop(tx);

    // Consumer: drains whatever the producer managed to push.
    let consumer = tokio::spawn({
        let items_processed = Arc::clone(&items_processed);
        async move {
            while let Some(_item) = rx.recv().await {
                sleep(Duration::from_millis(1)).await;
                items_processed.fetch_add(1, Ordering::Relaxed);
            }
        }
    });

    let _ = producer.await;
    let _ = consumer.await;

    Report {
        items_produced: items_produced.load(Ordering::Relaxed),
        items_processed: items_processed.load(Ordering::Relaxed),
        signal_received: signal_received.load(Ordering::Acquire),
        items_remaining_in_queue_at_exit: 0,
        exit_clean: true,
    }
}
