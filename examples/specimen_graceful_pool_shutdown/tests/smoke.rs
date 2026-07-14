use specimen_graceful_pool_shutdown::{
    assert_report_invariants, assert_tina_terminal_invariants, tina_impl, tokio_impl,
};

#[test]
fn tokio_smoke() {
    assert_report_invariants("tokio", &tokio_impl::run().expect("tokio"));
}

#[test]
fn tina_smoke() {
    let report = tina_impl::run().expect("tina");
    assert_report_invariants("tina", &report);
    assert_tina_terminal_invariants(&report);
}
