//! Smoke tests: each side runs the same A → B → C script and
//! preserves the terminal truth each runtime can actually observe.

use specimen_backpressure_chain::{expected_tina_report, expected_tokio_report, tina_impl, tokio_impl};

#[test]
fn tokio_smoke() {
    assert_eq!(
        tokio_impl::run().expect("tokio side ran"),
        expected_tokio_report()
    );
}

#[test]
fn tina_smoke() {
    assert_eq!(tina_impl::run().expect("tina side ran"), expected_tina_report());
}
