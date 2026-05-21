#[test]
fn completion_delivery_does_not_scan_registered_entries_by_id() {
    let dispatch = include_str!("../src/dispatch.rs");

    assert!(
        !dispatch.contains("entries.iter().position(|entry|"),
        "completion delivery must use the isolate-id index instead of scanning every registered entry"
    );
    assert!(
        !dispatch.contains("entries\n            .iter()\n            .position(|entry|"),
        "completion delivery must use the isolate-id index instead of scanning every registered entry"
    );
}
