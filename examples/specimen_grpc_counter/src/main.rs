fn main() {
    match specimen_grpc_counter::run_smoke() {
        Ok(value) => println!("grpc-counter value={value}"),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
