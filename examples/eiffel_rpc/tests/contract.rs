//! Pins the Tina/Tokio contracts so a regression in Connection's
//! in-flight-cap enforcement (Rock 2) or the Registry's mapping
//! (Rock 3) doesn't quietly produce a different demo output.

use eiffel_rpc::comparison::{run_tina_side, run_tokio_side};

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
