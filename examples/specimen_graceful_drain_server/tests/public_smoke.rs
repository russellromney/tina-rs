//! Public runner proof for the graceful-drain server specimen.
//!
//! Characterization pins the drain accounting. Public smoke exercises
//! the documented Tina path and reuses the crate's invariant
//! assertions.

use specimen_graceful_drain_server::{BURST_JOBS, Report, assert_report_invariants, tina_impl};

fn assert_drained(report: &Report) {
    // The admit/full split is timing-sensitive (producer burst rate vs
    // worker drain rate), so exact per-lane counts are not pinned;
    // only the totals and terminal flags are deterministic.
    assert_eq!(report.items_admitted + report.items_full, BURST_JOBS);
    assert!(report.items_full > 0);
    assert_eq!(report.items_processed, report.items_admitted);
    assert!(report.shutdown_observed);
    assert!(report.exit_clean);
}

/// Pins drain accounting before/after host-result migration.
#[test]
fn public_characterization() {
    assert_drained(&tina_impl::run().expect("tina side ran"));
}

/// Documented public runner path: `tina_impl::run()`.
#[test]
fn public_smoke() {
    let report = tina_impl::run().expect("tina side ran");
    assert_report_invariants("public", &report);
}
