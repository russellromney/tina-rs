//! Public runner proof for the request-scope fanout specimen.
//!
//! Characterization pins the cancel accounting: one scope cancel
//! closes every dispatched rail and late worker replies surface as
//! typed trace facts. Public smoke exercises the documented runner
//! path against a live `LocalSystem`.

use specimen_request_scope_fanout::{FANOUT, Report, run};
use tina_runtime::ScopeCancelCause;

fn assert_scope_cancelled(report: &Report) {
    assert_eq!(report.rails_total, FANOUT, "dispatched every fanout rail");
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
    // The fixture cancels well before any worker can reply.
    assert_eq!(report.child_replied, 0);
    assert_eq!(report.child_full, 0);
    assert_eq!(report.child_closed, 0);
    assert_eq!(report.child_timeout, 0);
    assert!(report.child_rejected.is_empty());
    assert!(report.child_timer_failed.is_empty());
    assert!(report.driver_timer_failures.is_empty());
    assert_eq!(report.cancel_acks, FANOUT, "one cancel ack per rail");
    assert_eq!(
        report.rails_pending_at_cancel + report.rails_settled_before_cancel,
        FANOUT,
        "every rail is in exactly one of {{pending-at-cancel, settled-before-cancel}}",
    );
    // The pending/settled split is timing-sensitive, so exact per-lane
    // counts are not pinned; the pin is that every pending-at-cancel
    // rail shows up as a late-reply trace fact.
    assert!(
        report.late_rejected_in_trace >= report.rails_pending_at_cancel,
        "every pending-at-cancel rail should appear as a late-reply trace fact: {report:?}",
    );
}

/// Pins scope-cancel accounting before/after host-result migration.
#[test]
fn public_characterization() {
    assert_scope_cancelled(&run().expect("tina side ran"));
}

/// Documented public runner path: `run()`.
#[test]
fn public_smoke() {
    assert_scope_cancelled(&run().expect("tina side ran"));
}
