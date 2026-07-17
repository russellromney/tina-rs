//! Public runner proof for the outbound-fetch specimen.
//!
//! Characterization pins the fetch arithmetic: every fetch succeeds
//! and receives the full payload. Public smoke exercises the
//! documented Tina path against the loopback test server.

use specimen_outbound_fetch::{FETCH_COUNT, RESPONSE, Report, tina_impl};

fn assert_fetched(report: Report) {
    assert_eq!(report.successful_fetches, FETCH_COUNT);
    assert_eq!(report.failed_fetches, 0);
    assert_eq!(report.bytes_received, RESPONSE.len() * FETCH_COUNT as usize);
    assert!(report.exit_clean);
}

/// Pins fetch arithmetic before/after host-result migration.
#[test]
fn public_characterization() {
    assert_fetched(tina_impl::run().expect("tina side ran"));
}

/// Documented public runner path: `tina_impl::run()`.
#[test]
fn public_smoke() {
    assert_fetched(tina_impl::run().expect("tina side ran"));
}
