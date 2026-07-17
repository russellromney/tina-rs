use system_soak_http_db::{RunConfig, run};

/// Public runner path documented in the README.
#[test]
fn public_smoke() {
    let config = RunConfig::default();
    let report = run(config).expect("soak ran");
    assert_eq!(
        report.ok
            + report.http_full
            + report.db_full
            + report.timer_failed
            + report.call_full
            + report.call_closed
            + report.call_timeout
            + report.call_rejected,
        report.total_requests,
        "report={report:?}"
    );
    assert!(report.ok > 0, "report={report:?}");
    assert!(
        report
            .discovery_lines
            .iter()
            .any(|line| line.contains("name=soak.http.in_flight")),
        "missing http discovery: {:?}",
        report.discovery_lines
    );
    assert!(
        report
            .discovery_lines
            .iter()
            .any(|line| line.contains("name=soak.db.in_flight")),
        "missing db discovery: {:?}",
        report.discovery_lines
    );
}

/// Accepted default soak workload counts.
#[test]
fn public_characterization() {
    let config = RunConfig::default();
    assert_eq!(config.workers, 8);
    assert_eq!(config.requests_per_worker, 16);
    assert_eq!(config.http_in_flight_cap, 4);
    assert_eq!(config.db_in_flight_cap, 2);
    assert_eq!(config.fake_http_ms, 5);
    assert_eq!(config.fake_db_ms, 8);
    assert_eq!(config.slow_threshold_ms, 12);
    assert_eq!(config.event_sink_cap, 8);
    assert_eq!(config.gateway_mailbox, 64);
    assert_eq!(config.call_timeout_ms, 5_000);
    assert_eq!(config.total_requests().expect("defaults"), 128);

    let report = run(config).expect("characterization soak");
    assert_eq!(report.total_requests, 128, "report={report:?}");
    assert_eq!(
        report.ok
            + report.http_full
            + report.db_full
            + report.timer_failed
            + report.call_full
            + report.call_closed
            + report.call_timeout
            + report.call_rejected,
        128,
        "report={report:?}"
    );
}
