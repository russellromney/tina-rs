//! Public runner proof for the idempotent-retry specimen.
//!
//! Characterization pins the retry arithmetic under the default
//! config. Public smoke exercises the documented Tina path. A retry
//! is the same logical operation, so the downstream charges exactly
//! once.

use specimen_idempotent_retry::{Outcome, Report, RunConfig, run};

fn assert_delivered(config: RunConfig, report: Report) {
    assert_eq!(report.idempotency_key, config.idempotency_key);
    assert_eq!(report.outcome, Outcome::Delivered);
    // Default config: the downstream rejects `downstream_fail_first`
    // times, then accepts — first try plus two retries delivers on the
    // third attempt, well inside the retry budget.
    assert_eq!(report.attempts, config.downstream_fail_first + 1);
    assert_eq!(report.retries, config.downstream_fail_first);
    assert_eq!(report.downstream_charges, 1, "idempotent: one charge");
}

/// Pins retry arithmetic before/after host-result migration.
#[test]
fn public_characterization() {
    let config = RunConfig::default();
    let report = run(config).expect("tina side ran");
    assert_delivered(config, report);
}

/// Documented public runner path: `run(RunConfig::default())`.
#[test]
fn public_smoke() {
    let config = RunConfig::default();
    let report = run(config).expect("tina side ran");
    assert_delivered(config, report);
}
