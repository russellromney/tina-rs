//! Public runner proof for the metrics shipper system.

use system_metrics_shipper::{RunConfig, lifecycle_for_drain_stage, run};
use tina_runtime::DrainStage;
use tina_runtime::lifecycle::Lifecycle;

fn assert_shipper_report(report: system_metrics_shipper::RunReport) {
    let steady = &report.steady;
    assert_eq!(steady.submitted, 32);
    assert_eq!(steady.accepted, 32);
    assert_eq!(steady.dropped_full, 0);
    assert_eq!(steady.stopping_rejects, 0);
    assert_eq!(steady.shipper_mailbox_full, 0);
    assert_eq!(steady.sink.events_received, steady.accepted as u64);
    assert!(steady.stats.buffer_high_water <= steady.stats.buffer_capacity);

    let overload = &report.overload;
    assert!(
        overload.dropped_full > 0,
        "expected buffer overflow to produce typed Dropped replies, got stats {:?}",
        overload.stats
    );
    assert_eq!(
        overload.stats.buffer_full_rejects as usize, overload.dropped_full,
        "buffer_full_rejects must match typed Dropped count"
    );
    assert_eq!(overload.accepted, overload.sink.events_received as usize);
    assert_eq!(overload.stats.events_lost_on_flush, 0);

    let shutdown = &report.shutdown;
    assert!(shutdown.stop_clean, "stop reply must be Stopped");
    assert!(
        shutdown.drained_batches >= 1,
        "drain must flush at least one batch; got stats {:?}",
        shutdown.stats
    );
    assert_eq!(
        shutdown.flushed_on_drain as u64, shutdown.sink.events_received,
        "every drained event must reach the sink",
    );

    assert_eq!(
        lifecycle_for_drain_stage(DrainStage::Open),
        Lifecycle::Ready
    );
    assert_eq!(
        lifecycle_for_drain_stage(DrainStage::Draining),
        Lifecycle::Draining,
    );
    assert_eq!(
        lifecycle_for_drain_stage(DrainStage::Stopped),
        Lifecycle::Stopped,
    );
}

fn default_config() -> RunConfig {
    RunConfig {
        events: 32,
        callers: 4,
        buffer_capacity: 8,
        batch_size: 4,
        batch_window_ms: 20,
        shipper_mailbox: 4,
        sink_mailbox: 4,
        call_timeout_ms: 2_000,
        flush_timeout_ms: 1_000,
        stop_timeout_ms: 2_000,
        sink_fail_every: 0,
        sink_flush_delay_ms: 0,
    }
}

/// Pins steady/overload/drain metrics semantics.
#[test]
fn public_characterization() {
    assert_shipper_report(run(default_config()).expect("run succeeds"));
}

/// Documented public runner path: `run(RunConfig)`.
#[test]
fn public_smoke() {
    assert_shipper_report(run(default_config()).expect("run succeeds"));
}
