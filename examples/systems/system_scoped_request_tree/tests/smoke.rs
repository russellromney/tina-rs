//! Streaming-body-disconnect proof for the scoped request tree.
//!
//! One clean upload completes; one mid-body disconnect cancels the
//! request scope's pending child and tombstones the deadline timer. The
//! report names the cancel cause, the cancelled child, the reclaimed
//! capacity, and the late-but-ignored timer — no ghosts, no fakes.

use system_scoped_request_tree::run;

#[test]
fn streaming_body_disconnect_cancels_request_tree() {
    let report = run().expect("scoped request tree run");

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
