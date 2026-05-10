//! Smoke tests: each side runs the same scripted producer (10 fast +
//! 2 trailing) through its batcher and produces matching counts.

use specimen_periodic_batcher::{expected_report, tina_impl, tokio_impl};

#[test]
fn tokio_smoke() {
    assert_eq!(tokio_impl::run().expect("tokio side ran"), expected_report());
}

#[test]
fn tina_smoke() {
    assert_eq!(tina_impl::run().expect("tina side ran"), expected_report());
}
