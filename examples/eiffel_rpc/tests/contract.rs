//! Pins the Tina/Tokio contracts so a regression in Connection's
//! in-flight-cap enforcement or the Registry's mapping doesn't
//! quietly produce a different demo output.

use eiffel_rpc::comparison::{run_tina_side, run_tina_typed_side, run_tokio_side};

#[test]
fn tina_side_visibly_overloads_at_in_flight_cap() {
    let burst = 4;
    let report = run_tina_side(burst);
    report.assert_tina_contract(burst);
}

#[test]
fn tokio_reference_silently_buffers_every_request() {
    let burst = 4;
    let report = run_tokio_side(burst);
    report.assert_tokio_contract(burst);
}

#[test]
fn tina_side_overload_scales_with_burst() {
    let burst = 16;
    let report = run_tina_side(burst);
    report.assert_tina_contract(burst);
}

/// Typed-surface smoke test: builds the macro-driven service,
/// registers it, accepts a connection, and produces a `SideReport`.
/// The shared raw-byte `drive_client` sends payloads the macro
/// rejects, but the connection's `max_in_flight = 1` cap fires
/// first, so over-cap requests come back as `Error(Full)` and the
/// one request that grabs the in-flight slot comes back as
/// `Error(Decode)` (counted as `other`). What we're pinning: the
/// macro plumbing (`#[service]` → `Dispatch` → `SingleService` →
/// `Registry`) wires up end-to-end without panic, and the
/// connection-level overload behavior dominates per-request decode.
#[test]
fn tina_typed_side_compiles_registers_and_routes() {
    let burst = 4;
    let report = run_tina_typed_side(burst);
    assert_eq!(
        report.ok, 0,
        "raw bytes don't satisfy the typed JSON tuple decoder"
    );
    assert_eq!(
        report.full,
        burst - 1,
        "in-flight cap dominates: over-cap requests get Full before decode runs",
    );
    assert_eq!(
        report.other, 1,
        "the one request that grabs the slot returns Decode for raw bytes",
    );
    assert_eq!(
        report.ok + report.full + report.other,
        burst,
        "every request must be accounted for: {report:?}",
    );
}
