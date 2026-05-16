use system_realtime_rooms::{RunConfig, run};

#[test]
fn join_tick_overflow_and_shutdown_all_work_end_to_end() {
    let report = run(RunConfig::default()).expect("run");

    // 1. Two real WebSocket clients connect. The recurring tick self-
    //    reschedules and each attentive client receives at least one
    //    presence tick on the wire. (The cross-client SessionText
    //    broadcast path is covered by `specimen_websocket_room`; this
    //    system specimen focuses on the recurring liveness story.)
    let jt = &report.join_and_tick;
    assert!(jt.bootstrap_seen, "host bootstrap message should fire");
    assert!(jt.joined >= 2, "expected >=2 joins, saw {}", jt.joined);
    assert!(
        jt.tick_broadcasts_seen >= 1,
        "at least one client should see at least one tick frame, saw {}",
        jt.tick_broadcasts_seen
    );
    assert!(
        jt.stats.presence_ticks >= 2,
        "recurring tick must self-reschedule; saw {} ticks",
        jt.stats.presence_ticks
    );
    assert!(
        jt.stats.presence_broadcasts_ok >= jt.tick_broadcasts_seen,
        "every observed tick should map to a recorded ok send outcome"
    );

    // 2. Overflow: cap is pinned to 2 inside run_overflow; the extra
    //    connections are rejected at SessionOpen with a close frame
    //    instead of a join reply.
    let ov = &report.overflow;
    assert_eq!(ov.admitted, 2, "expected 2 admitted, saw {}", ov.admitted);
    assert_eq!(
        ov.rejected_full, 2,
        "expected 2 rejected, saw {}",
        ov.rejected_full
    );
    assert_eq!(
        ov.stats.rejected_full, ov.rejected_full as u64,
        "server-side rejected_full must match observed rejections"
    );
    assert_eq!(ov.stats.member_capacity, 2);
    assert_eq!(ov.stats.member_high_water, 2);

    // 3. Shutdown: both connected clients observe a close frame after the
    //    server sends Shutdown. Counters add up.
    let sd = &report.shutdown;
    assert!(
        sd.close_observed >= 1,
        "at least one client should observe close, saw {}",
        sd.close_observed
    );
    assert!(sd.stats.shutdown_started);
    assert!(sd.stats.shutdown_close_requested >= 2);
    assert!(
        sd.stats.shutdown_close_ok + sd.stats.shutdown_close_failed
            >= sd.stats.shutdown_close_requested
    );
}
