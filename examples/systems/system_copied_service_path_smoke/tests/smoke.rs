#[test]
fn cheap_model_copy_runs() {
    let line = system_copied_service_path::smoke_line();
    assert!(line.contains("copied_service_path"), "{line}");
    assert!(line.contains("roles=3"), "{line}");
}

