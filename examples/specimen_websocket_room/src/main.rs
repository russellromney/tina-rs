use std::io::Read;

fn main() {
    let server = specimen_websocket_room::RoomServer::start();
    println!("ROOM_SERVER_ADDR={}", server.addr());
    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 1];
    let _ = stdin.read(&mut buf);
    let _ = server.stop();
}
