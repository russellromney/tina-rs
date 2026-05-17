use system_api_gateway_limits::{RunConfig, run};

#[test]
fn shared_scope_fills_and_releases_across_routes() {
    let config = RunConfig {
        upload_callers: 4,
        list_callers: 6,
        ..RunConfig::default()
    };
    let report = run(config).expect("gateway run");

    // Cap is 4; upload weight 2 means at most two uploads in flight.
    // Six list callers (weight 1) plus four upload callers race
    // against shared_cap=4. Total admitted weight peaks at 4.
    assert_eq!(
        report.upload_admitted + report.list_admitted + report.upload_full + report.list_full,
        config.upload_callers + config.list_callers,
        "every caller saw a reply",
    );

    assert!(
        report.upload_admitted > 0,
        "at least one upload should admit, got report={report:?}",
    );
    assert!(
        report.list_admitted > 0,
        "at least one list should admit, got report={report:?}",
    );
    let total_full = report.upload_full + report.list_full;
    assert!(
        total_full > 0,
        "shared cap is 4 but {} weighted callers raced — expected some Full, got report={report:?}",
        config.upload_callers * config.upload_weight + config.list_callers * config.list_weight,
    );
    assert_eq!(
        report.scope_full_count as usize, total_full,
        "scope.full_count should match observed Full replies",
    );

    // The scope drained: nothing held after the run.
    assert_eq!(
        report.scope_current_at_drain, 0,
        "owner stop must release every held charge — report={report:?}",
    );
    assert_eq!(
        report.scope_admitted, report.scope_released,
        "every admitted weight must be released — report={report:?}",
    );

    // Discovery lines are grep-friendly.
    assert!(
        report
            .discovery_lines
            .iter()
            .any(|l| l.starts_with("scope ") && l.contains("name=gateway.in_flight")),
        "missing scope discovery line: {:?}",
        report.discovery_lines
    );
    assert!(
        report
            .discovery_lines
            .iter()
            .any(|l| l.starts_with("capacity ") && l.contains("surface=gateway.in_flight")),
        "missing capacity discovery line: {:?}",
        report.discovery_lines
    );
    let cap_line = report
        .discovery_lines
        .iter()
        .find(|l| l.starts_with("capacity "))
        .unwrap();
    assert!(cap_line.contains("util_bp="), "util_bp missing: {cap_line}");

    // Summary line is one greppable token-soup.
    assert!(
        report
            .summary_line
            .starts_with("system=system_api_gateway_limits "),
        "summary_line shape: {}",
        report.summary_line
    );
}

#[test]
fn pure_upload_burst_fills_only_upload_lane_then_drains() {
    // No list callers; uploads alone should fill the shared scope
    // because shared_cap=4 / weight=2 = 2 concurrent uploads.
    let config = RunConfig {
        upload_callers: 5,
        list_callers: 0,
        ..RunConfig::default()
    };
    let report = run(config).expect("gateway run");

    assert_eq!(report.upload_admitted + report.upload_full, 5);
    assert!(
        report.scope_full_count > 0,
        "5 uploads against cap=4/weight=2 must see Full — report={report:?}",
    );
    assert_eq!(report.scope_current_at_drain, 0);
}
