use system_metrics_shipper::{RunConfig, lifecycle_for_drain_stage, run};
use tina_runtime::DrainStage;
use tina_runtime::lifecycle::{Lifecycle, ResourceKind, ShutdownStep};

#[test]
fn metrics_shipper_steady_overload_and_drain_are_typed() {
    let config = RunConfig {
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
    };
    let report = run(config).expect("run succeeds");

    let steady = &report.steady;
    assert_eq!(steady.submitted, 32);
    assert_eq!(steady.accepted, 32);
    assert_eq!(steady.dropped_full, 0);
    assert_eq!(steady.stopping_rejects, 0);
    assert_eq!(steady.shipper_mailbox_full, 0);
    assert_eq!(
        steady.sink.batches_received as usize,
        steady.stats.batches_flushed_by_size as usize
            + steady.stats.batches_flushed_by_time as usize
            + steady.stats.batches_flushed_on_drain as usize
    );
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
    assert!(
        shutdown.stats.batches_flushed_on_drain >= 1,
        "shipper stats must count the drain batch",
    );

    // Typed lifecycle surfaces. The shipper is non-HTTP so the same
    // helper proves it does not become HTTP-shaped by accident.
    let topology = &shutdown.topology;
    assert_eq!(topology.service, "system_metrics_shipper");
    assert_eq!(topology.state, Lifecycle::Ready);
    let component_kinds: std::collections::HashMap<&str, &str> = topology
        .components
        .iter()
        .map(|c| (c.name.as_str(), c.kind))
        .collect();
    assert_eq!(component_kinds.get("shipper"), Some(&"isolate"));
    assert_eq!(component_kinds.get("sink"), Some(&"isolate"));
    assert_eq!(component_kinds.get("flush_tick"), Some(&"timer"));

    assert_eq!(shutdown.health.service, "system_metrics_shipper");
    assert_eq!(shutdown.health.state, Lifecycle::Stopped);
    let health_line = shutdown.health.summary_line();
    assert!(health_line.contains("state=stopped"), "{health_line}");
    assert!(
        health_line.contains("service=system_metrics_shipper"),
        "{health_line}",
    );

    // Typed lifecycle transition: same shape as `mini_saas_api`, even
    // though the shipper drains itself rather than being host-driven.
    assert_eq!(
        shutdown.lifecycle_transitions,
        vec![
            Lifecycle::Starting,
            Lifecycle::Ready,
            Lifecycle::Draining,
            Lifecycle::Stopped,
        ],
    );

    let choreo = &shutdown.shutdown_choreography;
    assert!(choreo.clean, "shutdown choreography unclean: {choreo:#?}");
    let step_kinds: Vec<ShutdownStep> = choreo.steps.iter().map(|s| s.step).collect();
    assert_eq!(
        step_kinds,
        vec![
            ShutdownStep::DrainInFlight,
            ShutdownStep::CloseResource,
            ShutdownStep::StopOwner,
        ],
        "non-HTTP shutdown step order: {:#?}",
        choreo.steps,
    );
    let close = &choreo.closes[0];
    assert_eq!(close.name, "sink.isolate");
    assert_eq!(close.kind, ResourceKind::Child);

    // The drain stage → Lifecycle mapping is the bridge between the
    // existing `DrainState` helper and the new typed `Lifecycle`
    // vocabulary. Keep them aligned so future shipper-shaped specimens
    // can derive Lifecycle from drain state in one call.
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

#[test]
fn metrics_shipper_tick_token_invalidates_stale_timers() {
    // Tight batch size with a moderate window forces a size-triggered
    // flush before the timer can fire. The token discipline guarantees
    // the stale tick is recorded but does not double-flush.
    let report = run(RunConfig {
        events: 16,
        callers: 1,
        buffer_capacity: 16,
        batch_size: 4,
        batch_window_ms: 200,
        shipper_mailbox: 4,
        sink_mailbox: 4,
        call_timeout_ms: 2_000,
        flush_timeout_ms: 1_000,
        stop_timeout_ms: 2_000,
        sink_fail_every: 0,
        sink_flush_delay_ms: 0,
    })
    .expect("run succeeds");

    let steady = &report.steady;
    assert!(
        steady.stats.batches_flushed_by_size >= 1,
        "expected at least one size-triggered flush; got {:?}",
        steady.stats
    );
    assert_eq!(
        steady.stats.batches_flushed_by_size as usize
            + steady.stats.batches_flushed_by_time as usize
            + steady.stats.batches_flushed_on_drain as usize,
        steady.sink.batches_received as usize,
        "every batch on the wire must be counted by exactly one trigger",
    );
}
