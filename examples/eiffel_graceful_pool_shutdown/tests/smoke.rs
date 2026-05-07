use eiffel_graceful_pool_shutdown::{assert_report_invariants, tina_impl, tokio_impl};

#[test]
fn tokio_smoke() {
    assert_report_invariants("tokio", &tokio_impl::run().expect("tokio"));
}

#[test]
fn tina_smoke() {
    assert_report_invariants("tina", &tina_impl::run().expect("tina"));
}
