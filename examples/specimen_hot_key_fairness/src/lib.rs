//! Sharded keyspace under skewed traffic. 90% of writes target a
//! single hot key; the rest spread across cold keys. Each shard has
//! a bounded queue and a fixed processing rate, so the hot shard
//! must reject some writes while the cold shards stay responsive.
//!
//! Both sides report admits and full-rejects per shard. The point
//! is that overload on the hot shard is *visible*, not hidden in a
//! growing buffer.

pub mod tina_impl;
pub mod tokio_impl;

pub const SHARDS: u32 = 3;
pub const HOT_WRITES: u32 = 30;
/// Cold writes per shard. Kept at or below `SHARD_MAILBOX` so a
/// well-behaved cold shard absorbs the whole burst with no
/// rejections — the contrast with the hot shard's overflow is
/// what the smoke test asserts.
pub const COLD_WRITES_PER_SHARD: u32 = 4;
pub const SHARD_MAILBOX: usize = 4;
pub const PER_WRITE_MS: u64 = 5;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub hot_admitted: u32,
    pub hot_rejected: u32,
    pub cold_admitted: u32,
    pub cold_rejected: u32,
    pub exit_clean: bool,
}

pub fn assert_report_invariants(side: &str, r: &Report) {
    let hot_total = r.hot_admitted + r.hot_rejected;
    let cold_total = r.cold_admitted + r.cold_rejected;
    assert_eq!(hot_total, HOT_WRITES, "{side}: {r:?}");
    let cold_expected = (SHARDS - 1) * COLD_WRITES_PER_SHARD;
    assert_eq!(cold_total, cold_expected, "{side}: {r:?}");
    assert!(
        r.hot_rejected > 0,
        "{side}: hot shard should overflow under skew, got {r:?}",
    );
    assert_eq!(
        r.cold_rejected, 0,
        "{side}: cold shards should keep up at the configured rate, got {r:?}",
    );
    assert!(r.exit_clean, "{side}: {r:?}");
}
