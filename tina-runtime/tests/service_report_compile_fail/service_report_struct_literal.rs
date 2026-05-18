//! Negative fixture: building a `ServiceReport` by struct literal must not
//! compile. The fields are private; the only path is `ServiceReportBuilder`.
//! Bypassing the builder would let a caller skip lifecycle/readiness/health/
//! topology/pressure, which is exactly the half-shaped state the rail
//! prevents.

use std::convert::Infallible;

use tina_runtime::lifecycle::{Health, Lifecycle, Readiness, ServiceTopology};
use tina_runtime::service_pressure::ServicePressureBuilder;
use tina_runtime::service_report::{ServiceReplayStatus, ServiceReport};

fn main() -> Result<(), Infallible> {
    let _ = ServiceReport {
        service: "billing".into(),
        lifecycle: Lifecycle::Ready,
        readiness: Readiness::ready(),
        health: Health::new("billing", Lifecycle::Ready),
        topology: ServiceTopology::new("billing", Lifecycle::Ready),
        pressure: ServicePressureBuilder::new("billing").unwrap().finish().unwrap(),
        shutdown: None,
        replay: ServiceReplayStatus::not_captured("nope"),
    };
    Ok(())
}
