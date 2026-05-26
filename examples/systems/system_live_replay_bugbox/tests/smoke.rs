//! Smoke for the live-capture → sim replay → shrink workflow.
//!
//! Asserts:
//!
//! 1. Live capture wrote events (the observer is wired).
//! 2. Live sink received exactly the non-poison messages.
//! 3. Sim replay reproduces the saved trace shape (assert_replay_case
//!    succeeded inside `run()`).
//! 4. `discover_constants` returned one row per probe seed.
//! 5. The shrunk failing history is strictly smaller than the original
//!    and still contains the poison op (the bug is preserved).

use system_live_replay_bugbox::{Op, POISON_VALUE, run};

#[test]
fn live_capture_then_sim_replay_then_shrink() {
    let report = run().expect("bugbox run");

    // 1) Live trace observer was actually wired.
    assert!(
        report.live_trace_shape.event_count > 0,
        "live trace must capture events: {report:#?}",
    );
    assert_ne!(
        report.live_trace_shape.trace_hash, 0,
        "live trace hash must be non-zero: {report:#?}",
    );

    // 2) Live sink count == non-poison sends from the canonical case.
    let original = &report.capture.history;
    let non_poison = original
        .operations()
        .iter()
        .filter(|op| matches!(op, Op::Send(v) if *v != POISON_VALUE))
        .count();
    assert_eq!(
        report.live_received, non_poison,
        "live sink should receive every non-poison send: {report:#?}",
    );

    // 3) Sim replay reproduced.
    assert_eq!(
        report.sim_report.name, "system_live_replay_bugbox_canonical",
        "sim report identity must match the case",
    );
    assert!(
        report.sim_report.event_count > 0,
        "sim report must have events: {report:#?}",
    );

    // 4) Discovered constants — one per probe seed.
    assert!(
        report.discovered.len() >= 2,
        "discover_constants should report multiple seeds: {report:#?}",
    );
    for d in &report.discovered {
        assert!(d.event_count > 0, "discovered row must be non-empty: {d:?}");
        assert_ne!(d.trace_hash, 0, "discovered hash must be non-zero: {d:?}");
    }

    // 5) Shrink result is smaller than original, still has the bug, and
    //    preserves the live replay fact set by default.
    let shrunk = &report.shrunk.capture().history;
    assert!(
        shrunk.len() < original.len(),
        "shrunk history must be strictly smaller than original: shrunk={} original={}",
        shrunk.len(),
        original.len(),
    );
    assert!(
        shrunk
            .operations()
            .iter()
            .any(|op| matches!(op, Op::Send(v) if *v == POISON_VALUE)),
        "shrunk history must still trigger the bug (contain POISON): {report:#?}",
    );
    assert!(
        report.shrunk.capture().live_facts == report.capture.live_facts,
        "live-derived shrink must preserve proving facts by default: {report:#?}",
    );

    // 6) Live pressure surface clean: this workload has no bounded
    //    queue full / closed event. Catches future regressions where
    //    the producer/sink wiring leaks pressure that the bug shape
    //    is not supposed to involve.
    assert!(
        !report.live_pressure.non_zero(),
        "live pressure must be empty for this workload; got {:?}",
        report.live_pressure,
    );
    assert!(
        !report.capture_summary.replay_blocked,
        "canonical capture should be replayable: {:?}",
        report.capture_summary,
    );
    assert!(
        report.unsupported_mismatch_seen,
        "unsupported live facts must fail closed",
    );
    assert!(
        report.summary_line.contains("saved_bugbox="),
        "summary should point at the saved overload bugbox: {}",
        report.summary_line,
    );
    assert!(
        report.saved_bugbox_path.exists(),
        "saved bugbox path should exist until the smoke test cleans it: {:?}",
        report.saved_bugbox_path,
    );
    eprintln!("{}", report.summary_line);
    std::fs::remove_file(&report.saved_bugbox_path).expect("remove saved bugbox temp file");
}
