//! Public runner proof for the owned-state-leak specimen.
//!
//! The specimen is deliberately adversarial: a user-built
//! `Arc<Mutex<u64>>` is shared into an isolate on purpose. Do not "fix"
//! it — the anti-pattern is the demo. Characterization pins that it is
//! *exercised* (exactly `INTENTIONAL_ESCAPE_WRITES` writes landed through
//! the escape, proving the type system did not block it) and stays
//! *labeled* (the crate still documents exactly 4 compile-fail probes).

use specimen_owned_state_leak::{INTENTIONAL_ESCAPE_WRITES, Report, tina_impl};

fn assert_report(report: Report) {
    assert_eq!(
        report,
        Report {
            documented_compile_fails: 4,
            intentional_escape_writes: INTENTIONAL_ESCAPE_WRITES,
            exit_clean: true,
        }
    );
}

/// Pins the anti-pattern facts: the escaped `Arc<Mutex<u64>>` took
/// exactly `INTENTIONAL_ESCAPE_WRITES` writes (the type system did not
/// block it, as the README documents), the documented compile-fail probe
/// count still matches the README, and the runtime probe exited clean.
#[test]
fn public_characterization() {
    assert_report(tina_impl::run().expect("tina side ran"));
}

/// Documented public runner path: `tina_impl::run()` (the crate binary
/// calls it directly).
#[test]
fn public_smoke() {
    assert_report(tina_impl::run().expect("tina side ran"));
}
