//! Smoke tests: each side hits a flaky upstream that returns
//! `503 503 200`. Both sides retry and end up with the same report.

use eiffel_retrying_outbound_http::{expected_report, tina_impl, tokio_impl};

#[test]
fn tokio_smoke() {
    assert_eq!(tokio_impl::run().expect("tokio side ran"), expected_report());
}

#[test]
fn tina_smoke() {
    assert_eq!(tina_impl::run().expect("tina side ran"), expected_report());
}
