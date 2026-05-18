//! Negative fixture: `ServiceReportBuilder` cannot be constructed by struct
//! literal. The fields are private; the only path is
//! `ServiceReportBuilder::new`, which validates the service name.

use tina_runtime::service_report::ServiceReportBuilder;

fn main() {
    let _ = ServiceReportBuilder {
        service: "billing".into(),
        lifecycle: None,
        readiness: None,
        health: None,
        topology: None,
        pressure: None,
        shutdown_finished: None,
        shutdown_pending: None,
        replay: None,
    };
}
