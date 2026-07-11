//! Diagnostic-phrase pinning for compile-time safety rails.
//!
//! Compile-fail doctests (see `tina-runtime/src/registration.rs` and
//! `src/call/`)
//! prove that the wrong path fails to compile. They do **not** prove which
//! diagnostic phrase appeared: a fixture that broke for any other reason
//! would still pass the doctest. This test uses `trybuild` to compare full
//! `stderr` against committed fixtures so a regression that softens
//! `#[diagnostic::on_unimplemented]` on [`tina::CallableIsolate`] (or that
//! changes the `register_service` bound message) shows up as a diff against
//! `.stderr`.
//!
//! Updating the expected output: when a Rust compiler version changes the
//! surrounding text, regenerate with `TRYBUILD=overwrite cargo test
//! -p tina-runtime --test safety_rails_diagnostics` and review the diff for
//! the still-pinned Tina-facing phrases.

#[test]
fn diagnostic_phrases_are_pinned() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/safety_rails_compile_fail/missing_handle_call.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/split_message_combined.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/split_missing_handle_event.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/split_missing_handle_request.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/split_send_only.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/split_request_ignores_call.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/split_request_let_underscore.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/split_request_drop_call.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/split_request_partial_branch.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/split_request_double_consume.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/split_request_forged_effect.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/split_request_early_return.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/request_permit_fake_caller.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/request_call_fake_context.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/request_permit_forged.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/request_permit_mint.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/request_permit_clone.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/request_permit_reuse.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/request_permit_cross_isolate.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/request_permit_escape_turn.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/split_private_internal_event.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/split_request_on_event_lane.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/split_event_on_request_lane.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/event_only_with_reply.rs");
    cases.compile_fail("tests/safety_rails_compile_fail/split_request_call_in_string_literal.rs");
    cases.pass("tests/safety_rails_compile_pass/split_request_arg_named_msg.rs");
    cases.pass("tests/safety_rails_compile_pass/split_request_call_used_with_string.rs");
    cases.pass("tests/safety_rails_compile_pass/event_only_service.rs");
    cases.pass("tests/safety_rails_compile_pass/request_only_service.rs");
}
