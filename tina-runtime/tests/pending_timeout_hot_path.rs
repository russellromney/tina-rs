#[test]
fn pending_isolate_timeout_harvest_does_not_scan_the_whole_pending_table() {
    let dispatch = include_str!("../src/dispatch.rs");

    assert!(
        !dispatch.contains("pending_isolate_calls\n            .iter()\n            .enumerate()\n            .filter"),
        "timeout harvest must use the deadline index instead of scanning every pending call"
    );
    assert!(
        !dispatch.contains("pending_isolate_calls\n            .iter()\n            .position"),
        "pending call completion must use the call-id index instead of scanning every pending call"
    );
}
