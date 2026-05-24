#[test]
fn companion_compile_api_proof() {
    system_copied_service_path_companion::compile_api_proof();
    let line = system_copied_service_path_companion::companion_proof_line();
    assert!(line.contains("roles=3"), "{line}");
}

