#[test]
fn driver_completion_does_not_scan_or_shift_in_flight_tables_by_call_id() {
    let dispatch = include_str!("../src/dispatch.rs");

    assert!(
        !dispatch.contains("in_flight_calls\n            .iter()\n            .position"),
        "driver completion must use call-id indexes instead of scanning in-flight calls"
    );
    assert!(
        !dispatch.contains("translators\n            .iter()\n            .position"),
        "driver completion must use call-id indexes instead of scanning translators"
    );
}
