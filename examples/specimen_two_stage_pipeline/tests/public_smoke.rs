//! Public runner proof for the two-stage-pipeline specimen.
//!
//! Characterization pins the exact stage bucketing: labeled inputs
//! fail parse or validate deterministically, the rest complete.
//! Public smoke exercises the documented Tina path and reuses the
//! crate's invariant assertions.

use specimen_two_stage_pipeline::{
    PARSE_FAILURES, REQUESTS, Report, VALIDATE_FAILURES, assert_report_invariants, tina_impl,
};

fn assert_pipeline(report: &Report) {
    assert_eq!(report.requests, REQUESTS);
    assert_eq!(report.parse_failed, PARSE_FAILURES);
    assert_eq!(report.validate_failed, VALIDATE_FAILURES);
    assert_eq!(
        report.completed,
        REQUESTS - PARSE_FAILURES - VALIDATE_FAILURES
    );
    assert!(report.tina_terminals.is_empty());
    assert!(report.exit_clean);
}

/// Pins stage bucketing before/after host-result migration.
#[test]
fn public_characterization() {
    assert_pipeline(&tina_impl::run().expect("tina side ran"));
}

/// Documented public runner path: `tina_impl::run()`.
#[test]
fn public_smoke() {
    let report = tina_impl::run().expect("tina side ran");
    assert_report_invariants("public", &report);
}
