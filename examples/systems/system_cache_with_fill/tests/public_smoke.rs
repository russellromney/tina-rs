use system_cache_with_fill::{ConfigError, RunConfig, TerminalOutcome, run, run_mailbox_full};

/// Public runner path documented in the README.
#[test]
fn public_smoke() {
    let report = run(RunConfig {
        callers: 8,
        pending_capacity: 5,
        entry_capacity: 8,
        cache_mailbox: 64,
        fill_ms: 80,
        call_timeout_ms: 2_000,
    })
    .expect("run succeeds");

    assert_eq!(report.single_flight.callers, 8);
    assert_eq!(report.single_flight.filled, 5);
    assert_eq!(report.single_flight.busy, 3);
    assert_eq!(report.single_flight.stats.fills_started, 1);
    assert_eq!(report.entry_capacity.stats.entry_full_rejects, 1);
    assert_eq!(report.caller_gone.stats.callers_gone, 1);
    assert_eq!(
        run_mailbox_full(RunConfig::default()).expect("mailbox full"),
        TerminalOutcome::Full
    );
}

/// Accepted cache workload counts and pressure outcomes.
#[test]
fn public_characterization() {
    let config = RunConfig::default();
    assert_eq!(config.callers, 8);
    assert_eq!(config.pending_capacity, 5);
    assert_eq!(config.entry_capacity, 64);
    assert_eq!(config.cache_mailbox, 64);
    assert_eq!(config.fill_ms, 120);
    assert_eq!(config.call_timeout_ms, 2_000);
    assert_eq!(config.validate().expect("defaults"), config);

    assert!(matches!(
        RunConfig {
            callers: 0,
            ..RunConfig::default()
        }
        .validate(),
        Err(ConfigError::ZeroCallers)
    ));
    assert!(matches!(
        RunConfig {
            pending_capacity: usize::MAX,
            ..RunConfig::default()
        }
        .validate(),
        Err(ConfigError::PendingCapacityTooLarge { .. })
    ));
}
