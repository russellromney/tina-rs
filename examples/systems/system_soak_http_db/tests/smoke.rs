use system_soak_http_db::{RunConfig, run};

#[test]
fn soak_emits_grep_friendly_discovery_lines() {
    let config = RunConfig {
        workers: 8,
        requests_per_worker: 16,
        ..RunConfig::default()
    };
    let report = run(config).expect("soak ran");

    assert_eq!(
        report.ok + report.http_full + report.db_full,
        report.total_requests,
        "every request must produce one outcome (report={report:?})",
    );
    assert!(
        report.ok > 0,
        "at least some requests should succeed (report={report:?})",
    );
    // With http cap=4 and db cap=2 against 8 workers x 16 reqs, the
    // scopes must fill at least sometimes.
    assert!(
        report.http_full + report.db_full > 0,
        "expected at least one Full reply (report={report:?})",
    );

    // Three required discovery line kinds (Rock 4: grep-friendly).
    let has_http = report
        .discovery_lines
        .iter()
        .any(|l| l.starts_with("scope ") && l.contains("name=soak.http.in_flight"));
    let has_db = report
        .discovery_lines
        .iter()
        .any(|l| l.starts_with("scope ") && l.contains("name=soak.db.in_flight"));
    let has_events = report
        .discovery_lines
        .iter()
        .any(|l| l.starts_with("events ") && l.contains("sink=soak.slow_requests"));
    let has_unavailable = report
        .discovery_lines
        .iter()
        .any(|l| l.contains("surface=soak.outbound.pool") && l.contains("state=unavailable"));
    assert!(
        has_http,
        "missing http scope line: {:?}",
        report.discovery_lines
    );
    assert!(
        has_db,
        "missing db scope line: {:?}",
        report.discovery_lines
    );
    assert!(
        has_events,
        "missing events line: {:?}",
        report.discovery_lines
    );
    assert!(
        has_unavailable,
        "missing unavailable surface line: {:?}",
        report.discovery_lines
    );

    // util_bp must appear on the capacity surface=… lines so CI can
    // grep utilization without parsing weights.
    let cap_lines: Vec<&String> = report
        .discovery_lines
        .iter()
        .filter(|l| l.starts_with("capacity surface="))
        .collect();
    assert!(
        cap_lines.iter().any(|l| l.contains("util_bp=")),
        "no util_bp on capacity surface lines: {cap_lines:?}",
    );

    // Service summary names the unavailable surface so the on-call
    // can find a missing observer instead of assuming "no Full".
    assert!(
        report
            .service_summary_line
            .contains("soak.outbound.pool=unavailable"),
        "service summary missing unavailable hint: {}",
        report.service_summary_line
    );

    // CI-friendly assertion lines for any Full surface are copyable.
    for line in &report.copyable_assertion_failures {
        assert!(
            line.starts_with("FAIL "),
            "assertion failure line should start with FAIL: {line:?}",
        );
    }
}

#[test]
fn soak_with_no_pressure_passes_assert_no_full() {
    // Cap large enough that nothing fills. Slow threshold raised
    // higher than fake_http + fake_db so the event sink stays empty.
    let config = RunConfig {
        workers: 4,
        requests_per_worker: 4,
        http_in_flight_cap: 32,
        db_in_flight_cap: 32,
        pending_capacity: 64,
        slow_threshold_ms: 1_000,
        event_sink_cap: 32,
        ..RunConfig::default()
    };
    let report = run(config).expect("soak ran");
    assert_eq!(report.http_full + report.db_full, 0);
    assert!(
        report.copyable_assertion_failures.is_empty(),
        "expected no assertion failures: {:?}",
        report.copyable_assertion_failures
    );
}
