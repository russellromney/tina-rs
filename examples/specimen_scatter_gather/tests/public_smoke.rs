//! Public runner proof for the scatter/gather specimen.
//!
//! Characterization pins the aggregate arithmetic: every client gets
//! the right aggregate, nothing fails, and no Tina terminal fires.
//! Public smoke exercises the documented Tina path through the
//! coordinator and its worker shards.

use specimen_scatter_gather::{CLIENTS, Report, WORKERS, tina_impl};

fn assert_all_correct(report: Report) {
    assert_eq!(report.clients, CLIENTS);
    assert_eq!(report.workers, WORKERS);
    assert_eq!(report.aggregates_correct, CLIENTS);
    assert_eq!(report.aggregates_wrong, 0);
    assert_eq!(report.failed, 0);
    assert_eq!(report.tina_terminals, Default::default());
    assert!(report.exit_clean);
}

/// Pins aggregate arithmetic before/after host-result migration.
#[test]
fn public_characterization() {
    assert_all_correct(tina_impl::run().expect("tina side ran"));
}

/// Documented public runner path: `tina_impl::run()`.
#[test]
fn public_smoke() {
    assert_all_correct(tina_impl::run().expect("tina side ran"));
}
