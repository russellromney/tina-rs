//! Compile-fail proofs for `SharedWork` admission rails.
//!
//! - `SharedWorkTicket` has crate-private fields; user code cannot
//!   write a struct literal.
//! - `request_effect_after_shared_wait(permit, _)` only accepts the move-only
//!   turn permit returned beside a real ticket.
//!
//! Together these mean a `RequestEffect<I>` from `noop()` cannot
//! escape `SharedWork::wait` / `wait_call`.

#[test]
fn shared_work_ticket_cannot_be_forged_with_struct_literal() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/shared_work_compile_fail/shared_work_ticket_forged.rs");
}

#[test]
fn shared_work_request_effect_rejects_non_permit() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/shared_work_compile_fail/shared_work_request_effect_from_noop.rs");
}
