use specimen_cancellation_chain::{
    assert_tina_report_invariants, assert_tokio_report_invariants, tina_impl, tokio_impl,
};

#[test]
fn tokio_smoke() {
    let r = tokio_impl::run().expect("tokio side ran");
    assert_tokio_report_invariants(&r);
}

#[test]
fn tina_smoke() {
    let r = tina_impl::run().expect("tina side ran");
    assert_tina_report_invariants(&r);
}
