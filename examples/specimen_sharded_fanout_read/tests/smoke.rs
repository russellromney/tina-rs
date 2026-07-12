//! Smoke tests: each side reads three shards and reports the same
//! total. The Tina side proves the scatter/gather round-trip against
//! a real `ThreadedMultiShardRuntime`.

use specimen_sharded_fanout_read::{SHARD_RAW_IDS, expected_report, tina_impl, tokio_impl};
use tina_runtime::assert_service_owned_bound;

#[test]
fn tokio_smoke() {
    assert_eq!(
        tokio_impl::run().expect("tokio side ran"),
        expected_report()
    );
}

#[test]
fn tina_smoke() {
    let report = tina_impl::run().expect("tina side ran");
    assert_eq!(report, expected_report());
    assert_service_owned_bound(
        "specimen_sharded_fanout_read.targets",
        Some(SHARD_RAW_IDS.len()),
        Some(report.shards_replied as usize),
    )
    .expect("scatter fanout stayed under service-owned cap");
}
