use system_realtime_rooms::{RoomServer, RunConfig};

#[test]
fn invalid_room_mailbox_capacity_is_a_startup_error() {
    let result = RoomServer::start(RunConfig {
        room_mailbox_capacity: 0,
        ..RunConfig::default()
    });

    assert!(result.is_err(), "zero-capacity room mailbox must be rejected");
}
