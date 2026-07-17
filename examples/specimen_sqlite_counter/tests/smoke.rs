//! Smoke tests.
//!
//! Each side runs the same script (init + N increments + final read)
//! against a fresh temporary SQLite file and produces the same report,
//! including query/update metrics.

use specimen_sqlite_counter::{expected_report, tina_impl, tokio_impl};

#[test]
fn tokio_smoke() {
    assert_eq!(tokio_impl::run().expect("tokio side ran"), expected_report());
}

#[test]
fn tina_smoke() {
    assert_eq!(tina_impl::run().expect("tina side ran"), expected_report());
}

#[test]
fn temp_db_isolation_across_runs() {
    // Two independent runs must not share database state.
    let a = tina_impl::run().expect("first run");
    let b = tina_impl::run().expect("second run");
    assert_eq!(a, expected_report());
    assert_eq!(b, expected_report());
}
