//! Smoke tests: every client gets the right aggregate; no failures.

use specimen_scatter_gather::{CLIENTS, Report, WORKERS, tina_impl, tokio_impl};

fn assert_all_correct(report: Report) {
    assert_eq!(report.clients, CLIENTS);
    assert_eq!(report.workers, WORKERS);
    assert_eq!(report.aggregates_correct, CLIENTS);
    assert_eq!(report.aggregates_wrong, 0);
    assert_eq!(report.failed, 0);
    assert!(report.exit_clean);
}

#[test]
fn tokio_smoke() {
    assert_all_correct(tokio_impl::run().expect("tokio side ran"));
}

#[test]
fn tina_smoke() {
    assert_all_correct(tina_impl::run().expect("tina side ran"));
}
