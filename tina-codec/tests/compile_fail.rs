//! Compile-fail fixtures: the codec adapter surface stays typed and the
//! `Framer` trait stays sealed.

#[test]
fn compile_fail_fixtures() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/compile_fail/*.rs");
}
