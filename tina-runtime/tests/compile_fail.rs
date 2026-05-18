//! Compile-fail proofs for shared bridge vocabulary invariants.
//!
//! Each fixture is a `.rs` file under `tests/bridge_compile_fail/` with
//! a paired `.stderr` of the expected diagnostic. The fixtures prove
//! that wrong bridge wiring is a coding mistake the compiler catches:
//!
//! - `BridgePressure` fields are private, so a raw struct literal
//!   cannot forge an installed-capacity or a name;
//! - `BridgeOutcomeClass` is a typed enum, so a classifier signature
//!   cannot smuggle a string in place of one of the four typed arms.
//!
//! The plan asserts `cargo test -p tina-runtime --test compile_fail
//! -- bridge` as a proof command. This test binary takes that name and
//! filters by the leading test name `bridge_*`.
//!
//! Updating the expected output: when a Rust compiler version changes
//! the surrounding text, regenerate with
//! `TRYBUILD=overwrite cargo test -p tina-runtime --test compile_fail`
//! and review the diff for the still-pinned Tina-facing phrases.

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
