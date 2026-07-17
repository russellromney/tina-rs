use system_soak_http_db::{
    MAX_CAPACITY, MAX_DURATION_MS, MAX_REQUESTS_PER_WORKER, MAX_TOTAL_REQUESTS, MAX_WORKERS,
    RunConfig, RunConfigError, run,
};

#[test]
fn soak_emits_grep_friendly_discovery_lines() {
    let config = RunConfig {
        workers: 8,
        requests_per_worker: 16,
        ..RunConfig::default()
    };
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
        "every request must produce one outcome (report={report:?})",
    );
    assert!(
        report.ok > 0,
        "at least some requests should succeed (report={report:?})",
    );
    assert_eq!(report.timer_failed, 0, "timers should settle cleanly");
    // With http cap=4 and db cap=2 against 8 workers x 16 reqs, the
    // scopes must fill at least sometimes.
    assert!(
        report.http_full + report.db_full > 0,
        "expected at least one Full reply (report={report:?})",
    );

    // Three required discovery line kinds, kept grep-friendly.
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
fn caller_timeout_releases_parked_http_authority_on_shutdown() {
    let report = run(RunConfig {
        workers: 1,
        requests_per_worker: 1,
        http_in_flight_cap: 1,
        db_in_flight_cap: 1,
        fake_http_ms: 100,
        fake_db_ms: 100,
        call_timeout_ms: 1,
        ..RunConfig::default()
    })
    .expect("timed-out soak shuts down cleanly");

    assert_eq!(report.call_timeout, 1);
    assert_eq!(report.ok + report.http_full + report.db_full, 0);
    assert_eq!(report.slow_events_accepted, 0);
}

#[test]
fn caller_gone_before_handler_does_not_arm_http_timer() {
    let report = run(RunConfig {
        workers: 32,
        requests_per_worker: 1,
        http_in_flight_cap: 32,
        db_in_flight_cap: 32,
        fake_http_ms: 100,
        timer_capacity: 1,
        call_timeout_ms: 0,
        ..RunConfig::default()
    })
    .expect("already-timed-out callers settle without parked timers");

    assert_eq!(report.call_timeout, report.total_requests);
    assert_eq!(report.timer_failed, 0, "closed callers must not arm timers");
    assert_eq!(report.slow_events_accepted, 0);
}

#[test]
fn caller_timeout_releases_parked_db_authority_on_shutdown() {
    let report = run(RunConfig {
        workers: 1,
        requests_per_worker: 1,
        http_in_flight_cap: 1,
        db_in_flight_cap: 1,
        fake_http_ms: 1,
        fake_db_ms: 100,
        call_timeout_ms: 20,
        ..RunConfig::default()
    })
    .expect("DB-stage timeout shuts down cleanly");

    assert_eq!(report.call_timeout, 1);
    assert_eq!(report.slow_events_accepted, 0);
    let db = report
        .discovery_lines
        .iter()
        .find(|line| line.starts_with("scope ") && line.contains("name=soak.db.in_flight"))
        .expect("DB scope discovery line");
    assert!(db.contains("high=1"), "DB lease was never admitted: {db}");
}

#[test]
fn full_timer_lane_replies_with_timer_failed_and_settles_every_lease() {
    let report = run(RunConfig {
        workers: 8,
        requests_per_worker: 1,
        http_in_flight_cap: 8,
        db_in_flight_cap: 8,
        fake_http_ms: 100,
        fake_db_ms: 1,
        timer_capacity: 1,
        call_timeout_ms: 1_000,
        ..RunConfig::default()
    })
    .expect("timer pressure remains typed and shuts down cleanly");

    assert!(report.timer_failed > 0, "expected TimerFull: {report:?}");
    assert_eq!(report.call_timeout, 0, "timer Full must not become timeout");
    assert_eq!(
        report.ok
            + report.timer_failed
            + report.http_full
            + report.db_full
            + report.call_full
            + report.call_closed
            + report.call_timeout
            + report.call_rejected,
        report.total_requests
    );
}

#[test]
fn full_gateway_mailbox_remains_a_distinct_call_outcome() {
    let report = run(RunConfig {
        workers: 64,
        requests_per_worker: 1,
        http_in_flight_cap: 64,
        db_in_flight_cap: 64,
        fake_http_ms: 50,
        fake_db_ms: 1,
        gateway_mailbox: 1,
        timer_capacity: 64,
        call_timeout_ms: 1_000,
        ..RunConfig::default()
    })
    .expect("mailbox pressure remains typed and shuts down cleanly");

    assert!(report.call_full > 0, "expected call Full: {report:?}");
    assert_eq!(
        report.call_timeout, 0,
        "mailbox Full must not become timeout"
    );
    assert_eq!(
        report.slow_events_accepted, report.ok as u64,
        "only completed slow requests should emit events"
    );
}

