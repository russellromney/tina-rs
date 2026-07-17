#[test]
fn public_smoke() {
    let value = specimen_grpc_counter::run_smoke().expect("public gRPC runner");
    assert_eq!(value, 7);
}

#[test]
fn public_characterization() {
    let server = specimen_grpc_counter::start_server().expect("start public gRPC server");
    let report = server
        .native_grpc_smoke()
        .expect("native gRPC client path");
    assert_eq!(report.increment_value, 7);
    assert_eq!(
        report.forbidden_status,
        tina_http::GrpcStatusCode::PermissionDenied
    );
    assert!(!report.cancel_outcome.is_empty());
    server.shutdown().expect("clean public gRPC shutdown");
}
