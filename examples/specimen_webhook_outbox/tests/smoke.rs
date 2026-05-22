//! Smoke tests: both sides run Phase A + simulated crash + Phase B and produce
//! the same outcome — three webhooks sent, two marked before the crash, one
//! recovered and resent (at-least-once), three marked in total, and the journal
//! compacted from five records to one.

use specimen_webhook_outbox::{EXPECTED, Report, hand_impl, tina_impl};

fn assert_outcome(report: Report) {
    assert_eq!(report, EXPECTED, "got {report:?}");
}

#[test]
fn tina_smoke() {
    assert_outcome(tina_impl::run().expect("tina side ran"));
}

#[test]
fn hand_smoke() {
    assert_outcome(hand_impl::run().expect("hand side ran"));
}

#[test]
fn both_sides_agree() {
    assert_eq!(
        tina_impl::run().expect("tina"),
        hand_impl::run().expect("hand"),
        "the durable and hand-rolled outboxes must observe the same outcome"
    );
}
