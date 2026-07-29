//! Public runner proof for the service-policy extension.
//!
//! Public smoke drives the documented `run()`. Characterization pins the
//! exact scripted decision sequence, the replay fact, the accumulated
//! counters, and the typed configuration rejections. The script supplies
//! every `now`, so the transcript is wall-clock independent.

use std::time::Duration;

use tina_extension_service_policy::{MAX_KEYS, PerTenantWindow, PolicyConfigError, Report, run};

/// limit=2 per 1s window, table holds 2 tenants: admit, admit,
/// rate_limited (window full), admit (new key), full (table full),
/// admit (window rolled over).
const EXPECTED_DECISIONS: [&str; 6] = ["admit", "admit", "rate_limited", "admit", "full", "admit"];

fn assert_report(report: &Report) {
    assert_eq!(report.decisions, EXPECTED_DECISIONS);
    assert!(
        report.replayed_identical,
        "same (key, now) script must replay to identical decisions"
    );
    assert_eq!(report.rate_limited, 1);
    assert_eq!(report.table_full, 1);
}

/// Documented public runner path: `run()`.
#[test]
fn public_smoke() {
    assert_report(&run());
}

/// Pins the exact decision transcript and the typed config validation.
#[test]
fn public_characterization() {
    assert_report(&run());
    assert!(matches!(
        PerTenantWindow::try_new("ext.public.zero_limit", 1, 0, Duration::from_secs(1)),
        Err(PolicyConfigError::ZeroLimit)
    ));
    assert!(matches!(
        PerTenantWindow::try_new("ext.public.zero_window", 1, 1, Duration::ZERO),
        Err(PolicyConfigError::ZeroWindow)
    ));
    assert!(matches!(
        PerTenantWindow::try_new("ext.public.zero_keys", 0, 1, Duration::from_secs(1)),
        Err(PolicyConfigError::ZeroKeys)
    ));
    assert!(matches!(
        PerTenantWindow::try_new("ext.public.too_many_keys", MAX_KEYS + 1, 1, Duration::from_secs(1)),
        Err(PolicyConfigError::TooManyKeys {
            requested,
            max: MAX_KEYS,
        }) if requested == MAX_KEYS + 1
    ));
}
