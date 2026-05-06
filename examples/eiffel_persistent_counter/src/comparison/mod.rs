pub(crate) mod tina_impl;
pub(crate) mod tokio_impl;

// The script: phase A starts with a fresh dir, applies these many increments,
// then takes a snapshot. Phase B runs against the same dir, recovers, and
// applies these many additional increments without a snapshot. The final
// value is asserted across both sides.
pub(crate) const PHASE_A_INCREMENTS: u64 = 5;
pub(crate) const PHASE_B_INCREMENTS: u64 = 3;
pub(crate) const EXPECTED_FINAL_VALUE: u64 = PHASE_A_INCREMENTS + PHASE_B_INCREMENTS;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SideReport {
    pub phase_a_final: u64,
    pub phase_b_recovered: u64,
    pub phase_b_final: u64,
    pub snapshot_committed: bool,
    pub journal_records_phase_b: u64,
    pub exit_clean: bool,
}

pub(crate) fn print_report(side: &str, report: &SideReport) {
    println!(
        "side={side} \
         phase_a_final={} snapshot_committed={} \
         phase_b_recovered={} phase_b_final={} \
         journal_records_phase_b={} exit_clean={}",
        report.phase_a_final,
        report.snapshot_committed,
        report.phase_b_recovered,
        report.phase_b_final,
        report.journal_records_phase_b,
        report.exit_clean,
    );
}

pub(crate) fn assert_equivalent(tokio: &SideReport, tina: &SideReport) {
    assert_eq!(
        tokio.phase_a_final, PHASE_A_INCREMENTS,
        "tokio phase A counted every increment"
    );
    assert_eq!(
        tina.phase_a_final, PHASE_A_INCREMENTS,
        "tina phase A counted every increment"
    );
    assert!(tokio.snapshot_committed, "tokio took the phase A snapshot");
    assert!(tina.snapshot_committed, "tina took the phase A snapshot");
    assert_eq!(
        tokio.phase_b_recovered, PHASE_A_INCREMENTS,
        "tokio recovered the snapshot value"
    );
    assert_eq!(
        tina.phase_b_recovered, PHASE_A_INCREMENTS,
        "tina recovered the snapshot value"
    );
    assert_eq!(
        tokio.phase_b_final, EXPECTED_FINAL_VALUE,
        "tokio applied phase B increments on top of the recovered value"
    );
    assert_eq!(
        tina.phase_b_final, EXPECTED_FINAL_VALUE,
        "tina applied phase B increments on top of the recovered value"
    );
    assert_eq!(
        tokio.journal_records_phase_b, PHASE_B_INCREMENTS,
        "tokio wrote one journal record per phase B increment"
    );
    assert_eq!(
        tina.journal_records_phase_b, PHASE_B_INCREMENTS,
        "tina wrote one journal record per phase B increment"
    );
    assert!(tokio.exit_clean, "tokio exited cleanly");
    assert!(tina.exit_clean, "tina exited cleanly");
}
