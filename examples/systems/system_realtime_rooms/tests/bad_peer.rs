//! Bad-peer + slow-reader proof for the realtime room.
//!
//! Three things this test pins down for the system:
//!
//! 1. The HTTP front door survives a sequence of broken transport
//!    scenarios from `tina-proof-harness` (RST, half-close, malformed
//!    HTTP, slow HTTP reader) without crashing or stopping the room.
//!    Each scenario returns a typed [`BadPeerOutcome`] — no log
//!    scraping.
//!
//! 2. A real WebSocket peer that joins the room and then sends no
//!    inbound traffic is evicted by the room's typed `left_idle`
//!    counter once `idle_evict_after_ms` elapses. The slow peer does
//!    not leak its slot or take the room down. We use idle eviction
//!    (not OutboundQueueFull) because it is deterministic — the
//!    OutboundQueueFull path depends on kernel TCP send buffer sizing
//!    and would flake in CI.
//!
//! 3. After both pressure stories, a fresh good WebSocket client can
//!    still join, see the recurring tick, and shut the room down
//!    cleanly. "Pressure leaves no leak" is the visible assertion.

use std::time::{Duration, Instant};

use system_realtime_rooms::{RoomServer, RunConfig, test_client};
use tina_proof_harness::bad_peer::{self, BadPeerScenario};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

