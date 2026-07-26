//! Public runner proof for the rate-limited-worker specimen.
//!
//! Characterization pins the workload facts (burst size, queue capacity,
//! rate window) plus the structural report invariants both sides must
//! satisfy. The exact admitted/full split is timing-sensitive by design
//! — the worker may drain a slot before the producer's next push — so,
//! exactly like the crate's own smoke tests, it is not pinned to an
//! exact value.

use specimen_rate_limited_worker::{
    BURST_JOBS, QUEUE_CAPACITY, RATE_WINDOW_MS, Report, assert_report_invariants, tina_impl,
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

/// Pins the workload facts and the Tina-side report invariants: every
/// burst job accounted for, overload visible (`jobs_full > 0`), no
/// terminal submissions, every admitted job received and processed, the
/// exact `HostBurstSnapshot` partition retained, and
/// `BurstCloseSettlement::Delivered`.
#[test]
fn public_characterization() {
    assert_eq!(BURST_JOBS, 32);
    assert_eq!(QUEUE_CAPACITY, 4);
    assert_eq!(RATE_WINDOW_MS, 5);
    assert_report_invariants("tina", &run_tina());
}
