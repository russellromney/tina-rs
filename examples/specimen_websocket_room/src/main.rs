use std::io::Read;

fn main() -> anyhow::Result<()> {
    let tls = std::env::var_os("ROOM_SERVER_TLS").is_some();
    let stop = if tls {
        let server = specimen_websocket_room::TlsRoomServer::start()?;
        println!("ROOM_SERVER_ADDR={}", server.addr());
        Box::new(move || {
            let _ = server.stop();
        }) as Box<dyn FnOnce()>
    } else {
        let server = specimen_websocket_room::RoomServer::start()?;
        println!("ROOM_SERVER_ADDR={}", server.addr());
        Box::new(move || {
            let _ = server.stop();
        }) as Box<dyn FnOnce()>
    };
    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 1];
    let _ = stdin.read(&mut buf);
    stop();
    Ok(())
}
