//! Tokio-shaped sharded keyspace: `Vec<Arc<Mutex<HashMap>>>` with a
//! hand-rolled FNV-1a placement function on the caller side. There is
//! no per-shard *isolate*; the shard concept is just an array index,
//! and the only thing stopping a caller from writing to the wrong
//! shard is the discipline of always calling `placement(key)`. There
//! is no enforcement and no `WrongShard` typed error.
//!
//! The whole `run()` is synchronous because `std::sync::Mutex` is. No
//! Tokio runtime is started; "Tokio" here is a label for the
//! ecosystem-default `Arc<Mutex<...>>` shape, not for `tokio::spawn`.
//! Real Tokio code that wants finer-grained sharding usually lands
//! here too, just spelled with `tokio::sync::Mutex`.
//!
//! Compare to `tina_impl.rs`: there the shard is a real isolate that
//! owns its `BTreeMap`, the type system says "only one writer at a
//! time", and the handler re-checks
//! `placement.owner_for_str(&key) == ctx.shard_id()` so a routing bug
//! becomes a typed `WrongShard` reply instead of silent corruption.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::{Command, Report, SCRIPT, SHARD_RAW_IDS, fnv1a64, parse_commands};

pub fn run() -> anyhow::Result<Report> {
    // One mutex-guarded HashMap per shard. The shard list is shared with
    // the Tina side so both produce the same FNV-1a placement.
    let shards: Vec<Arc<Mutex<HashMap<String, String>>>> = SHARD_RAW_IDS
        .iter()
        .map(|_| Arc::new(Mutex::new(HashMap::new())))
        .collect();
    let mut report = Report::default();

    for cmd in parse_commands(SCRIPT.as_bytes())? {
        match cmd {
            Command::Set { key, value } => {
                let i = shard_index_of(&key, shards.len());
                shards[i].lock().expect("shard mutex").insert(key, value);
                report.ok += 1;
            }
            Command::Get { key } => {
                let i = shard_index_of(&key, shards.len());
                match shards[i].lock().expect("shard mutex").get(&key).cloned() {
                    Some(_) => report.values += 1,
                    None => report.misses += 1,
                }
            }
            Command::Del { key } => {
                let i = shard_index_of(&key, shards.len());
                match shards[i].lock().expect("shard mutex").remove(&key) {
                    Some(_) => report.deleted += 1,
                    None => report.misses += 1,
                }
            }
            Command::Sum => {
                report.sum = shards
                    .iter()
                    .map(|s| s.lock().expect("shard mutex").len() as u64)
                    .sum();
            }
            Command::Quit => break,
        }
    }
    Ok(report)
}

/// Same FNV-1a 64 mod-count placement that the Tina side uses. Caller
/// owns this — the shard `Vec` does not enforce that you use it.
fn shard_index_of(key: &str, shard_count: usize) -> usize {
    (fnv1a64(key.as_bytes()) % shard_count as u64) as usize
}
