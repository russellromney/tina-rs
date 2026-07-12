//! Smoke tests: each side handles the same scripted three-step
//! counter request sequence and reports the same statuses/bodies.

use specimen_axum_counter::{tina_impl, tokio_impl};

#[test]
fn tokio_smoke() {
    let report = tokio_impl::run();
    report.assert_expected();
}

#[test]
fn tina_smoke() {
    let report = tina_impl::run().expect("start Tina app");
    report.assert_expected();
}
