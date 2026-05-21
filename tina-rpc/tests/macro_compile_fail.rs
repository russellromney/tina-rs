//! `trybuild` UI tests pinning `#[tina_rpc::service]` diagnostics.
//!
//! A `compile_fail` doctest would prove a fixture fails, but not *why*. These
//! pin the actual diagnostic phrase so a regression to an opaque generated-code
//! error (e.g. E0415) is caught. To refresh after a deliberate wording change,
//! regenerate with `TRYBUILD=overwrite cargo test -p tina-rpc --test macro_compile_fail`.

#[test]
fn service_macro_diagnostics_are_pinned() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/macro_compile_fail/reserved_arg_name.rs");
}
