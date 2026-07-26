//! Public runner proof for the webhook-outbox specimen.
//!
//! Characterization pins the outbox persistence arithmetic: Phase A
//! sends three webhooks and durably marks two (the third "crashes"
//! before its mark), Phase B recovers the one pending record and
//! re-delivers it (at-least-once), and journal compaction drops the two
//! completed records, leaving the one still-pending enqueue.

use specimen_webhook_outbox::{EXPECTED, Report, tina_impl};

fn assert_outcome(report: Report) {
    assert_eq!(report, EXPECTED, "got {report:?}");
}

/// Pins the durable-outbox facts as literals against the Tina run, and
/// proves the exported `EXPECTED` constant still describes that outcome.
#[test]
fn public_characterization() {
    let report = tina_impl::run().expect("tina side ran");
    assert_eq!(report.phase_a_sent, 3);
    assert_eq!(report.phase_a_marked, 2);
    assert_eq!(report.recovered_pending, 1);
    assert_eq!(report.phase_b_resent, 1);
    assert_eq!(report.final_marked, 3);
    assert_eq!(report.journal_records_before_compaction, 5);
    assert_eq!(report.journal_records_after_compaction, 1);
    assert!(report.exit_clean);
    assert_outcome(report);
}

/// Documented public runner path: `tina_impl::run()` (the `tina` binary
/// mode).
#[test]
fn public_smoke() {
    assert_outcome(tina_impl::run().expect("tina side ran"));
}
