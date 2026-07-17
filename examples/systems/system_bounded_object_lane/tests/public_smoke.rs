use system_bounded_object_lane::{RunConfig, RunConfigError, run, run_put_terminals};

/// Public runner path documented in the README.
#[test]
fn public_smoke() {
    let report = run(RunConfig {
        callers: 10,
        lane_in_flight: 2,
        lane_mailbox: 32,
        work_ms: 100,
        call_timeout_ms: 2_000,
    })
    .expect("run succeeds");

    assert_eq!(report.callers, 10);
    assert_eq!(report.stored, 2);
    assert_eq!(report.busy, 8);
    assert_eq!(report.full, 0);
    assert_eq!(report.closed, 0);
    assert_eq!(report.timeout, 0);
    assert_eq!(report.rejected, 0);
    assert!(report.stats.counts_agree);
    assert!(report.stats.settlements_agree);

    let full = run_put_terminals(RunConfig {
        callers: 2,
        lane_in_flight: 1,
        lane_mailbox: 0,
        work_ms: 1,
        call_timeout_ms: 200,
    })
    .expect("mailbox full terminals");
    assert_eq!(full.full, 2);
    assert_eq!(full.busy, 0);
}

/// Accepted object-lane workload counts.
#[test]
fn public_characterization() {
    let config = RunConfig::default();
    assert_eq!(config.callers, 12);
    assert_eq!(config.lane_in_flight, 2);
    assert_eq!(config.lane_mailbox, 32);
    assert_eq!(config.work_ms, 120);
    assert_eq!(config.call_timeout_ms, 2_000);
    assert_eq!(config.validate().expect("defaults"), config);

    assert!(matches!(
        RunConfig {
            callers: 0,
            ..RunConfig::default()
        }
        .validate(),
        Err(RunConfigError::OutOfRange {
            field: "callers",
            ..
        })
    ));
    assert!(matches!(
        RunConfig {
            lane_in_flight: 10_001,
            ..RunConfig::default()
        }
        .validate(),
        Err(RunConfigError::OutOfRange {
            field: "lane_in_flight",
            ..
        })
    ));
}
