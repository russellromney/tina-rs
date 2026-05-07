//! Smoke tests: each side bursts a known number of jobs at a
//! bounded-capacity worker, signals shutdown, and asserts the worker
//! drained every admitted job.

use eiffel_graceful_drain_server::{assert_report_invariants, tina_impl, tokio_impl};

#[test]
fn tokio_smoke() {
    let report = tokio_impl::run().expect("tokio side ran");
    assert_report_invariants("tokio", &report);
}

#[test]
fn tina_smoke() {
    let report = tina_impl::run().expect("tina side ran");
    assert_report_invariants("tina", &report);
}
