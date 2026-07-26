//! Public runner proof for the WebSocket room specimen.

use specimen_websocket_room::{RoomServer, RoomServerConfig, run};

/// Documented public runner path: `run()`.
#[test]
fn public_smoke() {
    let report = run().expect("room run");
    assert!(report.joined >= 2, "{report:?}");
    assert!(report.broadcast_ok >= 1, "{report:?}");
    assert!(
        report.client_a_received || report.client_b_received,
        "{report:?}"
    );
}

/// Pins accepted cross-client broadcast facts and startup validation.
#[test]
fn public_characterization() {
    let report = run().expect("room run");
    assert!(report.live_members <= report.member_capacity, "{report:?}");
    assert!(report.broadcast_ok >= 1, "{report:?}");

    let err = RoomServer::start_with(RoomServerConfig {
        room_capacity: 2,
        ..RoomServerConfig::default()
    });
    assert!(err.is_err(), "only one named room is supported");
}
