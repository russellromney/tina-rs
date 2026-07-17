//! Public runner proof for the graceful-pool-shutdown specimen.
//!
//! Characterization pins the exact caller terminal counts. Public
//! smoke exercises the documented Tina path and reuses the crate's
//! invariant assertions, including the layered terminal accounting.

use specimen_graceful_pool_shutdown::{
    CALLERS, Report, WORKERS, assert_report_invariants, assert_tina_terminal_invariants, tina_impl,
};

fn assert_pool_stopped(report: &Report) {
    assert_eq!(report.completed, WORKERS);
    assert_eq!(report.closed, CALLERS - WORKERS);
    assert_eq!(report.failed, 0);
    assert_eq!(report.tina_terminals.acquire_closed, CALLERS - WORKERS);
    assert!(report.shutdown_close_observed);
    assert!(report.exit_clean);
}

/// Pins caller terminal counts before/after host-result migration.
#[test]
fn public_characterization() {
    assert_pool_stopped(&tina_impl::run().expect("tina side ran"));
}

/// Documented public runner path: `tina_impl::run()`.
#[test]
fn public_smoke() {
    let report = tina_impl::run().expect("tina side ran");
    assert_report_invariants("public", &report);
    assert_tina_terminal_invariants(&report);
}
