//! Smoke tests: each side runs the script end-to-end and produces
//! the expected counts. The script is fixed (`SET / GET / GET / DEL
//! / GET / QUIT`) so the counts are too: `ok=1, values=1, misses=2,
//! deleted=1`.

use eiffel_mini_keyspace::{Report, tina_impl, tokio_impl};

const EXPECTED: Report = Report {
    ok: 1,
    values: 1,
    misses: 2,
    deleted: 1,
};

#[test]
fn tokio_smoke() {
    let report = tokio_impl::run().expect("tokio side ran");
    assert_eq!(report, EXPECTED);
}

#[test]
fn tina_smoke() {
    let report = tina_impl::run().expect("tina side ran");
    assert_eq!(report, EXPECTED);
}
