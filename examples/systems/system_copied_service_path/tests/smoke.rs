use std::time::{Duration, Instant};
use system_copied_service_path::{MAX_CALLERS, MAX_CAPACITY, MAX_DURATION_MS, RunConfig, RunConfigError, run};

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
fn caller_timeout_does_not_wait_for_held_work_before_owner_shutdown() {
    let started = Instant::now();
    let report = run(RunConfig {
        capacity: 1,
        work_ms: 30_000,
        callers: 1,
        call_timeout_ms: 1,
        ..RunConfig::default()
    })
    .expect("owner shutdown must cancel long held work");

    assert!(
        started.elapsed() < Duration::from_secs(2),
        "run waited for the 30-second timer instead of shutting down: {report:?}"
    );
    assert_eq!(report.load.ops_timeout, 1, "report={report:?}");
    assert_eq!(report.scope_admitted, 1, "report={report:?}");
    assert_eq!(report.scope_released, 1, "report={report:?}");
    assert_eq!(report.scope_current_at_drain, 0, "report={report:?}");
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

#[test]
fn reporting_failure_still_reaches_clean_shutdown() {
    let error = run(RunConfig {
        mailbox: 0,
        ..RunConfig::default()
    })
    .expect_err("zero mailbox capacity must make the Stats call visibly full");

    assert!(
        error.to_string().contains("stats mailbox was full"),
        "unexpected reporting error: {error:#}"
    );
}

#[test]
fn public_config_bounds_reject_before_runtime_startup() {
    let cases = [
        (
            RunConfig {
                capacity: 0,
                ..RunConfig::default()
            },
            RunConfigError::Zero { field: "capacity" },
        ),
        (
            RunConfig {
                callers: 0,
                ..RunConfig::default()
            },
            RunConfigError::Zero { field: "callers" },
        ),
        (
            RunConfig {
                call_timeout_ms: 0,
                ..RunConfig::default()
            },
            RunConfigError::Zero {
                field: "call_timeout_ms",
            },
        ),
        (
            RunConfig {
                capacity: MAX_CAPACITY + 1,
                ..RunConfig::default()
            },
            RunConfigError::TooLarge {
                field: "capacity",
                value: MAX_CAPACITY + 1,
                max: MAX_CAPACITY,
            },
        ),
        (
            RunConfig {
                callers: MAX_CALLERS + 1,
                ..RunConfig::default()
            },
            RunConfigError::TooLarge {
                field: "callers",
                value: MAX_CALLERS + 1,
                max: MAX_CALLERS,
            },
        ),
        (
            RunConfig {
                work_ms: MAX_DURATION_MS + 1,
                ..RunConfig::default()
            },
            RunConfigError::DurationTooLarge {
                field: "work_ms",
                value_ms: MAX_DURATION_MS + 1,
                max_ms: MAX_DURATION_MS,
            },
        ),
        (
            RunConfig {
                call_timeout_ms: MAX_DURATION_MS + 1,
                ..RunConfig::default()
            },
            RunConfigError::DurationTooLarge {
                field: "call_timeout_ms",
                value_ms: MAX_DURATION_MS + 1,
                max_ms: MAX_DURATION_MS,
            },
        ),
    ];

    for (config, expected) in cases {
        let error = run(config).expect_err("invalid config must fail before startup");
        assert_eq!(error.downcast_ref::<RunConfigError>(), Some(&expected));
    }

    assert_eq!(
        RunConfig::default().validate().expect("defaults validate"),
        RunConfig::default()
    );
}
