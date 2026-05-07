use eiffel_cancellation_chain::{assert_report_invariants, tina_impl, tokio_impl};

#[test]
fn tokio_smoke() {
    let r = tokio_impl::run().expect("tokio side ran");
    assert_report_invariants("tokio", &r);
}

#[test]
fn tina_smoke() {
    let r = tina_impl::run().expect("tina side ran");
    assert_report_invariants("tina", &r);
}
