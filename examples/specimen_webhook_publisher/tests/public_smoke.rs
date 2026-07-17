//! Public runner proof for the webhook-publisher specimen.
//!
//! Characterization pins the recorded webhook bodies: three
//! increments arrive in order as `["1", "2", "3"]`. Public smoke
//! exercises the documented Tina path through `tina-reqwest-bridge`.

use specimen_webhook_publisher::{Report, tina_impl};

fn assert_bodies(report: Report) {
    report.assert_expected();
}

/// Pins recorded webhook bodies before/after host-result migration.
#[test]
fn public_characterization() {
    assert_bodies(tina_impl::run().expect("tina side ran"));
}

/// Documented public runner path: `tina_impl::run()`.
#[test]
fn public_smoke() {
    assert_bodies(tina_impl::run().expect("tina side ran"));
}
