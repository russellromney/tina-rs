//! Public runner proof for the mini SaaS API.

use mini_saas_api::{RunMode, SoakConfig, SoakConfigError, run, run_soak};

/// Documented public runner path: `run(RunMode::Smoke)`.
#[test]
fn public_smoke() {
    let report = run(RunMode::Smoke).expect("smoke run");
    assert!(report.health_ok);
    assert!(report.ready_ok);
    assert!(report.created_item);
    assert!(report.read_item);
    assert!(report.notified_item);
    assert!(report.shutdown_clean);
    assert!(report.scopes_drain_unreleased_zero);
}

/// Pins accepted smoke report facts and soak config validation.
#[test]
fn public_characterization() {
    let report = run(RunMode::Smoke).expect("smoke run");
    assert!(report.missing_404);
    assert!(report.method_405);
    assert!(report.bad_request_400);
    assert!(report.body_cap_413);
    assert!(report.db_constraint_409);
    assert!(report.shutdown_in_flight_typed);
    assert!(report.multi_turn_notify);
    assert!(report.summary_line().contains("system=mini_saas_api"));

    assert!(matches!(
        SoakConfig {
            workers: 0,
            ..SoakConfig::default()
        }
        .validate(),
        Err(SoakConfigError::ZeroWorkers)
    ));
    assert!(matches!(
        SoakConfig {
            op_count: 0,
            ..SoakConfig::default()
        }
        .validate(),
        Err(SoakConfigError::ZeroOps)
    ));
    assert!(matches!(
        SoakConfig {
            connect_timeout: std::time::Duration::ZERO,
            ..SoakConfig::default()
        }
        .validate(),
        Err(SoakConfigError::ZeroConnectTimeout)
    ));
    // Validated config is accepted (not run here — soak is a longer path).
    assert!(SoakConfig::default().validate().is_ok());
    let _ = run_soak;
}
