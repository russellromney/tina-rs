use system_metrics_shipper::{RunConfig, run};

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
