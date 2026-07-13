use system_cache_with_fill::{ConfigError, RunConfig, run};

#[test]
fn cache_fill_is_single_flight_and_stale_results_do_not_cache() {
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
    assert_eq!(report.single_flight.stats.fills_completed, 1);
    assert_eq!(report.single_flight.stats.pending_high_water, 5);
    assert_eq!(report.single_flight.stats.pending_full_rejects, 3);
    assert_eq!(report.single_flight.stats.pending_current, 0);
    assert_eq!(report.single_flight.stats.callers_gone, 0);
    assert_eq!(report.single_flight.stats.active_fills, 0);

    assert_eq!(report.stale_invalidation.stats.invalidations, 1);
    assert_eq!(report.stale_invalidation.stats.stale_replies, 1);
    assert_eq!(report.stale_invalidation.stats.stale_completions, 1);
    assert_eq!(report.stale_invalidation.stats.fills_started, 2);
    assert_eq!(report.stale_invalidation.stats.fills_completed, 1);
    assert_eq!(report.stale_invalidation.stats.pending_current, 0);
    assert_eq!(report.stale_invalidation.stats.active_fills, 0);

    assert_eq!(report.caller_gone.stats.fills_started, 1);
    assert_eq!(report.caller_gone.stats.fills_completed, 1);
    assert_eq!(report.caller_gone.stats.callers_gone, 1);
    assert_eq!(report.caller_gone.stats.pending_current, 0);
    assert_eq!(report.caller_gone.stats.active_fills, 0);

    assert_eq!(report.entry_capacity.stats.entry_full_rejects, 1);
    assert_eq!(report.entry_capacity.stats.entries, 1);
    assert_eq!(report.entry_capacity.stats.pending_current, 0);
}

#[test]
fn invalid_configuration_is_typed_and_rejected_before_startup() {
    let cases = [
        (
            RunConfig {
                callers: 0,
                ..RunConfig::default()
            },
            ConfigError::ZeroCallers,
        ),
        (
            RunConfig {
                pending_capacity: 0,
                ..RunConfig::default()
            },
            ConfigError::ZeroPendingCapacity,
        ),
        (
            RunConfig {
                entry_capacity: 0,
                ..RunConfig::default()
            },
            ConfigError::ZeroEntryCapacity,
        ),
        (
            RunConfig {
                cache_mailbox: 0,
                ..RunConfig::default()
            },
            ConfigError::ZeroCacheMailbox,
        ),
        (
            RunConfig {
                fill_ms: 0,
                ..RunConfig::default()
            },
            ConfigError::ZeroFillDelay,
        ),
        (
            RunConfig {
                call_timeout_ms: 0,
                ..RunConfig::default()
            },
            ConfigError::ZeroCallTimeout,
        ),
        (
            RunConfig {
                callers: usize::MAX,
                ..RunConfig::default()
            },
            ConfigError::TooManyCallers {
                requested: usize::MAX,
                max: 4_096,
            },
        ),
        (
            RunConfig {
                entry_capacity: usize::MAX,
                ..RunConfig::default()
            },
            ConfigError::EntryCapacityTooLarge {
                requested: usize::MAX,
                max: 65_536,
            },
        ),
        (
            RunConfig {
                pending_capacity: usize::MAX,
                ..RunConfig::default()
            },
            ConfigError::PendingCapacityTooLarge {
                requested: usize::MAX,
                max: 65_536,
            },
        ),
        (
            RunConfig {
                cache_mailbox: usize::MAX,
                ..RunConfig::default()
            },
            ConfigError::CacheMailboxTooLarge {
                requested: usize::MAX,
                max: 65_536,
            },
        ),
    ];

    for (config, expected) in cases {
        let error = run(config).expect_err("invalid configuration must fail");
        assert_eq!(error.downcast_ref::<ConfigError>(), Some(&expected));
    }
}
