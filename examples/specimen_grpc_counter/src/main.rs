fn main() {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("serve") {
        let addr = args
            .next()
            .unwrap_or_else(|| "127.0.0.1:57057".to_owned())
            .parse()
            .expect("serve address");
        match specimen_grpc_counter::start_server_on(addr) {
            Ok(server) => {
                println!("grpc-counter listening on {}", server.addr);
                loop {
                    std::thread::park();
                }
            }
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    }
    match specimen_grpc_counter::run_smoke() {
        Ok(value) => println!("grpc-counter value={value}"),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
