//! Negative fixture: replay status cannot be supplied as a plain string.
//! `ServiceReportBuilder::replay` takes a typed `ServiceReplayStatus`. A
//! caller cannot pass `"available"` or `"unsupported"` to mean "trust me,
//! the case exists" — the typed variants force naming the case, the
//! unsupported facts, or the not-captured reason.

use tina_runtime::service_report::ServiceReportBuilder;

fn main() {
    let _ = ServiceReportBuilder::new("billing").unwrap().replay("available");
}
