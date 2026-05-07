//! Smoke tests: each side runs end-to-end and accounts for every
//! request. Exact wire-shape invariants (in-flight cap, frame
//! decoding, etc.) live in `tina-rpc`'s own tests.

use eiffel_rpc::{RunConfig, tina_impl, tokio_impl};

#[test]
fn tokio_smoke() {
    let config = RunConfig { burst: 4 };
    let report = tokio_impl::run(config).expect("tokio side ran");
    assert_eq!(
        report.total(),
        config.burst,
        "every request must be accounted for: {report:?}",
    );
}

#[test]
fn tina_smoke() {
    let config = RunConfig { burst: 4 };
    let report = tina_impl::run(config).expect("tina side ran");
    assert_eq!(
        report.total(),
        config.burst,
        "every request must be accounted for: {report:?}",
    );
}
