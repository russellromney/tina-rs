//! Public runner proof for the Tower timeout counter specimen.
//!
//! Characterization pins the workload facts (timeout, handler cost,
//! concurrency limit, burst) plus the structural invariants both sides
//! must satisfy. The exact 200/503/504 split is timing-sensitive by
//! design — Tokio's `ConcurrencyLimit` pipelines two at a time while the
//! Tina isolate handles one mailbox message per turn — so, exactly like
//! the crate's own smoke tests, it is not pinned to an exact value.

use specimen_tower_timeout_counter::{
    BURST, CONCURRENCY, Report, SLOW_HANDLER_MS, TIMEOUT_MS, assert_report_invariants, tina_impl,
};

fn run_tina() -> Report {
    tina_impl::run().expect("tina side ran")
}

/// Documented public runner path: `tina_impl::run()` (the `tina` binary
/// mode).
#[test]
fn public_smoke() {
    assert_report_invariants("tina", &run_tina());
}

/// Pins the workload facts and the Tina-side invariants: every one of
/// the `BURST` calls produces exactly one outcome bucket, at least one
/// call succeeds, and the side exits clean.
#[test]
fn public_characterization() {
    assert_eq!(TIMEOUT_MS, 150);
    assert_eq!(SLOW_HANDLER_MS, 100);
    assert_eq!(CONCURRENCY, 2);
    assert_eq!(BURST, 8);
    assert_report_invariants("tina", &run_tina());
}
