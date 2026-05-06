use std::time::{Duration, Instant};

use tokio::runtime::Builder;
use tokio::sync::mpsc;
use tokio::time::sleep;

use super::TokioReport;

pub(crate) fn run() -> TokioReport {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let (run1_messages, run1_micros) = runtime.block_on(async_run());
    let (run2_messages, run2_micros) = runtime.block_on(async_run());

    TokioReport {
        run1_messages,
        run2_messages,
        run1_micros,
        run2_micros,
    }
}

async fn async_run() -> (Vec<u32>, Vec<u128>) {
    let (tx, mut rx) = mpsc::channel::<u32>(16);

    let producer = tokio::spawn(async move {
        for count in 0..6u32 {
            tx.send(count).await.expect("send");
            // Same nominal delay shape as the Tina-sim producer: 1, 2, or 3 ms.
            let delay = Duration::from_millis(1 + u64::from((count + 1) % 3));
            sleep(delay).await;
        }
    });

    let mut messages = Vec::new();
    let mut micros = Vec::new();
    let start = Instant::now();
    while let Some(value) = rx.recv().await {
        messages.push(value);
        micros.push(start.elapsed().as_micros());
    }

    producer.await.expect("producer");
    (messages, micros)
}
