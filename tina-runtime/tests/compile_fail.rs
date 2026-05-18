//! Phase 112 compile-fail proofs for protocol-fact wiring.
//!
//! These fixtures pin that the wrong protocol-fact wiring fails to compile:
//!
//! - an ordinary isolate (`Fact = Infallible`) cannot emit a `ProtocolFact`;
//! - a protocol isolate (`Fact = ProtocolFact`) cannot emit a different
//!   fact enum through `tina::fact::<Self>(...)`;
//! - an isolate whose `Fact` type lacks `IntoRuntimeFact` cannot register.
//!
//! Each fixture lives under `tests/safety_rails_compile_fail/`. The fixture
//! file is the source that must fail, and the adjacent `.stderr` file pins
//! the compiler's diagnostic shape. When a compiler upgrade changes the
//! surrounding text, regenerate with `TRYBUILD=overwrite cargo test
//! -p tina-runtime --test compile_fail`.

#[test]
fn protocol_fact_wiring_failures_fail_to_compile() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/safety_rails_compile_fail/protocol_fact_ordinary_isolate.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/protocol_fact_wrong_enum.rs");
    cases
        .compile_fail("tests/safety_rails_compile_fail/protocol_fact_missing_into_runtime_fact.rs");
}
