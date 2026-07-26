//! Public runner proof for the backpressure-chain specimen.
//!
//! Characterization pins the exact deadline arithmetic: six scripted
//! requests, alternating fast/slow C hops, one domain failure, and the
//! hop-provenance split the Tina side must observe. Public smoke
//! exercises the documented Tina runner path.

use specimen_backpressure_chain::{
    FAST_C_MS, REQUEST_COUNT, Report, SLOW_C_MS, TOTAL_DEADLINE_MS, expected_tina_report, tina_impl,
};

/// Pins the exact Tina counts under the fixed script.
#[test]
fn public_characterization() {
    // The fixed script constants: 80 ms chain budget, 20 ms fast C,
    // 200 ms slow C, six driver requests.
    assert_eq!(TOTAL_DEADLINE_MS, 80);
    assert_eq!(FAST_C_MS, 20);
    assert_eq!(SLOW_C_MS, 200);
    assert_eq!(REQUEST_COUNT, 6);

    let report = tina_impl::run().expect("tina side ran");
    assert_eq!(report, expected_tina_report());
    // Exact terminal split: two fast successes, three typed C-hop
    // timeouts, one service-domain failure, nothing lost at B or the
    // caller, clean exit.
    assert_eq!(
        report,
        Report {
            successful: 2,
            c_timed_out: 3,
            b_timed_out: 0,
            caller_timeout: 0,
            full: 0,
            closed: 0,
            rejected: 0,
            domain_failure: 1,
            runtime_failure: 0,
            exit_clean: true,
        }
    );
}

/// Documented public runner path: `tina_impl::run()`
/// (`cargo run --manifest-path examples/specimen_backpressure_chain/Cargo.toml -- both`).
#[test]
fn public_smoke() {
    assert_eq!(
        tina_impl::run().expect("tina side ran"),
        expected_tina_report()
    );
}
