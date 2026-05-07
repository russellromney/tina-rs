//! Smoke tests.
//!
//! Each side dynamically spawns N workers, joins their partial sums,
//! and produces the same total.

use eiffel_dynamic_worker_pool::{expected_report, tina_impl, tokio_impl};

#[test]
fn tokio_smoke() {
    assert_eq!(tokio_impl::run().expect("tokio side ran"), expected_report());
}

#[test]
fn tina_smoke() {
    assert_eq!(tina_impl::run().expect("tina side ran"), expected_report());
}
