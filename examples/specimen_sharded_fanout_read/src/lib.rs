//! Tokio-vs-Tina sharded fanout read.
//!
//! Three shards each own a `u64` counter, seeded with values
//! [`SEED_VALUES`]. Both sides issue a fanout read across every shard
//! and aggregate the total.
//!
//! - Tokio: `Vec<Arc<Mutex<u64>>>`. The reader iterates the array and
//!   sums under each lock. Placement is "the array index"; nothing
//!   structurally prevents wrong-shard access.
//! - Tina: per-shard `ShardCounter` isolate, `ShardPlacement` +
//!   `ShardServiceTable`, a `ScatterCoord` that fans `Get` out to
//!   every shard via `call` (one request/reply per target), collects
//!   `CallOutcome<ShardCounterReply>`, and builds a
//!   `ScatterGatherReport<u64>`. No `ReplyAdapter`, no `Bind` message.
//!
//! Both sides produce the same [`Report`].

pub mod tina_impl;
pub mod tokio_impl;

/// Shard ids both sides operate over.
pub const SHARD_RAW_IDS: [u32; 3] = [11, 22, 33];

/// Initial value owned by each shard, in the same order as
/// [`SHARD_RAW_IDS`].
pub const SEED_VALUES: [u64; 3] = [100, 200, 300];

/// What each side observed end-to-end.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// Sum of every shard's value.
    pub total_sum: u64,
    /// Number of shards that contributed a value.
    pub shards_replied: u32,
    /// Whether the run finished cleanly.
    pub exit_clean: bool,
}

/// Expected counts under [`SHARD_RAW_IDS`] and [`SEED_VALUES`].
pub fn expected_report() -> Report {
    Report {
        total_sum: SEED_VALUES.iter().sum(),
        shards_replied: SHARD_RAW_IDS.len() as u32,
        exit_clean: true,
    }
}
