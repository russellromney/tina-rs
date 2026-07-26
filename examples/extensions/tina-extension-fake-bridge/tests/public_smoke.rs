//! Public runner proof for the fake-bridge extension.
//!
//! Public smoke drives the documented `run()` across its four scenarios.
//! Characterization pins the exact lifecycle arithmetic: three happy
//! completions, exactly one late terminal after the caller deadline
//! fires first, one worker terminal for that abandoned job, and the
//! typed `Retryable(BridgeFull)` / `Unavailable(BridgeClosed)`
//! rejections. The worker gate makes the timeout race deterministic.

use tina_extension_fake_bridge::{Report, run};

fn assert_report(report: &Report) {
    assert_eq!(report.happy_completed, 3, "all happy jobs completed");
    assert!(report.happy_drained, "happy drain reached zero in-flight");
    assert!(
        report.caller_saw_external_may_continue,
        "a timed-out caller must be told external work may continue"
    );
    assert_eq!(
        report.late_result_count, 1,
        "the abandoned job lands as exactly one late terminal"
    );
    assert_eq!(report.worker_terminal_count, 1);
    assert!(
        report.saw_full,
        "submit past capacity is Retryable(BridgeFull)"
    );
    assert!(
        report.saw_closed,
        "submit after close is Unavailable(BridgeClosed)"
    );
}

/// Documented public runner path: `run()`.
#[test]
fn public_smoke() {
    assert_report(&run());
}

/// Pins the exact four-scenario lifecycle facts.
#[test]
fn public_characterization() {
    assert_report(&run());
}
