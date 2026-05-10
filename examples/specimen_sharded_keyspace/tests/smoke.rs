//! Smoke tests: each side runs the same script and produces the same
//! [`Report`]. Both sides use FNV-1a 64 mod-count placement over the
//! shared shard list, so each key lands on the same shard in both
//! implementations.

use specimen_sharded_keyspace::{Report, expected_report, tina_impl, tokio_impl};

fn assert_matches_expected(label: &str, report: Report) {
    assert_eq!(
        report,
        expected_report(),
        "{label} side produced an unexpected report"
    );
}

#[test]
fn tokio_smoke() {
    let report = tokio_impl::run().expect("tokio side ran");
    assert_matches_expected("tokio", report);
}

#[test]
fn tina_smoke() {
    let report = tina_impl::run().expect("tina side ran");
    assert_matches_expected("tina", report);
}

#[test]
fn tokio_and_tina_produce_byte_identical_reports() {
    let tokio = tokio_impl::run().expect("tokio side ran");
    let tina = tina_impl::run().expect("tina side ran");
    assert_eq!(tokio, tina, "tokio and tina reports diverged");
}
