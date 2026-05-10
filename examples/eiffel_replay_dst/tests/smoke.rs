//! Smoke tests for the saved replay case.
//!
//! Tina pins deterministic replay through `assert_replay_case` against
//! a saved `ReplayCase`. Tokio pins functional correctness across two
//! runs (timings drift, but the message sequence is stable).

use eiffel_replay_dst::{tina_impl, tokio_impl};
use tina_sim::dst::assert_replay_case;

#[test]
fn saved_replay_case_replays_byte_for_byte() {
    let report = assert_replay_case(&tina_impl::case(), tina_impl::run_case);
    assert_eq!(report.output.messages_received, 6);
}

#[test]
fn saved_replay_case_runs_twice_to_the_same_shape() {
    let case = tina_impl::case();
    let first = tina_impl::run_case(&case);
    let second = tina_impl::run_case(&case);
    assert_eq!(first.event_count, second.event_count);
    assert_eq!(first.trace_hash, second.trace_hash);
    assert_eq!(first.event_count, case.expected_event_count);
    assert_eq!(first.trace_hash, case.expected_trace_hash);
}

#[test]
fn different_seed_gives_a_different_trace_hash() {
    let case = tina_impl::case();
    let baseline = tina_impl::run_case(&case);
    let mut perturbed = tina_impl::case();
    perturbed.seed = case.seed.wrapping_add(57);
    let other = tina_impl::run_case(&perturbed);
    assert_ne!(
        baseline.trace_hash, other.trace_hash,
        "seeded faults must perturb the trace hash so the saved seed property is non-trivial",
    );
}

#[test]
fn tokio_messages_are_stable_across_runs() {
    let report = tokio_impl::run().expect("tokio ran");
    assert_eq!(
        report.run1_messages, report.run2_messages,
        "tokio is functionally correct: same message sequence twice",
    );
    // Wall-clock timings drift between runs in normal conditions, but
    // we do *not* assert that here — under unusually quiet systems
    // they can match by accident. The README explains that wall-clock
    // drift is the property Tina-sim retires.
}
