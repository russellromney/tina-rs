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
    assert_eq!(report.stats.work_completed, 2);
    assert_eq!(report.stats.completed, 2);
    assert_eq!(report.stats.busy, 8);
    assert_eq!(report.stats.current, 0);
    assert_eq!(report.stats.retired, 0);
    assert_eq!(report.stats.caller_gone, 0);
    assert!(report.stats.counts_agree);
    assert!(report.stats.settlements_agree);
    assert_eq!(report.dropped_permits, 0);
    assert_eq!(report.full, 0);
    assert_eq!(report.closed, 0);
    assert_eq!(report.timeout, 0);
    assert_eq!(report.rejected, 0);
    assert!(report.rejection_reasons.is_empty());
}

#[test]
fn report_failure_returns_after_bounded_shutdown() {
    let error = run(RunConfig {
        lane_mailbox: 0,
        ..RunConfig::default()
    })
    .expect_err("zero-capacity mailbox must refuse the stats call");

    assert!(
        format!("{error:#}").contains("stats call mailbox was full"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn invalid_config_returns_without_allocating_or_panicking() {
    let error = run(RunConfig {
        callers: 0,
        ..RunConfig::default()
    })
    .expect_err("zero callers is unsafe configuration");
    assert!(format!("{error:#}").contains("callers=0 is outside 1..=10000"));

    let error = run(RunConfig {
        lane_mailbox: 100_001,
        ..RunConfig::default()
    })
    .expect_err("oversized mailbox is unsafe configuration");
    assert!(format!("{error:#}").contains("lane_mailbox=100001"));
}
