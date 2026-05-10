//! Smoke tests: each side serves the scripted rustls client and
//! produces the expected counter behaviour. Both sides should be
//! indistinguishable from the wire.

use eiffel_native_https::{Report, tina_impl, tokio_impl};

const EXPECTED: Report = Report {
    successful_get: 2,
    successful_post: 3,
    final_counter_value: 3,
    got_404_for_missing: true,
    exit_clean: true,
};

#[test]
fn tokio_smoke() {
    assert_eq!(tokio_impl::run().expect("tokio side ran"), EXPECTED);
}

#[test]
fn tina_smoke() {
    assert_eq!(tina_impl::run().expect("tina side ran"), EXPECTED);
}
