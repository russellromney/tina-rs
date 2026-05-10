//! Smoke tests.
//!
//! Each side runs the same script (init + N increments + final read)
//! against a fresh SQLite file and produces the same final counter
//! value.

use specimen_sqlite_counter::{expected_report, tina_impl, tokio_impl};

#[test]
fn tokio_smoke() {
    assert_eq!(tokio_impl::run().expect("tokio side ran"), expected_report());
}

#[test]
fn tina_smoke() {
    assert_eq!(tina_impl::run().expect("tina side ran"), expected_report());
}
