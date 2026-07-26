//! Public runner proof for the per-tenant rate-limiter system.
//!
//! Public smoke drives the documented entry point
//! (`run(RunConfig::default())`, the function behind this crate's
//! `cargo test` smoke suite). Characterization pins the deterministic
//! limiter facts: the cold tenant is always fully admitted, exactly two
//! tenants ever touch the key table, and the key table never fills. The
//! exact hot admitted/limited split depends on wall-clock refill during
//! the request loop, so only its floor (the configured burst) is pinned.

use system_tenant_rate_limiter::{RunConfig, run};

/// Documented public runner path: `run(RunConfig::default())`.
#[test]
fn public_smoke() {
    let config = RunConfig::default();
    let report = run(config).expect("limiter run");

    assert!(report.hot_admitted >= config.burst as usize, "{report:?}");
    assert_eq!(
        report.hot_admitted + report.hot_limited,
        config.hot_requests,
        "{report:?}"
    );
    assert!(
        report.hot_limited > 0,
        "hot tenant never limited: {report:?}"
    );
    assert_eq!(report.cold_admitted, config.cold_requests, "{report:?}");
    assert_eq!(report.cold_limited, 0, "{report:?}");
    assert_eq!(
        report.snapshot.rate_limited_count, report.hot_limited as u64,
        "{report:?}"
    );
    assert_eq!(report.snapshot.full_count, 0, "{report:?}");
    assert_eq!(report.snapshot.live_tenants, 2, "{report:?}");
    assert!(
        report
            .snapshot
            .discovery_line
            .contains("surface=tenant.rate"),
        "discovery line: {:?}",
        report.snapshot.discovery_line
    );
}

/// Pins the deterministic limiter arithmetic under the default config.
#[test]
fn public_characterization() {
    let config = RunConfig::default();
    assert_eq!(config.mailbox, 32);
    assert_eq!(config.max_tenants, 4);
    assert_eq!(config.rate_per_sec, 10);
    assert_eq!(config.burst, 3);
    assert_eq!(config.hot_requests, 8);
    assert_eq!(config.cold_requests, 3);

    let report = run(config).expect("characterization run");
    // Deterministic: the cold tenant's fresh bucket (burst 3) admits all
    // three of its requests, and only two tenants ever touch the table.
    assert_eq!(report.cold_admitted, 3, "{report:?}");
    assert_eq!(report.cold_limited, 0, "{report:?}");
    assert_eq!(report.snapshot.live_tenants, 2, "{report:?}");
    assert_eq!(report.snapshot.full_count, 0, "{report:?}");
    assert_eq!(report.hot_admitted + report.hot_limited, 8, "{report:?}");
    // Burst floor is exact; refill beyond it is wall-clock dependent.
    assert!(report.hot_admitted >= 3, "{report:?}");
    // The retry-delay ledger tracks every limited hot request exactly.
    assert_eq!(
        report.hot_retry_afters_ms.len(),
        report.hot_limited,
        "{report:?}"
    );
    assert_eq!(
        report.snapshot.rate_limited_count, report.hot_limited as u64,
        "{report:?}"
    );
    assert!(
        report
            .summary_line
            .starts_with("system=system_tenant_rate_limiter "),
        "summary line shape: {}",
        report.summary_line
    );
}