#[test]
fn event_sink_drops_visibly_under_load() {
    // Force the slow-event sink to overflow by setting its cap small
    // and the slow threshold low enough that most requests exceed it.
    // The smoke test then asserts the dropped counter advanced — this
    // is the plan's required "bounded event sink drops visibly under
    // load" proof.
    let config = RunConfig {
        workers: 8,
        requests_per_worker: 16,
        slow_threshold_ms: 1,
        event_sink_cap: 2,
        ..RunConfig::default()
    };
    let report = run(config).expect("soak ran");

    assert!(
        report.slow_events_dropped > 0,
        "event sink should have dropped under load — report={report:?}",
    );
    // Discovery line surfaces the drop count for grep.
    let line = report
        .discovery_lines
        .iter()
        .find(|l| l.starts_with("events ") && l.contains("sink=soak.slow_requests"))
        .expect("events line");
    assert!(line.contains("dropped="), "{line}");
    let dropped_field = line
        .split_whitespace()
        .find(|tok| tok.starts_with("dropped=") && !tok.starts_with("dropped_"))
        .expect("dropped= token");
    let dropped: u64 = dropped_field
        .strip_prefix("dropped=")
        .unwrap()
        .parse()
        .expect("dropped= parses");
    assert_eq!(
        dropped, report.slow_events_dropped,
        "discovery line drop count must match report"
    );
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

#[test]
fn public_config_bounds_reject_before_runtime_startup() {
    let cases = [
        (
            RunConfig {
                workers: 0,
                ..RunConfig::default()
            },
            RunConfigError::Zero { field: "workers" },
        ),
        (
            RunConfig {
                requests_per_worker: 0,
                ..RunConfig::default()
            },
            RunConfigError::Zero {
                field: "requests_per_worker",
            },
        ),
        (
            RunConfig {
                http_in_flight_cap: 0,
                ..RunConfig::default()
            },
            RunConfigError::Zero {
                field: "http_in_flight_cap",
            },
        ),
        (
            RunConfig {
                db_in_flight_cap: 0,
                ..RunConfig::default()
            },
            RunConfigError::Zero {
                field: "db_in_flight_cap",
            },
        ),
        (
            RunConfig {
                event_sink_cap: 0,
                ..RunConfig::default()
            },
            RunConfigError::Zero {
                field: "event_sink_cap",
            },
        ),
        (
            RunConfig {
                workers: MAX_WORKERS + 1,
                ..RunConfig::default()
            },
            RunConfigError::TooLarge {
                field: "workers",
                value: MAX_WORKERS + 1,
                max: MAX_WORKERS,
            },
        ),
        (
            RunConfig {
                http_in_flight_cap: MAX_CAPACITY + 1,
                ..RunConfig::default()
            },
            RunConfigError::TooLarge {
                field: "http_in_flight_cap",
                value: MAX_CAPACITY + 1,
                max: MAX_CAPACITY,
            },
        ),
        (
            RunConfig {
                fake_http_ms: MAX_DURATION_MS + 1,
                ..RunConfig::default()
            },
            RunConfigError::DurationTooLarge {
                field: "fake_http_ms",
                value_ms: MAX_DURATION_MS + 1,
                max_ms: MAX_DURATION_MS,
            },
        ),
        (
            RunConfig {
                workers: MAX_WORKERS,
                requests_per_worker: (MAX_TOTAL_REQUESTS / MAX_WORKERS) + 1,
                ..RunConfig::default()
            },
            RunConfigError::TotalRequestsTooLarge {
                total: MAX_WORKERS * ((MAX_TOTAL_REQUESTS / MAX_WORKERS) + 1),
                max: MAX_TOTAL_REQUESTS,
            },
        ),
        (
            RunConfig {
                workers: usize::MAX,
                requests_per_worker: 2,
                ..RunConfig::default()
            },
            RunConfigError::TooLarge {
                field: "workers",
                value: usize::MAX,
                max: MAX_WORKERS,
            },
        ),
    ];

    for (config, expected) in cases {
        let error = run(config).expect_err("invalid config must fail before startup");
        assert_eq!(error.downcast_ref::<RunConfigError>(), Some(&expected));
    }

    // Checked overflow of workers * requests when both are within individual caps.
    let overflow = RunConfig {
        workers: MAX_WORKERS,
        requests_per_worker: MAX_REQUESTS_PER_WORKER,
        ..RunConfig::default()
    };
    let error = overflow
        .validate()
        .expect_err("product of max workers and max requests must overflow or exceed total max");
    assert!(
        matches!(
            error,
            RunConfigError::TotalRequestOverflow { .. }
                | RunConfigError::TotalRequestsTooLarge { .. }
        ),
        "unexpected overflow error: {error:?}"
    );

    assert_eq!(
        RunConfig::default().total_requests().expect("defaults"),
        8 * 16
    );
}
