//! Smoke tests: each side runs Phase A + simulated restart + Phase
//! B and recovers the right final value from disk.

use eiffel_persistent_counter::{
    EXPECTED_FINAL_VALUE, PHASE_A_INCREMENTS, PHASE_B_INCREMENTS, Report, tina_impl, tokio_impl,
};

fn assert_recovered(report: Report) {
    assert_eq!(report.phase_a_final, PHASE_A_INCREMENTS);
    assert!(report.snapshot_committed);
    assert_eq!(report.phase_b_recovered, PHASE_A_INCREMENTS);
    assert_eq!(report.phase_b_final, EXPECTED_FINAL_VALUE);
    assert_eq!(report.journal_records_phase_b, PHASE_B_INCREMENTS);
    assert!(report.exit_clean);
}

#[test]
fn tokio_smoke() {
    assert_recovered(tokio_impl::run().expect("tokio side ran"));
}

#[test]
fn tina_smoke() {
    assert_recovered(tina_impl::run().expect("tina side ran"));
}
