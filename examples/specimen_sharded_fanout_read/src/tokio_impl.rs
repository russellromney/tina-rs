//! Tokio reference. The shards are `Vec<Arc<Mutex<u64>>>`, one per
//! seed value. Reading the total is a tight loop that locks each
//! mutex and accumulates the sum. There is no per-shard owner
//! isolate, no typed "wrong shard" outcome — just an array index.

use std::sync::{Arc, Mutex};

use crate::{Report, SEED_VALUES, SHARD_RAW_IDS};

pub fn run() -> anyhow::Result<Report> {
    assert_eq!(SHARD_RAW_IDS.len(), SEED_VALUES.len());
    let shards: Vec<Arc<Mutex<u64>>> = SEED_VALUES
        .iter()
        .map(|v| Arc::new(Mutex::new(*v)))
        .collect();

    let mut total_sum = 0u64;
    let mut shards_replied = 0u32;
    for shard in &shards {
        let value = *shard.lock().expect("shard mutex");
        total_sum += value;
        shards_replied += 1;
    }

    Ok(Report {
        total_sum,
        shards_replied,
        exit_clean: true,
    })
}
