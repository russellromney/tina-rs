//! Smoke tests: each side runs end-to-end and accounts for every
//! fanout attempt. Exact wire-shape invariants live in
//! `tina-runtime`'s own tests.

use specimen_real_io_chat::{
    MAX_BROADCAST_TARGETS, MAX_BURST, MAX_SLOW_CONSUMER_CAPACITY, RunConfig, tina_impl, tokio_impl,
};

#[test]
fn tokio_smoke() {
    let config = RunConfig::default();
    let report = tokio_impl::run(config).expect("tokio side ran");
    assert_eq!(
        report.total(),
        config.burst,
        "every fanout attempt must be accounted for: {report:?}",
    );
}

#[test]
fn tina_smoke() {
    let config = RunConfig::default();
    let report = tina_impl::run(config).expect("tina side ran");
    assert_eq!(
        report.total(),
        config.burst,
        "every fanout attempt must be accounted for: {report:?}",
    );
}

#[test]
fn tina_caps_request_sized_fanout_before_effects() {
    let config = RunConfig {
        burst: 16,
        max_broadcast_targets: 4,
        slow_consumer_capacity: 1,
    };
    let report = tina_impl::run(config).expect("tina side ran");
    assert_eq!(
        report.total(),
        config.burst,
        "pre-shed and admitted fanout must both be accounted for: {report:?}",
    );
    assert_eq!(report.accepted, 1, "slow consumer cap admits one");
    assert_eq!(
        report.full, 15,
        "three admitted targets hit mailbox Full and twelve targets are shed before effects exist",
    );
    assert_eq!(report.buffered, 0);
}

#[test]
fn invalid_workload_bounds_fail_before_startup() {
    for config in [
        RunConfig {
            burst: 0,
            ..RunConfig::default()
        },
        RunConfig {
            burst: MAX_BURST + 1,
            ..RunConfig::default()
        },
        RunConfig {
            max_broadcast_targets: 0,
            ..RunConfig::default()
        },
        RunConfig {
            max_broadcast_targets: MAX_BROADCAST_TARGETS + 1,
            ..RunConfig::default()
        },
        RunConfig {
            slow_consumer_capacity: 0,
            ..RunConfig::default()
        },
        RunConfig {
            slow_consumer_capacity: MAX_SLOW_CONSUMER_CAPACITY + 1,
            ..RunConfig::default()
        },
    ] {
        assert!(tina_impl::run(config).is_err(), "Tina accepted {config:?}");
        assert!(
            tokio_impl::run(config).is_err(),
            "Tokio accepted {config:?}"
        );
    }
}
