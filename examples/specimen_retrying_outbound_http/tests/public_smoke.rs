//! Public runner proof for the retrying-outbound-HTTP specimen.
//!
//! Characterization pins the retry arithmetic: the upstream returns
//! `503` for the first [`FLAKY_503_RUNS`] requests and `200` after,
//! so the third attempt succeeds. Public smoke exercises the
//! documented Tina path through `tina-reqwest-bridge`.

use specimen_retrying_outbound_http::{FLAKY_503_RUNS, Report, tina_impl};

fn assert_retried(report: Report) {
    assert_eq!(report.attempts_made, FLAKY_503_RUNS + 1);
    assert_eq!(report.transient_failures, FLAKY_503_RUNS);
    assert!(report.final_ok);
    assert!(report.exit_clean);
}

/// Pins retry arithmetic before/after host-result migration.
#[test]
fn public_characterization() {
    assert_retried(tina_impl::run().expect("tina side ran"));
}

/// Documented public runner path: `tina_impl::run()`.
#[test]
fn public_smoke() {
    assert_retried(tina_impl::run().expect("tina side ran"));
}
