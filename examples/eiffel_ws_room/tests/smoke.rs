//! Smoke tests: each side runs the two-client scripted scenario and
//! reports both broadcasts seen by both clients.

use eiffel_ws_room::{tina_impl, tokio_impl};

#[test]
fn tokio_smoke() {
    let report = tokio_impl::run();
    report.assert_expected();
}

#[test]
fn tina_smoke() {
    let report = tina_impl::run();
    report.assert_expected();
}
