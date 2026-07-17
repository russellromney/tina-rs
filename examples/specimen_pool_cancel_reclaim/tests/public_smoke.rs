//! Public runner proof for the pool cancel/reclaim specimen.
//!
//! Characterization pins the exact reclaim counts: every parked
//! waiter is cancelled once, every retry is dispatched, and the park
//! wave drives waiter high water exactly to its cap. Public smoke
//! exercises the documented Tina path and reuses the crate's
//! invariant assertions.

use specimen_pool_cancel_reclaim::{
    Report, WAITERS, assert_report_invariants, assert_tina_capacity_invariants, tina_impl,
};

fn assert_reclaimed(report: &Report) {
    assert_eq!(report.cancelled, WAITERS);
    assert_eq!(report.retried_dispatched, WAITERS);
    assert_eq!(report.retried_full, 0);
    assert_eq!(report.retried_resourced, WAITERS);
    assert_eq!(report.cancel_outcomes.len(), WAITERS);
    assert!(report.release_failures.is_empty());
    assert!(report.pressure_terminal.is_none());
    assert!(report.pressure_settled);
    assert_eq!(report.waiters_max, WAITERS);
    assert_eq!(report.waiters_high_water, WAITERS);
    assert!(report.exit_clean);
}

/// Pins cancel/reclaim counts before/after host-result migration.
#[test]
fn public_characterization() {
    assert_reclaimed(&tina_impl::run().expect("tina side ran"));
}

/// Documented public runner path: `tina_impl::run()`.
#[test]
fn public_smoke() {
    let report = tina_impl::run().expect("tina side ran");
    assert_report_invariants("public", &report);
    assert_tina_capacity_invariants(&report);
}
