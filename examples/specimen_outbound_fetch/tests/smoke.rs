//! Smoke tests: each side fetches every endpoint successfully and
//! receives the full payload.

use specimen_outbound_fetch::{FETCH_COUNT, RESPONSE, Report, tina_impl, tokio_impl};

fn assert_fetched(report: Report) {
    assert_eq!(report.successful_fetches, FETCH_COUNT);
    assert_eq!(report.failed_fetches, 0);
    assert_eq!(report.bytes_received, RESPONSE.len() * FETCH_COUNT as usize);
    assert!(report.exit_clean);
}

#[test]
fn tokio_smoke() {
    assert_fetched(tokio_impl::run().expect("tokio side ran"));
}

#[test]
fn tina_smoke() {
    assert_fetched(tina_impl::run().expect("tina side ran"));
}
