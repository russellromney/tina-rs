//! Smoke tests: each side runs the same A → B → C script and
//! produces the same `Report` (3 successful, 3 timed out at C).

use specimen_backpressure_chain::{expected_report, tina_impl, tokio_impl};

#[test]
fn tokio_smoke() {
    assert_eq!(
        tokio_impl::run().expect("tokio side ran"),
        expected_report()
    );
}

#[test]
fn tina_smoke() {
    assert_eq!(tina_impl::run().expect("tina side ran"), expected_report());
}
