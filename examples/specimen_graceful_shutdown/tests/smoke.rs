//! Smoke tests: each side observes SIGINT, stops the producer, and
//! drains every item it produced.
//!
//! Each side has its own process-wide signal handler — running both
//! in one process means one side's handler fires during the other's
//! run. `cargo test` runs each `#[test]` in its own thread within
//! one process, but signal handlers are *process*-wide, so we run
//! the two tests sequentially and isolate setup carefully. If
//! flakiness shows up, run them in separate `cargo test` invocations.

use specimen_graceful_shutdown::{TOTAL_PLANNED_ITEMS, tina_impl, tokio_impl};

fn assert_drained(report: specimen_graceful_shutdown::Report) {
    assert!(report.signal_received, "signal must be observed");
    assert_eq!(
        report.items_remaining_in_queue_at_exit, 0,
        "every queued item must drain before exit",
    );
    assert!(
        report.items_produced > 0 && report.items_produced <= TOTAL_PLANNED_ITEMS,
        "producer should have pushed some items but stopped on signal: {report:?}",
    );
    assert_eq!(
        report.items_produced, report.items_processed,
        "every produced item must be processed",
    );
    assert!(report.exit_clean);
}

#[test]
fn tokio_smoke() {
    assert_drained(tokio_impl::run().expect("tokio side ran"));
}

#[test]
fn tina_smoke() {
    assert_drained(tina_impl::run().expect("tina side ran"));
}
