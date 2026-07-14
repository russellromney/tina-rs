//! Smoke proof that one scope cancel closes every dispatched rail and
//! that late completions surface as typed trace facts.

use specimen_request_scope_fanout::{CANCEL_AFTER_MS, FANOUT, WORK_MS, run};
use tina_runtime::ScopeCancelCause;

#[test]
fn one_cancel_closes_every_rail_and_late_replies_are_visible() {
    // CANCEL_AFTER_MS is much smaller than WORK_MS, so none of the
    // rails should reply naturally before the cancel fires. If this
    // ever starts flaking, raise WORK_MS or shrink CANCEL_AFTER_MS.
    const _: () = assert!(
        CANCEL_AFTER_MS < WORK_MS / 2,
        "test fixture inverted: cancel must come well before any natural reply",
    );

    let report = run().expect("specimen run");

    assert_eq!(report.rails_total, FANOUT, "dispatched every fanout rail",);
    assert_eq!(
        report.cause,
        ScopeCancelCause::CallerCancelled,
        "scope cancel cause must propagate to the report",
    );
    assert_eq!(report.cancel_outcomes.len(), FANOUT as usize);
    assert!(
        report
            .cancel_outcomes
            .iter()
            .all(|outcome| matches!(outcome, tina::CancelOutcome::Cancelled)),
        "every pending child should retain its exact cancellation ack: {report:?}",
    );
    assert_eq!(report.child_replied, 0, "fixture cancels before replies");
    assert_eq!(report.child_full, 0);
    assert_eq!(report.child_closed, 0);
    assert_eq!(report.child_timeout, 0);
    assert!(report.child_rejected.is_empty());
    assert!(report.child_timer_failed.is_empty());
    assert!(report.driver_timer_failures.is_empty());
    assert_eq!(
        report.cancel_acks, FANOUT,
        "one cancel ack per rail; got {}",
        report.cancel_acks,
    );
    assert_eq!(
        report.rails_pending_at_cancel + report.rails_settled_before_cancel,
        FANOUT,
        "every rail is in exactly one of {{pending-at-cancel, settled-before-cancel}}",
    );

    // The interesting honesty bit: late worker replies must surface as
    // typed trace facts. The fixture timing guarantees at least the
    // pending-at-cancel rails will fire late.
    assert!(
        report.late_rejected_in_trace >= report.rails_pending_at_cancel,
        "every pending-at-cancel rail should appear as a late-reply trace fact; \
         got {} rejected vs {} pending",
        report.late_rejected_in_trace,
        report.rails_pending_at_cancel,
    );
}
