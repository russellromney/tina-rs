//! Public runner proof for the persistent-counter specimen.
//!
//! Characterization pins the disk recovery arithmetic. Public smoke
//! exercises the documented Tina path. Persistence failure surfaces as a
//! typed `Failed` reply rather than a host spin on a shared slot.

use specimen_persistent_counter::{
    EXPECTED_FINAL_VALUE, PHASE_A_INCREMENTS, PHASE_B_INCREMENTS, Report, tina_impl,
};

fn assert_recovered(report: Report) {
    assert_eq!(report.phase_a_final, PHASE_A_INCREMENTS);
    assert!(report.snapshot_committed);
    assert_eq!(report.phase_b_recovered, PHASE_A_INCREMENTS);
    assert_eq!(report.phase_b_final, EXPECTED_FINAL_VALUE);
    assert_eq!(report.journal_records_phase_b, PHASE_B_INCREMENTS);
    assert!(report.exit_clean);
}

/// Pins persistence arithmetic before/after host-result migration.
#[test]
fn public_characterization() {
    assert_recovered(tina_impl::run().expect("tina side ran"));
}

/// Documented public runner path: `tina_impl::run()`.
#[test]
fn public_smoke() {
    assert_recovered(tina_impl::run().expect("tina side ran"));
}
