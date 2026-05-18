//! Compile-fail rails for the Phase 111 service product surface.
//!
//! These fixtures pin two invariants of the
//! [`tina_runtime::service_report`] surface:
//!
//! 1. Builder-owned types ([`ServicePressureBuilder`],
//!    [`ServiceReportBuilder`], [`ServiceReport`]) have private fields. A
//!    struct-literal construction outside the module would bypass name
//!    validation and the required-field check; the rail makes that bypass
//!    a compile error.
//! 2. Replay status is a typed enum. A caller cannot smuggle in a free-text
//!    label like `"available"`/`"unavailable"` and avoid naming the case,
//!    the unsupported facts, or the not-captured reason.
//!
//! Updating expected stderr: regenerate with `TRYBUILD=overwrite cargo test
//! -p tina-runtime --test compile_fail` and review the diff. The
//! Tina-facing phrases (field privacy, type mismatch on `ServiceReplayStatus`)
//! must remain.
//!
//! [`ServicePressureBuilder`]: tina_runtime::service_pressure::ServicePressureBuilder
//! [`ServiceReportBuilder`]: tina_runtime::service_report::ServiceReportBuilder
//! [`ServiceReport`]: tina_runtime::service_report::ServiceReport

#[test]
fn service_report_rails_are_pinned() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/service_report_compile_fail/service_report_struct_literal.rs");
    cases.compile_fail(
        "tests/service_report_compile_fail/service_pressure_builder_struct_literal.rs",
    );
    cases
        .compile_fail("tests/service_report_compile_fail/service_report_builder_struct_literal.rs");
    cases.compile_fail("tests/service_report_compile_fail/replay_status_stringly.rs");
}
