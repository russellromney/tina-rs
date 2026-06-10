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

#[test]
fn local_remote_ingress_does_not_scan_registered_entries_by_id() {
    let remote = include_str!("../src/remote.rs");

    assert!(
        !remote.contains("entries.iter().position(|entry|"),
        "local send/call ingress must use the isolate-id index instead of scanning every registered entry"
    );
    assert!(
        !remote.contains("entries\n            .iter()\n            .position(|entry|"),
        "local send/call ingress must use the isolate-id index instead of scanning every registered entry"
    );
}

#[test]
fn stopped_entry_gc_does_not_shift_remove_inside_a_loop() {
    let dispatch = include_str!("../src/dispatch.rs");

    // O(N^2): removing collectable entries one-at-a-time inside the GC
    // walk shifts the tail per removal. A burst must compact in one pass.
    assert!(
        !dispatch.contains("self.entries.remove(index)"),
        "gc_stopped_entries must compact in one pass (retain/swap), not Vec::remove inside the loop"
    );
}
