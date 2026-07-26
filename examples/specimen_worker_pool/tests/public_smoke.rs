//! Public runner proof for the worker-pool specimen.
//!
//! Characterization pins the pool shape (workers, clients, driver burst
//! cap) and the exact terminal outcome the crate's own smoke test pins:
//! every client gets exactly the reply its payload calls for, no wrong
//! replies, no terminal outcomes of any kind, and a clean exit.

use specimen_worker_pool::{CLIENTS, DRIVER_BURST_CAP, Report, WORKERS, tina_impl};

fn assert_report(report: &Report) {
    assert_eq!(report.clients, CLIENTS);
    assert_eq!(
        report.correct_replies, CLIENTS,
        "each reply routed to its caller with payload + worker_id: {report:?}",
    );
    assert_eq!(report.wrong_replies, 0);
    assert!(
        report.terminals.is_empty(),
        "no terminal outcomes expected: {report:?}",
    );
    assert!(report.exit_clean);
}

/// Documented public runner path: `tina_impl::run()` (the `tina` binary
/// mode).
#[test]
fn public_smoke() {
    assert_report(&tina_impl::run().expect("tina side ran"));
}

/// Pins the pool shape facts and the exact reply-routing outcome.
#[test]
fn public_characterization() {
    assert_eq!(WORKERS, 3);
    assert_eq!(CLIENTS, 8);
    assert_eq!(DRIVER_BURST_CAP, 8);
    assert_report(&tina_impl::run().expect("tina side ran"));
}
