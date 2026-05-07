//! Smoke tests: each side reads three shards and reports the same
//! total. The Tina side proves the scatter/gather round-trip against
//! a real `ThreadedMultiShardRuntime`.

use eiffel_sharded_fanout_read::{expected_report, tina_impl, tokio_impl};

#[test]
fn tokio_smoke() {
    assert_eq!(tokio_impl::run().expect("tokio side ran"), expected_report());
}

#[test]
fn tina_smoke() {
    assert_eq!(tina_impl::run().expect("tina side ran"), expected_report());
}
