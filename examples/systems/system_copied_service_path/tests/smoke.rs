use system_copied_service_path::{RunConfig, run};

#[test]
fn copied_service_path_smoke() {
    // 6 callers race against capacity 2: some are admitted, some see Full.
    let report = run(RunConfig::default()).expect("copied service path run");

    assert!(
        report.admitted > 0,
        "at least one caller should be admitted, got report={report:?}",
    );
    assert!(
        report.full > 0,
        "6 callers against capacity=2 must see Full — report={report:?}",
    );
    assert_eq!(
        report.admitted + report.full,
        RunConfig::default().callers,
        "every caller saw a reply — report={report:?}",
    );

    // Durable-state step: one ledger entry committed per admitted
    // request, on top of the recovered seed.
    assert_eq!(
        report.ledger_final_len,
        report.ledger_seed_len + report.admitted,
        "ledger should hold the seed plus one entry per admitted request — report={report:?}",
    );

    // The load-bearing claim: owner stop released every held charge.
    // `run()` already asserted this via
    // `assert_no_leaked_capacity_at_shutdown(&report.load)`; re-check the
    // raw scope fields here too so a future refactor that drops that
    // assertion still gets caught.
    assert!(
        report.load.leak_checked,
        "leak check must actually run — report={report:?}",
    );
    assert!(
        report.load.leak_clean,
        "leak check must report clean — report={report:?}",
    );
    assert_eq!(
        report.scope_current_at_drain, 0,
        "owner stop must release every held charge — report={report:?}",
    );
    assert_eq!(
        report.scope_admitted, report.scope_released,
        "every admitted charge must be released — report={report:?}",
    );

    assert!(
        report.discovery_line.starts_with("scope ")
            && report
                .discovery_line
                .contains("name=copied_service_path.in_flight"),
        "missing scope discovery line: {}",
        report.discovery_line,
    );
    assert!(
        report
            .summary_line
            .starts_with("system=system_copied_service_path "),
        "summary_line shape: {}",
        report.summary_line
    );
}
