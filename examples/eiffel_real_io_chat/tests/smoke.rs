//! Smoke tests: each side runs end-to-end and accounts for every
//! fanout attempt. Exact wire-shape invariants live in
//! `tina-runtime`'s own tests.

use eiffel_real_io_chat::{RunConfig, tina_impl, tokio_impl};

#[test]
fn tokio_smoke() {
    let config = RunConfig::default();
    let report = tokio_impl::run(config).expect("tokio side ran");
    assert_eq!(
        report.total(),
        config.burst,
        "every fanout attempt must be accounted for: {report:?}",
    );
}

#[test]
fn tina_smoke() {
    let config = RunConfig::default();
    let report = tina_impl::run(config).expect("tina side ran");
    assert_eq!(
        report.total(),
        config.burst,
        "every fanout attempt must be accounted for: {report:?}",
    );
}
