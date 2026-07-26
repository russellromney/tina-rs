//! Public runner proof for the bounded-batcher specimen.
//!
//! Characterization pins the workload constants and the terminal
//! accounting: every caller settles exactly once, no failure bucket
//! moves, and at least one batch flushes. Public smoke exercises the
//! documented Tina runner path.

use specimen_bounded_batcher::{
    BATCH_SIZE, BATCH_TIMEOUT_MS, CALLERS, MAX_PENDING, Report, SUBMISSION_CAPACITY, tina_impl,
};

fn assert_settled(report: Report) {
    assert_eq!(report.callers, CALLERS);
    assert_eq!(
        report.successes + report.full_rejects + report.failed,
        CALLERS,
        "every caller must settle exactly once: {report:?}"
    );
    assert_eq!(report.failed, 0, "{report:?}");
    assert_eq!(report.transport_full, 0, "{report:?}");
    assert_eq!(report.closed, 0, "{report:?}");
    assert_eq!(report.timeouts, 0, "{report:?}");
    assert_eq!(report.rejected, 0, "{report:?}");
    assert_eq!(report.timer_failures, 0, "{report:?}");
    assert_eq!(report.host_foreign_system, 0, "{report:?}");
    assert_eq!(report.host_parent_stopped, 0, "{report:?}");
    assert_eq!(report.host_command_full, 0, "{report:?}");
    assert_eq!(report.host_worker_stopped, 0, "{report:?}");
    assert_eq!(report.host_wait_timeout, 0, "{report:?}");
    assert_eq!(report.host_worker_unresponsive, 0, "{report:?}");
    assert_eq!(report.host_unknown_shard, 0, "{report:?}");
    assert_eq!(report.host_driver_shutdown_failed, 0, "{report:?}");
    assert_eq!(report.host_driver_park_failed, 0, "{report:?}");
    assert!(
        report.batches_size_flushed + report.batches_timer_flushed > 0,
        "expected at least one flush: {report:?}"
    );
    assert!(report.exit_clean, "{report:?}");
}

/// Pins workload constants and the terminal settlement accounting.
#[test]
fn public_characterization() {
    assert_eq!(CALLERS, 12);
    assert_eq!(BATCH_SIZE, 4);
    assert_eq!(BATCH_TIMEOUT_MS, 30);
    assert_eq!(MAX_PENDING, 16);
    assert_eq!(SUBMISSION_CAPACITY, 64);

    assert_settled(tina_impl::run().expect("tina side ran"));
    // The successes-vs-full_rejects and size-vs-timer flush splits are
    // wall-clock races between caller admission and the 30 ms batch
    // timer, so exact equality is not pinned for them; the accounting
    // above pins that every caller settles exactly once with no
    // failure bucket moving.
}

/// Documented public runner path: `tina_impl::run()`
/// (`cargo run --manifest-path examples/specimen_bounded_batcher/Cargo.toml -- tina`).
#[test]
fn public_smoke() {
    assert_settled(tina_impl::run().expect("tina side ran"));
}
