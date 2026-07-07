//! `trybuild` expansion-shape tests for `tina::flow!`.

#[test]
fn flow_macro_expansion_shape_is_pinned() {
    let cases = trybuild::TestCases::new();
    cases.pass("tests/flow_macro_compile_pass/renamed_paths.rs");
}
