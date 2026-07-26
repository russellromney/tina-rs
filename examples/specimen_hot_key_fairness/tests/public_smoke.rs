//! Public runner proof for the hot-key fairness specimen.
//!
//! Characterization pins the skew accounting: every hot write is
//! admitted or visibly rejected, the cold shards absorb their whole
//! burst with zero rejections, and the hot shard's overload is visible.
//! Public smoke exercises the documented Tina runner path.

use specimen_hot_key_fairness::{
    COLD_WRITES_PER_SHARD, HOT_WRITES, PER_WRITE_MS, SHARD_MAILBOX, SHARDS,
    assert_report_invariants, tina_impl,
};

/// Pins the exact per-shard settlement totals.
#[test]
fn public_characterization() {
    assert_eq!(SHARDS, 3);
    assert_eq!(HOT_WRITES, 30);
    assert_eq!(COLD_WRITES_PER_SHARD, 4);
    assert_eq!(SHARD_MAILBOX, 4);
    assert_eq!(PER_WRITE_MS, 5);

    let report = tina_impl::run().expect("tina side ran");
    assert_report_invariants("tina", &report);
    // Exact accounting the invariants imply: all 30 hot writes settle
    // with zero terminal losses, and both cold shards admit their whole
    // 4-write bursts with no rejections.
    assert_eq!(report.hot_terminal, 0, "{report:?}");
    assert_eq!(report.cold_terminal, 0, "{report:?}");
    assert_eq!(report.hot_admitted + report.hot_rejected, HOT_WRITES);
    assert_eq!(report.cold_admitted, (SHARDS - 1) * COLD_WRITES_PER_SHARD);
    assert_eq!(report.cold_rejected, 0, "{report:?}");
    assert!(
        report.hot_rejected > 0,
        "hot shard overload must be visible: {report:?}"
    );
    assert!(report.exit_clean, "{report:?}");
    // The exact hot admitted/rejected split is a wall-clock race
    // between the producer burst and the 5 ms per-write drain rate, so
    // exact equality is not pinned for it; the crate's invariant helper
    // asserts the same shape.
}

/// Documented public runner path: `tina_impl::run()`
/// (`cargo run --manifest-path examples/specimen_hot_key_fairness/Cargo.toml -- tina`).
#[test]
fn public_smoke() {
    let report = tina_impl::run().expect("tina side ran");
    assert_report_invariants("tina", &report);
}
