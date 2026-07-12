use system_bounded_object_lane::{RunConfig, run};

#[test]
fn overload_is_visible_as_busy_not_hidden_queueing() {
    let report = run(RunConfig {
        callers: 10,
        lane_in_flight: 2,
        lane_mailbox: 32,
        work_ms: 100,
        call_timeout_ms: 2_000,
    })
    .expect("run succeeds");

    assert_eq!(report.callers, 10);
    assert_eq!(report.failed, 0);
    assert_eq!(report.stored, 2);
    assert_eq!(report.busy, 8);
    assert_eq!(report.stats.accepted, 2);
    assert_eq!(report.stats.completed, 2);
    assert_eq!(report.stats.busy, 8);
    assert_eq!(report.stats.in_flight, 0);
}

#[test]
fn report_failure_returns_after_bounded_shutdown() {
    let error = run(RunConfig {
        lane_mailbox: 0,
        ..RunConfig::default()
    })
    .expect_err("zero-capacity mailbox must refuse the stats call");

    assert!(
        format!("{error:#}").contains("stats call failed: Full"),
        "unexpected error: {error:#}"
    );
}
