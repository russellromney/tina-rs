//! Smoke tests: each side runs its rate-limited worker and asserts
//! the structural invariants. Exact admit/full counts are
//! timing-sensitive on the threaded runtime, so the assertions are
//! shape-only.

use eiffel_rate_limited_worker::{assert_report_invariants, tina_impl, tokio_impl};

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
