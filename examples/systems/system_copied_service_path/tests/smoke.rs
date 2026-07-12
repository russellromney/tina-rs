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

#[test]
fn caller_timeout_still_settles_linear_authority() {
    let config = RunConfig {
        capacity: 1,
        work_ms: 30,
        callers: 1,
        call_timeout_ms: 1,
        ..RunConfig::default()
    };
    let report = run(config).expect("timeout run must drain and shut down cleanly");

    assert_eq!(report.load.ops_timeout, 1, "report={report:?}");
    assert_eq!(report.admitted, 1, "report={report:?}");
    assert_eq!(
        report.ledger_final_len,
        report.ledger_seed_len + report.admitted,
        "durable admission remains committed after caller loss: {report:?}",
    );
    assert_eq!(report.scope_current_at_drain, 0, "report={report:?}");
    assert_eq!(report.scope_admitted, report.scope_released);
}

#[test]
fn timer_pressure_is_typed_and_every_admission_settles() {
    let config = RunConfig {
        capacity: 4,
        work_ms: 50,
        callers: 4,
        timer_capacity: 1,
        ..RunConfig::default()
    };
    let report = run(config).expect("timer pressure run");
    let timer_full = report
        .load
        .err_kinds
        .iter()
        .find(|(kind, _)| kind == "work_timer_full")
        .map_or(0, |(_, count)| *count);

    assert!(
        timer_full > 0,
        "timer pressure was not observed: {report:?}"
    );
    assert_eq!(report.admitted, config.callers, "report={report:?}");
    assert_eq!(
        report.ledger_final_len,
        report.ledger_seed_len + report.admitted,
        "every admitted record is durable even if work admission fails: {report:?}",
    );
    assert_eq!(report.scope_current_at_drain, 0, "report={report:?}");
    assert_eq!(report.scope_admitted, report.scope_released);
}

#[test]
fn invalid_runtime_capacity_is_a_startup_error() {
    let error = run(RunConfig {
        timer_capacity: 0,
        ..RunConfig::default()
    })
    .expect_err("zero timer capacity must fail startup");

    assert!(
        error.to_string().contains("timer_capacity"),
        "unexpected startup error: {error:#}"
    );
}