#[test]
fn bad_http_peers_and_slow_ws_peer_do_not_break_the_room() {
    let config = RunConfig {
        member_capacity: 3,
        // Tighten idle eviction so the slow-reader test is fast and
        // deterministic. The room's `last_seen` advances on inbound
        // text; the slow client never sends any, so it ages out at
        // 200ms.
        idle_evict_after_ms: 200,
        // Faster tick so the eviction loop runs while the slow client
        // is still connected.
        presence_tick_ms: 30,
        ..RunConfig::default()
    };
    let server = RoomServer::start(config);
    let addr = server.addr();

    // 1) Sequence of HTTP-level bad peers. None of these should crash
    //    the listener or cause the room to enter shutdown. Each
    //    scenario also has a typed expectation on what the server did.
    //    At this layer the stable contract is not "the peer always
    //    drains a pretty 4xx": a half-closed or malformed TCP peer can
    //    observe reply bytes, EOF, or a reset depending on the OS and
    //    close timing. The stable proof is that the outcome records that
    //    transport truth and the listener/room remains healthy.
    let reset_outcome = bad_peer::run(
        "reset",
        addr,
        CONNECT_TIMEOUT,
        BadPeerScenario::ResetImmediately,
    );
    assert!(reset_outcome.connected, "{reset_outcome:?}");
    assert_eq!(
        reset_outcome.connects_ok, 1,
        "reset must record one successful connect: {reset_outcome:?}",
    );
    assert_eq!(
        reset_outcome.bytes_sent, 0,
        "reset must not send any bytes: {reset_outcome:?}",
    );

    let half_close_outcome = bad_peer::run(
        "half_close",
        addr,
        CONNECT_TIMEOUT,
        BadPeerScenario::HalfClose {
            request: b"GET /ws HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".to_vec(),
            drain_for: Duration::from_millis(300),
        },
    );
    assert!(half_close_outcome.connected, "{half_close_outcome:?}");
    assert!(
        half_close_outcome.bytes_read > 0
            || half_close_outcome.server_closed
            || half_close_outcome.peer_reset,
        "half_close must observe a visible terminal transport fact: {half_close_outcome:?}",
    );
    assert!(
        half_close_outcome.error.is_none()
            || half_close_outcome.peer_reset
            || half_close_outcome.server_closed
            || half_close_outcome.bytes_read > 0,
        "half_close errors must be reflected as reset/close/reply truth: {half_close_outcome:?}",
    );

    let malformed_outcome = bad_peer::run(
        "malformed",
        addr,
        CONNECT_TIMEOUT,
        BadPeerScenario::MalformedFrame {
            bytes: b"NOT A REAL HTTP REQUEST\r\n\r\n".to_vec(),
            drain_for: Duration::from_millis(300),
        },
    );
    assert!(malformed_outcome.connected, "{malformed_outcome:?}");
    assert!(
        malformed_outcome.server_closed || malformed_outcome.bytes_read > 0,
        "malformed must either get a reply or a server close: {malformed_outcome:?}",
    );

    let stalled_outcome = bad_peer::run(
        "stalled_reader_no_upgrade",
        addr,
        CONNECT_TIMEOUT,
        BadPeerScenario::StalledReader {
            request: b"GET /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".to_vec(),
            stall_for: Duration::from_millis(150),
        },
    );
    assert!(stalled_outcome.connected, "{stalled_outcome:?}");
    // /health is a tiny response; the server writes it even while we
    // stall, and after stall_for elapses we drain the bytes that were
    // buffered.
    assert!(
        stalled_outcome.bytes_read > 0,
        "stalled_reader must drain /health reply after stall: {stalled_outcome:?}",
    );

    let outcomes = [
        reset_outcome,
        half_close_outcome,
        malformed_outcome,
        stalled_outcome,
    ];

    let post_bad_http = server.snapshot();
    assert!(
        !post_bad_http.shutdown_started,
        "bad HTTP must not trigger room shutdown: {post_bad_http:?}",
    );
    assert_eq!(
        post_bad_http.live_members, 0,
        "bad HTTP must not enter the room (no upgrades succeeded): {post_bad_http:?}",
    );

    // 2) Slow-reader proof at the WebSocket layer. The slow client
    //    joins, drains its `join:` reply, then sends nothing for
    //    longer than `idle_evict_after_ms`. The room's recurring tick
    //    finds the stale `last_seen` and evicts the session with the
    //    typed `left_idle` counter.
    let mut slow = test_client::connect(addr).expect("slow client connects");
    let join_text = slow.recv_text_timeout(Duration::from_millis(500));
    assert!(
        matches!(join_text, test_client::RecvOutcome::Text(ref t) if t.starts_with("join:")),
        "slow client should be admitted, saw {join_text:?}",
    );
    let after_slow = server.wait_until(Duration::from_secs(3), |s| s.left_idle >= 1);
    drop(slow);
    assert!(
        after_slow.left_idle >= 1,
        "slow WS peer must be evicted via idle path: {after_slow:?}",
    );
    let drained = server.wait_until(Duration::from_secs(2), |s| s.live_members == 0);
    assert_eq!(
        drained.live_members, 0,
        "slow peer eviction must free its slot: {drained:?}",
    );

    // 3) The room is still serving traffic. A fresh good client should
    //    be admitted, see the recurring tick, and leave cleanly.
    let mut good = test_client::connect(addr).expect("good client connects after pressure");
    let join = good.recv_text_timeout(Duration::from_millis(500));
    assert!(
        matches!(join, test_client::RecvOutcome::Text(ref t) if t.starts_with("join:")),
        "good client should see join reply after pressure, saw {join:?}",
    );
    let mut ticks = 0u64;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && ticks == 0 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        good.send_text("still-here")
            .expect("good client refreshes liveness while waiting for tick");
        if let test_client::RecvOutcome::Text(t) =
            good.recv_text_timeout(remaining.min(Duration::from_millis(100)))
            && t.starts_with("tick:")
        {
            ticks += 1;
        }
    }
    assert!(
        ticks >= 1,
        "good client must see at least one tick after pressure",
    );
    drop(good);

    let final_stats = server.stop();
    assert!(
        final_stats.joined >= 2,
        "joined counter must include slow + good clients: {final_stats:?}",
    );

    // Visible output so `cargo test -- --nocapture` shows the typed
    // bad-peer + room facts that the run actually exercised.
    for outcome in &outcomes {
        eprintln!("{}", outcome.summary_line());
    }
    eprintln!(
        "room.summary live={} joined={} left_idle={} left_slow={} left_peer={} \
         presence_ticks={} shutdown_started={}",
        final_stats.live_members,
        final_stats.joined,
        final_stats.left_idle,
        final_stats.left_slow,
        final_stats.left_peer,
        final_stats.presence_ticks,
        final_stats.shutdown_started,
    );
}
