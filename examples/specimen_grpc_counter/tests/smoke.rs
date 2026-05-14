#[test]
fn specimen_grpc_counter_smoke() {
    let value = specimen_grpc_counter::run_smoke().expect("specimen smoke");
    assert_eq!(value, 7);
}
