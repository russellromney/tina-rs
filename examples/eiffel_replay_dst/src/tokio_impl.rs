//! Tokio reference: the same nominal workload run twice on a
//! current_thread runtime. Messages are deterministic;
//! wall-clock timings are not. There is no replay story.

use std::time::{Duration, Instant};

use tokio::runtime::Builder;
use tokio::sync::mpsc;
use tokio::time::sleep;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub run1_messages: Vec<u32>,
    pub run2_messages: Vec<u32>,
    pub run1_micros: Vec<u128>,
    pub run2_micros: Vec<u128>,
}

pub fn run() -> anyhow::Result<Report> {
    let runtime = Builder::new_current_thread().enable_all().build()?;
    let (run1_messages, run1_micros) = runtime.block_on(async_run());
    let (run2_messages, run2_micros) = runtime.block_on(async_run());

    Ok(Report {
        run1_messages,
        run2_messages,
        run1_micros,
        run2_micros,
    })
}

async fn async_run() -> (Vec<u32>, Vec<u128>) {
    let (tx, mut rx) = mpsc::channel::<u32>(16);

    let producer = tokio::spawn(async move {
        for count in 0..6u32 {
            tx.send(count).await.expect("send");
            // Same nominal delay shape as the Tina-sim producer:
            // 1, 2, or 3 ms.
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
