//! Public runner proof for the replay-DST specimen.
//!
//! Characterization runs the same saved replay the crate's own smoke
//! tests run and pins the replay facts as literals: seed 42 replays to
//! exactly 54 trace events with `stable_trace_hash`
//! `0xb968_e0f8_3f76_56b4` and six sink messages, and a different seed
//! fingerprints differently (the seed does real work).

use specimen_replay_dst::tina_impl;
use tina_sim::dst::assert_replay_case;

/// Documented public runner path: `tina_impl::run()` (the `tina` binary
/// mode), which asserts the saved case and then demos a seed sweep and
/// a deletion shrink.
#[test]
fn public_smoke() {
    let demo = tina_impl::run().expect("tina side ran");
    let case = tina_impl::case();
    assert_eq!(demo.saved.event_count, case.expected_event_count);
    assert_eq!(demo.saved.trace_hash, case.expected_trace_hash);
    assert_eq!(demo.saved.output.messages_received, 6);
    assert!(
        demo.sweep.is_ok(),
        "the demo seed sweep must keep passing: {:?}",
        demo.sweep.map(|s| s.seeds_examined),
    );
}

/// Pins the saved replay facts (P): same seed, same story — the case
/// replays byte-for-byte against the pinned event count and trace hash.
#[test]
fn public_characterization() {
    let case = tina_impl::case();
    assert_eq!(case.seed, 42);
    assert_eq!(case.expected_event_count, 54);
    assert_eq!(case.expected_trace_hash, 0xb968_e0f8_3f76_56b4);

    // The same replay the crate's own smoke tests run.
    let report = assert_replay_case(&case, tina_impl::run_case);
    assert_eq!(report.event_count, 54);
    assert_eq!(report.trace_hash, 0xb968_e0f8_3f76_56b4);
    assert_eq!(report.output.messages_received, 6);

    // Saved seed, saved bug: a different seed must fingerprint
    // differently, or the replay property would be vacuous.
    let mut perturbed = tina_impl::case();
    perturbed.seed = case.seed.wrapping_add(57);
    let other = tina_impl::run_case(&perturbed);
    assert_ne!(
        other.trace_hash, report.trace_hash,
        "seeded faults must perturb the trace hash",
    );
}
