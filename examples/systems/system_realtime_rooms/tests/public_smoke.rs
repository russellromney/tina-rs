//! Public runner proof for the realtime rooms system.

use system_realtime_rooms::{RunConfig, run};

/// Documented public runner path: `run(RunConfig::default())`.
#[test]
fn public_smoke() {
    let report = run(RunConfig::default()).expect("run");
    assert!(report.join_and_tick.bootstrap_seen);
    assert!(report.join_and_tick.joined >= 2);
    assert!(report.join_and_tick.tick_broadcasts_seen >= 1);
    assert!(report.join_and_tick.stats.presence_ticks >= 2);
    assert_eq!(report.overflow.admitted, 2);
    assert_eq!(report.overflow.rejected_full, 2);
    assert!(report.shutdown.close_observed >= 1);
    assert!(report.shutdown.stats.shutdown_started);
}

/// Pins accepted join/tick/overflow/shutdown facts.
#[test]
fn public_characterization() {
    let config = RunConfig::default();
    assert_eq!(config.member_capacity, 3);
    assert_eq!(config.presence_tick_ms, 80);
    assert_eq!(config.idle_evict_after_ms, 1_000);
    assert_eq!(config.room_mailbox_capacity, 256);

    let report = run(config).expect("run");
    assert!(
        report.join_and_tick.stats.presence_broadcasts_ok
            >= report.join_and_tick.tick_broadcasts_seen
    );
    assert_eq!(report.overflow.stats.member_capacity, 2);
    assert_eq!(report.overflow.stats.member_high_water, 2);
    assert!(report.shutdown.stats.shutdown_close_requested >= 2);
}
