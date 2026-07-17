//! Public runner proof for the scoped request tree.

use system_scoped_request_tree::{ScopedTreeReport, run};

fn assert_report(report: &ScopedTreeReport) {
    assert!(
        report.clean_completion,
        "the full upload should complete with 200: {}",
        report.summary_line(),
    );
    assert!(
        report.disconnect_cause_client,
        "the disconnect report must name ClientDisconnect: {}",
        report.disconnect_report_line,
    );
    assert!(
        report.disconnect_cancelled_children >= 1,
        "the disconnect must cancel at least one pending child: {}",
        report.disconnect_report_line,
    );
    assert!(
        report.disconnect_report_clean,
        "every rail the request used was scope-cancelable: {}",
        report.disconnect_report_line,
    );
    assert!(
        report.scope_capacity_reclaimed,
        "the scope set must reclaim the slot after teardown: {}",
        report.disconnect_report_line,
    );
    assert!(
        report.timeout_replied_504,
        "a request that blew its deadline must get a live-timer 504: {}",
        report.summary_line(),
    );
    assert!(
        report.timers_ignored_late >= 1,
        "a tombstoned deadline timer must fire late and be ignored, not run: {}",
        report.summary_line(),
    );
    assert!(
        report.enrich_cancel_ack_cancelled,
        "the enrich child's wait must really close (ack Cancelled): {}",
        report.disconnect_report_line,
    );
    assert_eq!(
        report.late_results, 0,
        "no late enrich result should have been delivered as success",
    );
    assert!(
        report.replay_fact_line.contains("fact=request_scope_set"),
        "sim/replay agreement line must carry the request-scope-set fact: {}",
        report.replay_fact_line,
    );
}

/// Documented public runner path: `run()`.
#[test]
fn public_smoke() {
    let report = run().expect("scoped request tree run");
    assert_report(&report);
}

/// Pins accepted protocol and report facts the public runner must preserve.
#[test]
fn public_characterization() {
    let report = run().expect("scoped request tree run");
    assert_report(&report);
    assert!(
        report.summary_line().starts_with("system=scoped_request_tree "),
        "{}",
        report.summary_line()
    );
    assert!(
        report.disconnect_report_line.contains("ClientDisconnect")
            || report.disconnect_cause_client,
        "disconnect report line must name the cause: {}",
        report.disconnect_report_line,
    );
}
