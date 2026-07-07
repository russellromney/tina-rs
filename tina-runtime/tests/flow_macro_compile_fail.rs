//! `trybuild` UI tests for `tina::flow!` authority failures.

#[test]
fn flow_macro_authority_failures_are_pinned() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/flow_macro_compile_fail/double_reply.rs");
    cases.compile_fail("tests/flow_macro_compile_fail/forgotten_request.rs");
}
