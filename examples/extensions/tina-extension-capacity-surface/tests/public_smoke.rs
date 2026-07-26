//! Public runner proof for the custom-capacity-surface extension.
//!
//! The documented public runner is this crate's smoke suite
//! (`cargo test`), which drives the public `run()`. Characterization
//! pins the exact joined-summary arithmetic: two surfaces side by side,
//! two overflow drops on the custom ring (cap 4, push 6), and the
//! aggregate `any_full()` reflecting them.

use tina_extension_capacity_surface::{Report, run};

fn assert_report(report: &Report) {
    assert!(
        report.custom_in_summary,
        "custom surface must join the summary"
    );
    assert!(report.runtime_in_summary, "runtime surface must join too");
    assert_eq!(report.surfaces, 2, "custom + runtime, no more");
    assert_eq!(report.custom_full_count, 2, "cap 4, push 6 -> 2 dropped");
    assert!(
        report.summary_any_full,
        "summary any_full() must reflect the custom overflow"
    );
}

/// Documented public runner path: `run()`.
#[test]
fn public_smoke() {
    assert_report(&run());
}

/// Pins the exact overflow arithmetic of the documented scenario.
#[test]
fn public_characterization() {
    assert_report(&run());
}
