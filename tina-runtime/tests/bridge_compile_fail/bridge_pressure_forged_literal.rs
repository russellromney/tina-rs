//! Negative fixture: `BridgePressure` fields are private. A bridge
//! author cannot forge a pressure report by writing a raw struct
//! literal — that would let a buggy adapter lie about installed
//! capacity, the surface name, or the late-result count. The only
//! allowed paths are `BridgePressure::measured(...)`, the per-bridge
//! `From<XxxPressureReport>` impls, and `BridgePressure::unavailable`.

use tina_runtime::bridge::BridgePressure;

fn main() {
    let _ = BridgePressure {
        name: String::from("aws.s3"),
        capacity: 32,
        current: 0,
        high_water: 0,
        full_count: 0,
        timeout_count: 0,
        closed_count: 0,
        late_result_count: 0,
        worker_terminal_count: 0,
    };
}
