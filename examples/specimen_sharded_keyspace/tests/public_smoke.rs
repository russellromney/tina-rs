//! Public runner proof for the sharded-keyspace specimen.
//!
//! Characterization pins the fixed script counts (5 sets, 1 hit,
//! 2 misses, 1 delete, sum=4). Public smoke exercises the documented
//! Tina path through placement-routed per-shard stores.

use specimen_sharded_keyspace::{Report, expected_report, tina_impl};

fn assert_script_counts(report: Report) {
    assert_eq!(report, expected_report());
}

/// Pins script counts before/after host-result migration.
#[test]
fn public_characterization() {
    assert_script_counts(tina_impl::run().expect("tina side ran"));
}

/// Documented public runner path: `tina_impl::run()`.
#[test]
fn public_smoke() {
    assert_script_counts(tina_impl::run().expect("tina side ran"));
}
