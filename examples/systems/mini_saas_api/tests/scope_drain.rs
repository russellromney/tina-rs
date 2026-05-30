//! Owner-stop scope sweep proves the notify-path request scope is
//! functional, not decorative: a notify held mid-outbound has a pending
//! child rail, and draining the scope set cancels that child.

use mini_saas_api::prove_drain_cancels_active_scope;

#[test]
fn owner_stop_sweep_cancels_a_pending_outbound_child() {
    let report = prove_drain_cancels_active_scope().expect("drain-active proof ran");

    assert_eq!(
        report.scopes_cancelled, 1,
        "the in-flight notify's scope must be drained: {}",
        report.drain_line,
    );
    assert!(
        report.children_cancelled >= 1,
        "the parked outbound request call must be cancelled by the sweep: {}",
        report.drain_line,
    );
    assert_eq!(
        report.unreleased, 0,
        "the scope set must be empty after the sweep: {}",
        report.drain_line,
    );
    assert!(
        report.slow_notify_aborted,
        "the stranded notify must not return a successful `notified`: {}",
        report.drain_line,
    );
}
