//! Public runner proof for the sharded-fanout-read specimen.
//!
//! Characterization pins the fanout arithmetic: every shard replies
//! and the total equals the sum of the seed values. Public smoke
//! exercises the documented Tina path and re-checks the
//! service-owned fanout cap.

use specimen_sharded_fanout_read::{Report, SHARD_RAW_IDS, expected_report, tina_impl};
use tina_runtime::assert_service_owned_bound;

fn assert_fanned_out(report: Report) {
    assert_eq!(report, expected_report());
}

/// Pins fanout totals before/after host-result migration.
#[test]
fn public_characterization() {
    assert_fanned_out(tina_impl::run().expect("tina side ran"));
}

/// Documented public runner path: `tina_impl::run()`.
#[test]
fn public_smoke() {
    let report = tina_impl::run().expect("tina side ran");
    assert_fanned_out(report);
    assert_service_owned_bound(
        "specimen_sharded_fanout_read.targets",
        Some(SHARD_RAW_IDS.len()),
        Some(report.shards_replied as usize),
    )
    .expect("scatter fanout stayed under service-owned cap");
}
