//! Smoke tests.
//!
//! Each side dynamically spawns N workers, joins their partial sums,
//! and produces the same total.

use specimen_dynamic_worker_pool::{
    WORK_VALUES, WORKER_COUNT, assert_tina_report_accounted, expected_report, tina_impl, tokio_impl,
};
use tina_runtime::assert_service_owned_bound;

#[test]
fn tokio_smoke() {
    assert_eq!(
        tokio_impl::run().expect("tokio side ran"),
        expected_report()
    );
}

#[test]
fn tina_smoke() {
    let report = tina_impl::run().expect("tina side ran");
    assert_tina_report_accounted(&report);
    assert_eq!(report, expected_report());
    assert_service_owned_bound(
        "specimen_dynamic_worker_pool.workers",
        Some(WORKER_COUNT as usize),
        Some(report.results_collected as usize),
    )
    .expect("worker fanout stayed under service-owned cap");
}

#[test]
fn tina_child_panic_settles_as_rejected_without_hanging_parent() {
    let report = tina_impl::run_with_failure(Some(0)).expect("tina failure path ran");
    assert_tina_report_accounted(&report);
    assert_eq!(report.results_collected, WORKER_COUNT - 1);
    assert_eq!(report.rejected_handler_panicked, 1, "{report:?}");
    assert_eq!(report.total_sum, WORK_VALUES[4..].iter().sum());
}
