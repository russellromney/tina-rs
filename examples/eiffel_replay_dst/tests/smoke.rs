//! Smoke tests: each side asserts the property the example is
//! about. Tina pins deterministic replay; Tokio pins functional
//! correctness across runs (timings drift, but the message sequence
//! is stable).

use eiffel_replay_dst::{tina_impl, tokio_impl};

#[test]
fn tina_replay_is_byte_identical_at_same_seed() {
    let report = tina_impl::run().expect("tina ran");
    assert_eq!(
        report.run_a1_event_count, report.run_a2_event_count,
        "event count is identical across replays of the same seed",
    );
    assert_eq!(
        report.run_a1_fingerprint, report.run_a2_fingerprint,
        "trace fingerprint is byte-identical across replays of the same seed",
    );
    assert_ne!(
        report.run_a1_fingerprint, report.run_b1_fingerprint,
        "trace fingerprint differs across distinct seeds (so the property is non-trivial)",
    );
}

#[test]
fn tokio_messages_are_stable_across_runs() {
    let report = tokio_impl::run().expect("tokio ran");
    assert_eq!(
        report.run1_messages, report.run2_messages,
        "tokio is functionally correct: same message sequence twice",
    );
    // Note: timings drift between runs in normal conditions, but we
    // do *not* assert that here — under unusually quiet systems they
    // can match by accident. The README explains that wall-clock
    // drift is the property Tina-sim retires.
}
