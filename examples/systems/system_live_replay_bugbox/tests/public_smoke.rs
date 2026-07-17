//! Public runner proof for the live-capture → sim replay → shrink workflow.

use system_live_replay_bugbox::{Op, POISON_VALUE, run};

fn assert_bugbox(report: system_live_replay_bugbox::BugboxReport) {
    assert!(
        report.live_trace_shape.event_count > 0,
        "live trace must capture events: {report:#?}",
    );
    assert_ne!(
        report.live_trace_shape.trace_hash, 0,
        "live trace hash must be non-zero: {report:#?}",
    );

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
    assert_eq!(
        report.sim_report.output.messages_received, non_poison,
        "sim sink receive count must match live non-poison fact",
    );
    assert!(
        report.sim_report.output.poison_sent,
        "sim output must record that poison was in the history",
    );

    assert_eq!(
        report.sim_report.name, "system_live_replay_bugbox_canonical",
        "sim report identity must match the case",
    );
    assert!(
        report.sim_report.event_count > 0,
        "sim report must have events: {report:#?}",
    );

    assert!(
        report.discovered.len() >= 2,
        "discover_constants should report multiple seeds: {report:#?}",
    );

    let shrunk = &report.shrunk.capture().history;
    assert!(
        shrunk.len() < original.len(),
        "shrunk history must be strictly smaller than original",
    );
    assert!(
        shrunk
            .operations()
            .iter()
            .any(|op| matches!(op, Op::Send(v) if *v == POISON_VALUE)),
        "shrunk history must still trigger the bug",
    );
    assert!(
        report.shrunk.capture().live_facts == report.capture.live_facts,
        "live-derived shrink must preserve proving facts",
    );
    assert!(!report.live_pressure.non_zero());
    assert!(!report.capture_summary.replay_blocked);
    assert!(report.unsupported_mismatch_seen);
    assert!(report.summary_line.contains("saved_bugbox="));
    assert!(report.saved_bugbox_path.exists());
    std::fs::remove_file(&report.saved_bugbox_path).expect("remove saved bugbox temp file");
}

/// Pins live/sim receive facts and shrink properties.
#[test]
fn public_characterization() {
    assert_bugbox(run().expect("bugbox run"));
}

/// Documented public runner path: `run()`.
#[test]
fn public_smoke() {
    assert_bugbox(run().expect("bugbox run"));
}
