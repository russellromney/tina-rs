//! Public runner proof for the cancellation-chain specimen.
//!
//! Characterization pins the cancellation arithmetic: every fanned-out
//! worker finishes (delivered before cancel or typed-rejected after),
//! and each pending wait is cancelled exactly once. Public smoke
//! exercises the documented Tina runner path.

use specimen_cancellation_chain::{
    CANCEL_AFTER_MS, FANOUT, WORK_MS, assert_tina_report_invariants, tina_impl,
};

/// Pins the exact settlement totals the exported helper enforces.
#[test]
fn public_characterization() {
    assert_eq!(FANOUT, 6);
    assert_eq!(WORK_MS, 100);
    assert_eq!(CANCEL_AFTER_MS, 30);

    let report = tina_impl::run().expect("tina side ran");
    assert_tina_report_invariants(&report);
    // Tina never preempts accepted work: every worker finishes, so the
    // delivered-plus-rejected total covers the fanout exactly, and
    // each still-pending wait was cancelled exactly once.
    assert_eq!(
        report.replies_before_cancel + report.replies_after_cancel,
        FANOUT,
        "every worker must finish, delivered or typed-rejected: {report:?}"
    );
    assert_eq!(
        report.cancel_cancelled,
        FANOUT - report.replies_before_cancel,
        "each pending call must be cancelled exactly once: {report:?}"
    );
    // The before/after-cancel split itself is a wall-clock race between
    // the 100 ms worker sleeps and the 30 ms cancel, so exact equality
    // is not pinned for it; the crate's own helper asserts the same
    // shape (`replies_before_cancel < FANOUT`).
}

/// Documented public runner path: `tina_impl::run()`
/// (`cargo run --manifest-path examples/specimen_cancellation_chain/Cargo.toml -- both`).
#[test]
fn public_smoke() {
    let report = tina_impl::run().expect("tina side ran");
    assert_tina_report_invariants(&report);
}
