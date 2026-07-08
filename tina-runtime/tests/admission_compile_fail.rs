//! Compile-fail proofs for the admission types.
//!
//! - [`KeyedPermit`] is move-only and cannot be released twice. The second
//!   `release(...)` after the first must not compile because the permit
//!   value has already been moved.
//! - The permit's inner fields are private; user code cannot forge a
//!   permit via a struct literal.

#[test]
fn keyed_permit_cannot_be_released_twice() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/admission_compile_fail/keyed_permit_double_release.rs");
}

#[test]
fn keyed_permit_cannot_be_forged_with_struct_literal() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/admission_compile_fail/keyed_permit_forged.rs");
}
