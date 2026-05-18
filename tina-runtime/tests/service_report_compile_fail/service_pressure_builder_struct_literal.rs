//! Negative fixture: `ServicePressureBuilder` is a builder-owned wrapper.
//! Building it by struct literal would bypass the service-name validation
//! that `ServicePressureBuilder::new` performs.
//!
//! The fields are private; the only path is `ServicePressureBuilder::new`.

use tina_runtime::service_pressure::ServicePressureBuilder;

fn main() {
    // `surfaces` and `seen` would let a caller construct a builder with
    // arbitrary, never-validated state. The compile-fail is the contract.
    let _ = ServicePressureBuilder {
        service: "bad name".into(),
        surfaces: Vec::new(),
        seen: Vec::new(),
    };
}
