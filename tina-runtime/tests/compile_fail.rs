//! Compile-fail proofs for public Tina wiring invariants.
//!
//! Each fixture is a `.rs` file with a paired `.stderr` of the expected
//! diagnostic. When a Rust compiler version changes surrounding text,
//! regenerate with:
//!
//! ```sh
//! TRYBUILD=overwrite cargo test -p tina-runtime --test compile_fail
//! ```
//!
//! Then review the diff for the still-pinned Tina-facing phrases.

#[test]
fn protocol_fact_wiring_failures_fail_to_compile() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/safety_rails_compile_fail/protocol_fact_ordinary_isolate.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/protocol_fact_wrong_enum.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/protocol_fact_missing_into_runtime_fact.rs");
}

#[test]
fn bridge_pressure_cannot_be_forged_with_raw_fields() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/bridge_compile_fail/bridge_pressure_forged_literal.rs");
}

#[test]
fn bridge_classifier_cannot_return_string_in_place_of_typed_class() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/bridge_compile_fail/bridge_classifier_stringly.rs");
}
