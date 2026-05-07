//! Smoke tests: each side runs the same Tower stack over its own
//! counter implementation and asserts structural invariants. Exact
//! split between successful / 503 / 504 is timing-sensitive (Tokio's
//! ConcurrencyLimit can pipeline two at a time, the Tina isolate
//! handles one mailbox message per turn) so the assertions are
//! shape-only.

use eiffel_tower_timeout_counter::{assert_report_invariants, tina_impl, tokio_impl};

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
